//! PID tracking, mirroring the C++ process helpers.
//!
//! Verified from the C++ sources:
//!
//! - `apply_game_profile` in `Main.cpp` checks liveness with
//!   `kill(pid, 0)`: a non-zero result means the game exited
//!   while a profile was being applied, and the session is
//!   aborted. A `kill` that fails with `EPERM` still means the
//!   process exists, only that the caller may not signal it;
//!   only `ESRCH` means the process is gone. This module
//!   preserves that distinction.
//! - `pidof` in `ProcessUtility.cpp` scans `/proc`, skips
//!   non-numeric directories, reads only the first
//!   null-terminated string from each process's `cmdline`, and
//!   matches it strictly (exact equality) or loosely (the
//!   target appears anywhere in the name, so
//!   `com.mobile.legends` also matches
//!   `com.mobile.legends:UnityKillsMe` in non-strict mode).
//!   It returns 0 when nothing matches, and stops at the first
//!   match in directory order.
//!
//! The `kill` check is done through `nix`, so this crate keeps
//! `#![forbid(unsafe_code)]`. The `/proc` scan needs no system
//! call wrapper at all.

use std::fs::{self, File, OpenOptions};
use std::path::Path;

use nix::sys::signal::kill;
use nix::unistd::Pid;
use thiserror::Error;

/// Errors from acquiring the single-instance daemon lock.
#[derive(Debug, Error)]
pub enum LockError {
    /// The lock file could not be opened or created.
    #[error("failed to open lock file: {0}")]
    Open(#[from] std::io::Error),
    /// The lock could not be acquired, usually because another
    /// daemon already holds it.
    #[error("failed to acquire lock: {0}")]
    Flock(#[from] nix::Error),
}

/// A held single-instance lock.
///
/// The lock is held for as long as this value lives, and is
/// released when it is dropped. This mirrors the C++
/// `LOCK_FILE` (`config_dir/.lock`), which the analysis found
/// was defined but never used; Stage 4 actually uses it.
#[derive(Debug)]
pub struct DaemonLock {
    /// The held flock. Kept alive only to hold the lock; it is
    /// never read, and dropping it releases the lock.
    #[allow(dead_code)]
    lock: nix::fcntl::Flock<File>,
}

impl DaemonLock {
    /// Open or create the lock file at `path` and acquire an
    /// exclusive, non-blocking flock on it.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be opened, or when
    /// another process already holds the lock.
    pub fn acquire(path: &Path) -> Result<Self, LockError> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        let lock = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock)
            .map_err(|(_, errno)| LockError::Flock(errno))?;
        Ok(Self { lock })
    }
}

/// Whether a process with the given PID is alive, mirroring
/// `kill(pid, 0) == 0` in C++, with `EPERM` counted as alive.
///
/// A PID of 0 or a negative PID is never alive for this purpose,
/// because `kill(0, 0)` would test the caller's own process
/// group, not a single game process, and C++ never passes such a
/// PID as a game PID.
#[must_use]
pub fn is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    match kill(Pid::from_raw(pid), None) {
        Ok(()) | Err(nix::Error::EPERM) => true,
        Err(_) => false,
    }
}

/// Find the PID of a process by its command-line name, mirroring
/// `pidof` in `ProcessUtility.cpp`.
///
/// With `strict` matching, only an exact match counts. Without
/// it, a process matches when the target appears anywhere in
/// its first command-line string. Returns 0 when nothing
/// matches, or when `/proc` cannot be read.
#[must_use]
pub fn pidof(target_name: &str, strict: bool) -> i32 {
    pidof_in(target_name, strict, Path::new("/proc"))
}

/// The same as [`pidof`], but scanning a given directory instead
/// of `/proc`, so tests can use a fake process tree.
#[must_use]
pub fn pidof_in(target_name: &str, strict: bool, proc_dir: &Path) -> i32 {
    if target_name.is_empty() {
        return 0;
    }
    let Ok(entries) = fs::read_dir(proc_dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        if dir_name.is_empty() || !dir_name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let Some(first) = first_null_terminated(&cmdline) else {
            continue;
        };
        let actual_name = String::from_utf8_lossy(first);
        let is_match = if strict {
            actual_name == target_name
        } else {
            actual_name.contains(target_name)
        };
        if is_match {
            return dir_name.parse::<i32>().unwrap_or(0);
        }
    }
    0
}

/// The bytes up to the first NUL byte, or `None` when the input
/// is empty or contains no NUL terminator.
fn first_null_terminated(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.is_empty() {
        return None;
    }
    let end = bytes.iter().position(|byte| *byte == 0)?;
    bytes.get(..end)
}

#[cfg(test)]
mod tests {
    use super::{first_null_terminated, is_alive, pidof_in};
    use std::fs;

    fn temp_dir_for(test_name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("encore-pid-{test_name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(fs::create_dir_all(&dir).is_ok());
        dir
    }

    #[test]
    fn alive_check_matches_self_and_rejects_dead_pids() {
        assert!(is_alive(std::process::id().cast_signed()));
        assert!(!is_alive(0));
        assert!(!is_alive(-1));
        // A high PID is extremely unlikely to be running in a
        // test environment; if it ever is, that failure would
        // itself be worth investigating rather than hiding.
        assert!(!is_alive(3_999_999));
    }

    #[test]
    fn pidof_matches_strict_and_loose_like_cpp() {
        let dir = temp_dir_for("pidof");
        let proc_123 = dir.join("123");
        let proc_456 = dir.join("456");
        assert!(fs::create_dir_all(&proc_123).is_ok());
        assert!(fs::create_dir_all(&proc_456).is_ok());
        assert!(fs::write(proc_123.join("cmdline"), b"com.a.game\0extra\0").is_ok());
        assert!(fs::write(proc_456.join("cmdline"), b"com.a.game:child\0").is_ok());
        assert!(fs::create_dir_all(dir.join("not-a-pid")).is_ok());
        assert_eq!(pidof_in("com.a.game", true, &dir), 123);
        assert_eq!(pidof_in("com.a.game:child", true, &dir), 456);
        // Loose matching is tested in its own directory, because
        // directory iteration order is unspecified and both
        // entries above contain the target.
        let loose_dir = temp_dir_for("pidof-loose");
        let loose_proc = loose_dir.join("789");
        assert!(fs::create_dir_all(&loose_proc).is_ok());
        assert!(fs::write(loose_proc.join("cmdline"), b"com.a.game:child\0").is_ok());
        assert_eq!(pidof_in("com.a.game", false, &loose_dir), 789);
        assert_eq!(pidof_in("com.missing", true, &dir), 0);
        assert_eq!(pidof_in("", true, &dir), 0);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&loose_dir);
    }

    #[test]
    fn first_null_terminated_handles_edge_cases() {
        assert_eq!(first_null_terminated(b"abc\0def"), Some(b"abc".as_slice()));
        assert_eq!(first_null_terminated(b""), None);
        assert_eq!(first_null_terminated(b"no-nul"), None);
    }
}
