//! Do Not Disturb execution, mirroring C++
//! `set_do_not_disturb` in `MiscUtility.cpp`.
//!
//! C++ forks, redirects the child's stdout and stderr
//! to `/dev/null`, execs `/system/bin/cmd` with separate
//! arguments, and waits: `cmd notification set_dnd
//! priority` for on, `cmd notification set_dnd off` for
//! off. A non-zero child status is logged by the parent.
//! The Rust version uses the timeout runner from
//! [`crate::process_runner`] instead of a blocking
//! `waitpid`, so a hung `cmd` cannot hang the daemon.
//!
//! Deliberate limit: `notify` (the other `MiscUtility`
//! function) is not implemented here. C++ drops to
//! uid/gid 2000 in the child before exec, which in Rust
//! needs `Command::pre_exec`, an `unsafe` hook that
//! belongs in [`encore_sys`] in a later step, not in a
//! crate that forbids `unsafe_code`. Running `notify`
//! as root would be a behaviour and security change, so
//! it is deferred rather than implemented unsafely.

use crate::process_runner::{CommandRunner, TimeoutRunner};

/// The `cmd` binary on Android.
pub const CMD_PATH: &str = "/system/bin/cmd";

/// The arguments for setting DND, without the program
/// name, as C++ passes them to `execvp`.
#[must_use]
pub fn dnd_args(enabled: bool) -> Vec<&'static str> {
    if enabled {
        vec!["notification", "set_dnd", "priority"]
    } else {
        vec!["notification", "set_dnd", "off"]
    }
}

/// Set DND through `runner`, returning an error when the
/// command cannot be started, times out, exceeds the
/// output cap, or exits non-zero. The last case mirrors
/// C++, where a non-zero child status is an error the
/// caller logs.
///
/// # Errors
///
/// Returns [`RunnerError`] from the runner, or a
/// non-zero-exit error as a string.
pub fn set_dnd(runner: &dyn CommandRunner, enabled: bool) -> Result<(), String> {
    let output = runner
        .run(
            CMD_PATH,
            &dnd_args(enabled),
            TimeoutRunner::DEFAULT_TIMEOUT,
            TimeoutRunner::DEFAULT_MAX_OUTPUT,
        )
        .map_err(|error| error.to_string())?;
    if output.success {
        Ok(())
    } else {
        Err("cmd notification set_dnd exited non-zero".to_owned())
    }
}

/// A convenience wrapper that sets DND with the real
/// timeout runner. On a host `/system/bin/cmd` does not
/// exist, so this returns a spawn error there; it is
/// device code, tested below with a fake runner.
///
/// # Errors
///
/// Returns the same error as [`set_dnd`].
pub fn set_dnd_default(enabled: bool) -> Result<(), String> {
    set_dnd(&TimeoutRunner, enabled)
}

#[cfg(test)]
mod tests {
    use super::{dnd_args, set_dnd};
    use crate::process_runner::{CommandRunner, RunOutput, RunnerError};
    use std::cell::RefCell;
    use std::time::Duration;

    struct RecordingRunner {
        calls: RefCell<Vec<(String, Vec<String>)>>,
        success: bool,
    }

    impl CommandRunner for RecordingRunner {
        fn run(
            &self,
            program: &str,
            args: &[&str],
            _timeout: Duration,
            _max_output_bytes: usize,
        ) -> Result<RunOutput, RunnerError> {
            self.calls.borrow_mut().push((
                program.to_owned(),
                args.iter().map(|arg| (*arg).to_owned()).collect(),
            ));
            Ok(RunOutput {
                output: String::new(),
                success: self.success,
            })
        }
    }

    #[test]
    fn dnd_args_match_cpp_and_runner_is_used() {
        assert_eq!(dnd_args(true), ["notification", "set_dnd", "priority"]);
        assert_eq!(dnd_args(false), ["notification", "set_dnd", "off"]);
        let runner = RecordingRunner {
            calls: RefCell::new(Vec::new()),
            success: true,
        };
        assert!(set_dnd(&runner, true).is_ok());
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 1);
        let Some(first) = calls.first() else { return };
        assert_eq!(first.0, "/system/bin/cmd");
        assert_eq!(first.1, ["notification", "set_dnd", "priority"]);
    }

    #[test]
    fn non_zero_exit_is_an_error() {
        let runner = RecordingRunner {
            calls: RefCell::new(Vec::new()),
            success: false,
        };
        assert!(set_dnd(&runner, false).is_err());
    }
}
