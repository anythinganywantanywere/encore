//! Daemonising, mirroring C++ `daemon(0, 0)` in
//! `cmd_run_daemon` (`jni/Main.cpp`).
//!
//! C++ calls `daemon(0, 0)` after loading the gamelist
//! and before initialising the file watcher: fork, let
//! the parent exit, make the child a session leader,
//! change directory to `/`, and redirect standard
//! streams to `/dev/null`. On failure it logs and
//! notifies a fatal error instead of continuing in the
//! foreground.
//!
//! The syscall work lives in [`encore_sys`], because
//! `nix` marks `fork` as `unsafe fn` and this crate
//! forbids `unsafe_code`. This module is the safe,
//! documented API over it, with the same precondition:
//! call it only at daemon startup, from the main thread,
//! before any other thread is spawned.
//!
//! Deliberate difference: the Rust daemon does not
//! daemonise by default. The `daemon` command runs in
//! the foreground unless `--daemonize` is given, so host
//! testing and log inspection stay straightforward, and
//! the daemonising path is exercised explicitly. On a
//! device, the module's `service.sh` (which is not
//! changed in this stage) would invoke the daemon with
//! `--daemonize` to match C++ behaviour.

pub use encore_sys::DaemonizeOutcome;

/// Detach from the controlling terminal, as C++
/// `daemon(0, 0)` does.
///
/// Returns [`DaemonizeOutcome::Parent`] in the parent,
/// whose caller should exit successfully, and
/// [`DaemonizeOutcome::Child`] in the child, which
/// continues as the daemon.
///
/// # Errors
///
/// Returns the underlying error when forking, creating
/// a new session, changing directory, or redirecting a
/// standard stream fails. The caller should fail daemon
/// startup in that case, as C++ does.
pub fn daemonize() -> Result<DaemonizeOutcome, String> {
    encore_sys::daemonize().map_err(|error| error.to_string())
}
