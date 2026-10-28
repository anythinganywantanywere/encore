//! Inotify file watching, mirroring `InotifyWatcher`.
//!
//! Verified from `jni/base/InotifyWatcher/InotifyWatcher.cpp` and
//! `jni/InotifyHandler.cpp`:
//!
//! - The watcher is created with `inotify_init1(O_NONBLOCK |
//!   O_CLOEXEC)`, and a stop eventfd lets a second thread wake the
//!   poll loop. This Rust watcher is simpler: it exposes a
//!   blocking `next_events` call on a non-blocking inotify file
//!   descriptor, and the daemon loop polls it with a sleep, so no
//!   stop eventfd or watcher thread is needed in Stage 4. A
//!   graceful stop from another thread is Stage 7 work, when the
//!   full daemon loop lands.
//! - `addFile` watches the file's **parent directory**, not the
//!   file itself, with the mask `IN_MODIFY | IN_CLOSE_WRITE |
//!   IN_DELETE | IN_MOVE | IN_CREATE`, and matches events by file
//!   name. A trailing separator on the path is stripped first.
//! - `addDirectory` watches the directory itself with
//!   `IN_CREATE | IN_DELETE | IN_MODIFY | IN_MOVED_FROM |
//!   IN_MOVED_TO | IN_CLOSE_WRITE`, and reports the full path of
//!   the changed entry.
//! - The handler in `InotifyHandler.cpp` reacts only to
//!   `IN_CLOSE_WRITE` for JSON files (gamelist, config,
//!   device mitigation), and only to `IN_CREATE` with the name
//!   `update` for the module-update directory. This module
//!   reports raw events; the daemon loop in `encored` applies
//!   those two filters, so the filtering rule is testable in
//!   one place and not hidden inside the watcher.
//! - Adding the same file twice, or a path with no directory
//!   part or an empty file name, fails in C++. This module
//!   returns an error for those cases instead of a bool, so the
//!   caller cannot ignore the failure silently.
//!
//! Implementation note: inotify and `kill` need operating-system
//! calls. They are done through `nix`, a maintained safe
//! wrapper, so this crate keeps `#![forbid(unsafe_code)]` and
//! contains no `unsafe` block of its own.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify, InotifyEvent, WatchDescriptor};
use thiserror::Error;

/// Which kind of watch an event came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchKind {
    /// A specific file inside a watched directory.
    File,
    /// A directory watched as a whole.
    Directory,
}

/// The type of a single inotify event, in priority order.
/// An inotify event can carry several mask bits; when it does,
/// the first matching type in this order wins, because the
/// daemon only distinguishes creation, close-write, deletion,
/// and modification, in that order of importance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchEventType {
    /// An entry was created.
    Created,
    /// A writer closed a file after writing to it.
    ClosedWrite,
    /// An entry was deleted or moved away.
    Deleted,
    /// An entry was modified.
    Modified,
    /// Any other event type.
    Other,
}

/// A single inotify event, translated from the raw event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchEvent {
    /// The kind of watch that produced this event.
    pub kind: WatchKind,
    /// The full path of the changed file, or of the directory
    /// plus the changed entry name for directory watches.
    pub path: PathBuf,
    /// The file name inside the watched directory, if any.
    /// For file watches this is always the watched file's name;
    /// for directory watches it is the changed entry's name.
    pub name: Option<String>,
    /// The type of this event.
    pub event_type: WatchEventType,
}

/// Errors from creating or using a watcher.
#[derive(Debug, Error)]
pub enum WatcherError {
    /// The path is not usable as a file watch (no directory part,
    /// or an empty file name).
    #[error("invalid watch path: {0}")]
    InvalidPath(String),
    /// The same file or directory is already watched.
    #[error("already watched: {0}")]
    AlreadyWatched(String),
    /// An inotify system call failed.
    #[error("inotify error: {0}")]
    Nix(#[from] nix::Error),
}

/// One watched directory, keyed by its watch descriptor.
///
/// C++ `addFile` reuses an existing directory watch when a second
/// file in the same directory is added, instead of adding a second
/// inotify watch on that directory (which would return the same
/// descriptor and overwrite the first file). This struct mirrors
/// that: one descriptor per directory, with a list of watched
/// file names, plus a flag for a whole-directory watch.
#[derive(Debug)]
struct Watch {
    directory: PathBuf,
    file_names: Vec<String>,
    whole_directory: bool,
}

/// An inotify watcher, mirroring `InotifyWatcher`.
#[derive(Debug)]
pub struct Watcher {
    inotify: Inotify,
    watches: HashMap<WatchDescriptor, Watch>,
}

impl Watcher {
    /// Create a new watcher with a non-blocking, close-on-exec
    /// inotify file descriptor, as in C++.
    ///
    /// # Errors
    ///
    /// Returns an error if inotify cannot be initialised.
    pub fn new() -> Result<Self, WatcherError> {
        let inotify = Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC)?;
        Ok(Self {
            inotify,
            watches: HashMap::new(),
        })
    }

    /// Watch a specific file, by watching its parent directory
    /// with the C++ file mask, and matching events by file name.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid path, a duplicate watch,
    /// or an inotify failure.
    pub fn add_file(&mut self, path: &Path) -> Result<(), WatcherError> {
        let cleaned = strip_trailing_separator(path);
        let Some(directory) = cleaned.parent().map(Path::to_path_buf) else {
            return Err(WatcherError::InvalidPath(path.display().to_string()));
        };
        if directory.as_os_str().is_empty() {
            return Err(WatcherError::InvalidPath(path.display().to_string()));
        }
        let Some(file_name) = cleaned
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            return Err(WatcherError::InvalidPath(path.display().to_string()));
        };
        if file_name.is_empty() {
            return Err(WatcherError::InvalidPath(path.display().to_string()));
        }
        // Reuse an existing watch on this directory, as C++ does.
        if let Some(watch) = self
            .watches
            .values_mut()
            .find(|watch| watch.directory == directory)
        {
            if watch.file_names.iter().any(|name| name == &file_name) {
                return Err(WatcherError::AlreadyWatched(path.display().to_string()));
            }
            watch.file_names.push(file_name);
            return Ok(());
        }
        let mask = AddWatchFlags::IN_MODIFY
            | AddWatchFlags::IN_CLOSE_WRITE
            | AddWatchFlags::IN_DELETE
            | AddWatchFlags::IN_MOVE
            | AddWatchFlags::IN_CREATE;
        let descriptor = self.inotify.add_watch(&directory, mask)?;
        self.watches.insert(
            descriptor,
            Watch {
                directory,
                file_names: vec![file_name],
                whole_directory: false,
            },
        );
        Ok(())
    }

    /// Watch a directory as a whole, with the C++ directory mask.
    ///
    /// # Errors
    ///
    /// Returns an error for a duplicate watch, or an inotify failure.
    pub fn add_directory(&mut self, path: &Path) -> Result<(), WatcherError> {
        let cleaned = strip_trailing_separator(path);
        // Reuse an existing watch on this directory, as C++ does
        // when a directory already has a file watch: just mark it
        // as a whole-directory watch. The file and directory masks
        // in C++ cover the same event set, so reusing the
        // descriptor does not lose events.
        if let Some(watch) = self
            .watches
            .values_mut()
            .find(|watch| watch.directory == cleaned)
        {
            if watch.whole_directory {
                return Err(WatcherError::AlreadyWatched(path.display().to_string()));
            }
            watch.whole_directory = true;
            return Ok(());
        }
        let mask = AddWatchFlags::IN_CREATE
            | AddWatchFlags::IN_DELETE
            | AddWatchFlags::IN_MODIFY
            | AddWatchFlags::IN_MOVED_FROM
            | AddWatchFlags::IN_MOVED_TO
            | AddWatchFlags::IN_CLOSE_WRITE;
        let descriptor = self.inotify.add_watch(&cleaned, mask)?;
        self.watches.insert(
            descriptor,
            Watch {
                directory: cleaned,
                file_names: Vec::new(),
                whole_directory: true,
            },
        );
        Ok(())
    }

    /// Read pending events, if any, and translate them to
    /// [`WatchEvent`] values.
    ///
    /// Events for a file watch are only returned when the event
    /// name matches the watched file. Events for a directory watch
    /// are returned for any entry in that directory. Events for a
    /// watch that is no longer known are dropped, as in C++.
    ///
    /// This call is non-blocking: it returns an empty vector when
    /// no events are pending.
    ///
    /// # Errors
    ///
    /// Returns an error if reading inotify fails for a reason
    /// other than "no events are pending".
    pub fn next_events(&mut self) -> Result<Vec<WatchEvent>, WatcherError> {
        let raw_events = match self.inotify.read_events() {
            Ok(events) => events,
            Err(nix::Error::EAGAIN) => return Ok(Vec::new()),
            Err(error) => return Err(WatcherError::Nix(error)),
        };
        let mut out = Vec::new();
        for event in raw_events {
            if let Some(translated) = self.translate(&event) {
                out.push(translated);
            }
        }
        Ok(out)
    }

    /// Translate one raw inotify event, or return `None` when it
    /// should be dropped.
    fn translate(&self, event: &InotifyEvent) -> Option<WatchEvent> {
        let watch = self.watches.get(&event.wd)?;
        let name_owned = event
            .name
            .as_ref()
            .map(|name| name.to_string_lossy().into_owned());
        let name_str = name_owned.as_deref();
        let event_type = if event.mask.contains(AddWatchFlags::IN_CREATE) {
            WatchEventType::Created
        } else if event.mask.contains(AddWatchFlags::IN_CLOSE_WRITE) {
            WatchEventType::ClosedWrite
        } else if event.mask.contains(AddWatchFlags::IN_DELETE)
            || event.mask.contains(AddWatchFlags::IN_MOVED_FROM)
        {
            WatchEventType::Deleted
        } else if event.mask.contains(AddWatchFlags::IN_MODIFY) {
            WatchEventType::Modified
        } else {
            WatchEventType::Other
        };
        if let Some(name) = name_str {
            if watch.file_names.iter().any(|file| file == name) {
                return Some(WatchEvent {
                    kind: WatchKind::File,
                    path: watch.directory.join(name),
                    name: Some(name.to_owned()),
                    event_type,
                });
            }
        }
        if watch.whole_directory {
            let full_path = match name_str {
                Some(entry) => watch.directory.join(entry),
                None => watch.directory.clone(),
            };
            return Some(WatchEvent {
                kind: WatchKind::Directory,
                path: full_path,
                name: name_str.map(str::to_owned),
                event_type,
            });
        }
        None
    }
}

/// Strip a trailing `/` from a path, as C++ `addFile` does, except
/// for the root path itself.
fn strip_trailing_separator(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let stripped = text.strip_suffix('/').unwrap_or(&text);
    if stripped.is_empty() {
        PathBuf::from("/")
    } else {
        PathBuf::from(stripped)
    }
}

#[cfg(test)]
mod tests {
    use super::{WatchEventType, Watcher, WatcherError};
    use std::fs;
    use std::time::Duration;

    fn temp_dir_for(test_name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("encore-watch-{test_name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(fs::create_dir_all(&dir).is_ok());
        dir
    }

    fn wait_for_events(watcher: &mut Watcher) -> Vec<super::WatchEvent> {
        for _ in 0..50 {
            if let Ok(events) = watcher.next_events() {
                if !events.is_empty() {
                    return events;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Vec::new()
    }

    #[test]
    fn file_watch_reports_close_write_for_the_watched_file_only() -> Result<(), WatcherError> {
        let dir = temp_dir_for("file");
        let target_file = dir.join("gamelist.json");
        let other = dir.join("other.json");
        assert!(fs::write(&target_file, "{}").is_ok());
        assert!(fs::write(&other, "{}").is_ok());
        let mut watcher = Watcher::new()?;
        assert!(watcher.add_file(&target_file).is_ok());
        assert!(matches!(
            watcher.add_file(&target_file),
            Err(WatcherError::AlreadyWatched(_))
        ));
        assert!(fs::write(&other, "{}").is_ok());
        assert!(fs::write(&target_file, "{\"a\":1}").is_ok());
        let events = wait_for_events(&mut watcher);
        assert!(
            events.iter().any(|event| event.path == target_file
                && event.event_type == WatchEventType::ClosedWrite),
            "expected a close-write event for the watched file, got {events:?}"
        );
        assert!(
            !events.iter().any(|event| event.path == other),
            "events for other files must be dropped, got {events:?}"
        );
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn multiple_files_in_the_same_directory_are_all_reported() -> Result<(), WatcherError> {
        let dir = temp_dir_for("multi");
        let first = dir.join("gamelist.json");
        let second = dir.join("config.json");
        assert!(fs::write(&first, "{}").is_ok());
        assert!(fs::write(&second, "{}").is_ok());
        let mut watcher = Watcher::new()?;
        assert!(watcher.add_file(&first).is_ok());
        assert!(watcher.add_file(&second).is_ok());
        assert!(fs::write(&first, "{\"a\":1}").is_ok());
        assert!(fs::write(&second, "{\"b\":1}").is_ok());
        let events = wait_for_events(&mut watcher);
        assert!(
            events.iter().any(|event| event.path == first
                && event.event_type == WatchEventType::ClosedWrite),
            "expected a close-write event for the first file, got {events:?}"
        );
        assert!(
            events.iter().any(
                |event| event.path == second && event.event_type == WatchEventType::ClosedWrite
            ),
            "expected a close-write event for the second file, got {events:?}"
        );
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn whole_directory_reports_created_entry() -> Result<(), WatcherError> {
        let dir = temp_dir_for("dir");
        let mut watcher = Watcher::new()?;
        assert!(watcher.add_directory(&dir).is_ok());
        assert!(fs::write(dir.join("update"), "").is_ok());
        let events = wait_for_events(&mut watcher);
        assert!(
            events
                .iter()
                .any(|event| event.event_type == WatchEventType::Created
                    && event.name.as_deref() == Some("update")),
            "expected a create event for 'update', got {events:?}"
        );
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn invalid_file_paths_are_rejected() -> Result<(), WatcherError> {
        let mut watcher = Watcher::new()?;
        assert!(matches!(
            watcher.add_file(std::path::Path::new("no-directory")),
            Err(WatcherError::InvalidPath(_))
        ));
        assert!(matches!(
            watcher.add_file(std::path::Path::new("/")),
            Err(WatcherError::InvalidPath(_))
        ));
        Ok(())
    }
}
