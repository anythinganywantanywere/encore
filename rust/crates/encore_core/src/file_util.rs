//! File utilities for the daemon, mirroring the small helpers in C++.
//!
//! Verified from the C++ sources:
//!
//! - `write2file` in `jni/include/Write2File.hpp` truncates a file
//!   in place and returns `false` on open failure. That pattern is
//!   **not** reproduced here: important files must use
//!   [`crate::atomic_write::write_atomic`] instead (analysis
//!   finding 4). This module therefore provides only the safe
//!   replacements and the read helpers the daemon actually needs.
//! - `get_uid_by_package_name` in `ProcessUtility.cpp` stats
//!   `/data/data/<package>`; the existence check in
//!   [`is_installed`] is the same idea, without reading a uid.
//! - `pidof` in `ProcessUtility.cpp` scans `/proc` for a matching
//!   command line. PID tracking itself is Stage 4 work and needs a
//!   `kill(pid, 0)` liveness check, which cannot be done without
//!   unsafe code or a new dependency under
//!   `#![forbid(unsafe_code)]`; it is deliberately not faked here
//!   with a process-list scan.
//!
//! All functions in this module take explicit paths. No path is
//! constructed from untrusted input here; callers must pass a
//! validated, allowlisted path.

use std::fs;
use std::path::Path;

use crate::atomic_write::{write_atomic, AtomicWriteError};

/// Check whether a package is installed, by checking for its data
/// directory under `data_dir`, mirroring the `access(F_OK)` check in
/// `GameRegistry::populate_from_base`.
#[must_use]
pub fn is_installed(data_dir: &Path, package_name: &str) -> bool {
    data_dir.join(package_name).exists()
}

/// Ensure the parent directory of `path` exists, creating it and
/// any missing ancestors if needed.
///
/// A path with no parent, or with an empty parent, is a no-op that
/// succeeds, because such a path is relative to the current directory.
///
/// # Errors
///
/// Returns an I/O error if the directory cannot be created.
pub fn ensure_parent_dir(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

/// Read a whole file as text.
///
/// # Errors
///
/// Returns an I/O error if the file cannot be opened or read, or if
/// it is not valid UTF-8.
pub fn read_text(path: &Path) -> std::io::Result<String> {
    fs::read_to_string(path)
}

/// Read the first line of a file, without its newline.
///
/// Returns `None` when the file is missing, unreadable, or empty.
///
/// # Errors
///
/// This function returns `None`, not an error, for those cases,
/// because callers such as the default-governor reader treat a
/// missing file as "use the fallback".
#[must_use]
pub fn read_first_line(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    text.lines().next().map(str::to_owned)
}

/// Write text to `path` atomically, with explicit permissions.
///
/// This is the safe replacement for C++ `write2file`: the content is
/// written to a temporary file in the same directory, synced, and
/// renamed over the target, so a kill mid-write cannot leave a
/// truncated file behind.
///
/// # Errors
///
/// Returns [`AtomicWriteError`] if the path is unusable, or if
/// creating, writing, syncing, or renaming fails.
pub fn write_text_atomic(
    path: &Path,
    contents: &str,
    permissions: u32,
) -> Result<(), AtomicWriteError> {
    write_atomic(path, contents.as_bytes(), permissions)
}

/// The size of a file in bytes, or `0` when it cannot be read.
#[must_use]
pub fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |meta| meta.len())
}

/// Whether a file contains a JSON object, mirroring the first
/// checks in C++ `DeviceMitigationStore::load_config`: the file
/// must exist, parse as JSON, and have an object root. The
/// mitigation rules themselves are not parsed here; they are
/// used by the profiler in a later stage.
#[must_use]
pub fn is_json_object(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    value.is_object()
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_parent_dir, file_size, is_installed, is_json_object, read_first_line,
        write_text_atomic,
    };
    use std::fs;

    fn temp_dir_for(test_name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("encore-file-{test_name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(fs::create_dir_all(&dir).is_ok());
        dir
    }

    #[test]
    fn installed_check_uses_data_dir() {
        let dir = temp_dir_for("installed");
        let data_dir = dir.join("data");
        assert!(fs::create_dir_all(data_dir.join("com.a")).is_ok());
        assert!(is_installed(&data_dir, "com.a"));
        assert!(!is_installed(&data_dir, "com.b"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parent_dir_and_first_line_and_size() {
        let dir = temp_dir_for("parent");
        let path = dir.join("sub").join("file.txt");
        assert!(ensure_parent_dir(&path).is_ok());
        assert!(path.parent().is_some_and(std::path::Path::exists));
        assert!(fs::write(&path, "first\nsecond\n").is_ok());
        assert_eq!(read_first_line(&path), Some("first".to_owned()));
        assert_eq!(read_first_line(&dir.join("missing")), None);
        assert_eq!(file_size(&path), 13);
        assert_eq!(file_size(&dir.join("missing")), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_object_check_rejects_missing_invalid_and_array() {
        let dir = temp_dir_for("json-object");
        let object_path = dir.join("object.json");
        let array_path = dir.join("array.json");
        let invalid_path = dir.join("invalid.json");
        assert!(fs::write(&object_path, "{\"a\":1}").is_ok());
        assert!(fs::write(&array_path, "[1,2]").is_ok());
        assert!(fs::write(&invalid_path, "{not json").is_ok());
        assert!(is_json_object(&object_path));
        assert!(!is_json_object(&array_path));
        assert!(!is_json_object(&invalid_path));
        assert!(!is_json_object(&dir.join("missing.json")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_text_write_round_trips() {
        let dir = temp_dir_for("atomic");
        let path = dir.join("out.txt");
        assert!(write_text_atomic(&path, "hello\n", 0o644).is_ok());
        assert_eq!(fs::read_to_string(&path).unwrap_or_default(), "hello\n");
        let _ = fs::remove_dir_all(&dir);
    }
}
