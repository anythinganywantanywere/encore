//! Graceful SIGTERM/SIGINT handling via a self-pipe.
//!
//! Verified from `jni/include/SignalHandler.hpp` and
//! `jni/Main.cpp`: C++ installs handlers for crash
//! signals, exit signals (SIGTERM, SIGINT), and user
//! signals (SIGHUP, SIGUSR1, SIGUSR2). Its exit handler
//! flushes logs and re-raises the signal, and
//! `signal_daemon_stop` calls `_exit(0)` from a signal
//! context because `joinThreadPool` blocks the main
//! thread. Neither is async-signal-safe, which is
//! analysis finding 9.
//!
//! The Rust replacement handles only the exit signals,
//! and only with async-signal-safe work: the handler
//! writes one byte to a pipe (see [`encore_sys`]), and
//! the main loop checks the pipe in normal code and
//! performs the graceful exit there (logging and
//! returning, not `_exit`). Crash signals keep their
//! default behaviour, and user signals are not handled
//! yet, because no Rust code registers callbacks for
//! them; that is a documented gap, not a silent change.
//!
//! Only one pipe may be installed per process; a second
//! installation fails, so the daemon loop cannot
//! accidentally replace its own handler.

pub use encore_sys::StopSignalPipe;

/// Install SIGTERM/SIGINT handlers that wake a pipe.
///
/// # Errors
///
/// Returns an error when a pipe is already installed,
/// when pipe creation fails, or when installing a
/// handler fails.
pub fn install_stop_signals() -> Result<StopSignalPipe, String> {
    StopSignalPipe::install().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::install_stop_signals;
    use nix::sys::signal::{raise, Signal};

    #[test]
    fn sigterm_wakes_the_pipe_and_drop_restores() {
        let result = install_stop_signals();
        assert!(result.is_ok());
        let Ok(pipe) = result else { return };
        assert!(!pipe.stop_requested());
        assert!(raise(Signal::SIGTERM).is_ok());
        assert!(pipe.stop_requested());
        assert!(!pipe.stop_requested());
        drop(pipe);
        // A second installation must succeed after drop.
        assert!(install_stop_signals().is_ok());
    }
}
