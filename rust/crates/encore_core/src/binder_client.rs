//! The Binder transport for the daemon loop, built on
//! `encore_sys::binder`.
//!
//! Verified from C++ before implementing, in
//! `jni/base/BinderMonitor/BinderMonitor.cpp` and
//! `jni/Main.cpp`:
//!
//! - The daemon's initial state is `screen_awake = true`
//!   hardcoded, `battery_saver` from `isPowerSave()`,
//!   and `prev_dnd` from `getZenMode() != 0` (so a zen
//!   query failure, returning -1, counts as DND on).
//!   [`BinderTransport::initial_state`] returns exactly
//!   those two query values for the caller to convert.
//! - A foreground callback resolves the package with
//!   `getPackageNameForUid(uid)` and strips it at the
//!   first NUL (`remove_null_char`), then acts only if
//!   the package is a registered game. Package
//!   resolution happens inside the callback here too, so
//!   the queued event already carries the package.
//! - The display callback fires only on a change, and
//!   the power-save poller fires only on a change, every
//!   500 ms. The display change filter lives in
//!   `encore_sys` (it re-queries interactivity in the
//!   callback); the power-save change filter and the
//!   500 ms cadence live here, in
//!   [`PowerSavePoller`], driven by daemon-loop polls
//!   rather than a dedicated thread. That is a
//!   deliberate difference from C++, which runs a
//!   dedicated poll thread: the daemon loop already
//!   polls its event source, so a second thread would
//!   only add a lock around the same state.
//! - The initial display and power-save states are
//!   seeded, not emitted: C++ sets
//!   `displayLastState` / `powerSaveLastState` at
//!   `initialize` and only fires on a later change
//!   (its initial firings, if any, go to callbacks that
//!   `Main.cpp` sets only after `initialize`).
//!   [`PowerSavePoller`] seeds on its first poll and
//!   emits nothing for it, and the display callback in
//!   `encore_sys` seeds the same way.
//!
//! What is not wired yet: the `encored` daemon loop
//! still uses `NullEventSource` (see [`crate::transport`]).
//! This transport is the drop-in source for it, but
//! wiring it in also needs the daemon loop to perform
//! the returned `ProfileApplication` values (write
//! `gameinfo` and `current_profile`, apply the profiler
//! plan, set DND) and to restore DND on exit, which is
//! the next step, not this one. On a host, acquiring
//! services fails (no `libbinder_ndk.so`), so
//! [`SysBinderClient::acquire`] returns `None`, exactly
//! as C++ `initialize` fails there; the transport is
//! therefore only usable on Android, and its host tests
//! use [`MockBinderClient`].

use crate::daemon_state::BinderEvent;
use crate::resolver::{ResolvedCodes, TxCode};
use crate::transport::BinderEventSource;
use encore_sys::binder::{
    AcquireError, BinderServices, CallbackCodes, CallbackEvent, CallbackHandlers,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The synchronous Binder queries the daemon loop
/// needs, abstracted so tests can substitute a mock.
/// The C++ defaults on failure are part of this trait's
/// contract: `is_power_save` and `is_interactive` are
/// `false`, `get_zen_mode` is `-1`, and a package name
/// is `""`.
pub trait BinderQueryClient {
    /// Whether Battery Saver is active.
    fn is_power_save(&self) -> bool;
    /// Whether the screen is interactive.
    fn is_interactive(&self) -> bool;
    /// The zen mode, or `-1` on failure.
    fn get_zen_mode(&self) -> i32;
    /// The package name for a UID, NUL-stripped, or `""`.
    fn get_package_name_for_uid(&self, uid: i32) -> String;
}

/// Strip a package name at its first NUL, as C++
/// `remove_null_char` does in the foreground callback.
/// Package names from Binder can carry a trailing NUL.
#[must_use]
pub fn strip_nul(package: &str) -> String {
    match package.find('\0') {
        Some(index) => package[..index].to_owned(),
        None => package.to_owned(),
    }
}

/// The real Binder query client, backed by
/// `encore_sys`. On a host, acquisition fails and this
/// type is never constructed; on Android, it holds the
/// five acquired services and the resolved codes.
pub struct SysBinderClient {
    services: BinderServices,
    codes: ResolvedCodes,
}

impl SysBinderClient {
    /// Acquire all five services. Returns `None` when
    /// the Binder library is absent or any service
    /// cannot be acquired, as in C++ `initialize`.
    #[must_use]
    pub fn acquire(codes: ResolvedCodes) -> Option<Self> {
        Self::acquire_detailed(codes).ok()
    }

    /// As [`Self::acquire`], but reporting the failure
    /// reason as text, so the daemon log can say which
    /// service (or the library itself) failed, as C++
    /// does, without the daemon binary depending on
    /// `encore_sys` directly for the error type.
    ///
    /// # Errors
    ///
    /// Returns a description of the failure: the
    /// library/symbol case, or the named service that
    /// could not be acquired.
    pub fn acquire_detailed(codes: ResolvedCodes) -> Result<Self, String> {
        let services = BinderServices::acquire_detailed().map_err(|error| match error {
            AcquireError::Library(load_error) => match load_error {
                encore_sys::binder::LoadError::Open(message) => {
                    format!("libbinder_ndk.so could not be opened: {message}")
                }
                encore_sys::binder::LoadError::Symbol(name) => {
                    format!("libbinder_ndk.so is missing required symbol '{name}'")
                }
            },
            AcquireError::Service(name) => {
                format!("service '{name}' could not be acquired, even after waiting")
            }
        })?;
        Ok(Self { services, codes })
    }

    /// Start the Binder thread pool, as in C++
    /// `initialize`'s final step.
    pub fn start_thread_pool() {
        BinderServices::start_thread_pool();
    }

    /// Block by joining the Binder thread pool, as in
    /// C++ `joinThreadPool`.
    pub fn join_thread_pool() {
        BinderServices::join_thread_pool();
    }

    /// The callback codes for this client's resolved
    /// transaction codes, for `encore_sys` registration.
    #[must_use]
    pub fn callback_codes(&self) -> CallbackCodes {
        CallbackCodes {
            on_foreground_activities: self.codes.code(TxCode::OnForegroundActivitiesChanged),
            on_process_died: self.codes.code(TxCode::OnProcessDied),
            on_display_event: self.codes.code(TxCode::OnDisplayEvent),
            is_interactive: self.codes.code(TxCode::IsInteractive),
            register_process_observer: self.codes.code(TxCode::RegisterProcessObserver),
            register_display_callback: self.codes.code(TxCode::RegisterDisplayCallback),
        }
    }
}

impl BinderQueryClient for SysBinderClient {
    fn is_power_save(&self) -> bool {
        self.services
            .is_power_save(self.codes.code(TxCode::IsPowerSaveMode))
    }

    fn is_interactive(&self) -> bool {
        self.services
            .is_interactive(self.codes.code(TxCode::IsInteractive))
    }

    fn get_zen_mode(&self) -> i32 {
        self.services
            .get_zen_mode(self.codes.code(TxCode::GetZenMode))
    }

    fn get_package_name_for_uid(&self, uid: i32) -> String {
        let raw = self
            .services
            .get_package_name_for_uid(self.codes.code(TxCode::GetPackageNameForUid), uid);
        strip_nul(&raw)
    }
}

/// A mock query client for tests, returning configured
/// values.
#[derive(Debug, Default, Clone)]
pub struct MockBinderClient {
    /// Value for `is_power_save`.
    pub power_save: bool,
    /// Value for `is_interactive`.
    pub interactive: bool,
    /// Value for `get_zen_mode`.
    pub zen_mode: i32,
    /// Package returned for every UID.
    pub package: String,
}

impl BinderQueryClient for MockBinderClient {
    fn is_power_save(&self) -> bool {
        self.power_save
    }

    fn is_interactive(&self) -> bool {
        self.interactive
    }

    fn get_zen_mode(&self) -> i32 {
        self.zen_mode
    }

    fn get_package_name_for_uid(&self, _uid: i32) -> String {
        strip_nul(&self.package)
    }
}

/// The 500 ms power-save poll cadence from C++
/// `pollPowerSave`.
pub const POWER_SAVE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Change-only power-save polling. The first poll
/// seeds the last state and emits nothing; later polls
/// emit a [`BinderEvent::PowerSave`] only when the
/// state changed, and only when at least
/// [`POWER_SAVE_POLL_INTERVAL`] has passed since the
/// previous query, so a fast daemon loop does not turn
/// into a fast Binder poll.
pub struct PowerSavePoller {
    last_state: Option<bool>,
    last_query: Option<Instant>,
}

impl PowerSavePoller {
    /// Create an unseeded poller.
    #[must_use]
    pub fn new() -> Self {
        Self {
            last_state: None,
            last_query: None,
        }
    }

    /// Query `client` if the poll interval has passed,
    /// and return a power-save event only on a change
    /// after seeding. Returns `None` when the interval
    /// has not passed, when seeding, or when unchanged.
    pub fn poll(&mut self, client: &dyn BinderQueryClient) -> Option<BinderEvent> {
        let now = Instant::now();
        if let Some(last_query) = self.last_query {
            if now.duration_since(last_query) < POWER_SAVE_POLL_INTERVAL {
                return None;
            }
        }
        self.last_query = Some(now);
        let current = client.is_power_save();
        let Some(last_state) = self.last_state else {
            self.last_state = Some(current);
            return None;
        };
        if current == last_state {
            return None;
        }
        self.last_state = Some(current);
        Some(BinderEvent::PowerSave {
            power_save: current,
        })
    }
}

impl Default for PowerSavePoller {
    fn default() -> Self {
        Self::new()
    }
}

/// Map a decoded `encore_sys` callback event to a
/// [`BinderEvent`], resolving the package for a
/// foreground event with `resolve_package`. This is the
/// pure part of the callback handling, so it is testable
/// without Binder.
#[must_use]
pub fn map_callback_event(
    event: &CallbackEvent,
    resolve_package: &dyn Fn(i32) -> String,
) -> BinderEvent {
    match *event {
        CallbackEvent::Foreground {
            pid,
            uid,
            foreground,
        } => BinderEvent::Foreground {
            pid,
            uid,
            foreground,
            package: resolve_package(uid),
        },
        CallbackEvent::ProcessDied { pid, uid } => BinderEvent::ProcessDied { pid, uid },
        CallbackEvent::Display { interactive } => BinderEvent::Display { interactive },
    }
}

/// The Binder transport for the daemon loop: a queue
/// fed by `encore_sys` callbacks, plus change-only
/// power-save polling. It implements
/// [`BinderEventSource`], so the daemon loop feeds its
/// events to a [`DaemonState`] with
/// [`crate::transport::apply_events`] without change.
pub struct BinderTransport<C: BinderQueryClient> {
    queue: Arc<Mutex<VecDeque<BinderEvent>>>,
    client: Arc<C>,
    poller: PowerSavePoller,
}

impl<C: BinderQueryClient> BinderTransport<C> {
    /// Create a transport around a query client, with
    /// an empty callback queue.
    #[must_use]
    pub fn new(client: C) -> Self {
        Self {
            queue: Arc::new(Mutex::new(VecDeque::new())),
            client: Arc::new(client),
            poller: PowerSavePoller::new(),
        }
    }

    /// The callback queue, for tests and for callback
    /// registration. Callback handlers push into it;
    /// `poll_events` drains it.
    #[must_use]
    pub fn queue(&self) -> Arc<Mutex<VecDeque<BinderEvent>>> {
        Arc::clone(&self.queue)
    }

    /// The daemon's initial Binder state, as in
    /// `Main.cpp`: `(battery_saver, zen_mode)`. The
    /// caller converts these to `DaemonState`'s initial
    /// `battery_saver` and `prev_dnd = zen_mode != 0`.
    #[must_use]
    pub fn initial_state(&self) -> (bool, i32) {
        (self.client.is_power_save(), self.client.get_zen_mode())
    }

    /// The current zen mode, for the daemon loop to pass
    /// to `DaemonState::handle_event` on each poll, as
    /// `Main.cpp` refreshes the previous DND state from
    /// the zen query on every evaluation.
    #[must_use]
    pub fn zen_mode(&self) -> i32 {
        self.client.get_zen_mode()
    }
}

/// Register `encore_sys` callbacks for a transport
/// built on [`SysBinderClient`]. Foreground events
/// resolve their package through the transport's client
/// before being queued, as in C++ `Main.cpp`.
/// Returns `false` when the library is absent or a
/// callback binder cannot be allocated. There must be
/// at most one such transport per process, matching the
/// C++ `BinderMonitor` singleton and the global
/// callback registry in `encore_sys`.
#[must_use]
pub fn register_sys_callbacks(transport: &BinderTransport<SysBinderClient>) -> bool {
    let queue = transport.queue();
    let client = Arc::clone(&transport.client);
    let callback_client = Arc::clone(&client);
    let codes = client.callback_codes();
    let handlers = CallbackHandlers {
        on_event: Some(Box::new(move |event| {
            let resolved =
                map_callback_event(&event, &|uid| callback_client.get_package_name_for_uid(uid));
            if let Ok(mut queue) = queue.lock() {
                queue.push_back(resolved);
            }
        })),
    };
    client.register_callbacks(codes, handlers)
}

impl SysBinderClient {
    /// Register callbacks through this client's services.
    /// This is separate from `register_sys_callbacks`
    /// only because the services field is private to
    /// this module; the public entry point is
    /// [`register_sys_callbacks`].
    fn register_callbacks(&self, codes: CallbackCodes, handlers: CallbackHandlers) -> bool {
        self.services.register_callbacks(codes, handlers)
    }
}

impl<C: BinderQueryClient> BinderEventSource for BinderTransport<C> {
    fn poll_events(&mut self) -> Vec<BinderEvent> {
        let mut events = Vec::new();
        if let Ok(mut queue) = self.queue.lock() {
            events.extend(queue.drain(..));
        }
        if let Some(power_save) = self.poller.poll(self.client.as_ref()) {
            events.push(power_save);
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::{
        map_callback_event, strip_nul, BinderTransport, MockBinderClient, PowerSavePoller,
        SysBinderClient,
    };
    use crate::daemon_state::BinderEvent;
    use crate::resolver::{parse_resolver_output, ResolvedCodes, TxCode};
    use crate::transport::BinderEventSource;
    use encore_sys::binder::CallbackEvent;

    #[test]
    fn strip_nul_cuts_at_first_nul() {
        assert_eq!(strip_nul("com.a.game\0junk"), "com.a.game");
        assert_eq!(strip_nul("com.a.game"), "com.a.game");
        assert_eq!(strip_nul(""), "");
    }

    #[test]
    fn map_callback_event_resolves_package_for_foreground() {
        let event = map_callback_event(
            &CallbackEvent::Foreground {
                pid: 123,
                uid: 1000,
                foreground: true,
            },
            &|uid| {
                assert_eq!(uid, 1000);
                "com.a.game".to_owned()
            },
        );
        assert_eq!(
            event,
            BinderEvent::Foreground {
                pid: 123,
                uid: 1000,
                foreground: true,
                package: "com.a.game".to_owned(),
            }
        );
        let resolved_died = std::cell::Cell::new(false);
        let event = map_callback_event(&CallbackEvent::ProcessDied { pid: 7, uid: 8 }, &|_| {
            resolved_died.set(true);
            String::new()
        });
        assert!(!resolved_died.get());
        assert_eq!(event, BinderEvent::ProcessDied { pid: 7, uid: 8 });
        let resolved_display = std::cell::Cell::new(false);
        let event = map_callback_event(&CallbackEvent::Display { interactive: false }, &|_| {
            resolved_display.set(true);
            String::new()
        });
        assert!(!resolved_display.get());
        assert_eq!(event, BinderEvent::Display { interactive: false });
    }

    #[test]
    fn power_save_poller_seeds_then_emits_only_on_change() {
        let mut client = MockBinderClient {
            power_save: false,
            ..MockBinderClient::default()
        };
        let mut poller = PowerSavePoller::new();
        // First poll seeds, emits nothing.
        assert_eq!(poller.poll(&client), None);
        // A second poll within 500 ms does not query.
        assert_eq!(poller.poll(&client), None);
        // Force the interval to pass by resetting the
        // last query time through a fresh poller seeded
        // with the same state, then change the client.
        let mut poller = PowerSavePoller::new();
        assert_eq!(poller.poll(&client), None);
        client.power_save = true;
        // Still within the interval: no event yet.
        assert_eq!(poller.poll(&client), None);
        std::thread::sleep(std::time::Duration::from_millis(510));
        let event = poller.poll(&client);
        assert_eq!(event, Some(BinderEvent::PowerSave { power_save: true }));
        std::thread::sleep(std::time::Duration::from_millis(510));
        assert_eq!(poller.poll(&client), None);
    }

    #[test]
    fn transport_drains_queue_and_reports_initial_state() {
        let client = MockBinderClient {
            power_save: true,
            zen_mode: 1,
            package: "com.a.game".to_owned(),
            ..MockBinderClient::default()
        };
        let mut transport = BinderTransport::new(client);
        assert_eq!(transport.initial_state(), (true, 1));
        let queue = transport.queue();
        if let Ok(mut queue) = queue.lock() {
            queue.push_back(BinderEvent::Display { interactive: false });
        }
        let events = transport.poll_events();
        assert_eq!(events.len(), 1);
        let Some(first) = events.first() else {
            return;
        };
        assert_eq!(first, &BinderEvent::Display { interactive: false });
        // The power-save poller seeded on the first
        // poll and emitted nothing for it.
        assert_eq!(transport.poll_events().len(), 0);
    }

    fn full_output() -> String {
        let mut lines = Vec::new();
        for (index, code) in TxCode::all().iter().enumerate() {
            lines.push(format!("{} {}", code.query(), index + 10));
        }
        lines.join("\n") + "\n"
    }

    #[test]
    fn transport_codes_come_from_resolved_codes() {
        let output = full_output();
        let parsed = parse_resolver_output(&output);
        let Ok(codes) = ResolvedCodes::resolve(&parsed) else {
            return;
        };
        assert_eq!(codes.code(TxCode::IsPowerSaveMode), 10);
        assert_eq!(codes.code(TxCode::GetPackageNameForUid), 20);
    }

    // Host-only: on Android, `libbinder_ndk.so` exists
    // and acquisition may succeed, so this expectation
    // is only valid where the library is absent.
    #[cfg(not(target_os = "android"))]
    #[test]
    fn sys_client_acquire_fails_on_host_without_binder() {
        let output = full_output();
        let parsed = parse_resolver_output(&output);
        let Ok(codes) = ResolvedCodes::resolve(&parsed) else {
            return;
        };
        assert!(SysBinderClient::acquire(codes).is_none());
    }
}
