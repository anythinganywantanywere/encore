//! The Binder event-source seam for the daemon loop.
//!
//! The actual `AIBinder` transport is not in this crate:
//! it needs the Android NDK, `/dev/binder`, and a device
//! (see [`crate::resolver`]). What the daemon loop needs
//! from a transport is much smaller, and it is defined
//! here so the loop can be written and tested against a
//! fake source now, and the real transport can be
//! dropped in later without changing the loop:
//!
//! - A source yields already-decoded [`BinderEvent`]
//!   values, in the order the callbacks in C++
//!   `BinderMonitor` would deliver them.
//! - The loop feeds them to a [`DaemonState`], and the
//!   state machine returns [`ProfileApplication`] values
//!   for the caller to perform (write `gameinfo` and
//!   `current_profile`, apply the profiler plan, set
//!   DND, in that order).
//!
//! `NullEventSource` is the source the host daemon loop
//! uses today: it yields nothing, because there is no
//! Binder on a host. `QueueEventSource` is the test
//! source, and the shape a real transport must match.
//!
//! [`BinderEvent`]: crate::daemon_state::BinderEvent
//! [`DaemonState`]: crate::daemon_state::DaemonState
//! [`ProfileApplication`]: crate::daemon_state::ProfileApplication

use crate::daemon_state::{BinderEvent, DaemonState, EvaluateInputs, ProfileApplication};
use std::collections::VecDeque;

/// A source of decoded Binder events, polled by the
/// daemon loop without blocking.
pub trait BinderEventSource {
    /// Drain the events that have arrived since the last
    /// poll, in delivery order. An empty vector means
    /// no events, not an error or an end of stream.
    fn poll_events(&mut self) -> Vec<BinderEvent>;
}

/// A source that never yields events. This is what the
/// host daemon loop uses until an Android transport
/// exists; it makes the absence of Binder explicit
/// instead of silently pretending events arrived.
#[derive(Debug, Default)]
pub struct NullEventSource;

impl BinderEventSource for NullEventSource {
    fn poll_events(&mut self) -> Vec<BinderEvent> {
        Vec::new()
    }
}

/// A source that yields a queued list of events, for
/// tests and for the shape a real transport must match.
#[derive(Debug, Default)]
pub struct QueueEventSource {
    /// Events still to be delivered.
    queue: VecDeque<BinderEvent>,
}

impl QueueEventSource {
    /// Create a source from an event list, in order.
    #[must_use]
    pub fn new(events: Vec<BinderEvent>) -> Self {
        Self {
            queue: events.into(),
        }
    }
}

impl BinderEventSource for QueueEventSource {
    fn poll_events(&mut self) -> Vec<BinderEvent> {
        self.queue.drain(..).collect()
    }
}

/// Feed every event from `source` through `state` and
/// collect the profile applications it causes, in
/// order. `zen_mode` is the raw value from the Binder
/// zen query at poll time, or -1 on failure, as in
/// [`DaemonState::handle_event`].
///
/// [`DaemonState::handle_event`]: crate::daemon_state::DaemonState::handle_event
#[must_use]
pub fn apply_events(
    state: &mut DaemonState,
    source: &mut dyn BinderEventSource,
    zen_mode: i32,
    inputs: &EvaluateInputs<'_>,
) -> Vec<ProfileApplication> {
    let mut applications = Vec::new();
    for event in source.poll_events() {
        if let Some(application) = state.handle_event(&event, zen_mode, inputs) {
            applications.push(application);
        }
    }
    applications
}

#[cfg(test)]
mod tests {
    use super::{apply_events, BinderEventSource, NullEventSource, QueueEventSource};
    use crate::config::{CpuGovernor, Preferences};
    use crate::daemon_state::{BinderEvent, DaemonState, EvaluateInputs};
    use crate::gamelist::GameEntry;
    use crate::profiler::{ProfilerOptions, Soc};
    use crate::registry::GameRegistry;
    use crate::sysfs::SysfsRoot;

    #[test]
    fn queue_source_feeds_state_machine_in_order() {
        let registry = GameRegistry::new();
        registry.replace(vec![GameEntry {
            package_name: "com.a.game".to_owned(),
            lite_mode: false,
            enable_dnd: false,
        }]);
        let mut state = DaemonState::new(false, false);
        let prefs = Preferences {
            enforce_lite_mode: false,
            use_device_mitigation: false,
            disable_tweaks: true,
            log_level: 4,
        };
        let governors = CpuGovernor {
            balance: "schedutil".to_owned(),
            powersave: "powersave".to_owned(),
        };
        let options = ProfilerOptions::default();
        let root = SysfsRoot::device();
        let inputs = EvaluateInputs {
            registry: &registry,
            preferences: &prefs,
            governors: &governors,
            soc: Soc::Unknown,
            options: &options,
            root: &root,
            is_alive: &|_| true,
        };
        let mut source = QueueEventSource::new(vec![
            BinderEvent::Foreground {
                pid: 123,
                uid: 1000,
                foreground: true,
                package: "com.a.game".to_owned(),
            },
            BinderEvent::Display { interactive: false },
        ]);
        let applications = apply_events(&mut state, &mut source, 0, &inputs);
        assert_eq!(applications.len(), 2);
        let Some(first) = applications.first() else {
            return;
        };
        assert_eq!(first.game_info, "com.a.game 123 1000");
        assert_eq!(first.profile_code, 1);
        let Some(second) = applications.get(1) else {
            return;
        };
        assert_eq!(second.profile_code, 2);
        let mut null = NullEventSource;
        assert_eq!(null.poll_events().len(), 0);
    }
}
