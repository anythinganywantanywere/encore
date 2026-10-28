//! The Binder transaction-code resolver protocol.
//!
//! Verified from `jni/base/BinderMonitor/BinderMonitor.cpp`:
//!
//! - At `BinderMonitor::initialize`, the daemon forks and
//!   execs `/system/bin/app_process` with
//!   `-Djava.class.path=<MODPATH>/binder_resolver.apk`,
//!   main class `com.rem01gaming.binderresolver.MainKt`,
//!   after setting the Android runtime environment
//!   variables (`ANDROID_ROOT=/system`, `ANDROID_DATA=/data`,
//!   and the `com.android.runtime`, `art`, `i18n`, and
//!   `tzdata` apex roots) without overwriting existing
//!   values. The Java source for that APK is not in this
//!   clone; only the prebuilt APK and the query strings
//!   the C++ side sends are visible, so the Java side is
//!   documented here as a protocol, not as verified code.
//! - The C++ side writes one query per line to the
//!   child's stdin, in the order of `kResolverQueries`,
//!   then closes stdin and reads stdout until EOF.
//! - Each output line is `<query> <code>`, split at the
//!   last space. Lines starting with `ERROR:` are logged
//!   and skipped, lines without a space are skipped, and
//!   lines whose code does not parse are logged and
//!   skipped. A non-zero exit or a signal does not discard
//!   the codes already parsed; the caller then fails if a
//!   required code is still zero.
//! - Fallbacks are 0 for every code except
//!   `OnDisplayEvent`, whose fallback is 1. Required
//!   codes are all except `OnProcessStarted` and
//!   `OnDisplayEvent`; a required code that is still 0
//!   after resolution makes `initialize` fail.
//! - Services, in order, are `power`, `notification`,
//!   `activity`, `display`, and `package`, each waited
//!   for up to 10 seconds (100 ms polls) when the
//!   `AServiceManager_waitForService` symbol is absent.
//!
//! What is NOT in this module, deliberately:
//!
//! - The actual subprocess execution. The C++ resolver
//!   runs with no timeout and an unbounded output buffer
//!   (analysis finding 6). A Rust runner must add both
//!   before it is used on a device, so no runner is
//!   claimed here. This module is the protocol: the query
//!   table, the output parser, and the required-code
//!   check, all testable on a host without Android.
//! - The `AIBinder` transport itself: `AServiceManager`,
//!   parcels, and the Binder thread pool need the Android
//!   NDK and `/dev/binder`, which do not exist on a host.
//!   The state machine that consumes Binder events lives
//!   in [`crate::daemon_state`]; the transport that feeds
//!   it is future Android-only work.

use std::collections::HashMap;

/// A transaction-code query, as sent to the resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TxCode {
    /// `android.os.IPowerManager` power-save query.
    IsPowerSaveMode,
    /// `android.os.IPowerManager` interactive query.
    IsInteractive,
    /// `android.app.INotificationManager` zen-mode query.
    GetZenMode,
    /// `android.app.IActivityManager` observer registration.
    RegisterProcessObserver,
    /// `android.hardware.display.IDisplayManager` callback.
    RegisterDisplayCallback,
    /// `android.app.IProcessObserver` foreground callback.
    OnForegroundActivitiesChanged,
    /// `android.app.IProcessObserver` foreground-services callback.
    OnForegroundServicesChanged,
    /// `android.app.IProcessObserver` process-died callback.
    OnProcessDied,
    /// `android.app.IProcessObserver` process-started callback, optional.
    OnProcessStarted,
    /// `android.hardware.display.IDisplayManagerCallback` event, optional.
    OnDisplayEvent,
    /// `android.content.pm.IPackageManager` name-for-uid query.
    GetPackageNameForUid,
}

impl TxCode {
    /// The query string sent to the resolver for this code.
    #[must_use]
    pub fn query(self) -> &'static str {
        match self {
            Self::IsPowerSaveMode => "android.os.IPowerManager.Stub::TRANSACTION_isPowerSaveMode",
            Self::IsInteractive => "android.os.IPowerManager.Stub::TRANSACTION_isInteractive",
            Self::GetZenMode => "android.app.INotificationManager.Stub::TRANSACTION_getZenMode",
            Self::RegisterProcessObserver => {
                "android.app.IActivityManager.Stub::TRANSACTION_registerProcessObserver"
            }
            Self::RegisterDisplayCallback => {
                "android.hardware.display.IDisplayManager.Stub::TRANSACTION_registerCallback"
            }
            Self::OnForegroundActivitiesChanged => {
                "android.app.IProcessObserver.Stub::TRANSACTION_onForegroundActivitiesChanged"
            }
            Self::OnForegroundServicesChanged => {
                "android.app.IProcessObserver.Stub::TRANSACTION_onForegroundServicesChanged"
            }
            Self::OnProcessDied => "android.app.IProcessObserver.Stub::TRANSACTION_onProcessDied",
            Self::OnProcessStarted => {
                "android.app.IProcessObserver.Stub::TRANSACTION_onProcessStarted"
            }
            Self::OnDisplayEvent => {
                "android.hardware.display.IDisplayManagerCallback.Stub::TRANSACTION_onDisplayEvent"
            }
            Self::GetPackageNameForUid => {
                "android.content.pm.IPackageManager.Stub::TRANSACTION_getNameForUid"
            }
        }
    }

    /// Whether a zero code after resolution fails
    /// initialisation, as in `kResolverQueries`.
    #[must_use]
    pub fn is_required(self) -> bool {
        !matches!(self, Self::OnProcessStarted | Self::OnDisplayEvent)
    }

    /// The fallback code used before resolution.
    #[must_use]
    pub fn fallback(self) -> u32 {
        u32::from(self == Self::OnDisplayEvent)
    }

    /// All codes, in the order the C++ side sends them.
    /// The underlying map order in C++ is unspecified, so
    /// this order is the documented query order, not a
    /// claim about the C++ iteration order.
    #[must_use]
    pub fn all() -> &'static [Self] {
        &[
            Self::IsPowerSaveMode,
            Self::IsInteractive,
            Self::GetZenMode,
            Self::RegisterProcessObserver,
            Self::RegisterDisplayCallback,
            Self::OnForegroundActivitiesChanged,
            Self::OnForegroundServicesChanged,
            Self::OnProcessDied,
            Self::OnProcessStarted,
            Self::OnDisplayEvent,
            Self::GetPackageNameForUid,
        ]
    }
}

/// The Binder services acquired at initialisation, in order.
pub const SERVICES: &[&str] = &["power", "notification", "activity", "display", "package"];

/// The resolver APK path, relative to the module path, as
/// in C++ (`MODPATH "/binder_resolver.apk"`).
pub const RESOLVER_APK_NAME: &str = "binder_resolver.apk";

/// The resolver main class, as passed to `app_process`.
pub const RESOLVER_MAIN_CLASS: &str = "com.rem01gaming.binderresolver.MainKt";

/// Parse resolver stdout into a query-to-code map,
/// following the C++ rules: `ERROR:` lines, lines without
/// a space, and unparseable codes are skipped.
#[must_use]
pub fn parse_resolver_output(output: &str) -> HashMap<String, u32> {
    let mut result = HashMap::new();
    for line in output.lines() {
        if line.starts_with("ERROR:") {
            continue;
        }
        let Some(space) = line.rfind(' ') else {
            continue;
        };
        let Some(query) = line.get(..space) else {
            continue;
        };
        let Some(code_text) = line.get(space + 1..) else {
            continue;
        };
        if let Ok(code) = code_text.trim().parse::<u32>() {
            result.insert(query.to_owned(), code);
        }
    }
    result
}

/// A resolved transaction-code table.
#[derive(Debug, Clone)]
pub struct ResolvedCodes {
    codes: HashMap<TxCode, u32>,
}

impl ResolvedCodes {
    /// Resolve codes from parsed resolver output, starting
    /// from fallbacks and failing on a required code that
    /// is still zero. Returns the missing required query
    /// on failure, so the caller can log it as C++ does.
    ///
    /// # Errors
    ///
    /// Returns the first missing required query string.
    pub fn resolve(parsed: &HashMap<String, u32>) -> Result<Self, String> {
        let mut codes = HashMap::new();
        for code in TxCode::all() {
            let mut value = code.fallback();
            if let Some(resolved) = parsed.get(code.query()) {
                value = *resolved;
            }
            if code.is_required() && value == 0 {
                return Err(code.query().to_owned());
            }
            codes.insert(*code, value);
        }
        Ok(Self { codes })
    }

    /// The code for a transaction, or 0 when unknown.
    #[must_use]
    pub fn code(&self, tx: TxCode) -> u32 {
        self.codes.get(&tx).copied().unwrap_or(0)
    }

    /// Whether the optional process-started callback is
    /// available, as C++ logs when it is not resolved.
    #[must_use]
    pub fn process_started_available(&self) -> bool {
        self.code(TxCode::OnProcessStarted) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_resolver_output, ResolvedCodes, TxCode, SERVICES};

    fn full_output() -> String {
        let mut lines = Vec::new();
        for (index, code) in TxCode::all().iter().enumerate() {
            lines.push(format!("{} {}", code.query(), index + 10));
        }
        lines.join("\n")
    }

    #[test]
    fn parse_skips_errors_and_bad_lines() {
        let output = "ERROR: something failed\nno-space-line\nandroid.os.IPowerManager.Stub::TRANSACTION_isPowerSaveMode 42\nandroid.os.IPowerManager.Stub::TRANSACTION_isInteractive not-a-number\n";
        let parsed = parse_resolver_output(output);
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            parsed.get("android.os.IPowerManager.Stub::TRANSACTION_isPowerSaveMode"),
            Some(&42)
        );
    }

    #[test]
    fn resolve_fails_on_missing_required_code() {
        let parsed =
            parse_resolver_output("android.os.IPowerManager.Stub::TRANSACTION_isPowerSaveMode 7\n");
        let result = ResolvedCodes::resolve(&parsed);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_uses_fallback_for_optional_display_event() {
        let mut parsed = parse_resolver_output(&full_output());
        parsed.remove(TxCode::OnDisplayEvent.query());
        parsed.remove(TxCode::OnProcessStarted.query());
        let result = ResolvedCodes::resolve(&parsed);
        assert!(result.is_ok());
        let Ok(resolved) = result else {
            return;
        };
        assert_eq!(resolved.code(TxCode::OnDisplayEvent), 1);
        assert_eq!(resolved.code(TxCode::OnProcessStarted), 0);
        assert!(!resolved.process_started_available());
        assert_eq!(
            SERVICES,
            &["power", "notification", "activity", "display", "package"]
        );
    }
}
