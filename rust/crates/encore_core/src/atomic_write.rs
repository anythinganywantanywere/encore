//! Atomic file writing for important files.
//!
//! The C++ daemon truncates files in place (`ModuleProperty::Change`,
//! `write2file`, `GameRegistry::populate_from_base`). The user's analysis
//! records this as finding 4: if the process dies mid-write, the file
//! can be left empty or corrupt, and `service.sh` already restores
//! `module.prop` from `.orig`, which suggests the corruption has happened
//! before.
//!
//! This module is the replacement pattern required by the task guide
//! (section 3.2): write to a temporary file in the same directory,
//! `fsync` it, set explicit permissions, then `rename` over the target.
//! It is pure I/O, not pure logic, so its tests use a temporary directory
//! instead of a real device path, as required by the guide's testing
//! section.
//!
//! No path is constructed from untrusted input here; callers must pass
//! a validated, allowlisted path (see `rust/VERIFICATION.md`).

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Counter used only to make temporary file names unique within a process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// An error from atomic writing.
#[derive(Debug, thiserror::Error)]
pub enum AtomicWriteError {
    /// The target path had no parent directory.
    #[error("target path has no parent directory")]
    NoParent,
    /// The target file name was missing.
    #[error("target path has no file name")]
    NoFileName,
    /// An underlying I/O operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Write `contents` to `path` atomically, with explicit permissions.
///
/// The temporary file is created in the same directory as `path`, so the
/// final `rename` cannot cross filesystems. Permissions are set on the
/// temporary file before the rename, so the target never inherits the
/// caller's umask.
///
/// # Errors
///
/// Returns [`AtomicWriteError`] if the path is unusable, or if creating,
/// writing, syncing, or renaming the temporary file fails. On failure the
/// temporary file is removed on a best-effort basis; the original target,
/// if any, is left untouched by the rename step, because `rename` is only
/// attempted after the temporary file has been fully written and synced.
pub fn write_atomic(
    path: &Path,
    contents: &[u8],
    permissions: u32,
) -> Result<(), AtomicWriteError> {
    let parent = path.parent().ok_or(AtomicWriteError::NoParent)?;
    let file_name = path
        .file_name()
        .ok_or(AtomicWriteError::NoFileName)?
        .to_string_lossy();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp_name = format!(".{file_name}.tmp-{}-{counter}", std::process::id());
    let temp_path: PathBuf = parent.join(temp_name);

    let result = write_atomic_inner(&temp_path, contents, permissions, path);
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn write_atomic_inner(
    temp_path: &Path,
    contents: &[u8],
    permissions: u32,
    target: &Path,
) -> Result<(), AtomicWriteError> {
    let mut file = fs::File::create(temp_path)?;
    file.set_permissions(fs::Permissions::from_mode(permissions))?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temp_path, target)?;
    // Best-effort directory sync so the rename itself is durable.
    // A failure here does not undo a successful rename, and the caller
    // already has a complete file at the target.
    if let Some(parent) = target.parent() {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::write_atomic;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir_for(test_name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("encore-atomic-{test_name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(fs::create_dir_all(&dir).is_ok());
        dir
    }

    #[test]
    fn writes_content_and_sets_permissions() {
        let dir = temp_dir_for("write");
        let target = dir.join("module.prop");
        assert!(write_atomic(&target, b"id=encore\n", 0o644).is_ok());
        let content = fs::read(&target).unwrap_or_default();
        assert_eq!(content, b"id=encore\n");
        let mode = match fs::metadata(&target) {
            Ok(metadata) => metadata.permissions().mode() & 0o777,
            Err(_) => 0,
        };
        assert_eq!(mode, 0o644);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn overwrites_without_leaving_a_temp_file() {
        let dir = temp_dir_for("overwrite");
        let target = dir.join("config.json");
        assert!(write_atomic(&target, b"v1", 0o600).is_ok());
        assert!(write_atomic(&target, b"v2", 0o600).is_ok());
        assert_eq!(fs::read(&target).unwrap_or_default(), b"v2");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries.count(),
            Err(_) => 0,
        };
        assert_eq!(entries, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_a_path_without_a_parent() {
        // A bare file name has no parent in the `Path::parent` sense used
        // here when joined against an empty path; the error path is
        // exercised via a path that is only a root.
        let result = write_atomic(std::path::Path::new("/"), b"x", 0o644);
        assert!(result.is_err());
    }
}
