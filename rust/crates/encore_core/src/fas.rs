//! Frame Aware Scheduling (FAS) control from the Encore daemon,
//! Level 2 integration (encore_fas as part of the Encore module).
//!
//! Encore and FAS do not touch each other's domain: Encore
//! applies CPU/GPU profiles through sysfs, FAS only measures
//! frame timing in the kernel (it never changes frequencies).
//! The daemon therefore treats FAS as optional telemetry:
//! when a gamelist game goes to the foreground it attaches
//! FAS to that game's PID, and when the game leaves it
//! detaches again. Any FAS failure is logged by the caller
//! and never stops profile application, because a game
//! must still get its performance profile even if the
//! kernel module did not load (for example, on a kernel
//! the shipped `.ko` was not built for).
//!
//! FAS itself is controlled through the `fas_ctl` binary
//! (the Rust `fas-ctl` from encore_fas, shipped in the
//! module as `system/bin/fas_ctl` and symlinked onto
//! `$PATH` by `customize.sh`, like `encored`). This module
//! deliberately shells out to that binary instead of
//! reimplementing the FAS ioctl protocol here: `fas_ctl`
//! already owns ELF symbol resolution for
//! `Surface::queueBuffer`, the ioctl numbers, and the
//! `ctx_id` protocol, and duplicating them would be a
//! second implementation to keep in sync. A direct-ioctl
//! version is a later optimisation, not a correctness
//! requirement, because attach/detach happen only on
//! game foreground/exit, not on the frame path.
//!
//! The default FPS targets are 60 and 120: they cover
//! the common game refresh rates, and the detector
//! tracks both and reports rate switches between them.
//! A per-game FPS field in the gamelist, or the display
//! refresh rate from Binder, would be a refinement;
//! neither exists in the current gamelist format, so
//! the default is recorded here rather than invented
//! per game.

use crate::process_runner::{CommandRunner, TimeoutRunner};
use std::time::{Duration, Instant};

/// The `fas_ctl` binary on Android, found through `$PATH`
/// (the module installer symlinks it there, like `encored`).
pub const FAS_CTL_PROGRAM: &str = "fas_ctl";

/// Default FAS frame-rate targets for a game, in fps.
pub const FAS_DEFAULT_FPS: &[u32] = &[60, 120];

/// The profile code for a foreground game, matching
/// `PERFORMANCE` in the C++ profile contract.
pub const FAS_GAME_PROFILE_CODE: i32 = 1;

/// How long a failed attach for one PID suppresses
/// further attach attempts for that same PID. Without
/// this, a missing kernel module turns every daemon
/// loop iteration (about 100 ms) into a `fas_ctl`
/// subprocess spawn plus a log line: on device this
/// produced 537 identical warnings in 66 seconds while
/// a game was foreground. With the backoff, a failed
/// PID is retried at most once per interval, so the
/// warning rate drops from about 8 per second to one
/// per 30 seconds, while a module loaded mid-game
/// still attaches within one interval, without a game
/// restart or a daemon restart.
pub const FAS_RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// Arguments for `fas_ctl attach <pid> <fps>...`, without
/// the program name. The library and symbol default to
/// `fas_ctl`'s own defaults (`/system/lib64/libgui.so`
/// and the `Surface::queueBuffer` symbols), so no `-l`
/// or `-s` flag is passed.
#[must_use]
pub fn fas_attach_args(pid: i32) -> Vec<String> {
    let mut args = vec!["attach".to_owned(), pid.to_string()];
    for fps in FAS_DEFAULT_FPS {
        args.push(fps.to_string());
    }
    args
}

/// Arguments for `fas_ctl detach <ctx_id>`, without the
/// program name.
#[must_use]
pub fn fas_detach_args(ctx_id: i32) -> Vec<String> {
    vec!["detach".to_owned(), ctx_id.to_string()]
}

/// Parse the `ctx_id` from `fas_ctl attach` output, which
/// prints `ctx_id: <n>` on its own line. Returns `None`
/// when the line is missing or does not parse, so the
/// caller treats the attach as failed rather than
/// recording a bogus listener id.
#[must_use]
pub fn parse_ctx_id(output: &str) -> Option<i32> {
    output.lines().find_map(|line| {
        let rest = line.strip_prefix("ctx_id:")?.trim();
        rest.parse::<i32>().ok()
    })
}

/// Extract the game PID for FAS from a profile
/// application, or `None` when this application is not
/// a foreground game. Only profile code 1 with a
/// `gameinfo` of the form `<package> <pid> <uid>` counts;
/// `"NULL 0 0"` and every other profile code mean no
/// game is active, so FAS must be detached, not attached.
#[must_use]
pub fn game_pid_for_fas(game_info: &str, profile_code: i32) -> Option<i32> {
    if profile_code != FAS_GAME_PROFILE_CODE {
        return None;
    }
    let mut parts = game_info.split_whitespace();
    let package = parts.next()?;
    if package == "NULL" {
        return None;
    }
    let pid = parts.next()?.parse::<i32>().ok()?;
    if pid > 0 {
        Some(pid)
    } else {
        None
    }
}

/// Attach FAS to `pid` through `runner`, returning the
/// listener id on success. A runner failure, a non-zero
/// exit, or missing `ctx_id` output are all errors the
/// caller logs; none of them stops profile application.
///
/// # Errors
/// Returns a string describing a runner failure, a
/// non-zero exit, or unparsable output.
pub fn fas_attach(runner: &dyn CommandRunner, pid: i32) -> Result<i32, String> {
    let args = fas_attach_args(pid);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = runner
        .run(
            FAS_CTL_PROGRAM,
            &arg_refs,
            TimeoutRunner::DEFAULT_TIMEOUT,
            TimeoutRunner::DEFAULT_MAX_OUTPUT,
        )
        .map_err(|error| error.to_string())?;
    if !output.success {
        return Err("fas_ctl attach exited non-zero".to_owned());
    }
    parse_ctx_id(&output.output).ok_or_else(|| "fas_ctl attach output has no ctx_id".to_owned())
}

/// Detach the FAS listener `ctx_id` through `runner`.
/// A failure is returned for the caller to log; the
/// tracker still clears the listener, because a failed
/// detach usually means the listener is already gone
/// (the process died, or the module was unloaded).
///
/// # Errors
/// Returns a string describing a runner failure or a
/// non-zero exit.
pub fn fas_detach(runner: &dyn CommandRunner, ctx_id: i32) -> Result<(), String> {
    let args = fas_detach_args(ctx_id);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = runner
        .run(
            FAS_CTL_PROGRAM,
            &arg_refs,
            TimeoutRunner::DEFAULT_TIMEOUT,
            TimeoutRunner::DEFAULT_MAX_OUTPUT,
        )
        .map_err(|error| error.to_string())?;
    if output.success {
        Ok(())
    } else {
        Err("fas_ctl detach exited non-zero".to_owned())
    }
}

/// What a tracker synchronisation did, for the caller
/// to log. No logging happens here: this module stays
/// testable without a logger, like the rest of the crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FasEvent {
    /// FAS was attached to a game PID with this listener id.
    Attached {
        /// The game PID.
        pid: i32,
        /// The FAS listener id.
        ctx_id: i32,
    },
    /// FAS was detached from a game PID.
    Detached {
        /// The game PID that was attached.
        pid: i32,
        /// The FAS listener id that was detached.
        ctx_id: i32,
    },
    /// An attach or detach failed; the daemon continues
    /// with profile application only.
    Failed {
        /// `"attach"` or `"detach"`.
        action: &'static str,
        /// The reason, from the runner or from output parsing.
        reason: String,
    },
}

/// Tracks the one FAS listener the daemon may hold at
/// a time: at most one game is foreground, so at most
/// one listener is active. Re-syncing with the same PID
/// is a no-op, so repeated evaluations of the same game
/// do not re-attach on every Binder event. A failed
/// attach is remembered with its time, and the same PID
/// is not retried until `FAS_RETRY_INTERVAL` has passed,
/// so a missing kernel module cannot turn the daemon
/// loop into a subprocess-spawning log flood.
#[derive(Debug, Clone, Default)]
pub struct FasTracker {
    /// The currently attached `(pid, ctx_id)`, if any.
    active: Option<(i32, i32)>,
    /// The last `(pid, time)` whose attach failed, if any.
    failed: Option<(i32, Instant)>,
}

impl FasTracker {
    /// The currently attached game PID, if any.
    #[must_use]
    pub fn active_pid(&self) -> Option<i32> {
        self.active.map(|(pid, _ctx_id)| pid)
    }

    /// Bring the tracker in line with the currently
    /// foreground game PID (`None` when no game is
    /// foreground), attaching, detaching, or doing
    /// nothing as needed, and return what happened.
    #[must_use]
    pub fn sync(&mut self, runner: &dyn CommandRunner, pid: Option<i32>) -> Vec<FasEvent> {
        self.sync_with_time(runner, pid, Instant::now())
    }

    /// The same synchronisation at an explicit time,
    /// so tests can control the retry backoff without
    /// sleeping. Production callers use `sync`.
    fn sync_with_time(
        &mut self,
        runner: &dyn CommandRunner,
        pid: Option<i32>,
        now: Instant,
    ) -> Vec<FasEvent> {
        let mut events = Vec::new();
        // No game is foreground: detach any active
        // listener, and clear the failed PID too, so a
        // later game with a reused PID gets a fresh
        // attach attempt. This branch must come first:
        // with no active listener, `pid` already equals
        // `active_pid()` (both `None`), so a dedupe
        // early-return here would never clear `failed`.
        let Some(new_pid) = pid else {
            if let Some((old_pid, old_ctx)) = self.active.take() {
                match fas_detach(runner, old_ctx) {
                    Ok(()) => events.push(FasEvent::Detached {
                        pid: old_pid,
                        ctx_id: old_ctx,
                    }),
                    Err(reason) => events.push(FasEvent::Failed {
                        action: "detach",
                        reason,
                    }),
                }
            }
            self.failed = None;
            return events;
        };
        if Some(new_pid) == self.active_pid() {
            return events;
        }
        if let Some((old_pid, old_ctx)) = self.active.take() {
            match fas_detach(runner, old_ctx) {
                Ok(()) => events.push(FasEvent::Detached {
                    pid: old_pid,
                    ctx_id: old_ctx,
                }),
                Err(reason) => events.push(FasEvent::Failed {
                    action: "detach",
                    reason,
                }),
            }
        }
        if let Some((failed_pid, failed_at)) = self.failed {
            if failed_pid == new_pid {
                if let Some(retry_at) = failed_at.checked_add(FAS_RETRY_INTERVAL) {
                    if now < retry_at {
                        return events;
                    }
                }
            }
        }
        match fas_attach(runner, new_pid) {
            Ok(ctx_id) => {
                self.active = Some((new_pid, ctx_id));
                self.failed = None;
                events.push(FasEvent::Attached {
                    pid: new_pid,
                    ctx_id,
                });
            }
            Err(reason) => {
                self.failed = Some((new_pid, now));
                events.push(FasEvent::Failed {
                    action: "attach",
                    reason,
                });
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::{
        fas_attach_args, fas_detach_args, game_pid_for_fas, parse_ctx_id, FasEvent, FasTracker,
    };
    use crate::process_runner::{CommandRunner, RunOutput, RunnerError};
    use std::cell::RefCell;
    use std::time::{Duration, Instant};

    struct RecordingRunner {
        calls: RefCell<Vec<(String, Vec<String>)>>,
        output: String,
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
                output: self.output.clone(),
                success: self.success,
            })
        }
    }

    #[test]
    fn attach_args_use_default_fps_and_detach_args_match() {
        assert_eq!(fas_attach_args(1234), ["attach", "1234", "60", "120"]);
        assert_eq!(fas_detach_args(7), ["detach", "7"]);
    }

    #[test]
    fn parse_ctx_id_accepts_only_a_valid_line() {
        assert_eq!(parse_ctx_id("ctx_id: 7\n"), Some(7));
        assert_eq!(
            parse_ctx_id("listener 3 on /x @ 0x1\nctx_id: 42\n"),
            Some(42)
        );
        assert_eq!(parse_ctx_id("no ctx here\n"), None);
        assert_eq!(parse_ctx_id("ctx_id: abc\n"), None);
    }

    #[test]
    fn game_pid_only_for_profile_one_and_real_gameinfo() {
        assert_eq!(game_pid_for_fas("com.a.game 123 1000", 1), Some(123));
        assert_eq!(game_pid_for_fas("NULL 0 0", 1), None);
        assert_eq!(game_pid_for_fas("com.a.game 123 1000", 2), None);
        assert_eq!(game_pid_for_fas("com.a.game 0 1000", 1), None);
        assert_eq!(game_pid_for_fas("com.a.game", 1), None);
    }

    #[test]
    fn tracker_attaches_detaches_and_dedupes() {
        let runner = RecordingRunner {
            calls: RefCell::new(Vec::new()),
            output: "ctx_id: 9\n".to_owned(),
            success: true,
        };
        let mut tracker = FasTracker::default();
        assert_eq!(
            tracker.sync(&runner, Some(123)),
            [FasEvent::Attached {
                pid: 123,
                ctx_id: 9
            }]
        );
        assert_eq!(tracker.active_pid(), Some(123));
        assert_eq!(tracker.sync(&runner, Some(123)), Vec::new());
        assert_eq!(
            tracker.sync(&runner, None),
            [FasEvent::Detached {
                pid: 123,
                ctx_id: 9
            }]
        );
        assert_eq!(tracker.active_pid(), None);
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 2);
        let Some(first) = calls.first() else { return };
        assert_eq!(first.1, ["attach", "123", "60", "120"]);
        let Some(second) = calls.get(1) else { return };
        assert_eq!(second.1, ["detach", "9"]);
    }

    #[test]
    fn tracker_attach_failure_backs_off_then_retries() {
        let runner = RecordingRunner {
            calls: RefCell::new(Vec::new()),
            output: String::new(),
            success: false,
        };
        let mut tracker = FasTracker::default();
        let now = Instant::now();
        // First attempt fails and is reported.
        let events = tracker.sync_with_time(&runner, Some(123), now);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events.first(),
            Some(FasEvent::Failed {
                action: "attach",
                ..
            })
        ));
        assert_eq!(tracker.active_pid(), None);
        assert_eq!(runner.calls.borrow().len(), 1);
        // Same PID inside the backoff window: no spawn,
        // no event, so the log does not flood.
        let Some(soon) = now.checked_add(Duration::from_secs(10)) else {
            return;
        };
        assert_eq!(tracker.sync_with_time(&runner, Some(123), soon), Vec::new());
        assert_eq!(runner.calls.borrow().len(), 1);
        // A different PID is not suppressed by the failure.
        assert_eq!(tracker.sync_with_time(&runner, Some(456), soon).len(), 1);
        assert_eq!(runner.calls.borrow().len(), 2);
        // After the interval, the original PID retries.
        let Some(later) = now.checked_add(Duration::from_secs(31)) else {
            return;
        };
        assert_eq!(tracker.sync_with_time(&runner, Some(123), later).len(), 1);
        assert_eq!(runner.calls.borrow().len(), 3);
        // Leaving the game clears the failure, so a
        // later game with a reused PID retries at once.
        assert_eq!(tracker.sync_with_time(&runner, None, later), Vec::new());
        assert_eq!(tracker.sync_with_time(&runner, Some(123), later).len(), 1);
        assert_eq!(runner.calls.borrow().len(), 4);
    }

    #[test]
    fn tracker_attach_failure_leaves_no_active_listener() {
        let runner = RecordingRunner {
            calls: RefCell::new(Vec::new()),
            output: String::new(),
            success: false,
        };
        let mut tracker = FasTracker::default();
        let events = tracker.sync(&runner, Some(123));
        assert_eq!(events.len(), 1);
        let Some(first) = events.first() else { return };
        assert!(matches!(
            first,
            FasEvent::Failed {
                action: "attach",
                ..
            }
        ));
        assert_eq!(tracker.active_pid(), None);
    }
}
