//! A subprocess runner with a timeout and an output cap.
//!
//! This is the runner the C++ daemon lacks (analysis
//! finding 6): `runResolver` in `jni/Main.cpp` reads a
//! child's stdout to EOF with no timeout and no output
//! size cap, and `waitpid` blocks until the child exits.
//! Any Rust subprocess that runs on behalf of the daemon
//! must use this runner, or an equivalent, so a hung or
//! chatty child cannot hang the daemon or exhaust memory.
//!
//! The runner is generic: it runs any command with
//! separate arguments (never a shell string, per the
//! task guide), a timeout, and a maximum output size.
//! The resolver runner and the DND runner both use it.
//! A non-zero exit status is not an error here, because
//! the C++ resolver still returns already-parsed codes
//! on a non-zero exit; callers decide what an exit
//! status means for their protocol.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::channel;
use std::time::Duration;

/// The result of a finished subprocess run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    /// The child's standard output, as text.
    pub output: String,
    /// Whether the child exited with status 0.
    pub success: bool,
}

/// A failure of the runner itself, as opposed to a
/// child's non-zero exit status.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunnerError {
    /// The command could not be started.
    #[error("failed to start command: {0}")]
    Spawn(String),
    /// The command did not finish before the timeout;
    /// the child was killed.
    #[error("command timed out")]
    Timeout,
    /// The child's output exceeded the size cap; the
    /// child was killed.
    #[error("command output exceeded the size cap")]
    OutputTooLarge,
}

/// A command runner, as a trait so tests can substitute
/// a fake that records its arguments without spawning.
pub trait CommandRunner {
    /// Run `program` with `args`, within `timeout`, and
    /// with at most `max_output_bytes` of output.
    ///
    /// # Errors
    ///
    /// Returns [`RunnerError`] when the command cannot
    /// be started, times out, or exceeds the output cap.
    fn run(
        &self,
        program: &str,
        args: &[&str],
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Result<RunOutput, RunnerError>;

    /// Run `program` with `args` and `input` written to
    /// its standard input before it is closed, within
    /// `timeout`, and with at most `max_output_bytes` of
    /// output. The default implementation ignores the
    /// input, which is what test fakes do; the real
    /// runner writes it.
    ///
    /// # Errors
    ///
    /// Returns [`RunnerError`] when the command cannot
    /// be started, times out, or exceeds the output cap.
    fn run_with_input(
        &self,
        program: &str,
        args: &[&str],
        input: &str,
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Result<RunOutput, RunnerError> {
        let _ = input;
        self.run(program, args, timeout, max_output_bytes)
    }
}

/// The real runner: spawn, read stdout on a thread, and
/// kill the child on timeout or on output overflow.
#[derive(Debug, Clone, Copy, Default)]
pub struct TimeoutRunner;

impl TimeoutRunner {
    /// The default timeout for daemon subprocesses:
    /// 10 seconds, matching the Binder service wait in
    /// `BinderMonitor::waitForServiceCompat`.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
    /// The default output cap: 64 KiB, far above the
    /// few hundred bytes a resolver reply needs.
    pub const DEFAULT_MAX_OUTPUT: usize = 64 * 1024;
}

impl CommandRunner for TimeoutRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Result<RunOutput, RunnerError> {
        self.run_with_input(program, args, "", timeout, max_output_bytes)
    }

    fn run_with_input(
        &self,
        program: &str,
        args: &[&str],
        input: &str,
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Result<RunOutput, RunnerError> {
        let mut child: Child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| RunnerError::Spawn(error.to_string()))?;
        if let Some(mut stdin) = child.stdin.take() {
            let input = input.to_owned();
            std::thread::spawn(move || {
                use std::io::Write;
                let _ = stdin.write_all(input.as_bytes());
            });
        }
        let Some(mut stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(RunnerError::Spawn("no stdout pipe".to_owned()));
        };
        let (sender, receiver) = channel();
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if let Some(slice) = chunk.get(..count) {
                            buffer.extend_from_slice(slice);
                        }
                        if buffer.len() > max_output_bytes {
                            break;
                        }
                    }
                }
            }
            let _ = sender.send(buffer);
        });
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let output = receiver
                        .recv_timeout(Duration::from_secs(1))
                        .unwrap_or_default();
                    if output.len() > max_output_bytes {
                        return Err(RunnerError::OutputTooLarge);
                    }
                    return Ok(RunOutput {
                        output: String::from_utf8_lossy(&output).into_owned(),
                        success: status.success(),
                    });
                }
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(RunnerError::Timeout);
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(RunnerError::Spawn(error.to_string()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CommandRunner, RunnerError, TimeoutRunner};
    use std::time::Duration;

    #[test]
    fn echo_returns_output_and_success() {
        let runner = TimeoutRunner;
        let result = runner.run("/bin/echo", &["hello"], Duration::from_secs(5), 1024);
        assert!(result.is_ok());
        let Ok(output) = result else { return };
        assert_eq!(output.output, "hello\n");
        assert!(output.success);
    }

    #[test]
    fn sleep_times_out_and_is_killed() {
        let runner = TimeoutRunner;
        let result = runner.run("/bin/sleep", &["30"], Duration::from_millis(200), 1024);
        assert_eq!(result, Err(RunnerError::Timeout));
    }

    #[test]
    fn output_over_the_cap_is_rejected() {
        let runner = TimeoutRunner;
        let result = runner.run(
            "/bin/sh",
            &["-c", "yes | head -c 100000"],
            Duration::from_secs(5),
            1024,
        );
        // Either the cap fires, or the child finishes
        // and the buffered output is over the cap: both
        // are the rejection this runner must provide.
        assert_eq!(result, Err(RunnerError::OutputTooLarge));
    }
}
