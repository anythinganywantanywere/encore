# Verification inventory — Encore 5.2.2, before writing Rust (task guide §2)

Checked against the clone at `../` (version file: `5.2.2`,
`jni/Application.mk`: `APP_ABI := arm64-v8a armeabi-v7a`,
`APP_PLATFORM := android-29`). No C++, shell, WebUI, or module file was
modified for Stage 1. The Rust workspace in this directory is additive only.

## 1. Paths written by the C++ daemon itself

The daemon itself writes very little directly. Most sysfs/procfs writes
happen in shell, called from the daemon (see §3).

| Path (C++) | Written by | Value / note |
|---|---|---|
| `/data/adb/.config/encore/gameinfo` (`GAME_INFO`) | `Profiler.cpp`: `write2file` | `"NULL 0 0\n"` or `"<pkg> <pid> <uid>\n"` |
| `/data/adb/.config/encore/current_profile` (`PROFILE_MODE`) | `Profiler.cpp`: `write2file` | Profile code 0–3, see `encore_core::profile` |
| `/data/adb/modules/encore/module.prop` (`MODULE_PROP`) | `Main.cpp` via `ModuleProperty::Change` | Only the `description` key, with a status prefix |
| `/data/adb/.config/encore/config.json` (`CONFIG_FILE`) | `EncoreConfigStore::save_config` | Preferences + CPU governor, pretty JSON, indent 2 |
| `/data/adb/.config/encore/gamelist.json` (`ENCORE_GAMELIST`) | `GameRegistry::populate_from_base` | Pretty JSON, indent 2, from `gamelist.txt` base file |
| `/data/adb/.config/encore/encore.log` (`LOG_FILE`) | Logging (`EncoreLog.hpp`) | Log file; rotation/size limit is **not** verified here — see TODO below |
| `/data/data/<package_name>` | `GameRegistry::populate_from_base`, `ProcessUtility` | Only `access()`/`stat()`, not a write. Package name is concatenated directly — see finding 7 |

Other config paths read by the daemon: `config.json`,
`device_mitigation.json`, `default_cpu_gov` (fallback governor
`"schedutil"`), `system_status` (defined, not traced to a write in this
pass), `soc_recognition` (read by shell, not by C++).

## 2. Paths written by shell (the actual tweak surface)

`scripts/encore_profiler.sh` is 982 lines and is where the per-SoC
tweak logic lives. The daemon only chooses a profile and calls
`encore_profiler <profile>`. A full allowlist for a Rust port must be
generated from this script, not guessed. Families verified by reading
the script (not an exhaustive list — generating the exhaustive list
is Stage 5 work):

- CPU: `/sys/devices/system/cpu/cpu*/cpufreq/scaling_governor`,
  `/sys/devices/system/cpu/cpufreq/policy*/scaling_*_freq`,
  `/proc/ppm/policy/hard_userlimit_*_cpu_freq`
- GPU / SoC-specific: `/sys/class/kgsl/kgsl-3d0/*` (Qualcomm/Adreno),
  `/proc/gpufreq*`, `/proc/gpufreqv2/*`, `/sys/kernel/ged/*`,
  `/sys/kernel/helio-dvfsrc/*`, `/sys/class/devfreq/*` (MediaTek),
  `/sys/kernel/tegra_gpu`, `/sys/kernel/gpu`, Mali devfreq paths
- Scheduler / memory / I/O: `/proc/sys/kernel/sched_*`,
  `/proc/sys/vm/vfs_cache_pressure`, `/proc/sys/vm/drop_caches`,
  `/sys/block/*/queue/read_ahead_kb`, `/sys/block/*/queue/nr_requests`,
  `/proc/sys/net/ipv4/tcp_*`, `/proc/sys/kernel/panic*`,
  `/sys/kernel/debug/sched_features`, `/dev/stune/top-app/*`
- Vendor: `/proc/ppm/policy_status`, `/proc/cpufreq/*`,
  `/sys/devices/platform/boot_dramboost/*`, `/sys/devices/system/cpu/eas/*`,
  `/proc/touchpanel/*` (Oppo/Oplus/Realme), `/sys/module/battery_saver/*`

// TODO(verify): generate the exhaustive, per-profile sysfs/procfs
allowlist from `encore_profiler.sh` before Stage 5, and confirm which
paths are guarded by `[ -d ]` / `[ -f ]` checks in the script versus
written unconditionally.

## 3. External processes / shell

- `Profiler.cpp`: `system("encore_profiler perfcommon")`,
  `system("encore_profiler performance_lite")`,
  `system("encore_profiler performance")`,
  `system("encore_profiler balance")`,
  `system("encore_profiler powersave")`. All strings are constants;
  no user input is interpolated into them (verified — this matches the
  "already good" note in the analysis).
- `ShellUtility.hpp`: `popen_direct` (arg-vector, no shell) and
  `systemv` (printf-style `system()` with a 512-byte buffer) are
  present but have no callers in this pass — dead code, finding 10.
- `EncoreUtility`: `notify()` drops privileges to uid/gid 2000 before
  exec (verified in `MiscUtility.cpp`); `ProcessUtility` uses
  `kill(pid, 0)` for liveness, which is also the fallback the task
  guide requires for kernels without `pidfd_open`.
- `scripts/encore_utility.sh`: ends with `$@`, i.e. it executes
  whatever command is passed as arguments. Safe only while every
  caller is trusted and the process runs as root — finding 8.
- Java companion: the analysis mentions a `binder_resolver.apk` run
  via `app_process` as root, with no timeout and no return-value
  check on `write()`. That path was **not** found in the C++ files
  read for Stage 1; it is carried as TODO(verify) below, not as a
  verified fact from this clone.

// TODO(verify): locate the APK resolver call site in the Java
companion / installer, confirm its timeout, output-size limit, and
whether the APK signature is verified by the daemon.

## 4. User / WebUI-controlled input

Treat all of these as untrusted (task guide §3.4), even though today
only root / WebUI can write them:

- `gamelist.json`: JSON object, package name → `{lite_mode,
  enable_dnd}`. C++ validation is only "not empty" (finding 7).
- `config.json`: preferences (`enforce_lite_mode`,
  `use_device_mitigation`, `disable_tweaks`, `log_level`) and
  `cpu_governor` (`balance`, `powersave`). Wrong types are ignored;
  missing fields keep defaults, except an absent governor, which
  becomes empty (see `encore_core::config`).
- `device_mitigation.json`: loaded by `DeviceMitigationStore`; its
  exact schema was not fully traced in Stage 1 — see TODO below.
- CLI arguments: `daemon`, `setup_gamelist <base_file_path>`,
  `check_gamelist`, `version`, `-h/--help`, `-V/--version`.
  `setup_gamelist` takes an arbitrary base-file path from the caller.
- Environment: `ENCORE_*` variables are cleared and re-set in
  `Profiler::set_profiler_env_vars` by iterating `environ` while
  calling `unsetenv` — finding 2 (array shifts, entries can be
  skipped; `setenv`/`unsetenv` are not thread-safe).

## 5. IPC with Java companion and Binder monitor

Verified from `jni/base/BinderMonitor/BinderMonitor.cpp`:

- Services obtained via `AServiceManager_getService`, with a 10-second
  wait helper (`waitForServiceCompat`, timeout 10000 ms):
  `power`, `notification`, `activity`, `display`, `package`.
- Transactions used: power-manager interactive / power-save mode,
  notification-manager Zen mode (DND), display-manager callback,
  activity-manager process-observer registration, package-manager
  package-name-for-uid lookup.
- Callbacks delivered to the daemon: foreground-activity changes
  (pid, uid), process death, display interactive state, power-save
  state (polled in a separate thread, `pollPowerSave`).
- Binder transactions are resolved at runtime from transaction-code
  tables in `BinderNDK` — this is, per the analysis, the hardest
  part of the migration, along with the 982-line shell profiler.
  It is **not** ported in Stage 1, and no FFI binding is claimed here.

// TODO(verify): the Java companion daemon's exact protocol, its
lock files, and whether it, rather than the C++ daemon, owns the
APK resolver. The task guide says the Java daemon stays Java/dex;
Rust only communicates with it, so this protocol must be documented
before Stage 6.

## 6. Old kernels and non-Qualcomm SoCs

- Build platform is `android-29`; no kernel-version gate was found in
  the C++ daemon itself. `DeviceInfo` reads the kernel release via
  `uname`, the SoC model from `/proc/device-tree/model`, and the
  device model from `ro.product.model`.
- Shell SoC dispatch uses a numeric `soc_recognition` code:
  1 MediaTek, 2 Snapdragon, 3 Exynos, 4 Unisoc, 5 Tensor, 6 Intel,
  7 Tegra, 8 Kirin (from `encore_utility.sh`). Profiler dispatch
  covers 1–6 in most profiles; Kirin/Intel paths are thinner in the
  script read for Stage 1.
- Non-Qualcomm paths are real, not stubs: MediaTek (`/proc/ppm`,
  `/proc/gpufreq*`, helio-dvfsrc), Exynos, Unisoc, Tensor, and Mali
  devfreq paths all appear in `encore_profiler.sh`.

// TODO(verify): behaviour on kernel 4.x specifically — `pidfd_open`
is not used by the C++ daemon (it uses `kill(pid, 0)`), so the
guide's `pidfd_open` fallback note is a Rust-design note, not a
C++ behaviour to preserve. Confirm the same for inotify: the C++
watcher uses `inotify_init1(O_NONBLOCK | O_CLOEXEC)` and
`IN_MODIFY | IN_CLOSE_WRITE | IN_DELETE | IN_MOVE | IN_CREATE`,
which are old-kernel safe, but the exact event set per watched file
must be re-checked before Stage 4.

## 7. Reconciliation: task guide vs analysis PDF

| Topic | Task guide (uploaded README) | Verified / PDF correction | Stage 1 decision |
|---|---|---|---|
| Android targets | `aarch64`, `armv7`, `x86_64`, `i686`; `riscv64` Tier 3 | `Application.mk` builds only `arm64-v8a` + `armeabi-v7a` | **Corrected**: Rust targets in scope are `aarch64-linux-android` and `armv7-linux-androideabi` only. x86_64/i686/riscv64 are out of scope unless upstream changes `Application.mk`. Do not make them a blocker. |
| Single-instance lock | `flock(LOCK_EX \| LOCK_NB)` required | `LOCK_FILE` / `JAVA_LOCK_FILE` defined, no use found | Not implemented in Stage 1; required in Stage 3/4, before the Rust daemon is shipped. |
| Atomic writes | Required for `module.prop` and config | C++ truncates in place; `service.sh` restores `module.prop` from `.orig` | Pure pattern implemented in `encore_core::atomic_write`; not yet wired to the daemon. |
| Registry data race | Ownership / snapshot pattern required | `find_game_ptr` returns a pointer into a vector after the mutex is released; `apply_game_profile` reads `enable_dnd` after a long `system()` call, while inotify can reload the list | Not fixed in Stage 1 (C++ untouched). Acceptance test for Stage 2/4: reload gamelist while a profile is being applied. |
| Dead code | No dead code | `popen_direct`, `systemv` have no callers | Not removed in Stage 1 (C++ untouched); removal is Stage 7 work, after parity is proven. |
| CI pinning | `cargo deny` / `cargo audit` in CI | Existing CI pins actions to major tags (`@v7`, `@v1`, `@v2`), not commit SHAs | New Rust CI uses the same major-tag convention as the existing workflows. SHA-pinning remains an open item (finding 11), not claimed as fixed. |
| Log rotation | Log file must have size limit / rotation | Not verified in this pass | TODO(verify) before Stage 3. |

## 8. Findings checklist (from the analysis PDF, status at Stage 1)

1. Dangling pointer / data race in `GameRegistry` — documented, not fixed (C++ untouched).
2. Iterating `environ` while `unsetenv` — documented, not fixed.
3. No single-instance lock in use — documented, not fixed.
4. Non-atomic file writes — Rust pattern implemented, not wired.
5. Blocking `system()` / `fork` + `waitpid` under `g_state_mtx` in Binder thread — documented, not fixed.
6. APK resolver without timeout — **not located in C++ in Stage 1**, see TODO in §3.
7. Weak input validation — strict validator implemented as a pure module, not wired.
8. `encore_utility.sh` ends with `$@` — documented, shell untouched.
9. Signal handler not async-signal-safe; `_exit(0)` skips `atexit` log flush — documented, not fixed.
10. Dead code `popen_direct` / `systemv` — documented, not removed.
11. No tests / sanitizers / clang-tidy; CI pinned to major tags — Rust Stage 1 adds unit tests for the new pure modules; C++ CI unchanged.

## 9. What Stage 1 deliberately does not do

- It does not change C++, shell, WebUI, website, translations,
  `gamelist.txt`, or the config schema.
- It does not replace `encored`, `service.sh`, or the installer.
- It does not claim Android cross-compilation, device testing,
  Binder parity, or sysfs parity. Those are later stages, on a real
  device, with at least one Qualcomm and one non-Qualcomm SoC.

## 10. Stage 2 changes (leaf modules, wired)

Stage 2 adds, in `encore_core`, without changing C++ or shell:

- `registry`: a snapshot registry. The current list is one
  `Arc<Vec<GameEntry>>` behind a read-write lock. `snapshot` clones
  the `Arc`, so a reader keeps its list alive after a replace;
  `replace` swaps the whole `Arc`, never mutating a list a snapshot
  may hold; `find` returns an owned copy. This is the Rust fix for
  finding 1, at the registry layer. The daemon-side race (holding a
  game entry across a long profile apply) is still Stage 4/5 work,
  because the daemon loop itself is not ported yet.
- `config_store`: file-backed `ConfigStore`, mirroring
  `EncoreConfigStore::load_config` / `save_config` / `reload`,
  including missing-file and unparsable-file default creation, the
  non-object-root error that keeps the old config on reload, and
  the partial-config empty-governor quirk. Saving is atomic with
  explicit `0644` permissions, instead of truncating `fopen`.
- `module_prop_store`: file-backed `Change` (missing file treated
  as empty, merged in key order) written atomically with `0644`,
  plus `get_module_version` mirroring `Main.cpp` (first
  `version=` line, quotes stripped only when both are present).
- `registry::populate_from_base` and `render_gamelist`: the pure
  parts of `GameRegistry::populate_from_base`. Trailing
  whitespace only is trimmed, leading whitespace is kept, and the
  rendered JSON preserves base-file order by rendering by hand,
  because the default `serde_json` map sorts keys.

In `encored`, `setup_gamelist` and `check_gamelist` are now real:

- `setup_gamelist <base_file>` reads the base file, keeps a package
  only when a directory of that name exists under the data
  directory, and writes `gamelist.json` atomically. On success it
  is silent, as in C++; on failure it prints an error and exits
  with failure.
- `check_gamelist` loads `gamelist.json` into a snapshot registry
  and prints, to stderr as in C++, `<path> is valid` and
  `Registered games: <count>`. A missing file or a parse error
  exits with failure.
- `version` now reads the version from `module.prop`, as in C++
  (`unknown` when the file is missing, with an error to stderr).
  It does not print a C++ `__DATE__`/`__TIME__` build stamp.
- `daemon` still exits with code 2 and a clear not-implemented
  message. No Binder, inotify, sysfs, profiler, or logging work is
  claimed in Stage 2.

Host testing hook, not device behaviour: when the environment
variables `ENCORE_CONFIG_DIR`, `ENCORE_MODPATH`, and
`ENCORE_DATA_DIR` are set, `encored` uses them instead of
`/data/adb/.config/encore`, `/data/adb/modules/encore`, and
`/data/data`. When they are absent, the device paths are used
unchanged. This is how `setup_gamelist` / `check_gamelist` were
verified on a host in Stage 2, with a fake data directory.

Stage 2 status of findings: finding 1 is fixed at the registry
layer; finding 4 is fixed for `module.prop`, `config.json`, and
`gamelist.json` writes made through the new Rust stores; finding 7
remains partially open — the strict package-name validator from
Stage 1 is still not wired into `setup_gamelist` /
`check_gamelist`, because the C++ commands accept any non-empty
name and changing that in Stage 2 would silently change behaviour.
Wiring strict validation is a documented decision for a later
stage, not a silent Stage 2 change.

## 11. Stage 3 changes (logging and file utilities)

Verified from `jni/include/EncoreLog.hpp` (the spdlog submodule
itself is not present in this clone, so sink behaviour beyond the
wrapper could not be checked from source):

- Log file `encore.log` under the config directory, line pattern
  `YYYY-MM-DD HH:MM:SS.mmm L message`, initial level trace,
  `flush_on` info.
- Config log level 0-5 maps to critical, error, warn, info,
  debug, trace; other values map to info. Default config value
  is 4 (debug).
- `set_log_level` is a no-op when unchanged, otherwise logs an
  info message about the change. Config reload applies the log
  level (see `InotifyHandler.cpp`).
- No size limit or rotation is set in `EncoreLog.hpp` itself.
  Log rotation was therefore a TODO(verify) item, not a verified
  C++ behaviour.

Stage 3 adds, in `encore_core`, without changing C++ or shell:

- `log`: a file logger with the C++ pattern and level mapping,
  a runtime-changeable level (`set_level_from_config`, with the
  C++ no-op-on-unchanged behaviour), UTC timestamps computed
  without a new dependency, single-line sanitisation against log
  injection, and a size limit with rotation: 1 MiB per file by
  default, 3 rotated files kept. Rotation is a deliberate
  improvement over C++, not a parity claim.
- `file_util`: `is_installed` (data-directory existence),
  `ensure_parent_dir`, `read_text`, `read_first_line`,
  `write_text_atomic`, and `file_size`. The C++ `write2file`
  truncating pattern is deliberately not reproduced; atomic
  writing is the replacement. PID liveness via `kill(pid, 0)`
  is not faked here: it needs unsafe code or a new dependency
  under `forbid(unsafe_code)`, so it stays Stage 4 work.

In `encored`, every command now creates a logger writing to
`encore.log` under the config directory. The level comes from
`ENCORE_LOG_LEVEL` (0-5, default 4), because `setup_gamelist`
and `check_gamelist` do not load `config.json`; applying the
config log level on reload is Stage 4 work. If the log file
cannot be opened, the command prints a warning to stderr and
continues without logging — a log failure must not kill a
command. Logged values in Stage 3 are only command outcomes,
counts, and config paths; no secrets, tokens, or credentials
are logged.

Stage 3 status of findings: log rotation/size limit, which was
an open TODO, is now implemented for the Rust logger. Finding 9
(signal handler flushing spdlog) is not addressed in Stage 3,
because the Rust daemon loop and its signal handling are not
ported yet.

## 12. Stage 4 changes (inotify watcher and PID tracking)

Verified from `jni/base/InotifyWatcher/InotifyWatcher.cpp`,
`jni/InotifyHandler.cpp`, and `jni/Main.cpp`:

- `addFile` watches the parent directory with
  `IN_MODIFY | IN_CLOSE_WRITE | IN_DELETE | IN_MOVE |
  IN_CREATE`, matching events by file name, and reuses an
  existing directory watch for a second file in the same
  directory. `addDirectory` watches the directory with
  `IN_CREATE | IN_DELETE | IN_MODIFY | IN_MOVED_FROM |
  IN_MOVED_TO | IN_CLOSE_WRITE`.
- The handler reacts only to `IN_CLOSE_WRITE` for
  `gamelist.json`, `config.json`, and
  `device_mitigation.json`, and only to `IN_CREATE` with the
  name `update` for the module directory, where it stops the
  daemon.
- `cmd_run_daemon` refuses to start when `modpath/update`
  exists, requires `gamelist.json` to exist and parse, and
  requires the device-mitigation file to load; it then
  daemonises, initialises the watcher (which loads config
  first and applies its log level), and blocks on Binder.
- PID liveness in C++ is `kill(pid, 0)`, and `pidof` scans
  `/proc`, reading only the first null-terminated `cmdline`
  string, with strict exact matching or loose substring
  matching.

Stage 4 adds, without changing C++ or shell:

- `watcher`: an inotify watcher through `nix` (see below),
  with the C++ masks, directory-watch reuse, file-name
  matching, and a translated event type. A bug found by the
  manual daemon test is fixed here: an early version added a
  new inotify watch per file, so three files in the same
  config directory overwrote each other and only the last
  file's reloads fired. The fix reuses one descriptor per
  directory, as C++ does, and is covered by a test with two
  files in the same directory.
- `process`: `is_alive` via `kill(pid, 0)` (EPERM counts as
  alive, only ESRCH counts as dead), `pidof`/`pidof_in`
  mirroring the C++ `/proc` scan, and `DaemonLock`, an
  exclusive non-blocking flock on `config_dir/.lock` (the
  C++ `LOCK_FILE`, which the analysis found was defined but
  never used). Finding 3 is therefore fixed for the Rust
  daemon.
- `file_util::is_json_object`: the first checks of C++
  device-mitigation loading (exists, parses, object root).
  The mitigation rules themselves are not parsed in Stage 4;
  they are profiler input in Stage 5.
- In `encored`, `daemon` is now a real foreground event
  loop: it checks the update file, loads gamelist/config/
  mitigation, applies the config log level, acquires the
  single-instance lock, watches the three JSON files and the
  module directory, logs "daemon started", and then reloads
  on close-write, applies a new config log level on config
  reload, and exits successfully on a module `update`
  creation. An optional `ENCORE_TRACK_PID` (host testing
  only) demonstrates PID tracking: when that PID exits, the
  daemon logs it and clears the tracked PID.

Deliberate Stage 4 limits, not parity claims:

- No Binder, no profiler, no Android notifications, and no
  daemonising (`daemon(0, 0)`): the daemon runs in the
  foreground and only reacts to file changes. Active-game
  PID tracking from Binder foreground callbacks is Stage 6
  work.
- No graceful signal handling yet: stopping the daemon
  happens via the module `update` file, or by killing the
  process. Finding 9 (signal handler) remains open.
- The new dependency `nix` 0.29 (features `inotify`,
  `signal`, `process`, `fs`; pulling in `libc`, `bitflags`,
  `cfg-if`, `cfg_aliases`) is justified in writing here:
  inotify and `kill`/`flock` need operating-system calls,
  and `nix` provides maintained safe wrappers for them, so
  `encore_core` keeps `#![forbid(unsafe_code)]` with no
  `unsafe` block of its own. No other new dependency was
  added.

## §13 Stage 5: profiler / per-SoC tweaks

Verified from `scripts/encore_profiler.sh` (982 lines)
and `jni/Profiler.cpp` before implementing:

- C++ dispatch: `run_perfcommon` writes `gameinfo`
  `"NULL 0 0"` and profile code 0, then runs
  `encore_profiler perfcommon` unless tweaks are disabled;
  `apply_performance_profile` writes `gameinfo` and code 1
  and runs `performance_lite` when lite mode is on, else
  `performance`; balance writes code 2 and runs `balance`;
  powersave writes code 3 and runs `powersave`.
- Script dispatch: `perfcommon`, `performance`,
  `performance_lite` (`performance_profile lite`), `balance`,
  `powersave`. `powersave_profile` calls `balance_profile`
  first, then sets the powersave governor, stune preference,
  battery saver, and SoC powersave tweaks.
- Script SoC dispatch: 1 MediaTek, 2 Snapdragon, 3 Exynos,
  4 Unisoc, 5 Tensor, 6 Tegra. No other code gets SoC
  tweaks in the profiler script.
- Frequency helpers: `which_maxfreq` (largest),
  `which_minfreq` (smallest), `which_midfreq` (the
  `((n + 1) / 2)`-th largest). Lite performance pins the
  CPU minimum to the middle frequency; full performance
  pins both ends to the maximum. Balance restores the
  hardware range.
- CPU frequency path: if `/proc/ppm` exists, the PPM
  helpers write `"cluster max"` / `"cluster min"` to
  `/proc/ppm/policy/hard_userlimit_*_cpu_freq`; otherwise
  the direct helpers write `scaling_max_freq` /
  `scaling_min_freq` per CPU.
- Mitigation flags: `ENCORE_DISABLE_DDR_TWEAK` skips DDR
  tweaks, `ENCORE_NO_PERFORMANCE_CPUGOV` keeps the balanced
  governor instead of `performance`, and
  `ENCORE_QCOM_NO_GPU_POWERSAVE` skips the Snapdragon GPU
  powersave tweak. The balanced and powersave governors
  come from `ENCORE_BALANCED_CPUGOV` /
  `ENCORE_POWERSAVE_CPUGOV`, set from config by C++.
- SoC detection in `module/customize.sh`: extra sysfs
  checks first (KGSL devfreq or legacy KGSL -> Snapdragon,
  GED hal -> MediaTek, Tegra GPU -> Tegra), then
  device-tree model, then `getprop` values, then
  `/proc/cpuinfo` hardware/processor, then model name.
  The `recognize_soc` patterns, in order, are: `mt` ->
  MediaTek, `sm`/`qcom`/`Qualcomm` -> Snapdragon,
  `exynos`/`universal`/`samsung`/`erd`/`s5e` -> Exynos,
  `Unisoc`/`ums`/`sp`/`SC` -> Unisoc, `gs`/`Tensor` ->
  Tensor, `kirin` -> Kirin.

Stage 5 adds, without changing C++ or shell:

- `sysfs`: the static allowlist, generated by collecting
  every `apply`, `write`, `echo >`, and `tee` target in
  `encore_profiler.sh` and collapsing per-CPU, per-policy,
  per-block, per-thermal-zone, per-devfreq, and per-Mali
  nodes into `*` patterns (103 patterns). `is_allowed`
  rejects relative paths, `..` components, and anything
  not matching a pattern. `SysfsRoot` resolves device
  paths under `/` on a device or under a temporary
  directory in tests. `apply_write`/`apply_plan` write
  directly (sysfs nodes are not regular files, so atomic
  rename does not apply), skip missing files, and continue
  after every failure, mirroring the shell helpers.
- `profiler`: plans as data, not shell execution.
  `plan(root, request, soc, options)` returns concrete
  `(path, value)` writes in script order, for perfcommon,
  performance (lite or full), balance, and powersave, for
  all six profiler SoC families plus common-only Kirin and
  Unknown. Frequency nodes are resolved at plan time from
  the fake or real sysfs under `root`, so a plan is
  deterministic given a sysfs snapshot. `detect_soc` mirrors
  the install-time detection order, and `Soc::from_code`
  follows the profiler script's codes.
- No new dependency in Stage 5. Host gates pass: fmt,
  clippy `-D warnings`, tests 62 (58 `encore_core` + 4
  `encored`), deny, audit (1380 advisories, 20 crates, no
  vulnerabilities).

Deliberate Stage 5 limits, not parity claims:

- The profiler is not yet wired into the daemon loop:
  applying a plan on a game foreground event needs the
  Binder foreground callbacks, which are Stage 6. The
  `daemon` command therefore still does not apply
  profiles, and the C++ daemon remains shipped.
- The script's `chmod` steps are not replicated: on sysfs,
  permissions are kernel-controlled and the daemon runs as
  root, so chmod does not change whether a write succeeds.
- Background `&` jobs in the script are applied
  sequentially; the script waits for them, so the final
  state is the same without parallelism.
- SoC code discrepancy, preserved as found: the utility
  script maps 6 to Intel, 7 to Tegra, 8 to Kirin, but the
  profiler maps 6 to Tegra and has no 7 or 8, and the
  install script sets Tegra to 6. Profiler code 6 is
  therefore Tegra here; Kirin gets common tweaks only.
  // TODO(verify): confirm intended Intel/Kirin profiler
  behaviour with upstream.
- No sample `/proc/ppm/policy_status` file was available;
  the MediaTek policy-status toggle is replicated from the
  script's grep and substring logic, not from a real file.
  // TODO(verify): check a real policy_status file.
- Device-mitigation rules are still not parsed: the
  mitigation flags in a plan are inputs supplied by the
  caller (the daemon in Stage 6), not evaluated from
  `device_mitigation.json` here.

## §14 Stage 6: Binder / Java companion

Verified from `jni/Main.cpp`, `jni/base/BinderMonitor/`,
`jni/DeviceMitigationStore.cpp`, `jni/base/DeviceInfo/`,
and `jni/base/EncoreUtility/MiscUtility.cpp` before
implementing. The Java source for the resolver APK is
not in this clone, so the Java side is documented as a
protocol, not as verified code:

- Resolver protocol: the daemon forks and execs
  `/system/bin/app_process` with
  `-Djava.class.path=<MODPATH>/binder_resolver.apk` and
  main class `com.rem01gaming.binderresolver.MainKt`,
  after setting `ANDROID_ROOT`, `ANDROID_DATA`, and the
  runtime/art/i18n/tzdata apex roots without overwriting.
  It writes one query line per transaction code to the
  child's stdin, closes it, and reads stdout to EOF.
  Output lines are `<query> <code>`, split at the last
  space; `ERROR:` lines, space-less lines, and
  unparseable codes are skipped. A non-zero exit or a
  signal does not discard already-parsed codes; a
  required code still at 0 then fails initialisation.
  Fallbacks are 0 except `OnDisplayEvent` (1).
  Required codes are all except `OnProcessStarted` and
  `OnDisplayEvent`. Services, in order, are `power`,
  `notification`, `activity`, `display`, `package`,
  waited for up to 10 seconds when the
  `AServiceManager_waitForService` symbol is absent.
  This resolver runs with no timeout and an unbounded
  output buffer in C++ (finding 6); that gap is not
  claimed fixed here, and no Rust subprocess runner is
  provided until a timeout and output cap are added.
- Binder callbacks: process-observer parcels carry
  pid/uid/foreground, pid/uid/service-types, pid/uid on
  death, and pid/process-uid/package-uid/package-name/
  process-name on start, in that order. The display
  callback ignores its parcel, queries interactivity,
  and fires only on a change from the last state. Power
  save is polled every 500 ms and fires only on a change.
  `isPowerSave` and `getZenMode` return 0 / -1 on Binder
  failure; `getPackageNameForUid` returns empty on failure.
- Daemon state and evaluation, from `Main.cpp`: initial
  screen-awake is `true` regardless of the actual display
  state; battery saver and previous DND come from the
  initial Binder queries. Foreground background events,
  empty or unregistered packages, and already-active PIDs
  are ignored. Package names are stripped at the first
  NUL. Process death clears package, PID, and
  last-applied PID but not UID. Evaluation order is
  active awake game, then battery saver, then balance;
  performance wins over battery saver. A failed game
  application clears package, PID, UID, and last-applied
  PID. Balance and powersave clear UID but not package
  or PID. Every application writes `gameinfo` and the
  profile code even when tweaks are disabled. DND: a
  DND-enabled game sets DND on and marks the request;
  a non-DND game restores the previous state; leaving
  performance mode restores DND only if a game had
  requested it. Zen is refreshed on every evaluation
  when no game override is active, and a zen failure
  (-1) counts as DND on, as in C++.
- Mitigation: `default.items` plus `device_rules`, each
  with `filter_type` all/any, `items`, and
  `filter_condition` keyed by `soc`/`model`/`uname` with
  operators `match`/`contains`/`regex`. The profiler
  uses cached items (rules evaluated at load) plus
  default items when `use_device_mitigation` is true.
  Device info is `/proc/device-tree/model`,
  `ro.product.model`, and the kernel release, each
  `"Unknown"` on failure. The shipped default file has
  only `DISABLE_DDR_TWEAK`, `NO_PERFORMANCE_CPUGOV`, and
  `QCOM_NO_GPU_POWERSAVE`, and no device rules.
- DND and notifications in C++ are fork + exec of
  `/system/bin/cmd` (`notification set_dnd priority|off`,
  and `notification post` after dropping to uid/gid 2000
  for notify). Those executions are not in Rust yet;
  the state machine returns DND actions as data instead.

Stage 6 adds, without changing C++ or shell:

- `resolver`: the query table, service list, APK name,
  main class, output parser, and required-code check,
  all testable on a host.
- `mitigation`: full parsing, `all`/`any` evaluation,
  `match`/`contains`/`regex` conditions, cached and
  re-evaluated item sets, and mapping of the three known
  items to profiler flags.
- `daemon_state`: the Binder-driven state machine. It
  consumes decoded events and returns a
  `ProfileApplication` (gameinfo content, profile code,
  profiler plan from Stage 5, DND action) without holding
  a lock across profile application, which is the
  architectural fix for finding 5. The registry entry is
  read once as an owned snapshot, so the DND decision
  cannot read a freed pointer after profile application,
  which is the daemon-layer fix for finding 1. The
  daemon loop in `encored` does not yet receive Binder
  events; when a transport exists, it feeds events to
  this machine and performs the returned applications
  (write files, apply plan, set DND, in that order).
- New dependency `regex` 1, justified in writing in
  `rust/Cargo.toml`: C++ uses `std::regex` for mitigation
  conditions and Rust's standard library has no regex
  engine. It is used only for mitigation conditions.
  Host gates pass: fmt, clippy `-D warnings`, tests 74
  (70 `encore_core` + 4 `encored`), deny, audit
  (1380 advisories, 24 crates, no vulnerabilities).

Deliberate Stage 6 limits, not parity claims:

- No `AIBinder` transport, no resolver subprocess
  runner, no Binder thread pool, no DND/notification
  execution, and no daemon-loop Binder wiring: those
  need the Android NDK, `/dev/binder`, a device, and a
  timeout-bounded runner, none of which exist on a host.
  The C++ daemon remains shipped.
- The Java resolver's source is not in the clone, so its
  internal behaviour (beyond the query/response lines
  the C++ side sends and parses) is unverified.
  // TODO(verify): obtain or inspect the resolver source
  before relying on a code value on a new Android version.
- Finding 6 is still open at the transport layer: the
  C++ resolver has no timeout and an unbounded output
  buffer. A Rust runner must add both.
- Finding 9 (signal handling) remains open, as in
  Stage 4.

## §15 Stage 7, host part only

Stage 7 in the guide is "remove C++ only after behaviour
parity is proven". That full stage cannot be done on a
host: it needs the Android NDK, `/dev/binder`, a device,
and device tests on at least one Qualcomm and one
non-Qualcomm SoC. What this stage does is the host part
of Stage 7, chosen by the user, with the device part
explicitly left open and C++ still shipped:

- `encore_sys`: the thin unsafe crate the guide plans
  for. Every other crate keeps
  `#![forbid(unsafe_code)]`; this crate wraps only the
  `nix` calls that are themselves `unsafe fn` (`fork`,
  `sigaction`), each with a `// SAFETY:` comment.
  `daemonize` is called only at daemon startup, from
  the main thread, before any other thread exists, so
  the forked child inherits a single-threaded process.
  The signal handler writes exactly one byte to a pipe
  and does nothing else, which is async-signal-safe.
- `daemonize`: fork, parent exits, child becomes a
  session leader, changes directory to `/`, and
  redirects standard streams to `/dev/null`, as C++
  `daemon(0, 0)` does. Wired as `daemon --daemonize`,
  after the preconditions and the watcher are set up,
  so startup errors are still visible on the terminal;
  the default remains foreground for host testing.
  Manual test on this VM: `daemon --daemonize` returned
  exit 0, the child ran with PID = session = process
  group 37858, and the log showed the daemon started.
- `signals`: SIGTERM/SIGINT through a self-pipe. The
  handler only wakes the main loop; the loop logs
  "Encore Tweaks daemon exited" and returns, instead of
  C++ `_exit(0)` from a signal handler. This is the
  signal-layer fix for finding 9. Crash signals keep
  default behaviour, and SIGHUP/SIGUSR1/SIGUSR2 have no
  callbacks yet, because no Rust code registers any.
  Manual test: `kill -TERM` on the daemonised child
  above logged the exit line and the process was gone.
- `process_runner`: a generic subprocess runner with a
  10-second timeout (matching the Binder service wait)
  and a 64 KiB output cap, killing the child on either.
  Commands are always separate arguments, never a shell
  string. A non-zero exit is not a runner error, because
  the C++ resolver still returns parsed codes then.
- `resolver_runner`: the C++ resolver runner with the
  same APK arguments and the same six `ANDROID_*`
  environment variables (set only when absent), but
  queries written to the child's stdin through the
  timeout runner, and the Stage 6 protocol parser and
  required-code check applied to the output. This is
  the runner-layer fix for finding 6. Real execution
  still needs `app_process` and the APK on a device.
- `dnd`: `cmd notification set_dnd priority|off` through
  the timeout runner, mirroring C++ `set_do_not_disturb`.
  `notify` is deliberately not implemented: C++ drops
  to uid/gid 2000 before exec, which in Rust needs
  `Command::pre_exec`, an `unsafe` hook for a later
  `encore_sys` step. Running notify as root would be a
  security change, so it is deferred, not faked.
- `transport`: the Binder event-source seam. A source
  yields decoded events; `apply_events` feeds them to
  `DaemonState` and collects the profile applications.
  `NullEventSource` (yields nothing) is what the host
  daemon loop uses, making the absence of Binder
  explicit. `QueueEventSource` is the test source and
  the shape a real Android transport must match.
  The host daemon loop does not yet apply profile
  applications from Binder events, because there is no
  event source on a host and applying a plan to a host
  `/sys` would be wrong; that wiring is device work.

Host gates pass: fmt, clippy `-D warnings`, tests 83
(79 `encore_core` + 4 `encored`, `encore_sys` has no
unit tests because forking a test harness is not a
test), deny, audit (1380 advisories, 25 crates, no
vulnerabilities). The crate count rose by one because
`encore_sys` itself is a new crate in the lockfile.

Deliberate Stage 7 limits, not parity claims:

- No NDK cross-build, no `AIBinder` transport, no
  Binder thread pool, no real resolver execution, no
  `notify`, no device tests, and no C++ removal. The
  C++ daemon remains shipped, and no Rust binary is
  installed by `service.sh` or the module installer.
- The `daemon --daemonize` default differs from C++ on
  purpose (see above); on a device the module must
  invoke it with `--daemonize` to match C++.

## §16. Binder transport, part 1 (device work, started 2026-10-28)

Verified again from C++ before implementing, this time
from `jni/base/BinderMonitor/BinderMonitor.cpp`,
`jni/base/BinderMonitor/BinderMonitor.hpp`,
`jni/base/BinderNDK/BinderNDK.cpp`, and `jni/Main.cpp`
(the Stage 6 summary was re-checked against the code,
not copied from memory):

- C++ does not link `libbinder_ndk.so` at build time.
  `BinderNDK.cpp` `dlopen`s it at runtime and `dlsym`s
  every function, with fallbacks (null, false, or
  `STATUS_UNKNOWN_ERROR`) when a symbol is absent.
  Rust mirrors that in `encore_sys::binder`: only the
  functions `BinderMonitor` actually uses are loaded,
  not the full NDK surface, so the code compiles on a
  host without NDK headers and fails gracefully there.
- Services, in C++ order: `power`, `notification`,
  `activity`, `display`, `package`. A service that is
  not immediately available is waited for up to 10
  seconds with 100 ms polls when the
  `AServiceManager_waitForService` symbol is absent;
  when it is present, it is used directly.
- Single-value transactions, as in
  `transactReadInt32` / `transactReadString`: prepare,
  write the interface token, transact with flags 0, read
  the status header and require `AStatus_isOk`, then
  read the value. Any failure returns the caller's
  error default (`false` for booleans, `-1` for zen
  mode, `""` for a package), and the reply parcel,
  status, and status description are freed on every
  path. Interface tokens are the five C++ tokens.
- Callback classes: `android.app.IProcessObserver` and
  `android.hardware.display.IDisplayManagerCallback`,
  each with a no-op create/destroy and an `onTransact`
  handler. The process observer reads, in order,
  pid/uid/foreground, pid/uid/service-types, pid/uid,
  or pid/process-uid/package-uid/package-name/
  process-name, and returns `STATUS_OK`, or
  `STATUS_UNKNOWN_ERROR` for an unknown code. The
  display callback ignores its parcel, re-queries
  interactivity, and fires only on a change.
- `Main.cpp` only sets the foreground-activities and
  process-died callbacks; foreground-services and
  process-started are left empty in C++. Rust delivers
  only foreground, died, and display events, matching
  that. `Main.cpp` also sets the initial state as
  `screen_awake = true` hardcoded, `battery_saver` from
  `isPowerSave()`, and `prev_dnd` from
  `getZenMode() != 0` (so a zen failure, returning -1,
  counts as DND on); `BinderTransport::initial_state`
  returns exactly those two query values.
- Registration transactions use the *registration*
  codes (`registerProcessObserver`,
  `registerCallback`), not the callback codes
  (`onForegroundActivitiesChanged`, `onDisplayEvent`);
  this was caught and fixed during implementation, and
  is recorded here so it is not regressed.
- The display and power-save initial states are seeded,
  not emitted, as in C++: `initialize` sets
  `displayLastState` / `powerSaveLastState` and only
  fires on a later change (its initial firings, if any,
  go to callbacks that `Main.cpp` sets only after
  `initialize`).

Implemented in this part:

- `crates/encore_sys/src/binder.rs`: the `dlopen` /
  `dlsym` loader, `BinderServices::acquire`,
  `is_power_save` / `is_interactive` / `get_zen_mode` /
  `get_package_name_for_uid`, callback registration
  with a global mutex-guarded registry, and
  `start_thread_pool` / `join_thread_pool`. Service
  pointers are held for the process lifetime, as in
  C++, and are never freed by this module. Every
  `unsafe` block has a `// SAFETY:` comment.
- `crates/encore_core/src/binder_client.rs`:
  `BinderQueryClient` (with `MockBinderClient` for
  tests), `SysBinderClient` (services + resolved
  codes, NUL-stripping packages as in `Main.cpp`'s
  `remove_null_char`), `map_callback_event` (pure
  callback-to-`BinderEvent` mapping), `PowerSavePoller`
  (500 ms cadence, seeds on first poll, emits only on
  a change), and `BinderTransport` (a
  `BinderEventSource` whose queue is fed by
  `encore_sys` callbacks, plus the power-save poller).
  `register_sys_callbacks` registers a transport's
  queue with `encore_sys`, resolving foreground
  packages inside the callback, as in C++.
- One deliberate difference from C++: power-save
  polling is driven by daemon-loop polls, not a
  dedicated thread. The daemon loop already polls its
  event source, so a second thread would only add a
  lock around the same state; the 500 ms cadence and
  change-only firing are preserved.

Host gates pass: fmt, clippy `-D warnings`, tests 89
(85 `encore_core` + 4 `encored`, `encore_sys` 0), deny,
audit (1380 advisories, 25 crates, no
vulnerabilities). New tests: NUL stripping, callback
mapping (including that died/display events do not
resolve a package), power-save seeding/change-only/
500 ms cadence, transport queue draining and initial
state, resolved-code mapping, and a host-only test
that `SysBinderClient::acquire` returns `None` without
`libbinder_ndk.so` (marked `#[cfg(not(target_os =
"android"))]`, so it does not falsely fail on a
device where the library exists). No new dependency.

Deliberate limits of this part, not parity claims:

- The `encored` daemon loop is **not** yet wired to
  `BinderTransport`: it still uses `NullEventSource`.
  Wiring it in also needs the loop to perform the
  returned `ProfileApplication` values (write `gameinfo`
  and `current_profile`, apply the profiler plan, set
  DND) and to restore DND on exit, which is the next
  step, not this one.
- No NDK cross-build, no device test, and no
  `STATUS_UNKNOWN_ERROR` constant verification against
  NDK headers yet (marked `// TODO(verify):` in the
  code; any non-zero value is treated as an error by
  the Binder driver for an unknown code, so unknown-code
  behaviour does not depend on the exact value).
- There is at most one `BinderTransport` per process,
  matching the C++ `BinderMonitor` singleton and the
  global callback registry in `encore_sys`.
- The C++ daemon remains shipped, and no Rust binary is
  installed by `service.sh` or the module installer.

## §17. Daemon wiring, Android cross-build, and flashable test module (2026-10-28)

This is the first Rust build that is packaged as a
module at all. It is a **test** build, not a parity
claim: it has never been flashed, and device behaviour
below is what the code is written to do, not what has
been observed on a device.

### Daemon wiring

`encored`'s daemon loop now, after daemonising (so the
parent does not touch Binder, as in C++), sets up
Binder when `/dev/binder` exists, and skips it with a
warning when it does not. The `/dev/binder` check is
the host/device switch: on a host it does not exist,
so host daemon runs keep working with file watching
only, exactly as before; on a device it always exists,
so a Binder failure there is a startup failure, as in
C++ `encore_main_daemon` (resolver failure, service
acquisition failure, or callback registration failure
all fail startup, they do not silently continue
without Binder).

On Binder setup success, the loop:

- applies perfcommon first, then the initial
  evaluation (balance or powersave, with no game
  active), as in C++ `encore_main_daemon`;
- on every loop iteration, builds fresh evaluation
  inputs from the current config store, registry, SoC,
  and mitigation options, feeds Binder events through
  `DaemonState` via `apply_events`, and performs every
  resulting application in C++ order: write `gameinfo`
  and `current_profile` (with a trailing newline, as
  in `Profiler.cpp`, atomically in Rust), apply the
  profiler plan with `apply_plan` (continuing after
  every failure), then set DND via the timeout runner;
- on graceful exit (SIGTERM/SIGINT or module update),
  restores DND to the user's previous state when a
  game had requested it. This differs from C++, whose
  signal handler calls `_exit(0)` and therefore leaves
  DND as the game set it; a graceful Rust exit should
  not leave the user stuck in DND.

SoC detection at daemon startup uses sysfs
existence, the device-tree model, and `/proc/cpuinfo`,
in `customize.sh` order except for `getprop`
properties, which Rust does not yet read (see below).
Mitigation options are re-parsed from
`device_mitigation.json` on every iteration, so a
mitigation reload takes effect without a restart.

Host gates pass after wiring: fmt, clippy `-D
warnings`, tests 89 (85 `encore_core` + 4 `encored`),
deny, audit (1380 advisories, 25 crates, no
vulnerabilities). No new dependency. Host manual
test, in `/tmp/encore-test8` with a fake gamelist /
config / mitigation: `setup_gamelist` and
`check_gamelist` succeeded (1 game), then `daemon`
ran, logged "Binder not available (/dev/binder
missing), continuing without Binder events" and
"detected SoC family: Unknown" (correct on a host
with no KGSL), and on SIGTERM logged "Encore Tweaks
daemon exited" gracefully.

Deliberate limits of the wiring, not parity claims:

- `notify` (the other `MiscUtility` function, which
  drops to uid/gid 2000) is still not implemented, so
  fatal-error notifications and the "tweaks applied"
  module-description update that C++ posts via
  `notify` do not happen; failures are logged to
  `encore.log` and printed, which is what a device
  test must check instead.
- SoC/model detection from `getprop`
  (`ro.board.platform`, `ro.soc.model`,
  `ro.product.model`) is not yet implemented in Rust.
  For whyred this does not matter for the first test:
  the KGSL sysfs path is checked first and already
  decides Snapdragon. For a device whose SoC is only
  detectable via properties, Rust falls back to
  `Soc::Unknown` (common-only plan), a safe fallback
  rather than a wrong SoC plan, but not full parity.
- The `getprop` gap also means mitigation rules
  keyed on `model` will not match (model is empty),
  while rules keyed on `soc` (device-tree model) and
  `uname` (kernel release) do work.

### Android cross-build

- Android NDK r27d, downloaded as
  `android-ndk-r27d-linux.zip` (634 MB) and unpacked
  under `~/workspace/android-ndk/` (not part of the
  repo); `cargo-ndk` 4.1.2; Rust targets
  `aarch64-linux-android` and
  `armv7-linux-androideabi`; platform 29, matching
  `jni/Application.mk` (`android-29`).
- Command: `cargo ndk -t arm64-v8a -t armeabi-v7a
  --platform 29 build --release -p encored`.
  Both targets compiled on the first attempt, with
  no Android-specific code changes: `nix`, `regex`,
  `serde_json`, `libc`, `dlopen`, `fork`, and
  `sigaction` all build for Bionic as written.
- Binaries (release profile: `opt-level = "z"`,
  `lto = true`, `panic = "abort"`, `strip = true`):
  - `aarch64-linux-android/release/encored`: 1,596,128
    bytes, ELF 64-bit LSB PIE, ARM aarch64,
    interpreter `/system/bin/linker64`, stripped.
    (Original C++ arm64: 861,272 bytes.)
  - `armv7-linux-androideabi/release/encored`:
    1,127,288 bytes, ELF 32-bit LSB PIE, ARM EABI5,
    interpreter `/system/bin/linker`, stripped.
    (Original C++ armv7: 562,788 bytes.)
  - Rust binaries are larger than C++ ones; that is
    expected (Rust standard library, no shared
    `libc++`), and binary size is the lowest task
    priority, after security, correctness, and
    cleanliness.

### Flashable test module zip

Built from the original module zip
(`encore-5.2.2-1571-47cdb70-main.zip`, 111 files) by
the local helper `~/workspace/build-rust-module-zip.sh`
(not part of the repo), with exactly these changes:

- `libs/arm64-v8a/encored` and
  `libs/armeabi-v7a/encored` replaced by the Rust
  binaries above.
- `service.sh`: `encored daemon` changed to
  `encored daemon --daemonize`, because C++
  `daemon` detaches itself with `daemon(0, 0)` while
  Rust `daemon` stays foreground by default and only
  detaches with `--daemonize`. Without this change,
  `service.sh` would block on the foreground daemon
  instead of returning after launch. This is the only
  script change; `customize.sh`, `verify.sh`, the APK,
  config, and webroot are unchanged.
- `module.prop`: id stays `encore` (so it replaces
  the original module cleanly), name changed to
  "Encore Tweaks (Rust test)" so the manager UI
  shows which build is installed; version and
  versionCode unchanged.
- Every `.sha256` regenerated exactly as upstream
  `gen_sha256sum.sh` does (hash only, no filename),
  for every file except `*.sha256` itself. This is
  required: `verify.sh` aborts installation on a
  checksum mismatch for files it extracts.

Verification of the built zip (not a device test):

- 111 files, 2.8 MB, zip comment
  "5.2.2-1571-47cdb70-main Rust test build".
- Root structure is a module structure, unlike the
  earlier source zips: `META-INF/com/google/android/
  update-binary`, `module.prop`, `service.sh`,
  `customize.sh`, `verify.sh`, and
  `binder_resolver.apk` are all at the zip root.
- All 49 non-`.sha256` files pass their `.sha256`
  check after extraction, including both Rust
  binaries (arm64 hash matches the cross-built
  binary exactly).
- `customize.sh` is unchanged, so its
  Magisk/KernelSU/APatch branches are unchanged:
  it maps installer `ARCH` `arm64` to `arm64-v8a`
  and `arm` to `armeabi-v7a`, extracts only the
  matching binary, and for KernelSU/APatch it touches
  `skip_mount`, removes `action.sh`, and symlinks
  `encored` into `/data/adb/ksu/bin` or
  `/data/adb/ap/bin` when those directories exist.
  One zip therefore installs in all three managers
  by format, as the original zip does. Only KernelSU
  can be tested on the user's whyred (the manager
  installed there); Magisk and APatch installation
  are format-supported but untested, and must not be
  claimed as tested until a device with each manager
  has flashed it.

### What a device test must check (whyred, KernelSU first)

1. Flash the module zip in KernelSU, reboot.
2. `ls /data/adb/modules/encore/system/bin/encored`
   exists; `ls /data/adb/.config/encore/` exists.
3. `/data/adb/.config/encore/encore.log` contains, in
   order: "Encore Tweaks daemon started", "Binder
   initialised: battery saver …, zen mode …",
   "detected SoC family: Snapdragon", and no
   "failed to resolve Binder transaction codes" or
   "failed to acquire Binder services".
4. `cat /data/adb/.config/encore/current_profile`
   is 2 (balance) or 3 (powersave) at idle, and
   `gameinfo` is "NULL 0 0".
5. Open a game listed in `gamelist.json`: log
   shows the foreground event, `current_profile`
   becomes 1, and `gameinfo` becomes
   "<package> <pid> <uid>".
6. Exit the game / turn the screen off:
   `current_profile` returns to 2, and DND (for a
   DND game) returns to the user's previous state.
7. Edit `gamelist.json` / `config.json` and check
   the reload log lines; create then remove
   `/data/adb/modules/encore/update` only as a stop
   test if the daemon is to be stopped, otherwise
   leave it absent.
8. Rollback: uninstall "Encore Tweaks (Rust test)"
   in KernelSU and reboot; the original zip can be
   flashed again afterwards.

If step 3 fails, the daemon did not start on the
device: send the full `encore.log`, not just the
last lines, because the failure reason (resolver,
service acquisition, or callback registration) is
logged there. If step 5 fails while step 3 passes,
the transport is up but profile application is not:
send `encore.log`, `current_profile`, and `gameinfo`.

## §18. First device test failure: Binder service acquisition (2026-10-28)

Device: whyred, KernelSU, LineageOS Android 13
(SDK 33), kernel 4.19.325 aarch64, chipset
Snapdragon sdm660, after flashing
`encore-rust-module-flashable.zip` and rebooting.

Observed, from the WebUI bugreport
(`encore.log`, 12:43 WIB) and the user's Termux
output, which agree exactly:

- `2026-10-28 05:42:06.603 I encored: Encore
  Tweaks daemon started`
- `2026-10-28 05:42:13.107 E encored: failed to
  acquire Binder services`
- No `current_profile` file, no `encored` process
  afterwards, WebUI "Terhenti / Layanan tidak
  aktif" and "Profil Saat Ini: Tidak diketahui".

Ruled out by the Termux output:

- The installed binary is the Rust one (1,596,128
  bytes) and it runs: `encored version` prints
  "Encore Tweaks 5.2.2 (1571-47cdb70-main)".
- The install itself succeeded: `gamelist.json`
  (162 bytes), `config.json`, `default_cpu_gov`,
  `device_mitigation.json`, and `soc_recognition`
  (2 bytes, i.e. code 2, Snapdragon, matching
  `customize.sh`) are all present in
  `/data/adb/.config/encore/`.
- The KernelSU symlink
  `/data/adb/ksu/bin/encored` exists and points at
  the module binary, so `service.sh`'s bare
  `encored` call is not the failure.

Not yet ruled out, because the v1 log line was
generic (unlike C++, which logs the failing service
name): whether `libbinder_ndk.so` failed to load
(or a required symbol was missing), or one
specific service among `power`, `notification`,
`activity`, `display`, `package` could not be
acquired even after waiting. The 6.5 s gap between
"daemon started" and the failure is mostly the
resolver run, so the acquisition failure itself
was fast, which argues against the 10 s poll
fallback having been used for a missing service,
but that is an inference, not evidence.

Fix applied for the next test build (v2), not a
fix for the underlying failure: `BinderServices`
now has `acquire_detailed() -> Result<Self,
AcquireError>` (`Library` vs `Service(name)`),
`SysBinderClient::acquire_detailed()` converts
that to a human-readable reason, and `encored`
logs "failed to acquire Binder services: <reason>".
This mirrors the C++ per-service log line that
Rust was missing. No behaviour changed: the same
services, the same wait, the same failure outcome.
Host gates pass after this change (fmt, clippy
`-D warnings`, tests 89, deny, audit), and the v2
module zip was rebuilt and its `.sha256` files
verified the same way as v1.

## §19. Second device test: library load failure, narrowed (2026-10-28)

- v2 log (bugreport 12:52 WIB, daemon start
  05:50:45 UTC): "failed to acquire Binder
  services: libbinder_ndk.so could not be loaded
  or a required symbol is missing". So the failure
  is in `load_binder_ndk`, not in service
  acquisition, and `service list` on the device
  shows all five services present, so a missing
  service was already ruled out.
- The user's Termux run of
  `/data/adb/modules/encore/system/bin/encored
  daemon` in the foreground also printed "ERROR:
  Failed to initialize Binder monitor", so the
  failure reproduces outside boot / daemonize
  context, in a root Termux shell.
- `/system/lib64/libbinder_ndk.so` exists on the
  device (90,296 bytes), so the file itself is not
  absent; what is still unknown is whether
  `dlopen` fails (and with what `dlerror`) or one
  required symbol is missing.
- v3 diagnostic change (not yet a fix): the
  loader now returns `LoadError::Open(dlerror)` or
  `LoadError::Symbol(name)`, checked symbol by
  symbol, and the daemon log prints that detail.
  Host gates pass after this change.

## §20. Root cause found: `AParcel_writeInterfaceToken` does not exist (2026-10-28)

v3 log (bugreport 12:59 WIB, two daemon starts,
05:58:10 and 05:58:53 UTC): "failed to acquire
Binder services: libbinder_ndk.so is missing
required symbol 'AParcel_writeInterfaceToken'".

That symbol is not in `libbinder_ndk.so` at any
NDK r27 API level (checked 29, 30, 31, 33, 34,
35: zero matches), so this was never going to
load on any device, not a whyred quirk. C++ loads
the same symbol with `dlsym` in `BinderNDK.cpp`,
but its `FORWARD` macro returns
`STATUS_UNKNOWN_ERROR` when the pointer is null,
and every C++ caller ignores that return value,
so the C++ call is a no-op. The transactions still
work because the NDK writes the interface-token
header itself in `AIBinder_prepareTransaction`,
from the class associated with the service in
`initialize`. Rust, by contrast, made the symbol
required in the loader and checked its return
value in every transaction helper, so the loader
failed before any service was even acquired.

Fix (v4): `AParcel_writeInterfaceToken` is removed
entirely from `encore_sys::binder` — no loader
entry, no field, no call in `transact_read_int32`,
`transact_read_string`, or `register_binder`. The
transaction helpers now rely on
`prepare_transaction` plus the associated class,
exactly as the NDK intends and as C++ effectively
does. Host gates pass after this change (fmt,
clippy `-D warnings`, tests 89, deny, audit).
The next device test must confirm not only that
acquisition succeeds, but that queries and
callback registration actually work, because the
first transaction is the first real test of the
token header.

## §21. First successful device run: v4 on whyred (2026-10-28)

Bugreport 13:07 WIB, daemon start 06:06:35 UTC:

- "Binder initialised: battery saver false, zen
  mode 0" — the first successful Binder queries
  on a device, proving the §20 fix: transactions
  work with the token header written by
  `prepare_transaction` alone.
- "detected SoC family: Snapdragon" — KGSL sysfs
  detection works on whyred, as predicted.
- "applied profile 0: 155 sysfs writes applied,
  14 missing, 0 rejected, 0 failed" — perfcommon
  plan applied at startup.
- "applied profile 2: 51 sysfs writes applied, 6
  missing, 0 rejected, 0 failed" — initial
  evaluation to balance, with no game active and
  no battery saver.
- WebUI at the same time: "Berfungsi", "PID
  Daemon: 8841", "Profil Saat Ini: Seimbang"
  (balance), kernel aarch64, Snapdragon sdm660,
  SDK 33.

This is the first device proof that the Rust
daemon starts, talks to Binder, detects the SoC,
and applies profiles. It does not yet prove game
foreground (profile 1, `gameinfo`), game exit,
screen-off, battery-saver, DND, or
gamelist/config reload on a device; those are
the next device tests, in §17 order.

## §22. Level 2: encore_fas integrated, toggleable from the app (2026-10-28, host only)

User chose Level 2, then revised it: FAS must be a setting
in the Encore app, on/off by the user, not always on.

- `encore_core::fas` (new): attach/detach through the
  `fas_ctl` binary on `$PATH` via the timeout runner
  (10 s, 64 KiB cap, separate args, no shell), never
  by reimplementing the FAS ioctl protocol. Default
  targets 60 and 120 fps. `FasTracker` holds at most
  one listener, dedupes by PID, and clears on detach
  failure. `parse_ctx_id` reads `ctx_id: <n>` from
  `fas_ctl attach` output.
- Daemon loop (`encored/src/main.rs`): every profile
  application also syncs FAS, but only when the new
  setting is on; a game application (profile 1, real
  `gameinfo`) attaches, any other application detaches.
  A config reload re-syncs from the current state, so
  toggling FAS in the app attaches to the current game
  or detaches immediately, without a Binder event and
  without a reboot. Daemon exit (signal or module
  update) detaches. Any FAS failure is a warning only;
  profile application never fails because of FAS.
- Setting: `preferences.enable_fas` in `config.json`,
  default false, so existing users see no change until
  they enable it. In Rust it lives on `ConfigData`, not
  on `Preferences`, so `Preferences` still mirrors C++
  exactly and stays under clippy's bool limit; the C++
  daemon ignores the unknown JSON field.
- WebUI (source only, `webui/`): `EncoreConfig` store
  gains `isFasEnabled`/`setFas` (default false), new
  page `FasToggle.vue` (no reboot modal, by design:
  the daemon watches `config.json`), route
  `/settings/fas`, a Settings list entry, and locale
  keys in `en.json` and `id.json` only (other locales
  fall back to English until Weblate updates them).
  `module/webroot/` is built, not tracked, so this
  change reaches the app only when webroot is rebuilt
  at packaging time.
- Installer/service: `service.sh` insmods
  `system/lib/modules/encore_fas.ko` before the daemon,
  logging failure to `fas_load.log` and continuing
  without FAS. `customize.sh` extracts `fas_ctl` and
  the `.ko` only when the zip contains them (guarded
  `unzip -l` checks, because `extract()` aborts on a
  missing file), and symlinks `fas_ctl` for KSU/APatch
  when present.
- Host gates pass: fmt, clippy `-D warnings`, tests
  94 (90 encore_core + 4 encored, up from 89).
  NOT device tested: no integrated module zip has been
  built, no `.ko` for whyred exists yet (a `.ko` is
  kernel-specific; the QEMU v6.12 `.ko` will not load
  on whyred 4.19), and the WebUI toggle has not been
  seen running on a device.

## §23. Integrated module zip from combined source (2026-10-28, not device tested)

Built after the user cancelled the remaining QEMU kernel
builds ("tidak perlu build kernel"): a flashable module
zip assembled from the combined source, following
`compile_zip.sh` (not from the original zip), at
`encore-rust-fas-module-flashable.zip` (115 files,
3.2 MB, sha256
8cd2a595178787bf0ba3fbdf5d44c42be70f3d45b4d75d7c10107bd788f18e52).

- `libs/arm64-v8a/encored` 1,601,584 bytes and
  `libs/armeabi-v7a/encored` 1,131,076 bytes, Rust
  cross-build with FAS code (strings prove `enable_fas`,
  `FAS attached/detached`, `fas_ctl` in both binaries).
- `libs/arm64-v8a/fas_ctl` and
  `libs/armeabi-v7a/fas_ctl`, Rust fas-ctl cross-build
  (queueBuffer symbols in both). The armv7 tool's ELF
  resolver is aarch64-only, so on a 32-bit device its
  attach will fail gracefully; that is a pre-existing
  FAS limitation, not a packaging bug.
- Webroot rebuilt from `webui/` with npm + vite
  (CI uses bun; the build output is the same vite
  output): `settings/fas` and `enable_fas` strings are
  in the built JS, and the file list matches the
  original webroot except for new content hashes.
- `module.prop` name "Encore Tweaks (Rust FAS test)",
  id encore, version/versionCode unchanged from the
  original; `service.sh` insmods a `.ko` only when
  present and runs `encored daemon --daemonize`;
  `customize.sh` extracts per-arch `fas_ctl` from
  `libs/` with a guard, plus the older guarded
  `system/bin/fas_ctl` and `.ko` paths.
- Verification on the finished zip: structure clean
  (META-INF and module.prop at root, no `module/`
  prefix, no placeholders, no `.map`), all 51
  non-sha256 files pass their `.sha256` checks
  (0 failures), every unguarded `customize.sh`
  extract target exists in the zip, and every binary
  is the correct ELF PIE for its arch.
- Deliberately absent, by design: no `.ko` in this zip,
  because no target-kernel `.ko` was built (the user
  cancelled kernel builds, and the QEMU v6.12 `.ko`
  must not be shipped to devices). `service.sh`
  skips insmod when the file is absent, and the
  daemon logs FAS attach failures as warnings only,
  so Encore works fully and FAS stays in fallback
  until a `.ko` for the target kernel is built.
  Device flash/test of this zip has not been done.

## §24. Fixed zip: fas_ctl symlink guard (2026-10-28, device-found bug)

Device test of the §23 zip found `/data/adb/ksu/bin/fas_ctl`
missing, while `encored`/`encore_profiler`/`encore_utility`
symlinks were created at install time. Cause: the guard in
`customize.sh` checked `[ -f "$BIN_PATH/fas_ctl" ]`, the
final module path, at install time, when the module is
still in staging (and a previous install there has no
`fas_ctl`), so the guard failed on a clean install and
the symlink was never created. Fix: check
`[ -f "$MODPATH/system/bin/fas_ctl" ]` (where the file
was just copied during install) while keeping the link
target as `$BIN_PATH/fas_ctl`, the final location.
Fixed zip `encore-rust-fas-module-flashable-fixed.zip`
(115 files, 3.2 MB, sha256
b437efd62c3482f440f8f76b33b2384dce07dc0f3da7e105e2ee357852744117)
differs from the §23 zip only in `customize.sh` (plus
its regenerated `.sha256`); all 51 non-sha256 files
pass their checks. Device re-test of the fixed zip
(install + reboot + `ls -l /data/adb/ksu/bin/fas_ctl`)
PASSED 2026-10-28 17:17 WIB: after reinstall + reboot,
the symlink shows timestamp 17:15 (the fixed zip's
install time, not the 17:12 manual symlink), target
`/data/adb/modules/encore/system/bin/fas_ctl`.
Symlink bug closed.

## §25. FAS retry backoff (2026-10-28, device-found log spam)

The §23/§24 zip, with the FAS toggle on and no `.ko`,
logged 537 identical `FAS attach failed` warnings in
66 seconds of gaming (about 8 per second). Cause:
`FasTracker` stored nothing on a failed attach, and
the daemon's watcher sync runs about every 100 ms, so
every iteration re-spawned `fas_ctl`. Fix in
`encore_core::fas`: the tracker now remembers the
failed `(pid, time)` and suppresses retries for that
PID for `FAS_RETRY_INTERVAL` (30 s); a different PID
is not suppressed, leaving the game clears the failure
(so a reused PID retries at once), and a success
clears it. The warning therefore fires at most once
per 30 s per PID, while a module loaded mid-game
still attaches within one interval. The sync entry
point is unchanged for callers; tests use an explicit
`sync_with_time` so the backoff is deterministic
without sleeping. A first version of the fix failed
its own new test: the dedupe early-return
(`pid == active_pid`, both `None`) made the
game-exit clear branch unreachable; the fix
restructures the `None` branch first, and the test
now covers it. Host gates pass: fmt, clippy
`-D warnings`, tests 95 (91 encore_core + 4 encored,
up from 94). Fixed zip
`encore-rust-fas-module-flashable-fixed2.zip`
(115 files, 3.2 MB, sha256
5ed80c7e5e8735ee732d590aa3e604f8ef6e12c10c2923e4a9a3b7f5b50034c3)
differs from the §24 zip only in the two `encored`
binaries (plus their `.sha256`); device re-test
PASSED 2026-10-28 17:34 WIB, see §26 for the full
quick-test results (backoff, game exit, battery
saver, screen-off, runtime toggle, DND).

## §26. Device quick tests 1-4 (2026-10-28, whyred/KernelSU, fixed2 zip)

All tests below ran in a single daemon run starting
2026-10-28 10:31:49 UTC (fixed2 zip, FAS toggle on,
no `.ko`, so FAS attach always fails gracefully).
All profile lines in every log show 0 rejected and
0 failed. Times below are UTC unless noted; WIB is
UTC+7. The daemon was never restarted during these
tests; game process `com.mobilechess.gp` ran first
as pid 28735 all day, then as pid 9984 after a
restart (gameinfo uid 10200, correct; an earlier
`28735 0` gameinfo uid 0 was transient and did not
recur, and profiles do not depend on uid).

Backoff proof (spans all tests): FAS warnings
dropped from 537 in 66 s (§25) to at most one per
30 s per PID. Examples: 10:32:37.775, 10:32:38.827
(1.05 s later, a different foreground PID, allowed
by design), 10:33:08.971 (exactly 30.14 s after the
second warning); later sessions repeat the pattern
with intervals 30.02-30.10 s, including a 10-minute
silence with the toggle off (11:09:37-11:19:32).
No `FAS attached/detached` lines, as expected with
no `.ko` (detach is a no-op when nothing attached).

Game foreground/exit: profile 1 (52 writes, 6
missing) applied on every game foreground, profile
2 (51 writes, 6 missing) on every exit, proven
repeatedly (exits at 10:33:30, 10:35:42, 10:50:02,
11:09:22, 11:29:33, 12:03:58, and others).

Test 1, Battery Saver: idle + Battery Saver ON
applied profile 3 at 10:43:06 (64 writes, 6 missing,
0 failed), `current_profile` 3; OFF at 10:43:29
returned to profile 2. PASSED.

Test 2, screen-off: game foreground, lockscreen
for about 2 minutes (user-reported duration). Log
gap 10:58:19 profile 2 to 11:00:42 profile 1
(2 min 23 s) has zero log lines in between:
balance during lock, performance on unlock, FAS
silent during lock. `logcat screen_toggled` gives
no output on this ROM, so that verification method
does not work here; the pass rests on the user's
lock report plus the matching log gap. PASSED.

Test 3, runtime FAS toggle (no reboot, no daemon
restart): Phase A, toggle OFF: `config reloaded`
at 11:09:25 (3 s after profile 2), `enable_fas`
false, then profile 1 at 11:09:36 and 11:09:37
with NO FAS warning after either, where every
enabled session warned within 0.1-0.25 s of
profile 1. Phase B, toggle ON: `config reloaded`
at 11:20:07, `enable_fas` true, then profile 1 at
11:20:51 with a warning 0.15 s later, a second
profile 1 + warning at 11:20:52, then backoff
warnings at 11:21:23, 11:21:53, 11:22:23, 11:22:53.
PASSED in both directions.

Test 4, DND: game `com.mobilechess.gp` has
`enable_dnd` true (WebUI Pengaturan Game shows
Jangan Ganggu ON, Mode Lite OFF, Tweaks ON).
`settings get global zen_mode` was 1 (priority)
during game sessions. A first after-exit check
showed zen_mode still 1, but the before-game
value had never been recorded, so that alone
proved nothing. A controlled retest closed it:
game exit applied profile 2 at 12:03:58,
`current_profile` 2, `zen_mode` 0 after exit.
PASSED (on = 1 during game, off = 0 after exit).

Bonus proofs in the same logs: config reload
works on device (`Log level changed to debug` +
`config reloaded` at 10:41:40 and again at
10:41:44); gamelist reload works on device
(`gamelist reloaded: 2 games` at 11:34:10).

Still NOT proven by these tests: FAS actually
attaching/measuring frames (needs a whyred 4.19
`.ko`; see §23), Magisk/APatch installs,
non-Qualcomm devices, and the Rust `notify`
(uid/gid-2000 drop, still unimplemented).
