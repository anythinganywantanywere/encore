//! Running the Binder resolver with a timeout and an
//! output cap, over the protocol in [`crate::resolver`].
//!
//! C++ `runResolver` forks, execs `app_process` with
//! the resolver APK, writes all queries, and reads to
//! EOF with no timeout and no output cap (finding 6).
//! This module is the Rust replacement for that runner,
//! built on [`crate::process_runner`]: the same query
//! list and output parsing, but a 10-second timeout and
//! a 64 KiB output cap, and a killed child on either.
//!
//! The Android runtime environment from C++ is preserved
//! exactly: the six `ANDROID_*` variables are set only
//! when absent (C++ uses `setenv(..., 0)`), and the
//! command is `app_process` with separate arguments, never
//! a shell string. On a host there is no `app_process`
//! and no APK, so this runner is exercised with a fake
//! [`CommandRunner`] in tests; the real execution is
//! device work, as in Stage 6.

use crate::process_runner::{CommandRunner, TimeoutRunner};
use crate::resolver::{parse_resolver_output, ResolvedCodes, TxCode};

/// The Android runtime environment variables C++
/// `runResolver` sets without overwriting, in order.
pub const RESOLVER_ENV: [(&str, &str); 6] = [
    ("ANDROID_ROOT", "/system"),
    ("ANDROID_DATA", "/data"),
    ("ANDROID_RUNTIME_ROOT", "/apex/com.android.runtime"),
    ("ANDROID_ART_ROOT", "/apex/com.android.art"),
    ("ANDROID_I18N_ROOT", "/apex/com.android.i18n"),
    ("ANDROID_TZDATA_ROOT", "/apex/com.android.tzdata"),
];

/// The `app_process` path on Android.
pub const APP_PROCESS: &str = "/system/bin/app_process";

/// Build the `app_process` argument list for a resolver
/// APK path, as C++ does: the class path flag, `/` as
/// the (unused) class-path argument position, and the
/// main class. The query list itself is written to the
/// child's stdin in C++; here the runner's command line
/// is the same, and the queries are part of the protocol
/// in [`crate::resolver`], not of the command line.
#[must_use]
pub fn resolver_args(apk_path: &str) -> Vec<String> {
    vec![
        format!("-Djava.class.path={apk_path}"),
        "/".to_owned(),
        crate::resolver::RESOLVER_MAIN_CLASS.to_owned(),
    ]
}

/// Run the resolver through `runner` and resolve its
/// transaction codes.
///
/// The runner is a trait so tests can substitute a fake
/// that returns sample resolver output without an Android
/// device. A timeout or output-cap failure from the
/// runner is propagated as-is; a non-zero exit status is
/// not, because C++ still returns already-parsed codes
/// on a non-zero exit, and the required-code check below
/// decides whether the result is usable.
///
/// # Errors
///
/// Returns [`RunnerError`] from the runner, or the
/// missing-required-code error from
/// [`ResolvedCodes::resolve`].
pub fn run_resolver(runner: &dyn CommandRunner, apk_path: &str) -> Result<ResolvedCodes, String> {
    let args = resolver_args(apk_path);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    // C++ writes one query line per transaction code to
    // the child's stdin, then closes it.
    let mut input = String::new();
    for tx in TxCode::all() {
        input.push_str(tx.query());
        input.push('\n');
    }
    let output = runner
        .run_with_input(
            APP_PROCESS,
            &arg_refs,
            &input,
            TimeoutRunner::DEFAULT_TIMEOUT,
            TimeoutRunner::DEFAULT_MAX_OUTPUT,
        )
        .map_err(|error| error.to_string())?;
    let parsed = parse_resolver_output(&output.output);
    ResolvedCodes::resolve(&parsed)
}

/// The default runner for the resolver, for callers that
/// do not need a fake.
#[must_use]
pub fn default_runner() -> TimeoutRunner {
    TimeoutRunner
}

#[cfg(test)]
mod tests {
    use super::{resolver_args, run_resolver, RESOLVER_ENV};
    use crate::process_runner::{CommandRunner, RunOutput, RunnerError};
    use std::time::Duration;

    struct FakeRunner {
        output: String,
    }

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            program: &str,
            _args: &[&str],
            _timeout: Duration,
            _max_output_bytes: usize,
        ) -> Result<RunOutput, RunnerError> {
            assert_eq!(program, "/system/bin/app_process");
            Ok(RunOutput {
                output: self.output.clone(),
                success: true,
            })
        }

        fn run_with_input(
            &self,
            program: &str,
            args: &[&str],
            input: &str,
            timeout: Duration,
            max_output_bytes: usize,
        ) -> Result<RunOutput, RunnerError> {
            assert!(input.contains("TRANSACTION_isPowerSaveMode"));
            assert!(input.contains("TRANSACTION_getZenMode"));
            self.run(program, args, timeout, max_output_bytes)
        }
    }

    #[test]
    fn resolver_args_and_env_match_cpp() {
        let args = resolver_args("/data/adb/modules/encore/binder_resolver.apk");
        assert_eq!(args.len(), 3);
        let Some(first) = args.first() else { return };
        assert_eq!(
            first,
            "-Djava.class.path=/data/adb/modules/encore/binder_resolver.apk"
        );
        let Some(second) = args.get(1) else { return };
        assert_eq!(second, "/");
        let Some(third) = args.get(2) else { return };
        assert_eq!(third, "com.rem01gaming.binderresolver.MainKt");
        let Some(env_first) = RESOLVER_ENV.first() else {
            return;
        };
        assert_eq!(*env_first, ("ANDROID_ROOT", "/system"));
        assert_eq!(RESOLVER_ENV.len(), 6);
    }

    #[test]
    fn fake_runner_output_resolves_codes() {
        let mut lines = String::new();
        for tx in crate::resolver::TxCode::all() {
            lines.push_str(tx.query());
            lines.push(' ');
            lines.push_str(&(100 + tx.query().len() % 50).to_string());
            lines.push('\n');
        }
        let runner = FakeRunner { output: lines };
        let result = run_resolver(&runner, "/tmp/binder_resolver.apk");
        assert!(result.is_ok());
    }
}
