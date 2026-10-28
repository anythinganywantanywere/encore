//! `encored` binary — Rust migration Stage 7 (host part).
//!
//! Stage 4 adds the daemon event loop, on top of Stages 2 and 3:
//!
//! - The `daemon` command loads the gamelist, validates the
//!   device-mitigation file, and loads the config store, mirroring
//!   the preconditions in C++ `cmd_run_daemon` and
//!   `init_file_watcher`, then watches `gamelist.json`,
//!   `config.json`, `device_mitigation.json`, and the module
//!   directory with inotify.
//! - On a close-write of `gamelist.json` the registry is
//!   reloaded; on a close-write of `config.json` the config is
//!   reloaded and the log level is applied from it; on a
//!   close-write of `device_mitigation.json` the file is
//!   re-validated as a JSON object. On creation of a file named
//!   `update` in the module directory the daemon stops, as in C++.
//! - The daemon holds a single-instance flock on
//!   `config_dir/.lock` (the C++ `LOCK_FILE`), so a second
//!   daemon fails instead of running alongside the first.
//! - PID tracking (`is_alive`, `pidof`) is wired as library
//!   code and, for host testing only, as an optional tracked PID
//!   from `ENCORE_TRACK_PID`: when that PID exits, the daemon
//!   logs it and clears the tracked PID. Active-game PID
//!   tracking from Binder foreground callbacks is Stage 6 work,
//!   because there is no Binder monitor in Rust yet.
//! - The daemon runs in the foreground in Stage 4, unlike C++
//!   `daemon(0, 0)`. Foreground running makes host testing and
//!   log inspection straightforward; daemonising is device
//!   integration work for a later stage.
//! - Profiler and Binder handling are not in Stage 4: the
//!   daemon logs that it started and then only reacts to file
//!   changes. It does not apply performance profiles, and it
//!   does not talk to Android services. This binary must not be
//!   installed as `encored` in `service.sh` or the module
//!   installer yet, and the C++ daemon remains the shipped daemon.
//!
//! Paths mirror the C++ constants in `jni/include/Encore.hpp`:
//! `gamelist.json` lives under the config directory, and
//! `module.prop` lives under the module directory. For host testing,
//! and only for host testing, the directories can be overridden with
//! the environment variables `ENCORE_CONFIG_DIR`, `ENCORE_MODPATH`,
//! and `ENCORE_DATA_DIR` (the directory checked for
//! `/data/data/<package>` existence, default `/data/data`). When
//! those variables are absent, the device paths are used unchanged,
//! so device behaviour does not change.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use encore_core::atomic_write::write_atomic;
use encore_core::binder_client::{register_sys_callbacks, BinderTransport, SysBinderClient};
use encore_core::config_store::ConfigStore;
use encore_core::daemon_state::{DaemonState, DndAction, EvaluateInputs, ProfileApplication};
use encore_core::daemonize::{daemonize, DaemonizeOutcome};
use encore_core::dnd::set_dnd_default;
use encore_core::file_util::{is_installed, is_json_object};
use encore_core::log::{LogLevel, Logger, DEFAULT_MAX_LOG_BYTES, DEFAULT_MAX_ROTATED_FILES};
use encore_core::mitigation::{parse_mitigation, DeviceInfo};
use encore_core::module_prop_store::get_module_version;
use encore_core::process::{is_alive, DaemonLock};
use encore_core::profiler::{detect_soc, ProfilerOptions, Soc, SocDetection};
use encore_core::registry::{populate_from_base, render_gamelist, GameRegistry};
use encore_core::resolver_runner::{default_runner, run_resolver};
use encore_core::signals::install_stop_signals;
use encore_core::sysfs::{apply_plan, SysfsRoot};
use encore_core::transport::apply_events;
use encore_core::watcher::{WatchEvent, WatchEventType, WatchKind, Watcher};

/// A parsed top-level command.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    /// Show usage.
    Help,
    /// Show version.
    Version,
    /// Start the daemon; `true` means detach as C++ does.
    Daemon(bool),
    /// Set up a gamelist from a base file.
    SetupGamelist(String),
    /// Validate a gamelist.
    CheckGamelist,
    /// An unknown command, with its name for the error message.
    Unknown(String),
    /// A command with the wrong number of arguments.
    InvalidArguments,
}

/// Parse command-line arguments, mirroring the C++ `main` dispatch.
///
/// The first argument is the program name and is ignored, as in C++.
/// No argument means usage and failure, as in C++.
fn parse_args(args: &[String]) -> Command {
    let Some(command) = args.get(1) else {
        return Command::InvalidArguments;
    };
    match command.as_str() {
        "-h" | "--help" => Command::Help,
        "-V" | "--version" | "version" => Command::Version,
        "daemon" => match args.get(2) {
            None => Command::Daemon(false),
            Some(flag) if flag == "-h" || flag == "--help" => Command::Help,
            Some(flag) if flag == "--daemonize" => Command::Daemon(true),
            Some(_) => Command::InvalidArguments,
        },
        "setup_gamelist" => {
            if args.len() >= 3 {
                if let Some(flag) = args.get(2) {
                    if flag == "-h" || flag == "--help" {
                        return Command::Help;
                    }
                }
            }
            match args.get(2) {
                Some(path) => Command::SetupGamelist(path.clone()),
                None => Command::InvalidArguments,
            }
        }
        "check_gamelist" => {
            if args.len() >= 3 {
                if let Some(flag) = args.get(2) {
                    if flag == "-h" || flag == "--help" {
                        return Command::Help;
                    }
                }
            }
            Command::CheckGamelist
        }
        other => Command::Unknown(other.to_owned()),
    }
}

/// Check whether the current process runs as root (uid 0).
///
/// Reads `/proc/self/status` and looks for the `Uid:` line, whose first
/// field is the real uid. This avoids unsafe code and any new
/// dependency, and it works on Linux, which is the only target for
/// this daemon. If the file cannot be read or parsed, the check fails
/// closed: the caller treats the process as not root.
fn is_root() -> bool {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return false;
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            if let Some(first) = rest.split_whitespace().next() {
                return first == "0";
            }
            return false;
        }
    }
    false
}

/// Resolve a directory from an environment override, or a default.
///
/// The override is for host testing only; see the module documentation.
fn dir_from_env(name: &str, default: &str) -> PathBuf {
    match env::var(name) {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(default),
    }
}

/// The config directory, default `/data/adb/.config/encore`.
fn config_dir() -> PathBuf {
    dir_from_env("ENCORE_CONFIG_DIR", "/data/adb/.config/encore")
}

/// The module directory, default `/data/adb/modules/encore`.
fn modpath() -> PathBuf {
    dir_from_env("ENCORE_MODPATH", "/data/adb/modules/encore")
}

/// The data directory checked for installed packages, default
/// `/data/data`.
fn data_dir() -> PathBuf {
    dir_from_env("ENCORE_DATA_DIR", "/data/data")
}

/// Build `gamelist.json` at `gamelist_path` from a base file.
///
/// Mirrors `cmd_setup_gamelist` and
/// `GameRegistry::populate_from_base`: a package is kept only when a
/// directory of that name exists under `data_dir`, and the output is
/// written atomically with `0644` permissions.
///
/// # Errors
///
/// Returns a message for the caller to print on failure, mirroring
/// the C++ command printing an error and returning failure.
fn run_setup_gamelist(
    gamelist_path: &Path,
    base_path: &Path,
    data_dir: &Path,
) -> Result<usize, String> {
    let base_text = fs::read_to_string(base_path).map_err(|error| {
        format!(
            "Failed to open base gamelist: {}: {error}",
            base_path.display()
        )
    })?;
    let entries = populate_from_base(&base_text, |package_name| {
        is_installed(data_dir, package_name)
    });
    let rendered = render_gamelist(&entries);
    write_atomic(gamelist_path, rendered.as_bytes(), 0o644).map_err(|error| {
        format!(
            "Failed to create gamelist: {}: {error}",
            gamelist_path.display()
        )
    })?;
    Ok(entries.len())
}

/// Validate `gamelist.json` at `gamelist_path`, returning the
/// registered game count.
///
/// Mirrors `cmd_check_gamelist`: a missing file is an error, a
/// parse error is an error, and a successful load reports the count
/// to the caller.
///
/// # Errors
///
/// Returns a message for the caller to print on failure.
fn run_check_gamelist(gamelist_path: &Path) -> Result<usize, String> {
    if !gamelist_path.exists() {
        return Err(format!("{} does not exist", gamelist_path.display()));
    }
    let text = fs::read_to_string(gamelist_path)
        .map_err(|error| format!("Failed to read {}: {error}", gamelist_path.display()))?;
    let registry = GameRegistry::new();
    registry
        .load_from_text(&text)
        .map_err(|error| format!("Failed to parse {}: {error}", gamelist_path.display()))?;
    Ok(registry.len())
}

/// The log level value from `ENCORE_LOG_LEVEL` (0-5, default 4),
/// for host testing. On a device this variable is absent, so the
/// default applies until the daemon loads `config.json`.
fn env_log_level() -> i32 {
    match env::var("ENCORE_LOG_LEVEL") {
        Ok(value) => value.parse::<i32>().unwrap_or(4),
        Err(_) => 4,
    }
}

/// Create the command logger, or `None` when the log file cannot be
/// opened. A log failure must not kill the command, so the caller
/// prints a warning and continues without logging.
fn create_logger(config_dir: &Path) -> Option<Logger> {
    let level_value = env_log_level();
    let log_path = config_dir.join("encore.log");
    match Logger::new(
        &log_path,
        DEFAULT_MAX_LOG_BYTES,
        DEFAULT_MAX_ROTATED_FILES,
        LogLevel::from_config(level_value),
    ) {
        Ok(logger) => Some(logger),
        Err(error) => {
            eprintln!(
                "WARNING: Could not open log file {}: {error}",
                log_path.display()
            );
            None
        }
    }
}

/// Log an info message when a logger is available. Log errors are
/// ignored: logging must not fail a command.
fn log_info(logger: Option<&Logger>, tag: &str, message: &str) {
    if let Some(logger) = logger {
        let _ = logger.info(tag, message);
    }
}

/// Log an error message when a logger is available.
fn log_error(logger: Option<&Logger>, tag: &str, message: &str) {
    if let Some(logger) = logger {
        let _ = logger.error(tag, message);
    }
}

/// Log a warning message when a logger is available.
fn log_warn(logger: Option<&Logger>, tag: &str, message: &str) {
    if let Some(logger) = logger {
        let _ = logger.warn(tag, message);
    }
}

/// Run the `version` command.
fn run_version(logger: Option<&Logger>) -> ExitCode {
    let prop_path = modpath().join("module.prop");
    if !prop_path.exists() {
        eprintln!("ERROR: Could not open {}", prop_path.display());
    }
    let version = get_module_version(&prop_path);
    log_info(logger, "encored", &format!("version: {version}"));
    println!("Encore Tweaks {version}");
    ExitCode::SUCCESS
}

/// Load the daemon's stores, mirroring the preconditions in
/// C++ `cmd_run_daemon` and `init_file_watcher`.
///
/// Returns `None` on failure, after logging and printing the
/// error, so the caller can exit with failure.
fn load_daemon_stores(
    logger: Option<&Logger>,
    gamelist_path: &Path,
    config_path: &Path,
    mitigation_path: &Path,
    default_gov_path: &Path,
) -> Option<(GameRegistry, ConfigStore)> {
    if !gamelist_path.exists() {
        log_error(logger, "encored", "gamelist.json is missing");
        eprintln!("ERROR: {} is missing", gamelist_path.display());
        return None;
    }
    let registry = GameRegistry::new();
    let Ok(gamelist_text) = fs::read_to_string(gamelist_path) else {
        log_error(logger, "encored", "failed to read gamelist.json");
        eprintln!("ERROR: Failed to read {}", gamelist_path.display());
        return None;
    };
    if let Err(error) = registry.load_from_text(&gamelist_text) {
        log_error(
            logger,
            "encored",
            &format!("failed to parse gamelist.json: {error}"),
        );
        eprintln!("ERROR: Failed to parse {}", gamelist_path.display());
        return None;
    }
    if !is_json_object(mitigation_path) {
        log_error(
            logger,
            "encored",
            "device_mitigation.json is missing or not a JSON object",
        );
        eprintln!("ERROR: Failed to parse {}", mitigation_path.display());
        return None;
    }
    let config_store = match ConfigStore::load(config_path, default_gov_path) {
        Ok(store) => store,
        Err(error) => {
            log_error(
                logger,
                "encored",
                &format!("failed to load config.json: {error}"),
            );
            eprintln!("ERROR: Failed to load {}", config_path.display());
            return None;
        }
    };
    Some((registry, config_store))
}

/// Handle one daemon event. Returns `true` when the daemon
/// should stop, which happens only on a module update creation.
fn handle_daemon_event(
    logger: Option<&Logger>,
    event: &WatchEvent,
    registry: &GameRegistry,
    config_store: &ConfigStore,
    gamelist_path: &Path,
    config_path: &Path,
    mitigation_path: &Path,
) -> bool {
    if event.kind == WatchKind::Directory
        && event.event_type == WatchEventType::Created
        && event.name.as_deref() == Some("update")
    {
        log_info(
            logger,
            "encored",
            "Module update file detected, signaling daemon to stop",
        );
        return true;
    }
    if event.kind != WatchKind::File || event.event_type != WatchEventType::ClosedWrite {
        return false;
    }
    if event.path == gamelist_path {
        reload_gamelist(logger, registry, gamelist_path);
    } else if event.path == config_path {
        reload_config(logger, config_store);
    } else if event.path == mitigation_path {
        reload_mitigation(logger, mitigation_path);
    }
    false
}

/// Reload the gamelist, keeping the old registry on failure.
fn reload_gamelist(logger: Option<&Logger>, registry: &GameRegistry, gamelist_path: &Path) {
    match fs::read_to_string(gamelist_path) {
        Ok(text) => match registry.load_from_text(&text) {
            Ok(()) => log_info(
                logger,
                "encored",
                &format!("gamelist reloaded: {} games", registry.len()),
            ),
            Err(error) => log_warn(
                logger,
                "encored",
                &format!("failed to reload gamelist: {error}, keeping old registry"),
            ),
        },
        Err(error) => log_warn(
            logger,
            "encored",
            &format!("failed to read gamelist for reload: {error}"),
        ),
    }
}

/// Reload the config and apply its log level.
fn reload_config(logger: Option<&Logger>, config_store: &ConfigStore) {
    match config_store.reload() {
        Ok(()) => {
            let prefs = config_store.get().preferences;
            if let Some(logger) = logger {
                let _ = logger.set_level_from_config(prefs.log_level);
            }
            log_info(logger, "encored", "config reloaded");
        }
        Err(error) => log_warn(
            logger,
            "encored",
            &format!("failed to reload config: {error}"),
        ),
    }
}

/// Re-validate the device-mitigation file on reload.
fn reload_mitigation(logger: Option<&Logger>, mitigation_path: &Path) {
    if is_json_object(mitigation_path) {
        log_info(
            logger,
            "encored",
            "device mitigation reloaded (rules not yet applied)",
        );
    } else {
        log_warn(
            logger,
            "encored",
            "device mitigation reload failed: not a JSON object",
        );
    }
}

/// Device information for mitigation rules and `SoC`
/// detection, from files only: the device-tree model,
/// the kernel release, and `/proc/cpuinfo`. The model
/// property (`ro.product.model`) and the `SoC` properties
/// from `getprop` are not yet read by Rust code, so
/// mitigation rules keyed on `model` and `SoC` detection
/// that depends only on properties will not match;
/// sysfs-based detection (KGSL, GED, Tegra) covers the
/// main cases, and anything else falls back to
/// `Soc::Unknown`, which applies only the common plan,
/// a safe fallback rather than a wrong `SoC` plan.
fn device_info() -> DeviceInfo {
    let soc = fs::read_to_string("/proc/device-tree/model")
        .unwrap_or_default()
        .trim_matches(|character: char| character == '\0' || character.is_whitespace())
        .to_owned();
    let uname = fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_default()
        .trim()
        .to_owned();
    DeviceInfo {
        soc,
        model: String::new(),
        uname,
    }
}

/// Detect the `SoC` family from sysfs, device-tree, and
/// `/proc/cpuinfo`, in the same order as `customize.sh`
/// except for the `getprop` properties (see
/// [`device_info`]).
fn detect_device_soc() -> Soc {
    let root = SysfsRoot::device();
    let mut existing_paths = Vec::new();
    for path in [
        "/sys/class/kgsl/kgsl-3d0/devfreq",
        "/sys/devices/platform/kgsl-2d0.0/kgsl",
        "/sys/kernel/ged/hal",
        "/sys/kernel/tegra_gpu",
    ] {
        if root.exists(path) {
            existing_paths.push(path.to_owned());
        }
    }
    let mut cpuinfo_hardware = String::new();
    let mut cpuinfo_model = String::new();
    if let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo") {
        for line in cpuinfo.lines() {
            if let Some(value) = line.strip_prefix("Hardware") {
                if cpuinfo_hardware.is_empty() {
                    if let Some(value) = value.split(':').nth(1) {
                        value.trim().clone_into(&mut cpuinfo_hardware);
                    }
                }
            }
            if let Some(value) = line.strip_prefix("model name") {
                if cpuinfo_model.is_empty() {
                    if let Some(value) = value.split(':').nth(1) {
                        value.trim().clone_into(&mut cpuinfo_model);
                    }
                }
            }
        }
    }
    detect_soc(&SocDetection {
        existing_paths,
        model: device_info().soc,
        soc_properties: Vec::new(),
        cpuinfo_hardware,
        cpuinfo_model,
    })
}

/// Profiler options from the mitigation file and the
/// current preferences, or defaults when the file
/// cannot be parsed (the daemon already refused to
/// start on a non-object mitigation file, so a parse
/// failure here only means the flags are empty).
fn profiler_options(mitigation_path: &Path, use_device_mitigation: bool) -> ProfilerOptions {
    let Ok(text) = fs::read_to_string(mitigation_path) else {
        return ProfilerOptions::default();
    };
    let Ok(data) = parse_mitigation(&text, &device_info()) else {
        return ProfilerOptions::default();
    };
    data.cached_mitigation_items(use_device_mitigation)
        .apply_to_options(&ProfilerOptions::default())
}

/// Perform one profile application, in C++ order: write
/// `gameinfo` and `current_profile` (with a trailing
/// newline, as in `Profiler.cpp`), apply the profiler
/// plan (continuing after every failure, as in
/// `apply_plan`), then set DND. A failure of any step
/// is logged but does not stop the daemon, as required
/// by the task guide: one tweak's failure must not kill
/// the daemon.
fn perform_application(
    logger: Option<&Logger>,
    config_dir: &Path,
    root: &SysfsRoot,
    application: &ProfileApplication,
) {
    let gameinfo_path = config_dir.join("gameinfo");
    if let Err(error) = write_atomic(
        &gameinfo_path,
        format!("{}\n", application.game_info).as_bytes(),
        0o644,
    ) {
        log_warn(
            logger,
            "encored",
            &format!("failed to write gameinfo: {error}"),
        );
    }
    let profile_path = config_dir.join("current_profile");
    if let Err(error) = write_atomic(
        &profile_path,
        format!("{}\n", application.profile_code).as_bytes(),
        0o644,
    ) {
        log_warn(
            logger,
            "encored",
            &format!("failed to write current_profile: {error}"),
        );
    }
    let summary = apply_plan(root, &application.plan);
    log_info(
        logger,
        "encored",
        &format!(
            "applied profile {}: {} sysfs writes applied, {} missing, {} rejected, {} failed",
            application.profile_code,
            summary.applied,
            summary.missing,
            summary.rejected,
            summary.failed
        ),
    );
    if let DndAction::Set(enabled) = application.dnd {
        if let Err(error) = set_dnd_default(enabled) {
            log_warn(
                logger,
                "encored",
                &format!("failed to set DND to {enabled}: {error}"),
            );
        }
    }
}

/// Restore DND to the user's previous state on daemon
/// exit, when a game had requested it. This differs
/// from C++, whose signal handler calls `_exit(0)` and
/// therefore leaves DND as the game set it; a graceful
/// Rust exit should not leave the user stuck in DND.
fn restore_dnd_on_exit(logger: Option<&Logger>, state: &DaemonState) {
    if state.dnd.requested {
        if let Err(error) = set_dnd_default(state.dnd.previous) {
            log_warn(
                logger,
                "encored",
                &format!("failed to restore DND on exit: {error}"),
            );
        }
    }
}

/// The Binder context for the daemon loop, or `None`
/// on a host without Binder.
struct BinderContext {
    /// The transport, fed by `encore_sys` callbacks.
    transport: BinderTransport<SysBinderClient>,
    /// The Binder-driven state machine.
    state: DaemonState,
}

/// Set up Binder for the daemon loop, after daemonising
/// (so the parent does not touch Binder), mirroring
/// C++ `encore_main_daemon`'s `BinderMonitor::initialize`
/// failure path: on any failure, log and return `None`
/// with a flag that the caller must fail startup, not
/// continue without Binder on a device.
///
/// Returns `Ok(None)` only when `/dev/binder` does not
/// exist, which is the host case: the daemon continues
/// with file watching only, as before, and logs that
/// Binder is unavailable. On a device `/dev/binder`
/// always exists, so a failure there is a startup
/// failure, not a silent fallback.
fn setup_binder(logger: Option<&Logger>, module_dir: &Path) -> Result<Option<BinderContext>, ()> {
    if !Path::new("/dev/binder").exists() {
        log_warn(
            logger,
            "encored",
            "Binder not available (/dev/binder missing), continuing without Binder events",
        );
        return Ok(None);
    }
    let apk_path = module_dir.join("binder_resolver.apk");
    let apk_path = apk_path.to_string_lossy().into_owned();
    let codes = match run_resolver(&default_runner(), &apk_path) {
        Ok(codes) => codes,
        Err(error) => {
            log_error(
                logger,
                "encored",
                &format!("failed to resolve Binder transaction codes: {error}"),
            );
            return Err(());
        }
    };
    let client = match SysBinderClient::acquire_detailed(codes) {
        Ok(client) => client,
        Err(reason) => {
            log_error(
                logger,
                "encored",
                &format!("failed to acquire Binder services: {reason}"),
            );
            return Err(());
        }
    };
    let transport = BinderTransport::new(client);
    if !register_sys_callbacks(&transport) {
        log_error(logger, "encored", "failed to register Binder callbacks");
        return Err(());
    }
    SysBinderClient::start_thread_pool();
    let (battery_saver, zen_mode) = transport.initial_state();
    let state = DaemonState::new(battery_saver, zen_mode != 0);
    log_info(
        logger,
        "encored",
        &format!("Binder initialised: battery saver {battery_saver}, zen mode {zen_mode}"),
    );
    Ok(Some(BinderContext { transport, state }))
}

/// Handle Binder events for one loop iteration, building
/// fresh evaluation inputs from the current config,
/// registry, `SoC`, and mitigation options, and performing
/// every resulting application.
/// References needed to build evaluation inputs for one
/// Binder iteration, grouped to keep function argument
/// counts under clippy's limit.
struct EvalContext<'a> {
    logger: Option<&'a Logger>,
    registry: &'a GameRegistry,
    config_store: &'a ConfigStore,
    mitigation_path: &'a Path,
    config_dir: &'a Path,
    soc: Soc,
    root: &'a SysfsRoot,
}

fn handle_binder_events(binder: &mut BinderContext, ctx: &EvalContext<'_>) {
    let config = ctx.config_store.get();
    let options = profiler_options(
        ctx.mitigation_path,
        config.preferences.use_device_mitigation,
    );
    let inputs = EvaluateInputs {
        registry: ctx.registry,
        preferences: &config.preferences,
        governors: &config.cpu_governor,
        soc: ctx.soc,
        options: &options,
        root: ctx.root,
        is_alive: &is_alive,
    };
    let zen_mode = binder.transport.zen_mode();
    let applications = apply_events(&mut binder.state, &mut binder.transport, zen_mode, &inputs);
    for application in &applications {
        perform_application(ctx.logger, ctx.config_dir, ctx.root, application);
    }
}

/// Run the daemon event loop until a module update stops it,
/// or the watcher fails.
/// Startup, as in C++ `encore_main_daemon`: apply
/// perfcommon first, then the initial evaluation
/// (balance or powersave, with no game active).
fn startup_application(binder: &mut BinderContext, ctx: &EvalContext<'_>) {
    let config = ctx.config_store.get();
    let perfcommon =
        DaemonState::perfcommon_application(ctx.root, config.preferences.disable_tweaks);
    perform_application(ctx.logger, ctx.config_dir, ctx.root, &perfcommon);
    let options = profiler_options(
        ctx.mitigation_path,
        config.preferences.use_device_mitigation,
    );
    let inputs = EvaluateInputs {
        registry: ctx.registry,
        preferences: &config.preferences,
        governors: &config.cpu_governor,
        soc: ctx.soc,
        options: &options,
        root: ctx.root,
        is_alive: &is_alive,
    };
    let zen_mode = binder.transport.zen_mode();
    if let Some(application) = binder.state.evaluate(zen_mode, &inputs) {
        perform_application(ctx.logger, ctx.config_dir, ctx.root, &application);
    }
}

/// References needed to handle one watcher poll, grouped
/// to keep function argument counts under clippy's limit.
struct WatchContext<'a> {
    logger: Option<&'a Logger>,
    watcher: &'a mut Watcher,
    registry: &'a GameRegistry,
    config_store: &'a ConfigStore,
    gamelist_path: &'a Path,
    config_path: &'a Path,
    mitigation_path: &'a Path,
    binder_state: Option<&'a DaemonState>,
}

/// Handle one watcher poll. Returns `Ok(true)` when the
/// daemon should stop (module update), `Ok(false)` to
/// continue, or `Err` when the watcher itself failed.
fn handle_watcher_events(ctx: &mut WatchContext<'_>) -> Result<bool, String> {
    let events = ctx.watcher.next_events().map_err(|error| {
        log_error(
            ctx.logger,
            "encored",
            &format!("watcher read failed: {error}"),
        );
        format!("{error}")
    })?;
    for event in events {
        if handle_daemon_event(
            ctx.logger,
            &event,
            ctx.registry,
            ctx.config_store,
            ctx.gamelist_path,
            ctx.config_path,
            ctx.mitigation_path,
        ) {
            if let Some(state) = ctx.binder_state {
                restore_dnd_on_exit(ctx.logger, state);
            }
            log_info(ctx.logger, "encored", "Encore Tweaks daemon exited");
            return Ok(true);
        }
    }
    Ok(false)
}

/// All inputs to the daemon loop, grouped to keep the
/// function argument count under clippy's limit.
struct DaemonLoop<'a> {
    logger: Option<&'a Logger>,
    watcher: &'a mut Watcher,
    registry: &'a GameRegistry,
    config_store: &'a ConfigStore,
    gamelist_path: &'a Path,
    config_path: &'a Path,
    mitigation_path: &'a Path,
    config_dir: &'a Path,
    soc: Soc,
    binder: &'a mut Option<BinderContext>,
}

fn daemon_loop(args: DaemonLoop<'_>) -> ExitCode {
    let DaemonLoop {
        logger,
        watcher,
        registry,
        config_store,
        gamelist_path,
        config_path,
        mitigation_path,
        config_dir,
        soc,
        binder,
    } = args;
    let signal_pipe = match install_stop_signals() {
        Ok(pipe) => Some(pipe),
        Err(error) => {
            log_warn(
                logger,
                "encored",
                &format!("signal handling unavailable: {error}"),
            );
            None
        }
    };
    let mut tracked_pid: Option<i32> = match env::var("ENCORE_TRACK_PID") {
        Ok(value) => value.parse::<i32>().ok().filter(|pid| *pid > 0),
        Err(_) => None,
    };
    if let Some(pid) = tracked_pid {
        log_info(logger, "encored", &format!("tracking PID {pid}"));
    }
    let root = SysfsRoot::device();
    if let Some(binder) = binder.as_mut() {
        let ctx = EvalContext {
            logger,
            registry,
            config_store,
            mitigation_path,
            config_dir,
            soc,
            root: &root,
        };
        startup_application(binder, &ctx);
    }
    loop {
        if let Some(pipe) = &signal_pipe {
            if pipe.stop_requested() {
                if let Some(binder) = binder.as_ref() {
                    restore_dnd_on_exit(logger, &binder.state);
                }
                log_info(logger, "encored", "Encore Tweaks daemon exited");
                return ExitCode::SUCCESS;
            }
        }
        if let Some(binder) = binder.as_mut() {
            let ctx = EvalContext {
                logger,
                registry,
                config_store,
                mitigation_path,
                config_dir,
                soc,
                root: &root,
            };
            handle_binder_events(binder, &ctx);
        }
        if let Some(pid) = tracked_pid {
            if !is_alive(pid) {
                log_info(
                    logger,
                    "encored",
                    &format!("tracked PID {pid} exited, clearing tracked PID"),
                );
                tracked_pid = None;
            }
        }
        let binder_state = binder.as_ref().map(|binder| &binder.state);
        let mut watch_ctx = WatchContext {
            logger,
            watcher,
            registry,
            config_store,
            gamelist_path,
            config_path,
            mitigation_path,
            binder_state,
        };
        match handle_watcher_events(&mut watch_ctx) {
            Ok(true) => return ExitCode::SUCCESS,
            Ok(false) => {}
            Err(error) => {
                eprintln!("ERROR: File watcher failed: {error}");
                return ExitCode::FAILURE;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Create the daemon watcher and watch the three JSON files
/// and the module directory, logging and failing on error.
fn setup_daemon_watcher(
    logger: Option<&Logger>,
    gamelist_path: &Path,
    config_path: &Path,
    mitigation_path: &Path,
    module: &Path,
) -> Option<Watcher> {
    let mut watcher = match Watcher::new() {
        Ok(watcher) => watcher,
        Err(error) => {
            log_error(
                logger,
                "encored",
                &format!("failed to initialise watcher: {error}"),
            );
            eprintln!("ERROR: Failed to initialize file watcher");
            return None;
        }
    };
    for path in [gamelist_path, config_path, mitigation_path] {
        if let Err(error) = watcher.add_file(path) {
            log_error(
                logger,
                "encored",
                &format!("failed to watch {}: {error}", path.display()),
            );
            eprintln!("ERROR: Failed to add watch for {}", path.display());
            return None;
        }
    }
    if let Err(error) = watcher.add_directory(module) {
        log_error(
            logger,
            "encored",
            &format!("failed to watch module directory: {error}"),
        );
        eprintln!("ERROR: Failed to add watch for {}", module.display());
        return None;
    }
    Some(watcher)
}

/// Run the `daemon` command, with Binder wired in.
///
/// Mirrors the preconditions in C++ `cmd_run_daemon` and
/// `init_file_watcher`, plus `encore_main_daemon`'s
/// Binder initialisation and profile evaluation, with
/// one host exception: when `/dev/binder` does not
/// exist, Binder is skipped and the daemon continues
/// with file watching only, as before. Android
/// notifications (`notify`, which drops to uid/gid
/// 2000) are still not implemented. When `detach` is
/// true, the daemon detaches after the preconditions
/// and the watcher are set up, as C++ `daemon(0, 0)`
/// does after its preconditions, and Binder is set up
/// after detaching, so the parent does not touch it.
fn run_daemon(logger: Option<&Logger>, detach: bool) -> ExitCode {
    if !is_root() {
        eprintln!("ERROR: Please run this program as root");
        return ExitCode::FAILURE;
    }
    let config = config_dir();
    let module = modpath();
    let gamelist_path = config.join("gamelist.json");
    let config_path = config.join("config.json");
    let mitigation_path = config.join("device_mitigation.json");
    let default_gov_path = config.join("default_cpu_gov");
    if module.join("update").exists() {
        log_error(
            logger,
            "encored",
            "module update detected, daemon will not start",
        );
        eprintln!("ERROR: Module update detected, please reboot to complete module update.");
        return ExitCode::FAILURE;
    }
    let Some((registry, config_store)) = load_daemon_stores(
        logger,
        &gamelist_path,
        &config_path,
        &mitigation_path,
        &default_gov_path,
    ) else {
        return ExitCode::FAILURE;
    };
    let config_level = config_store.get().preferences.log_level;
    if config_level != env_log_level() {
        if let Some(logger) = logger {
            let _ = logger.set_level_from_config(config_level);
        }
    }
    let lock_path = config.join(".lock");
    let _lock = match DaemonLock::acquire(&lock_path) {
        Ok(lock) => lock,
        Err(error) => {
            log_error(
                logger,
                "encored",
                &format!("failed to acquire daemon lock: {error}"),
            );
            eprintln!(
                "ERROR: Another encored daemon is already running, or the lock failed: {error}"
            );
            return ExitCode::FAILURE;
        }
    };
    let Some(mut watcher) = setup_daemon_watcher(
        logger,
        &gamelist_path,
        &config_path,
        &mitigation_path,
        &module,
    ) else {
        return ExitCode::FAILURE;
    };
    if detach {
        match daemonize() {
            Ok(DaemonizeOutcome::Parent(_)) => return ExitCode::SUCCESS,
            Ok(DaemonizeOutcome::Child) => {}
            Err(error) => {
                log_error(logger, "encored", &format!("failed to daemonize: {error}"));
                eprintln!("ERROR: Failed to daemonize service");
                return ExitCode::FAILURE;
            }
        }
    }
    log_info(logger, "encored", "Encore Tweaks daemon started");
    let Ok(mut binder) = setup_binder(logger, &module) else {
        eprintln!("ERROR: Failed to initialize Binder monitor");
        return ExitCode::FAILURE;
    };
    let soc = detect_device_soc();
    log_info(logger, "encored", &format!("detected SoC family: {soc:?}"));
    daemon_loop(DaemonLoop {
        logger,
        watcher: &mut watcher,
        registry: &registry,
        config_store: &config_store,
        gamelist_path: &gamelist_path,
        config_path: &config_path,
        mitigation_path: &mitigation_path,
        config_dir: &config,
        soc,
        binder: &mut binder,
    })
}

/// Run the `setup_gamelist` command.
fn run_setup(logger: Option<&Logger>, base_path: &str) -> ExitCode {
    if !is_root() {
        eprintln!("ERROR: Please run this program as root");
        return ExitCode::FAILURE;
    }
    let gamelist_path = config_dir().join("gamelist.json");
    match run_setup_gamelist(&gamelist_path, Path::new(base_path), &data_dir()) {
        Ok(count) => {
            log_info(
                logger,
                "encored",
                &format!(
                    "setup_gamelist succeeded: {count} games in {}",
                    gamelist_path.display()
                ),
            );
            ExitCode::SUCCESS
        }
        Err(message) => {
            log_error(
                logger,
                "encored",
                &format!("setup_gamelist failed: {message}"),
            );
            eprintln!("ERROR: Failed to setup gamelist from {base_path}: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Run the `check_gamelist` command.
fn run_check(logger: Option<&Logger>) -> ExitCode {
    if !is_root() {
        eprintln!("ERROR: Please run this program as root");
        return ExitCode::FAILURE;
    }
    let gamelist_path = config_dir().join("gamelist.json");
    match run_check_gamelist(&gamelist_path) {
        Ok(count) => {
            log_info(
                logger,
                "encored",
                &format!(
                    "check_gamelist succeeded: {count} games in {}",
                    gamelist_path.display()
                ),
            );
            eprintln!("{} is valid", gamelist_path.display());
            eprintln!("Registered games: {count}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            log_error(
                logger,
                "encored",
                &format!("check_gamelist failed: {message}"),
            );
            eprintln!("ERROR: {message}");
            ExitCode::FAILURE
        }
    }
}

fn print_usage(program_name: &str) {
    println!("Encore Tweaks CLI (Rust Stage 4)");
    println!();
    println!("Usage: {program_name} <COMMAND> [OPTIONS]");
    println!();
    println!("Commands:");
    println!("  daemon               Start Encore Tweaks daemon");
    println!("  setup_gamelist       Setup initial gamelist from base file");
    println!("  check_gamelist       Validate gamelist file");
    println!("  version              Show version information");
    println!();
    println!("Global Options:");
    println!("  -h, --help           Show this help message");
    println!("  -V, --version        Show version information");
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let program_name = match args.first() {
        Some(name) => match std::path::Path::new(name).file_stem() {
            Some(stem) => stem.to_string_lossy().into_owned(),
            None => name.clone(),
        },
        None => "encored".to_owned(),
    };
    let logger = create_logger(&config_dir());

    match parse_args(&args) {
        Command::Help => {
            print_usage(&program_name);
            ExitCode::SUCCESS
        }
        Command::Version => run_version(logger.as_ref()),
        Command::Unknown(name) => {
            log_error(
                logger.as_ref(),
                "encored",
                &format!("unknown command: {name}"),
            );
            eprintln!("ERROR: Unknown command: {name}");
            eprintln!("See '{program_name} --help' for available commands.");
            ExitCode::FAILURE
        }
        Command::InvalidArguments => {
            print_usage(&program_name);
            ExitCode::FAILURE
        }
        Command::Daemon(detach) => run_daemon(logger.as_ref(), detach),
        Command::SetupGamelist(base_path) => run_setup(logger.as_ref(), &base_path),
        Command::CheckGamelist => run_check(logger.as_ref()),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_args, run_check_gamelist, run_setup_gamelist, Command};
    use std::fs;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    fn temp_dir_for(test_name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("encored-{test_name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(fs::create_dir_all(&dir).is_ok());
        dir
    }

    #[test]
    fn parses_help_version_and_commands() {
        assert_eq!(parse_args(&args(&["encored", "--help"])), Command::Help);
        assert_eq!(parse_args(&args(&["encored", "version"])), Command::Version);
        assert_eq!(
            parse_args(&args(&["encored", "daemon"])),
            Command::Daemon(false)
        );
        assert_eq!(
            parse_args(&args(&["encored", "daemon", "--daemonize"])),
            Command::Daemon(true)
        );
        assert_eq!(
            parse_args(&args(&["encored", "check_gamelist"])),
            Command::CheckGamelist
        );
        assert_eq!(
            parse_args(&args(&["encored", "setup_gamelist", "base.txt"])),
            Command::SetupGamelist("base.txt".to_owned())
        );
    }

    #[test]
    fn rejects_missing_and_unknown_commands() {
        assert_eq!(parse_args(&args(&["encored"])), Command::InvalidArguments);
        assert_eq!(
            parse_args(&args(&["encored", "setup_gamelist"])),
            Command::InvalidArguments
        );
        assert_eq!(
            parse_args(&args(&["encored", "bogus"])),
            Command::Unknown("bogus".to_owned())
        );
    }

    #[test]
    fn setup_and_check_round_trips_with_a_fake_data_dir() {
        let dir = temp_dir_for("setup-check");
        let data_dir = dir.join("data");
        let config_dir = dir.join("config");
        assert!(fs::create_dir_all(&data_dir).is_ok());
        assert!(fs::create_dir_all(&config_dir).is_ok());
        assert!(fs::create_dir_all(data_dir.join("com.a.game")).is_ok());
        let base_path = dir.join("gamelist.txt");
        assert!(fs::write(&base_path, "com.a.game\ncom.missing.game\n").is_ok());
        let gamelist_path = config_dir.join("gamelist.json");
        let count = run_setup_gamelist(&gamelist_path, &base_path, &data_dir).unwrap_or_default();
        assert_eq!(count, 1);
        assert_eq!(run_check_gamelist(&gamelist_path).unwrap_or_default(), 1);
        assert!(run_check_gamelist(&config_dir.join("missing.json")).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn setup_fails_on_a_missing_base_file() {
        let dir = temp_dir_for("setup-missing");
        let result = run_setup_gamelist(
            &dir.join("gamelist.json"),
            &dir.join("missing.txt"),
            &dir.join("data"),
        );
        assert!(result.is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
