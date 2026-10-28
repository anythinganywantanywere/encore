//! The Binder-driven daemon state machine, from `jni/Main.cpp`.
//!
//! Verified from `Main.cpp` (`DaemonState`,
//! `apply_game_profile`, `evaluate_and_apply_profile`,
//! and the four callbacks in `encore_main_daemon`):
//!
//! - Initial state: mode perfcommon, no active game,
//!   screen awake, battery saver from the Binder power
//!   service, and previous DND from the Binder zen mode.
//!   A quirk is preserved: the C++ code sets
//!   `screen_awake` to `true` at startup regardless of the
//!   actual display state, even though the Binder monitor
//!   queries interactivity during initialisation. The
//!   first display event corrects it.
//! - Zen tracking: at the start of every evaluation, when
//!   the daemon is not currently overriding DND for a
//!   game, the previous DND state is refreshed from the
//!   current zen mode. A zen failure returns -1 in C++,
//!   and `-1 != 0` is true, so a failure is treated as
//!   DND on. That quirk is preserved here: the caller
//!   passes the raw zen value, and `-1` counts as on.
//! - Foreground events: background (`foreground=false`)
//!   events are ignored entirely, as are events whose
//!   package is empty or not registered as a game, and
//!   events for the already-active PID. A new game PID
//!   sets the active package, PID, and UID, then evaluates.
//!   The package name is stripped at its first NUL, as
//!   C++ `remove_null_char` does.
//! - Process-died events: only for the active PID, they
//!   clear DND if the game requested it, clear the active
//!   package, PID, and last-applied PID (but not the UID,
//!   as in C++), then evaluate.
//! - Display events set screen-awake and evaluate;
//!   power-save events set battery-saver and evaluate.
//! - Evaluation order: (1) an active, awake game, unless
//!   already in performance mode for that exact PID;
//!   (2) battery saver; (3) balance. Performance wins
//!   over battery saver when a game is active and awake.
//! - Game application: the registry entry is looked up
//!   once, and the entry's `lite_mode` and `enable_dnd`
//!   are used for both the profile and the DND decision.
//!   This is the fix for analysis finding 1 at the daemon
//!   layer: C++ re-reads the raw registry pointer after
//!   the long profile application, during which a gamelist
//!   reload could free it. Here the lookup returns an
//!   owned copy (see [`crate::registry`]), and no pointer
//!   is re-read after applying.
//! - Game application failure clears the active package,
//!   PID, UID, and last-applied PID, whether the game left
//!   the registry or its process exited (checked with
//!   `kill(pid, 0)` in C++).
//! - Mode changes clear the last-applied PID, and the
//!   balance/powersave branches clear the active UID but
//!   not the package or PID, as in C++. Those quirks are
//!   preserved and marked in the code, not silently fixed.
//! - Profile files: every application writes `gameinfo`
//!   and the profile code, even when tweaks are disabled.
//!   Performance writes `"<pkg> <pid> <uid>"` and code 1;
//!   perfcommon, balance, and powersave write
//!   `"NULL 0 0"` and codes 0, 2, and 3. When tweaks are
//!   disabled, the profiler plan is empty but the files
//!   are still written, as in `Profiler.cpp`.
//! - DND: a game with `enable_dnd` sets DND on and marks
//!   the game as requesting it; a game without it sets
//!   DND to the previous state and clears the request.
//!   Leaving performance mode (balance or powersave, or
//!   a game exit) restores DND to the previous state only
//!   if a game had requested it.
//! - Finding 5, at the architecture level: C++ holds the
//!   global state mutex across profile application and
//!   DND calls, on a Binder thread. This state machine
//!   holds no lock and performs no I/O: it returns an
//!   action, and the caller applies it after the event
//!   handler returns. The Binder transport that feeds
//!   events to this machine is not in this crate (see
//!   [`crate::resolver`]).
//!
//! What this module does not do: it does not talk to
//! Binder, does not set DND, does not post notifications,
//! and does not write files. Those are caller actions,
//! represented by the returned [`ProfileApplication`].

use crate::config::Preferences;
use crate::profile::ProfileMode;
use crate::profiler::{plan, PlannedWrite, ProfileRequest, ProfilerOptions, Soc};
use crate::registry::GameRegistry;
use crate::sysfs::SysfsRoot;

/// A Binder event, already decoded from a parcel, as the
/// callbacks in `BinderMonitor` deliver them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinderEvent {
    /// A foreground-activity change, with the package
    /// name already resolved for the UID (empty on
    /// failure, as in C++).
    Foreground {
        /// Process ID.
        pid: i32,
        /// User ID.
        uid: i32,
        /// Whether the activity came to the foreground.
        foreground: bool,
        /// Package name for the UID, NUL-stripped.
        package: String,
    },
    /// A process-died notification.
    ProcessDied {
        /// Process ID.
        pid: i32,
        /// User ID.
        uid: i32,
    },
    /// A display interactivity change.
    Display {
        /// Whether the screen is interactive.
        interactive: bool,
    },
    /// A battery-saver state change.
    PowerSave {
        /// Whether battery saver is active.
        power_save: bool,
    },
}

/// A DND action for the caller to perform, or none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DndAction {
    /// Set DND to the given state.
    Set(bool),
    /// Leave DND unchanged.
    None,
}

/// One profile application, as the caller should perform
/// it: write the files, apply the plan (unless empty),
/// and perform the DND action, in that order.
#[derive(Debug, Clone)]
pub struct ProfileApplication {
    /// The `gameinfo` content, without a trailing newline
    /// decision left to the writer: C++ writes it with
    /// a trailing newline.
    pub game_info: String,
    /// The profile code to write to `current_profile`.
    pub profile_code: i32,
    /// The profiler plan, empty when tweaks are disabled
    /// or when the profile needs no sysfs writes.
    pub plan: Vec<PlannedWrite>,
    /// The DND action for this application.
    pub dnd: DndAction,
}

/// Inputs shared by every evaluation, grouped so the
/// evaluation functions stay under clippy's argument limit.
pub struct EvaluateInputs<'a> {
    /// The game registry snapshot source.
    pub registry: &'a GameRegistry,
    /// Config preferences.
    pub preferences: &'a Preferences,
    /// Config CPU governors.
    pub governors: &'a crate::config::CpuGovernor,
    /// The `SoC` family for profiler plans.
    pub soc: Soc,
    /// Mitigation flags for profiler plans.
    pub options: &'a ProfilerOptions,
    /// The sysfs root for profiler plans.
    pub root: &'a SysfsRoot,
    /// Liveness check for a game PID.
    pub is_alive: &'a dyn Fn(i32) -> bool,
}

/// DND tracking, grouped so `DaemonState` does not hold
/// four separate booleans: whether a game is currently
/// overriding DND, and the user's DND state before it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DndTracking {
    /// Whether a game is currently overriding DND.
    pub requested: bool,
    /// The user's DND state before any game override.
    pub previous: bool,
}

/// Daemon state, mirroring C++ `DaemonState`.
#[derive(Debug, Clone)]
pub struct DaemonState {
    /// Current profile mode.
    pub cur_mode: ProfileMode,
    /// Active game package, empty when none.
    pub active_package: String,
    /// Active game PID, 0 when none.
    pub active_game_pid: i32,
    /// Active game UID, 0 when cleared.
    pub active_game_uid: i32,
    /// PID the performance profile was last applied for.
    pub last_applied_pid: i32,
    /// Whether the screen is interactive.
    pub screen_awake: bool,
    /// Whether battery saver is active.
    pub battery_saver_state: bool,
    /// DND override tracking.
    pub dnd: DndTracking,
}

impl Default for DaemonState {
    fn default() -> Self {
        Self {
            cur_mode: ProfileMode::PerfCommon,
            active_package: String::new(),
            active_game_pid: 0,
            active_game_uid: 0,
            last_applied_pid: 0,
            screen_awake: true,
            battery_saver_state: false,
            dnd: DndTracking::default(),
        }
    }
}

impl DaemonState {
    /// A fresh state, with battery saver and previous DND
    /// taken from the initial Binder queries, as in C++.
    #[must_use]
    pub fn new(battery_saver: bool, prev_dnd: bool) -> Self {
        Self {
            battery_saver_state: battery_saver,
            dnd: DndTracking {
                requested: false,
                previous: prev_dnd,
            },
            ..Self::default()
        }
    }

    /// The perfcommon application for daemon startup:
    /// `gameinfo` `"NULL 0 0"`, code 0, and the perfcommon
    /// plan unless tweaks are disabled.
    #[must_use]
    pub fn perfcommon_application(root: &SysfsRoot, disable_tweaks: bool) -> ProfileApplication {
        let plan = if disable_tweaks {
            Vec::new()
        } else {
            plan(
                root,
                ProfileRequest::PerfCommon,
                Soc::Unknown,
                &ProfilerOptions::default(),
            )
        };
        ProfileApplication {
            game_info: "NULL 0 0".to_owned(),
            profile_code: ProfileMode::PerfCommon.code(),
            plan,
            dnd: DndAction::None,
        }
    }

    /// Handle one Binder event and return the profile
    /// application it causes, or `None` when the event
    /// changes nothing (ignored event, or already in the
    /// target mode for the same game).
    ///
    /// `zen_mode` is the raw value from the Binder zen
    /// query, or -1 on failure; `-1` counts as DND on,
    /// as in C++. `is_alive` is the liveness check for a
    /// game PID, normally [`crate::process::is_alive`].
    /// `soc`, `options`, and `root` are used only when a
    /// profiler plan is built.
    pub fn handle_event(
        &mut self,
        event: &BinderEvent,
        zen_mode: i32,
        inputs: &EvaluateInputs<'_>,
    ) -> Option<ProfileApplication> {
        let mut pre_dnd = DndAction::None;
        match event {
            BinderEvent::Foreground {
                pid,
                uid,
                foreground,
                package,
            } => {
                if !foreground {
                    return None;
                }
                let package = strip_at_nul(package);
                if package.is_empty() || inputs.registry.find(&package).is_none() {
                    return None;
                }
                if self.active_game_pid == *pid {
                    return None;
                }
                self.active_package = package;
                self.active_game_pid = *pid;
                self.active_game_uid = *uid;
            }
            BinderEvent::ProcessDied { pid, .. } => {
                if self.active_game_pid != *pid {
                    return None;
                }
                pre_dnd = self.clear_dnd();
                self.active_package.clear();
                self.active_game_pid = 0;
                self.last_applied_pid = 0;
                // UID is deliberately not cleared here, as in C++.
            }
            BinderEvent::Display { interactive } => {
                self.screen_awake = *interactive;
            }
            BinderEvent::PowerSave { power_save } => {
                self.battery_saver_state = *power_save;
            }
        }
        let mut application = self.evaluate(zen_mode, inputs);
        if pre_dnd != DndAction::None {
            if let Some(app) = &mut application {
                app.dnd = pre_dnd;
            }
        }
        application
    }

    /// Evaluate the current state and return the profile
    /// application, or `None` when already in the target
    /// mode. This is the direct translation of
    /// `evaluate_and_apply_profile`, returning an action
    /// instead of performing I/O under a lock.
    pub fn evaluate(
        &mut self,
        zen_mode: i32,
        inputs: &EvaluateInputs<'_>,
    ) -> Option<ProfileApplication> {
        if !self.dnd.requested {
            self.dnd.previous = zen_mode != 0;
        }
        if self.active_game_pid != 0 && self.screen_awake {
            if self.cur_mode == ProfileMode::Performance
                && self.last_applied_pid == self.active_game_pid
            {
                return None;
            }
            if let Some(application) = self.apply_game_profile(inputs) {
                self.last_applied_pid = self.active_game_pid;
                return Some(application);
            }
            // A failed game application falls through to
            // powersave or balance, as in C++.
        }
        if self.battery_saver_state {
            if self.cur_mode == ProfileMode::Powersave {
                return None;
            }
            self.cur_mode = ProfileMode::Powersave;
            self.last_applied_pid = 0;
            self.active_game_uid = 0;
            let dnd = self.clear_dnd();
            return Some(Self::powersave_application(inputs, dnd));
        }
        if self.cur_mode == ProfileMode::Balance {
            return None;
        }
        self.cur_mode = ProfileMode::Balance;
        self.last_applied_pid = 0;
        self.active_game_uid = 0;
        let dnd = self.clear_dnd();
        Some(Self::balance_application(inputs, dnd))
    }

    /// Apply the performance profile for the active game,
    /// or return `None` and clear the game on failure.
    /// The registry entry is read once into an owned copy.
    fn apply_game_profile(&mut self, inputs: &EvaluateInputs<'_>) -> Option<ProfileApplication> {
        let Some(entry) = inputs.registry.find(&self.active_package) else {
            self.clear_game();
            return None;
        };
        if !(inputs.is_alive)(self.active_game_pid) {
            self.clear_game();
            return None;
        }
        self.cur_mode = ProfileMode::Performance;
        let lite = entry.lite_mode || inputs.preferences.enforce_lite_mode;
        let game_info = format!(
            "{} {} {}",
            self.active_package, self.active_game_pid, self.active_game_uid
        );
        let plan = if inputs.preferences.disable_tweaks {
            Vec::new()
        } else {
            let full_options = full_options(inputs);
            plan(
                inputs.root,
                ProfileRequest::Performance { lite },
                inputs.soc,
                &full_options,
            )
        };
        let dnd = if entry.enable_dnd {
            self.dnd.requested = true;
            DndAction::Set(true)
        } else {
            self.dnd.requested = false;
            DndAction::Set(self.dnd.previous)
        };
        Some(ProfileApplication {
            game_info,
            profile_code: ProfileMode::Performance.code(),
            plan,
            dnd,
        })
    }

    fn balance_application(inputs: &EvaluateInputs<'_>, dnd: DndAction) -> ProfileApplication {
        let plan = if inputs.preferences.disable_tweaks {
            Vec::new()
        } else {
            let full_options = full_options(inputs);
            plan(
                inputs.root,
                ProfileRequest::Balance,
                inputs.soc,
                &full_options,
            )
        };
        ProfileApplication {
            game_info: "NULL 0 0".to_owned(),
            profile_code: ProfileMode::Balance.code(),
            plan,
            dnd,
        }
    }

    fn powersave_application(inputs: &EvaluateInputs<'_>, dnd: DndAction) -> ProfileApplication {
        let plan = if inputs.preferences.disable_tweaks {
            Vec::new()
        } else {
            let full_options = full_options(inputs);
            plan(
                inputs.root,
                ProfileRequest::Powersave,
                inputs.soc,
                &full_options,
            )
        };
        ProfileApplication {
            game_info: "NULL 0 0".to_owned(),
            profile_code: ProfileMode::Powersave.code(),
            plan,
            dnd,
        }
    }

    /// Restore DND to the previous state if a game
    /// requested it, and clear the request.
    fn clear_dnd(&mut self) -> DndAction {
        if self.dnd.requested {
            self.dnd.requested = false;
            DndAction::Set(self.dnd.previous)
        } else {
            DndAction::None
        }
    }

    /// Clear the active game completely, as on a failed
    /// game application.
    fn clear_game(&mut self) {
        self.active_package.clear();
        self.active_game_pid = 0;
        self.active_game_uid = 0;
        self.last_applied_pid = 0;
    }
}

/// Build profiler options with the config governors
/// filled in, as the C++ profiler does from config.
fn full_options(inputs: &EvaluateInputs<'_>) -> ProfilerOptions {
    ProfilerOptions {
        balanced_gov: inputs.governors.balance.clone(),
        powersave_gov: inputs.governors.powersave.clone(),
        ..inputs.options.clone()
    }
}

/// Strip a string at its first NUL, as C++
/// `remove_null_char` does.
fn strip_at_nul(text: &str) -> String {
    match text.find('\0') {
        Some(index) => text.get(..index).unwrap_or("").to_owned(),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{BinderEvent, DaemonState, DndAction, EvaluateInputs, ProfileApplication};
    use crate::config::{CpuGovernor, Preferences};
    use crate::gamelist::GameEntry;
    use crate::profile::ProfileMode;
    use crate::profiler::{ProfilerOptions, Soc};
    use crate::registry::GameRegistry;
    use crate::sysfs::SysfsRoot;

    fn registry_with_game() -> GameRegistry {
        let registry = GameRegistry::new();
        registry.replace(vec![GameEntry {
            package_name: "com.a.game".to_owned(),
            lite_mode: false,
            enable_dnd: true,
        }]);
        registry
    }

    fn prefs() -> Preferences {
        Preferences {
            enforce_lite_mode: false,
            use_device_mitigation: false,
            disable_tweaks: true,
            log_level: 4,
        }
    }

    fn governors() -> CpuGovernor {
        CpuGovernor {
            balance: "schedutil".to_owned(),
            powersave: "powersave".to_owned(),
        }
    }

    fn run_event(
        state: &mut DaemonState,
        event: &BinderEvent,
        registry: &GameRegistry,
    ) -> Option<ProfileApplication> {
        let prefs = prefs();
        let governors = governors();
        let options = ProfilerOptions::default();
        let root = SysfsRoot::device();
        let inputs = EvaluateInputs {
            registry,
            preferences: &prefs,
            governors: &governors,
            soc: Soc::Unknown,
            options: &options,
            root: &root,
            is_alive: &|_| true,
        };
        state.handle_event(event, 0, &inputs)
    }

    fn foreground(pid: i32, package: &str) -> BinderEvent {
        BinderEvent::Foreground {
            pid,
            uid: 1000,
            foreground: true,
            package: package.to_owned(),
        }
    }

    #[test]
    fn foreground_game_applies_performance_and_dnd() {
        let registry = registry_with_game();
        let mut state = DaemonState::new(false, false);
        let application = run_event(&mut state, &foreground(123, "com.a.game"), &registry);
        assert!(application.is_some());
        let application = application.unwrap_or(ProfileApplication {
            game_info: String::new(),
            profile_code: 0,
            plan: Vec::new(),
            dnd: DndAction::None,
        });
        assert_eq!(application.game_info, "com.a.game 123 1000");
        assert_eq!(application.profile_code, ProfileMode::Performance.code());
        assert_eq!(application.dnd, DndAction::Set(true));
        assert_eq!(state.cur_mode, ProfileMode::Performance);
        let again = run_event(&mut state, &foreground(123, "com.a.game"), &registry);
        assert!(again.is_none());
    }

    #[test]
    fn background_non_game_and_dead_pid_are_ignored_or_reset() {
        let registry = registry_with_game();
        let mut state = DaemonState::new(false, false);
        let background = run_event(
            &mut state,
            &BinderEvent::Foreground {
                pid: 123,
                uid: 1000,
                foreground: false,
                package: "com.a.game".to_owned(),
            },
            &registry,
        );
        assert!(background.is_none());
        let non_game = run_event(&mut state, &foreground(124, "com.other.app"), &registry);
        assert!(non_game.is_none());
        let _ = run_event(&mut state, &foreground(123, "com.a.game"), &registry);
        let died = run_event(
            &mut state,
            &BinderEvent::ProcessDied {
                pid: 123,
                uid: 1000,
            },
            &registry,
        );
        assert!(died.is_some());
        let died = died.unwrap_or(ProfileApplication {
            game_info: String::new(),
            profile_code: 0,
            plan: Vec::new(),
            dnd: DndAction::None,
        });
        assert_eq!(died.profile_code, ProfileMode::Balance.code());
        assert_eq!(died.dnd, DndAction::Set(false));
        assert_eq!(state.active_game_pid, 0);
    }

    #[test]
    fn screen_off_falls_back_to_balance_and_back() {
        let registry = registry_with_game();
        let mut state = DaemonState::new(false, false);
        let _ = run_event(&mut state, &foreground(123, "com.a.game"), &registry);
        let off = run_event(
            &mut state,
            &BinderEvent::Display { interactive: false },
            &registry,
        );
        assert!(off.is_some());
        assert_eq!(state.cur_mode, ProfileMode::Balance);
        let on = run_event(
            &mut state,
            &BinderEvent::Display { interactive: true },
            &registry,
        );
        assert!(on.is_some());
        assert_eq!(state.cur_mode, ProfileMode::Performance);
    }

    #[test]
    fn performance_wins_over_powersave_and_powersave_wins_over_balance() {
        let registry = registry_with_game();
        let mut state = DaemonState::new(true, false);
        let _ = run_event(&mut state, &foreground(123, "com.a.game"), &registry);
        assert_eq!(state.cur_mode, ProfileMode::Performance);
        let again = run_event(&mut state, &foreground(123, "com.a.game"), &registry);
        assert!(again.is_none());
        let mut idle = DaemonState::new(true, false);
        let powersave = run_event(
            &mut idle,
            &BinderEvent::PowerSave { power_save: true },
            &registry,
        );
        assert!(powersave.is_some());
        assert_eq!(idle.cur_mode, ProfileMode::Powersave);
    }

    #[test]
    fn registry_miss_falls_through_to_balance() {
        let registry = registry_with_game();
        let mut missing = DaemonState::new(false, false);
        missing.active_package = "com.gone.game".to_owned();
        missing.active_game_pid = 999;
        let prefs = prefs();
        let governors = governors();
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
        let fallback = missing.evaluate(0, &inputs);
        assert!(fallback.is_some());
        assert_eq!(missing.cur_mode, ProfileMode::Balance);
        assert_eq!(missing.active_game_pid, 0);
    }

    #[test]
    fn perfcommon_application_writes_null_and_code_zero() {
        let root = SysfsRoot::device();
        let application = DaemonState::perfcommon_application(&root, true);
        assert_eq!(application.game_info, "NULL 0 0");
        assert_eq!(application.profile_code, ProfileMode::PerfCommon.code());
        assert_eq!(application.plan.len(), 0);
    }
}
