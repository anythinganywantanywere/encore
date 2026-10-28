//! Thin, unsafe FFI wrappers for the Encore Rust migration.
//!
//! This is the `encore-sys` crate the task guide plans for:
//! every other crate keeps `#![forbid(unsafe_code)]`, and the
//! few operations that cannot be expressed safely in Rust
//! live here, each with a concrete `// SAFETY:` comment.
//! Nothing else in this crate does work on behalf of the
//! daemon; it only wraps the syscalls below.
//!
//! Operations wrapped here, and why they need `unsafe`:
//!
//! - `fork`, `setsid`, `chdir`, `dup2`: `nix` 0.29 marks
//!   `fork` and `sigaction` as `unsafe fn` because forking
//!   a multi-threaded process, or installing a handler
//!   that does non-async-signal-safe work, is unsound.
//!   `daemonize` is called only at daemon startup, from
//!   the main thread, before any watcher or worker thread
//!   exists, so the forked child inherits a single-threaded
//!   process. The signal handler installed here writes
//!   exactly one byte to a pipe and does nothing else.
//! - `sigaction`: same reason; the handler below is a
//!   plain `extern "C"` function that performs only a
//!   `write(2)` to a pipe, which is async-signal-safe.
//!
//! Callers must respect the documented preconditions:
//! call `daemonize` only at startup, and install at most
//! one stop-signal pipe per process.

#![warn(missing_docs)]

/// Android Binder NDK transport (dlopen loader, service
/// acquisition, single-value transactions, and callback
/// registration), mirroring `jni/base/BinderMonitor/` and
/// `jni/base/BinderNDK/`. See `binder` for the verified
/// C++ behaviour it preserves.
pub mod binder;

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use nix::fcntl::{open as nix_open, OFlag};
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::stat::Mode;
use nix::unistd::{chdir, dup2, fork, pipe2, setsid, ForkResult};

/// The result of [`daemonize`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonizeOutcome {
    /// The parent process, with the child's PID. The
    /// caller should exit successfully.
    Parent(i32),
    /// The child process, now a session leader with its
    /// working directory at `/` and standard streams
    /// connected to `/dev/null`. The caller continues
    /// as the daemon.
    Child,
}

/// Detach from the controlling terminal, as C++
/// `daemon(0, 0)` does: fork, let the parent exit, make
/// the child a session leader, change directory to `/`,
/// and redirect standard input, output, and error to
/// `/dev/null`.
///
/// # Preconditions
///
/// Call only at daemon startup, from the main thread,
/// before any other thread is spawned. Forking a
/// multi-threaded process would leave the child with
/// locks held by threads that no longer exist.
///
/// # Errors
///
/// Returns a `nix` error when forking, creating a new
/// session, changing directory, opening `/dev/null`, or
/// redirecting a standard stream fails. On error the
/// process is not daemonised and the caller should fail
/// startup, as C++ does when `daemon(0, 0)` fails.
pub fn daemonize() -> nix::Result<DaemonizeOutcome> {
    // SAFETY: called only at startup, single-threaded,
    // per the documented precondition, so no other
    // thread's locks are inherited by the child.
    let fork_result = unsafe { fork() }?;
    match fork_result {
        ForkResult::Parent { child } => Ok(DaemonizeOutcome::Parent(child.as_raw())),
        ForkResult::Child => {
            // SAFETY: the child is single-threaded (it
            // was just forked from a single-threaded
            // parent), so `setsid` cannot race another
            // thread's session state.
            setsid()?;
            // SAFETY: `chdir("/")` takes a plain path
            // and cannot race, because the child has no
            // other thread changing the directory.
            chdir("/")?;
            let null = nix_open("/dev/null", OFlag::O_RDWR, Mode::empty())?;
            let null_fd = null.as_raw_fd();
            for target in [0, 1, 2] {
                // SAFETY: `null_fd` is a valid, open
                // file descriptor for `/dev/null`, and
                // `target` is a standard stream number.
                // The descriptor is kept alive until all
                // three redirections are done.
                dup2(null_fd, target)?;
            }
            // Closing the extra descriptor, leaving 0, 1,
            // and 2 pointing at `/dev/null`.
            let _ = nix::unistd::close(null_fd);
            Ok(DaemonizeOutcome::Child)
        }
    }
}

/// The write end of the stop-signal self-pipe, read by
/// the signal handler. `-1` means no pipe is installed.
static STOP_PIPE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// Whether a stop-signal pipe is currently installed,
/// so a second installation fails instead of replacing
/// the first pipe's handler.
static STOP_PIPE_INSTALLED: AtomicBool = AtomicBool::new(false);

/// The signal handler for SIGTERM and SIGINT. It writes
/// exactly one byte to the self-pipe and returns. That
/// is the only async-signal-safe work it does: no
/// allocation, no locking, no logging, and no exit.
extern "C" fn stop_signal_handler(_signal: libc::c_int) {
    let fd = STOP_PIPE_WRITE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        // SAFETY: `fd` is the write end of a pipe created
        // by `install_stop_signals` in this process, and
        // `write(2)` of one byte to a pipe is
        // async-signal-safe. A failed or partial write
        // is ignored: the byte is only a wake-up, and
        // the main loop re-checks the pipe anyway.
        unsafe {
            let _ = libc::write(fd, [1u8].as_ptr().cast(), 1);
        }
    }
}

/// A self-pipe for graceful SIGTERM/SIGINT handling.
///
/// This is the fix for analysis finding 9 at the signal
/// layer: C++ `signal_daemon_stop` calls `_exit(0)`
/// from a signal context, and its exit handler flushes
/// logs, neither of which is async-signal-safe. Here the
/// handler only wakes the main loop, and the main loop
/// performs the graceful exit (logging, restoring DND
/// if needed, and returning) in normal code.
///
/// Installing this pipe replaces the SIGTERM and SIGINT
/// handlers for the process; dropping it restores the
/// previous handlers.
pub struct StopSignalPipe {
    /// The read end, polled by the main loop.
    read_fd: OwnedFd,
    /// The write end, used by the signal handler.
    write_fd: OwnedFd,
    /// The previous SIGTERM action, restored on drop.
    old_term: SigAction,
    /// The previous SIGINT action, restored on drop.
    old_int: SigAction,
}

impl StopSignalPipe {
    /// Install SIGTERM/SIGINT handlers that wake this
    /// pipe, and return the pipe.
    ///
    /// # Errors
    ///
    /// Returns an error when a pipe is already installed
    /// in this process, when pipe creation fails, or when
    /// installing a handler fails.
    pub fn install() -> nix::Result<Self> {
        if STOP_PIPE_INSTALLED.swap(true, Ordering::SeqCst) {
            return Err(nix::errno::Errno::EBUSY);
        }
        let result = Self::install_inner();
        if result.is_err() {
            STOP_PIPE_INSTALLED.store(false, Ordering::SeqCst);
        }
        result
    }

    fn install_inner() -> nix::Result<Self> {
        let (read_fd, write_fd) = pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK)?;
        STOP_PIPE_WRITE_FD.store(write_fd.as_raw_fd(), Ordering::SeqCst);
        let action = SigAction::new(
            SigHandler::Handler(stop_signal_handler),
            SaFlags::empty(),
            SigSet::empty(),
        );
        // SAFETY: `stop_signal_handler` is a plain
        // `extern "C"` function that only writes one
        // byte to a pipe, so it is safe to install as a
        // signal handler. The previous actions are saved
        // and restored on drop.
        let old_term = unsafe { nix::sys::signal::sigaction(Signal::SIGTERM, &action) }?;
        let old_int = match unsafe { nix::sys::signal::sigaction(Signal::SIGINT, &action) } {
            Ok(old) => old,
            Err(error) => {
                // SAFETY: restoring the saved SIGTERM
                // action on the failure path, for the
                // same reason as the install above.
                let _ = unsafe { nix::sys::signal::sigaction(Signal::SIGTERM, &old_term) };
                return Err(error);
            }
        };
        Ok(Self {
            read_fd,
            write_fd,
            old_term,
            old_int,
        })
    }

    /// Whether a stop signal has arrived since the last
    /// check. This drains the pipe, so a burst of signals
    /// is reported once, which is all a graceful exit
    /// needs.
    #[must_use]
    pub fn stop_requested(&self) -> bool {
        let mut buffer = [0u8; 64];
        let mut requested = false;
        loop {
            match nix::unistd::read(self.read_fd.as_raw_fd(), &mut buffer) {
                Ok(0) => break,
                Ok(_) => requested = true,
                Err(_) => break,
            }
        }
        requested
    }
}

impl Drop for StopSignalPipe {
    fn drop(&mut self) {
        // SAFETY: restoring previously saved actions,
        // which were valid when installed. The write
        // descriptor is cleared first, so a signal
        // arriving during restore cannot write to a
        // closed pipe.
        STOP_PIPE_WRITE_FD.store(-1, Ordering::SeqCst);
        let _ = unsafe { nix::sys::signal::sigaction(Signal::SIGTERM, &self.old_term) };
        let _ = unsafe { nix::sys::signal::sigaction(Signal::SIGINT, &self.old_int) };
        // Keep the write end alive until after the
        // handlers are restored, so the handler cannot
        // observe a closed descriptor.
        let _ = &self.write_fd;
        STOP_PIPE_INSTALLED.store(false, Ordering::SeqCst);
    }
}
