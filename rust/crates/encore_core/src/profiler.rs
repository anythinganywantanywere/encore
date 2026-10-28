//! Profiler plans as pure data, from `scripts/encore_profiler.sh`.
//!
//! The C++ daemon decides the profile and runs, in order,
//! `encore_profiler perfcommon`, `performance`,
//! `performance_lite`, `balance`, or `powersave`
//! (`jni/Profiler.cpp`). The shell script then applies
//! hundreds of tweaks, dispatched by the SoC code read from
//! `soc_recognition` at install time.
//!
//! This module does not translate the script line by line
//! and does not run a shell. It builds a planned list of
//! concrete writes, `(path, value)`, which the caller
//! applies with [`crate::sysfs::apply_plan`], so every
//! write is checked against the sysfs allowlist and one
//! failed tweak cannot kill the daemon.
//!
//! Verified facts, and the places they were checked:
//!
//! - Profile dispatch in the script's final `case`: five
//!   arguments, where `performance_lite` is
//!   `performance_profile lite`, and `powersave_profile`
//!   first calls `balance_profile`, then overrides the
//!   governor and adds SoC powersave tweaks.
//! - SoC dispatch in the script's `case $SOC`: 1 MediaTek,
//!   2 Snapdragon, 3 Exynos, 4 Unisoc, 5 Tensor, 6 Tegra.
//!   No other code gets SoC-specific tweaks there.
//! - `LITE_MODE` only changes performance: full mode pins
//!   CPUs/GPUs to their maximum frequency, lite mode pins
//!   the minimum to the middle frequency instead.
//! - Mitigation flags, set by the C++ daemon as `ENCORE_*`
//!   environment variables before running the script:
//!   `ENCORE_DISABLE_DDR_TWEAK` skips DDR tweaks,
//!   `ENCORE_NO_PERFORMANCE_CPUGOV` keeps the balanced
//!   governor instead of `performance`, and
//!   `ENCORE_QCOM_NO_GPU_POWERSAVE` skips the Snapdragon
//!   GPU powersave tweak.
//!
//! Deliberate differences and open items:
//!
//! - SoC code mapping discrepancy, preserved as found, not
//!   silently fixed: `encore_utility.sh` maps 6 to Intel,
//!   7 to Tegra, 8 to Kirin, but the profiler's `case`
//!   maps 6 to Tegra and has no 7 or 8. The install script
//!   (`module/customize.sh`) sets Tegra to 6 via its extra
//!   detection, so profiler code 6 is Tegra here, and
//!   Kirin (7) gets only the common tweaks.
//!   // TODO(verify): confirm the intended Intel/Kirin
//!   profiler behaviour with upstream.
//! - Background `&` jobs in the script (I/O tweaks, some
//!   Exynos/Tensor DRAM loops) are applied sequentially
//!   here. The script waits for them before exiting, so
//!   the final state is the same, just without parallelism.
//! - The script's `chmod` steps are not replicated, for the
//!   reason given in [`crate::sysfs`].
//! - The MediaTek PPM `policy_status` toggle is replicated
//!   from the script's grep and substring logic, but no
//!   sample `policy_status` file was available to confirm
//!   the row format on a real device.
//!   // TODO(verify): check a real `/proc/ppm/policy_status`.

use crate::sysfs::SysfsRoot;

/// A `SoC` family, using the profiler script's codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Soc {
    /// Code 1.
    MediaTek,
    /// Code 2.
    Snapdragon,
    /// Code 3.
    Exynos,
    /// Code 4.
    Unisoc,
    /// Code 5.
    Tensor,
    /// Code 6 in the profiler script.
    Tegra,
    /// Code 7 from install-time detection; the profiler
    /// script has no Kirin-specific tweaks.
    Kirin,
    /// Unknown or unrecognised `SoC`: common tweaks only.
    Unknown,
}

impl Soc {
    /// Map a profiler-script `SoC` code to a family.
    /// Returns `None` for codes the profiler does not
    /// dispatch, including 0, 7, and 8.
    #[must_use]
    pub fn from_code(code: i32) -> Option<Self> {
        match code {
            1 => Some(Self::MediaTek),
            2 => Some(Self::Snapdragon),
            3 => Some(Self::Exynos),
            4 => Some(Self::Unisoc),
            5 => Some(Self::Tensor),
            6 => Some(Self::Tegra),
            _ => None,
        }
    }
}

/// Inputs for install-time `SoC` detection, mirroring
/// `module/customize.sh`: extra sysfs checks first, then
/// the device-tree model, then `getprop` values, then
/// `/proc/cpuinfo` hardware and model-name lines.
#[derive(Debug, Clone, Default)]
pub struct SocDetection {
    /// Paths that exist on the device, used for the extra
    /// checks: `/sys/class/kgsl/kgsl-3d0/devfreq`,
    /// `/sys/devices/platform/kgsl-2d0.0/kgsl`,
    /// `/sys/kernel/ged/hal`, and `/sys/kernel/tegra_gpu`.
    pub existing_paths: Vec<String>,
    /// Device-tree model string, if any.
    pub model: String,
    /// Values of the `ro.board.platform`, `ro.soc.model`,
    /// and related properties, in order.
    pub soc_properties: Vec<String>,
    /// `/proc/cpuinfo` hardware/processor value, if any.
    pub cpuinfo_hardware: String,
    /// `/proc/cpuinfo` model-name value, if any.
    pub cpuinfo_model: String,
}

/// Detect a `SoC` family from install-time inputs, in the
/// same order as `customize.sh`.
#[must_use]
pub fn detect_soc(input: &SocDetection) -> Soc {
    if input.existing_paths.iter().any(|path| {
        path == "/sys/class/kgsl/kgsl-3d0/devfreq"
            || path == "/sys/devices/platform/kgsl-2d0.0/kgsl"
    }) {
        return Soc::Snapdragon;
    }
    if input
        .existing_paths
        .iter()
        .any(|path| path == "/sys/kernel/ged/hal")
    {
        return Soc::MediaTek;
    }
    if input
        .existing_paths
        .iter()
        .any(|path| path == "/sys/kernel/tegra_gpu")
    {
        return Soc::Tegra;
    }
    for candidate in [
        input.model.as_str(),
        &input.soc_properties.join(" "),
        input.cpuinfo_hardware.as_str(),
        input.cpuinfo_model.as_str(),
    ] {
        if let Some(soc) = recognise_soc_text(candidate) {
            return soc;
        }
    }
    Soc::Unknown
}

/// One `case` arm of `recognize_soc`, in script order,
/// on a lowercased input.
fn recognise_soc_text(text: &str) -> Option<Soc> {
    let lower = text.to_lowercase();
    if lower.contains("mt") {
        return Some(Soc::MediaTek);
    }
    if lower.contains("sm") || lower.contains("qcom") || lower.contains("qualcomm") {
        return Some(Soc::Snapdragon);
    }
    if lower.contains("exynos")
        || lower.contains("universal")
        || lower.contains("samsung")
        || lower.contains("erd")
        || lower.contains("s5e")
    {
        return Some(Soc::Exynos);
    }
    if lower.contains("unisoc")
        || lower.contains("ums")
        || lower.contains("sp")
        || lower.contains("sc")
    {
        return Some(Soc::Unisoc);
    }
    if lower.contains("gs") || lower.contains("tensor") {
        return Some(Soc::Tensor);
    }
    if lower.contains("kirin") {
        return Some(Soc::Kirin);
    }
    None
}

/// A profiler run requested by the C++ daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileRequest {
    /// `encore_profiler perfcommon`, run at daemon start.
    PerfCommon,
    /// `encore_profiler performance` or `performance_lite`.
    Performance {
        /// Whether lite mode is on.
        lite: bool,
    },
    /// `encore_profiler balance`.
    Balance,
    /// `encore_profiler powersave`.
    Powersave,
}

/// Mitigation and governor inputs to a plan, mirroring
/// the `ENCORE_*` environment variables and the config's
/// CPU governor preferences.
#[derive(Debug, Clone)]
pub struct ProfilerOptions {
    /// `ENCORE_DISABLE_DDR_TWEAK` is set.
    pub disable_ddr: bool,
    /// `ENCORE_NO_PERFORMANCE_CPUGOV` is set.
    pub no_performance_gov: bool,
    /// `ENCORE_QCOM_NO_GPU_POWERSAVE` is set.
    pub qcom_no_gpu_powersave: bool,
    /// Config balanced governor, the script's
    /// `ENCORE_BALANCED_CPUGOV`.
    pub balanced_gov: String,
    /// Config powersave governor, the script's
    /// `ENCORE_POWERSAVE_CPUGOV`.
    pub powersave_gov: String,
}

impl Default for ProfilerOptions {
    fn default() -> Self {
        Self {
            disable_ddr: false,
            no_performance_gov: false,
            qcom_no_gpu_powersave: false,
            balanced_gov: String::from("schedutil"),
            powersave_gov: String::from("schedutil"),
        }
    }
}

/// One planned write, in application order.
pub type PlannedWrite = (String, String);

fn push(writes: &mut Vec<PlannedWrite>, path: &str, value: &str) {
    writes.push((path.to_owned(), value.to_owned()));
}

fn push_if_exists(writes: &mut Vec<PlannedWrite>, root: &SysfsRoot, path: &str, value: &str) {
    if root.exists(path) {
        push(writes, path, value);
    }
}

/// Parse a whitespace-separated frequency list, as in
/// `scaling_available_frequencies`, ignoring non-numeric
/// words the way the script's numeric sort would.
#[must_use]
pub fn parse_frequencies(text: &str) -> Vec<u64> {
    text.split_whitespace()
        .filter_map(|word| word.parse::<u64>().ok())
        .collect()
}

/// The maximum frequency, as `which_maxfreq` computes it.
#[must_use]
pub fn max_frequency(frequencies: &[u64]) -> Option<u64> {
    frequencies.iter().max().copied()
}

/// The minimum frequency, as `which_minfreq` computes it.
#[must_use]
pub fn min_frequency(frequencies: &[u64]) -> Option<u64> {
    frequencies.iter().min().copied()
}

/// The middle frequency, as `which_midfreq` computes it:
/// sort descending, take the `((n + 1) / 2)`-th entry.
#[must_use]
pub fn mid_frequency(frequencies: &[u64]) -> Option<u64> {
    if frequencies.is_empty() {
        return None;
    }
    let mut sorted = frequencies.to_vec();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    let index = sorted.len().div_ceil(2);
    sorted.get(index.saturating_sub(1)).copied()
}

/// First `freq = <digits>` value in a `MediaTek` opp dump,
/// as the script's `sed ... | head -n 1` computes it.
fn first_opp_freq(text: &str) -> Option<String> {
    opp_freqs(text).into_iter().next()
}

/// Last `freq = <digits>` value, as `... | tail -n 1`.
fn last_opp_freq(text: &str) -> Option<String> {
    opp_freqs(text).into_iter().last()
}

fn opp_freqs(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.split_once("freq = ") {
            let digits: String = rest.1.chars().take_while(char::is_ascii_digit).collect();
            if !digits.is_empty() {
                found.push(digits);
            }
        }
    }
    found
}

/// Last bracketed index in a `MediaTek` opp table, as
/// `awk -F'[][]' '{print $2}' | tail -n 1` computes it.
fn mtk_min_opp_index(text: &str) -> Option<String> {
    let mut last: Option<String> = None;
    for line in text.lines() {
        let Some(open) = line.find('[') else {
            continue;
        };
        let Some(after_open) = line.get(open + 1..) else {
            continue;
        };
        let Some(close) = after_open.find(']') else {
            continue;
        };
        let Some(value) = after_open.get(..close) else {
            continue;
        };
        if !value.is_empty() {
            last = Some(value.to_owned());
        }
    }
    last
}

/// Build a full plan for a profile, reading frequency
/// tables and existing nodes from `root` where the script
/// reads them at run time.
#[must_use]
pub fn plan(
    root: &SysfsRoot,
    request: ProfileRequest,
    soc: Soc,
    options: &ProfilerOptions,
) -> Vec<PlannedWrite> {
    match request {
        ProfileRequest::PerfCommon => plan_perfcommon(root),
        ProfileRequest::Performance { lite } => plan_performance(root, soc, lite, options),
        ProfileRequest::Balance => plan_balance(root, soc, options),
        ProfileRequest::Powersave => plan_powersave(root, soc, options),
    }
}

/// The `perfcommon` plan: kernel, network, scheduler,
/// memory, I/O, and thermal common tweaks.
fn plan_perfcommon(root: &SysfsRoot) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    for (path, value) in [
        ("/proc/sys/kernel/panic", "0"),
        ("/proc/sys/kernel/panic_on_oops", "0"),
        ("/proc/sys/kernel/panic_on_warn", "0"),
        ("/proc/sys/kernel/softlockup_panic", "0"),
    ] {
        push(&mut writes, path, value);
    }
    for dir in root.glob("/sys/block/*") {
        let base = root.device_path(&dir);
        push(&mut writes, &format!("{base}/queue/iostats"), "0");
        push(&mut writes, &format!("{base}/queue/add_random"), "0");
    }
    if let Some(algo) = pick_tcp_congestion(root) {
        push(
            &mut writes,
            "/proc/sys/net/ipv4/tcp_congestion_control",
            &algo,
        );
    }
    for (path, value) in [
        ("/proc/sys/net/ipv4/tcp_low_latency", "1"),
        ("/proc/sys/net/ipv4/tcp_ecn", "1"),
        ("/proc/sys/net/ipv4/tcp_fastopen", "3"),
        ("/proc/sys/net/ipv4/tcp_sack", "1"),
        ("/proc/sys/net/ipv4/tcp_timestamps", "0"),
        ("/proc/sys/kernel/perf_cpu_time_max_percent", "3"),
        ("/proc/sys/kernel/sched_schedstats", "0"),
        ("/proc/sys/kernel/task_cpustats_enable", "0"),
        ("/proc/sys/kernel/sched_autogroup_enabled", "0"),
        ("/proc/sys/kernel/sched_child_runs_first", "1"),
        ("/proc/sys/kernel/sched_nr_migrate", "32"),
        ("/proc/sys/kernel/sched_migration_cost_ns", "50000"),
        ("/proc/sys/kernel/sched_min_granularity_ns", "1000000"),
        ("/proc/sys/kernel/sched_wakeup_granularity_ns", "1500000"),
        ("/proc/sys/vm/page-cluster", "0"),
        ("/proc/sys/vm/stat_interval", "15"),
        ("/proc/sys/vm/compaction_proactiveness", "0"),
        ("/sys/module/mmc_core/parameters/use_spi_crc", "0"),
        ("/sys/module/opchain/parameters/chain_on", "0"),
        ("/sys/module/cpufreq_bouncing/parameters/enable", "0"),
        ("/proc/task_info/task_sched_info/task_sched_info_enable", "0"),
        ("/proc/oplus_scheduler/sched_assist/sched_assist_enabled", "0"),
        (
            "/proc/sys/kernel/sched_lib_name",
            "libunity.so, libil2cpp.so, libmain.so, libUE4.so, libgodot_android.so, libgdx.so, libgdx-box2d.so, libminecraftpe.so, libLive2DCubismCore.so, libyuzu-android.so, libryujinx.so, libcitra-android.so, libhdr_pro_engine.so, libandroidx.graphics.path.so, libeffect.so",
        ),
        ("/proc/sys/kernel/sched_lib_mask_force", "255"),
    ] {
        push(&mut writes, path, value);
    }
    for dir in root.glob("/sys/class/thermal/thermal_zone*") {
        let base = root.device_path(&dir);
        push(&mut writes, &format!("{base}/policy"), "step_wise");
    }
    writes
}

/// The first available congestion algorithm, in the
/// script's preference order, or `None` when the list
/// cannot be read.
fn pick_tcp_congestion(root: &SysfsRoot) -> Option<String> {
    let available = root.read("/proc/sys/net/ipv4/tcp_available_congestion_control")?;
    let words: Vec<&str> = available.split_whitespace().collect();
    for algo in ["bbr3", "bbr2", "bbrplus", "bbr", "westwood", "cubic"] {
        if words.contains(&algo) {
            return Some(algo.to_owned());
        }
    }
    None
}

/// The battery-saver value for a profile, from the script:
/// a numeric parameter takes `0`/`1`, a letter parameter
/// takes `N`/`Y`.
fn battery_saver_value(root: &SysfsRoot, enabled: bool) -> Option<String> {
    let path = "/sys/module/battery_saver/parameters/enabled";
    if !root.exists(path) {
        return None;
    }
    let content = root.read(path).unwrap_or_default();
    if content.chars().any(|c| c.is_ascii_digit()) {
        Some(if enabled { "1" } else { "0" }.to_owned())
    } else {
        Some(if enabled { "Y" } else { "N" }.to_owned())
    }
}

/// CPU governor writes for every CPU and policy node.
fn governor_writes(root: &SysfsRoot, governor: &str) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    for node in root.glob("/sys/devices/system/cpu/cpu*/cpufreq/scaling_governor") {
        push(&mut writes, &root.device_path(&node), governor);
    }
    for node in root.glob("/sys/devices/system/cpu/cpufreq/policy*/scaling_governor") {
        push(&mut writes, &root.device_path(&node), governor);
    }
    writes
}

/// The touchpanel writes for a profile.
fn touchpanel_writes(game_switch: &str, limit: &str, direction: &str) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    push(
        &mut writes,
        "/proc/touchpanel/game_switch_enable",
        game_switch,
    );
    push(&mut writes, "/proc/touchpanel/oplus_tp_limit_enable", limit);
    push(&mut writes, "/proc/touchpanel/oppo_tp_limit_enable", limit);
    push(
        &mut writes,
        "/proc/touchpanel/oplus_tp_direction",
        direction,
    );
    push(&mut writes, "/proc/touchpanel/oppo_tp_direction", direction);
    writes
}

/// The I/O writes for `mmcblk0`, `mmcblk1`, and `sd*`
/// block devices.
fn io_writes(root: &SysfsRoot, read_ahead: &str, nr_requests: &str) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    for dir in root.glob("/sys/block/*") {
        let name = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        let is_target = matches!(name.as_deref(), Some("mmcblk0" | "mmcblk1"))
            || name.as_deref().is_some_and(|name| name.starts_with("sd"));
        if !is_target {
            continue;
        }
        let base = root.device_path(&dir);
        push(
            &mut writes,
            &format!("{base}/queue/read_ahead_kb"),
            read_ahead,
        );
        push(
            &mut writes,
            &format!("{base}/queue/nr_requests"),
            nr_requests,
        );
    }
    writes
}

/// CPU frequency writes, choosing the PPM path when
/// `/proc/ppm` exists, as the script does.
fn cpu_freq_writes(root: &SysfsRoot, mode: CpuFreqMode, lite: bool) -> Vec<PlannedWrite> {
    if root.exists("/proc/ppm") {
        cpu_ppm_writes(root, mode, lite)
    } else {
        cpu_direct_writes(root, mode, lite)
    }
}

/// CPU frequency modes: pin to maximum, or restore the
/// hardware minimum and maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CpuFreqMode {
    /// Maximum performance, with the lite-mode middle
    /// minimum when `lite` is true.
    Max,
    /// Restore the hardware range.
    Unlock,
}

fn cpu_direct_writes(root: &SysfsRoot, mode: CpuFreqMode, lite: bool) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    for dir in root.glob("/sys/devices/system/cpu/cpu*/cpufreq") {
        let base = root.device_path(&dir);
        let max = read_freq_file(root, &format!("{base}/cpuinfo_max_freq"));
        let min = read_freq_file(root, &format!("{base}/cpuinfo_min_freq"));
        let available = read_available(root, &format!("{base}/scaling_available_frequencies"));
        match mode {
            CpuFreqMode::Max => {
                if let Some(max) = max {
                    push(
                        &mut writes,
                        &format!("{base}/scaling_max_freq"),
                        &max.to_string(),
                    );
                    let low = if lite {
                        mid_frequency(&available)
                    } else {
                        Some(max)
                    };
                    if let Some(low) = low {
                        push(
                            &mut writes,
                            &format!("{base}/scaling_min_freq"),
                            &low.to_string(),
                        );
                    }
                }
            }
            CpuFreqMode::Unlock => {
                if let Some(max) = max {
                    push(
                        &mut writes,
                        &format!("{base}/scaling_max_freq"),
                        &max.to_string(),
                    );
                }
                if let Some(min) = min {
                    push(
                        &mut writes,
                        &format!("{base}/scaling_min_freq"),
                        &min.to_string(),
                    );
                }
            }
        }
    }
    writes
}

fn cpu_ppm_writes(root: &SysfsRoot, mode: CpuFreqMode, lite: bool) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    for (cluster, dir) in root
        .glob("/sys/devices/system/cpu/cpufreq/policy*")
        .into_iter()
        .enumerate()
    {
        let base = root.device_path(&dir);
        let max = read_freq_file(root, &format!("{base}/cpuinfo_max_freq"));
        let min = read_freq_file(root, &format!("{base}/cpuinfo_min_freq"));
        let available = read_available(root, &format!("{base}/scaling_available_frequencies"));
        match mode {
            CpuFreqMode::Max => {
                if let Some(max) = max {
                    push(
                        &mut writes,
                        "/proc/ppm/policy/hard_userlimit_max_cpu_freq",
                        &format!("{cluster} {max}"),
                    );
                    let low = if lite {
                        mid_frequency(&available)
                    } else {
                        Some(max)
                    };
                    if let Some(low) = low {
                        push(
                            &mut writes,
                            "/proc/ppm/policy/hard_userlimit_min_cpu_freq",
                            &format!("{cluster} {low}"),
                        );
                    }
                }
            }
            CpuFreqMode::Unlock => {
                if let Some(max) = max {
                    push(
                        &mut writes,
                        "/proc/ppm/policy/hard_userlimit_max_cpu_freq",
                        &format!("{cluster} {max}"),
                    );
                }
                if let Some(min) = min {
                    push(
                        &mut writes,
                        "/proc/ppm/policy/hard_userlimit_min_cpu_freq",
                        &format!("{cluster} {min}"),
                    );
                }
            }
        }
    }
    writes
}

fn read_freq_file(root: &SysfsRoot, path: &str) -> Option<u64> {
    let text = root.read(path)?;
    text.trim().parse::<u64>().ok()
}

fn read_available(root: &SysfsRoot, path: &str) -> Vec<u64> {
    root.read(path)
        .map_or_else(Vec::new, |text| parse_frequencies(&text))
}

/// Devfreq modes, matching the script's helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DevfreqMode {
    /// Maximum to both ends.
    Max,
    /// Maximum to the top, middle to the bottom.
    Mid,
    /// Minimum to both ends.
    Min,
    /// Hardware range to both ends.
    Unlock,
}

/// Devfreq writes for one node, using `max_freq`/
/// `min_freq`, or the Qualcomm `hw_` variants.
fn devfreq_writes(
    root: &SysfsRoot,
    node: &str,
    mode: DevfreqMode,
    use_hw: bool,
) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    let available = read_available(root, &format!("{node}/available_frequencies"));
    let (top, bottom) = match mode {
        DevfreqMode::Max => (max_frequency(&available), max_frequency(&available)),
        DevfreqMode::Mid => (max_frequency(&available), mid_frequency(&available)),
        DevfreqMode::Min => (min_frequency(&available), min_frequency(&available)),
        DevfreqMode::Unlock => (max_frequency(&available), min_frequency(&available)),
    };
    let (top_name, bottom_name) = if use_hw {
        ("hw_max_freq", "hw_min_freq")
    } else {
        ("max_freq", "min_freq")
    };
    if let Some(top) = top {
        push(&mut writes, &format!("{node}/{top_name}"), &top.to_string());
    }
    if let Some(bottom) = bottom {
        push(
            &mut writes,
            &format!("{node}/{bottom_name}"),
            &bottom.to_string(),
        );
    }
    writes
}

/// Devfreq writes for every node matching a pattern.
fn devfreq_glob_writes(
    root: &SysfsRoot,
    pattern: &str,
    mode: DevfreqMode,
    use_hw: bool,
) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    for node in root.glob(pattern) {
        let node_path = root.device_path(&node);
        writes.extend(devfreq_writes(root, &node_path, mode, use_hw));
    }
    writes
}

/// The first Mali node under `/sys/devices/platform`,
/// as the script's `find ... -iname "*.mali" -print -quit`.
fn first_mali_node(root: &SysfsRoot) -> Option<String> {
    let mut nodes = root.glob("/sys/devices/platform/*.mali");
    nodes.sort();
    nodes.into_iter().next().map(|node| root.device_path(&node))
}

/// The Mali power policy for a normal profile: `adaptive`
/// when the current policy mentions it, else
/// `coarse_demand`, as the script's `grep -q` does.
fn mali_normal_policy(root: &SysfsRoot, node: &str) -> String {
    let path = format!("{node}/power_policy");
    let current = root.read(&path).unwrap_or_default();
    if current.contains("adaptive") {
        "adaptive".to_owned()
    } else {
        "coarse_demand".to_owned()
    }
}

/// The `MediaTek` PPM policy-status toggle, for `0` in
/// performance and `1` in normal, from the script's grep
/// and `${row:1:1}` logic.
fn ppm_policy_status_writes(root: &SysfsRoot, value: &str) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    let Some(text) = root.read("/proc/ppm/policy_status") else {
        return writes;
    };
    for line in text.lines() {
        if !line.contains("PWR_THRO") && !line.contains("THERMAL") {
            continue;
        }
        let Some(cluster) = line.chars().nth(1) else {
            continue;
        };
        push(
            &mut writes,
            "/proc/ppm/policy_status",
            &format!("{cluster} {value}"),
        );
    }
    writes
}

/// The performance plan: common performance tweaks, CPU
/// frequencies, I/O, and the `SoC`-specific section.
fn plan_performance(
    root: &SysfsRoot,
    soc: Soc,
    lite: bool,
    options: &ProfilerOptions,
) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    if let Some(value) = battery_saver_value(root, false) {
        push(
            &mut writes,
            "/sys/module/battery_saver/parameters/enabled",
            &value,
        );
    }
    push(&mut writes, "/proc/sys/kernel/split_lock_mitigate", "0");
    push_if_exists(
        &mut writes,
        root,
        "/sys/kernel/debug/sched_features",
        "NEXT_BUDDY",
    );
    push_if_exists(
        &mut writes,
        root,
        "/sys/kernel/debug/sched_features",
        "NO_TTWU_QUEUE",
    );
    push_if_exists(
        &mut writes,
        root,
        "/dev/stune/top-app/schedtune.prefer_idle",
        "1",
    );
    push_if_exists(&mut writes, root, "/dev/stune/top-app/schedtune.boost", "1");
    writes.extend(touchpanel_writes("1", "0", "1"));
    push(&mut writes, "/proc/sys/vm/vfs_cache_pressure", "80");
    let governor = if !lite && !options.no_performance_gov {
        "performance"
    } else {
        options.balanced_gov.as_str()
    };
    writes.extend(governor_writes(root, governor));
    writes.extend(cpu_freq_writes(root, CpuFreqMode::Max, lite));
    writes.extend(io_writes(root, "32", "32"));
    writes.extend(soc_performance_writes(root, soc, lite, options));
    push(&mut writes, "/proc/sys/vm/drop_caches", "3");
    writes
}

/// The balance plan: common balance tweaks, CPU unlock,
/// I/O, and the `SoC`-specific normal section.
fn plan_balance(root: &SysfsRoot, soc: Soc, options: &ProfilerOptions) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    if let Some(value) = battery_saver_value(root, false) {
        push(
            &mut writes,
            "/sys/module/battery_saver/parameters/enabled",
            &value,
        );
    }
    push(&mut writes, "/proc/sys/kernel/split_lock_mitigate", "1");
    push_if_exists(
        &mut writes,
        root,
        "/sys/kernel/debug/sched_features",
        "NEXT_BUDDY",
    );
    push_if_exists(
        &mut writes,
        root,
        "/sys/kernel/debug/sched_features",
        "TTWU_QUEUE",
    );
    push_if_exists(
        &mut writes,
        root,
        "/dev/stune/top-app/schedtune.prefer_idle",
        "0",
    );
    push_if_exists(&mut writes, root, "/dev/stune/top-app/schedtune.boost", "0");
    writes.extend(touchpanel_writes("0", "1", "0"));
    push(&mut writes, "/proc/sys/vm/vfs_cache_pressure", "120");
    writes.extend(governor_writes(root, options.balanced_gov.as_str()));
    writes.extend(cpu_freq_writes(root, CpuFreqMode::Unlock, false));
    writes.extend(io_writes(root, "128", "64"));
    writes.extend(soc_normal_writes(root, soc, options));
    writes
}

/// The powersave plan: the balance plan, then the
/// powersave governor, stune preference, battery saver,
/// and the `SoC` powersave section, in script order.
fn plan_powersave(root: &SysfsRoot, soc: Soc, options: &ProfilerOptions) -> Vec<PlannedWrite> {
    let mut writes = plan_balance(root, soc, options);
    push_if_exists(
        &mut writes,
        root,
        "/dev/stune/top-app/schedtune.prefer_idle",
        "1",
    );
    if let Some(value) = battery_saver_value(root, true) {
        push(
            &mut writes,
            "/sys/module/battery_saver/parameters/enabled",
            &value,
        );
    }
    writes.extend(governor_writes(root, options.powersave_gov.as_str()));
    writes.extend(soc_powersave_writes(root, soc, options));
    writes
}

/// `SoC`-specific performance writes.
fn soc_performance_writes(
    root: &SysfsRoot,
    soc: Soc,
    lite: bool,
    options: &ProfilerOptions,
) -> Vec<PlannedWrite> {
    match soc {
        Soc::MediaTek => mediatek_performance_writes(root, lite, options),
        Soc::Snapdragon => snapdragon_performance_writes(root, lite, options),
        Soc::Exynos => exynos_performance_writes(root, lite, options),
        Soc::Unisoc => unisoc_performance_writes(root, lite),
        Soc::Tensor => tensor_performance_writes(root, lite, options),
        Soc::Tegra => tegra_performance_writes(root, lite),
        Soc::Kirin | Soc::Unknown => Vec::new(),
    }
}

/// `SoC`-specific normal (balance) writes.
fn soc_normal_writes(root: &SysfsRoot, soc: Soc, options: &ProfilerOptions) -> Vec<PlannedWrite> {
    match soc {
        Soc::MediaTek => mediatek_normal_writes(root, options),
        Soc::Snapdragon => snapdragon_normal_writes(root, options),
        Soc::Exynos => exynos_normal_writes(root, options),
        Soc::Unisoc => unisoc_normal_writes(root),
        Soc::Tensor => tensor_normal_writes(root, options),
        Soc::Tegra => tegra_normal_writes(root),
        Soc::Kirin | Soc::Unknown => Vec::new(),
    }
}

/// `SoC`-specific powersave writes.
fn soc_powersave_writes(
    root: &SysfsRoot,
    soc: Soc,
    options: &ProfilerOptions,
) -> Vec<PlannedWrite> {
    match soc {
        Soc::MediaTek => mediatek_powersave_writes(root),
        Soc::Snapdragon => snapdragon_powersave_writes(root, options),
        Soc::Exynos => exynos_powersave_writes(root),
        Soc::Unisoc => unisoc_powersave_writes(root),
        Soc::Tensor => tensor_powersave_writes(root),
        Soc::Tegra => tegra_powersave_writes(root),
        Soc::Kirin | Soc::Unknown => Vec::new(),
    }
}

fn mediatek_performance_writes(
    root: &SysfsRoot,
    lite: bool,
    options: &ProfilerOptions,
) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    writes.extend(ppm_policy_status_writes(root, "0"));
    for (path, value) in [
        ("/sys/kernel/fpsgo/common/force_onoff", "0"),
        ("/proc/cpufreq/cpufreq_cci_mode", "1"),
        ("/proc/cpufreq/cpufreq_power_mode", "3"),
        (
            "/sys/devices/platform/boot_dramboost/dramboost/dramboost",
            "1",
        ),
        ("/sys/devices/system/cpu/eas/enable", "0"),
        (
            "/sys/module/sspm_v3/holders/ged/parameters/is_GED_KPI_enabled",
            "0",
        ),
        ("/proc/gpufreq/gpufreq_opp_freq", "0"),
        ("/proc/gpufreqv2/fix_target_opp_index", "-1"),
    ] {
        push(&mut writes, path, value);
    }
    if !lite {
        if root.exists("/proc/gpufreqv2") {
            push(&mut writes, "/proc/gpufreqv2/fix_target_opp_index", "0");
        } else if let Some(text) = root.read("/proc/gpufreq/gpufreq_opp_dump") {
            if let Some(freq) = first_opp_freq(&text) {
                push(&mut writes, "/proc/gpufreq/gpufreq_opp_freq", &freq);
            }
        }
    }
    if root.exists("/proc/gpufreq/gpufreq_power_limited") {
        for setting in [
            "ignore_batt_oc",
            "ignore_batt_percent",
            "ignore_low_batt",
            "ignore_thermal_protect",
            "ignore_pbm_limited",
        ] {
            push(
                &mut writes,
                "/proc/gpufreq/gpufreq_power_limited",
                &format!("{setting} 1"),
            );
        }
    }
    if let Some(node) = first_mali_node(root) {
        push(&mut writes, &format!("{node}/power_policy"), "always_on");
    }
    push(
        &mut writes,
        "/proc/mtk_batoc_throttling/battery_oc_protect_stop",
        "stop 1",
    );
    if !options.disable_ddr {
        push(
            &mut writes,
            "/sys/kernel/helio-dvfsrc/dvfsrc_force_vcore_dvfs_opp",
            "0",
        );
        for dir in root.glob("/sys/devices/platform/*.dvfsrc") {
            let base = root.device_path(&dir);
            push(
                &mut writes,
                &format!("{base}/helio-dvfsrc/dvfsrc_req_ddr_opp"),
                "0",
            );
        }
        let mode = if lite {
            DevfreqMode::Mid
        } else {
            DevfreqMode::Max
        };
        writes.extend(devfreq_writes(
            root,
            "/sys/class/devfreq/mtk-dvfsrc-devfreq",
            mode,
            false,
        ));
    }
    push(&mut writes, "/sys/kernel/eara_thermal/enable", "0");
    writes
}

fn snapdragon_performance_writes(
    root: &SysfsRoot,
    lite: bool,
    options: &ProfilerOptions,
) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    if !options.disable_ddr {
        let mode = if lite {
            DevfreqMode::Mid
        } else {
            DevfreqMode::Max
        };
        for pattern in [
            "/sys/class/devfreq/*memlat*",
            "/sys/class/devfreq/*latfloor*",
            "/sys/class/devfreq/*ddr-lat*",
        ] {
            writes.extend(devfreq_glob_writes(root, pattern, mode, false));
        }
        for component in ["DDR", "LLCC", "L3"] {
            let node = format!("/sys/devices/system/cpu/bus_dcvs/{component}");
            writes.extend(devfreq_writes(root, &node, mode, true));
        }
    }
    let gpu_node = "/sys/class/kgsl/kgsl-3d0/devfreq";
    if lite {
        writes.extend(devfreq_writes(root, gpu_node, DevfreqMode::Unlock, false));
        writes.extend(kgsl_pwrlevel_writes(root, false));
    } else {
        writes.extend(devfreq_writes(root, gpu_node, DevfreqMode::Max, false));
        writes.extend(kgsl_pwrlevel_writes(root, true));
    }
    push(&mut writes, "/sys/class/kgsl/kgsl-3d0/bus_split", "0");
    push(&mut writes, "/sys/class/kgsl/kgsl-3d0/force_clk_on", "1");
    writes
}

/// Snapdragon KGSL power levels, mode 1 for performance
/// and mode 0 for the default, as in the script.
fn kgsl_pwrlevel_writes(root: &SysfsRoot, performance: bool) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    if performance {
        push(&mut writes, "/sys/class/kgsl/kgsl-3d0/min_pwrlevel", "0");
        push(&mut writes, "/sys/class/kgsl/kgsl-3d0/max_pwrlevel", "0");
    } else if let Some(levels) = root.read("/sys/class/kgsl/kgsl-3d0/num_pwrlevels") {
        let levels = levels.trim();
        push(&mut writes, "/sys/class/kgsl/kgsl-3d0/min_pwrlevel", levels);
        push(&mut writes, "/sys/class/kgsl/kgsl-3d0/max_pwrlevel", "0");
    }
    writes
}

fn tegra_performance_writes(root: &SysfsRoot, lite: bool) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    let node = "/sys/kernel/tegra_gpu";
    if !root.exists(node) {
        return writes;
    }
    let available = read_available(root, &format!("{node}/available_frequencies"));
    if let Some(max) = max_frequency(&available) {
        push(
            &mut writes,
            &format!("{node}/gpu_cap_rate"),
            &max.to_string(),
        );
        let floor = if lite {
            min_frequency(&available)
        } else {
            Some(max)
        };
        if let Some(floor) = floor {
            push(
                &mut writes,
                &format!("{node}/gpu_floor_rate"),
                &floor.to_string(),
            );
        }
    }
    writes
}

fn exynos_gpu_writes(
    root: &SysfsRoot,
    max_value: Option<u64>,
    min_value: Option<u64>,
) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    let node = "/sys/kernel/gpu";
    if !root.exists(node) {
        return writes;
    }
    if let Some(max) = max_value {
        push(
            &mut writes,
            &format!("{node}/gpu_max_clock"),
            &max.to_string(),
        );
    }
    if let Some(min) = min_value {
        push(
            &mut writes,
            &format!("{node}/gpu_min_clock"),
            &min.to_string(),
        );
    }
    writes
}

fn exynos_gpu_freq_node(root: &SysfsRoot) -> Option<Vec<u64>> {
    for candidate in [
        "/sys/kernel/gpu/gpu_available_frequencies",
        "/sys/kernel/gpu/gpu_freq_table",
    ] {
        if root.exists(candidate) {
            return Some(read_available(root, candidate));
        }
    }
    None
}

fn exynos_performance_writes(
    root: &SysfsRoot,
    lite: bool,
    options: &ProfilerOptions,
) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    if let Some(available) = exynos_gpu_freq_node(root) {
        let max = max_frequency(&available);
        let min = if lite { min_frequency(&available) } else { max };
        writes.extend(exynos_gpu_writes(root, max, min));
    }
    if let Some(node) = first_mali_node(root) {
        push(&mut writes, &format!("{node}/power_policy"), "always_on");
    }
    if !options.disable_ddr {
        let mode = if lite {
            DevfreqMode::Mid
        } else {
            DevfreqMode::Max
        };
        writes.extend(devfreq_glob_writes(
            root,
            "/sys/class/devfreq/*devfreq_mif*",
            mode,
            false,
        ));
    }
    writes
}

fn unisoc_gpu_node(root: &SysfsRoot) -> Option<String> {
    let mut nodes = root.glob("/sys/class/devfreq/*.gpu");
    nodes.sort();
    nodes.into_iter().next().map(|node| root.device_path(&node))
}

fn unisoc_performance_writes(root: &SysfsRoot, lite: bool) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    if let Some(node) = unisoc_gpu_node(root) {
        let mode = if lite {
            DevfreqMode::Unlock
        } else {
            DevfreqMode::Max
        };
        writes.extend(devfreq_writes(root, &node, mode, false));
    }
    writes
}

fn tensor_mali_node(root: &SysfsRoot) -> Option<String> {
    let mut nodes = root.glob("/sys/devices/platform/*.mali");
    nodes.sort();
    nodes.into_iter().next().map(|node| root.device_path(&node))
}

fn tensor_performance_writes(
    root: &SysfsRoot,
    lite: bool,
    options: &ProfilerOptions,
) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    if let Some(node) = tensor_mali_node(root) {
        let available = read_available(root, &format!("{node}/available_frequencies"));
        if let Some(max) = max_frequency(&available) {
            push(
                &mut writes,
                &format!("{node}/scaling_max_freq"),
                &max.to_string(),
            );
            let min = if lite {
                min_frequency(&available)
            } else {
                Some(max)
            };
            if let Some(min) = min {
                push(
                    &mut writes,
                    &format!("{node}/scaling_min_freq"),
                    &min.to_string(),
                );
            }
        }
        push(&mut writes, &format!("{node}/power_policy"), "always_on");
    }
    if !options.disable_ddr {
        let mode = if lite {
            DevfreqMode::Mid
        } else {
            DevfreqMode::Max
        };
        writes.extend(devfreq_glob_writes(
            root,
            "/sys/class/devfreq/*devfreq_mif*",
            mode,
            false,
        ));
    }
    writes
}

fn mediatek_normal_writes(root: &SysfsRoot, options: &ProfilerOptions) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    writes.extend(ppm_policy_status_writes(root, "1"));
    for (path, value) in [
        ("/sys/kernel/fpsgo/common/force_onoff", "2"),
        ("/proc/cpufreq/cpufreq_cci_mode", "0"),
        ("/proc/cpufreq/cpufreq_power_mode", "0"),
        (
            "/sys/devices/platform/boot_dramboost/dramboost/dramboost",
            "0",
        ),
        ("/sys/devices/system/cpu/eas/enable", "2"),
        (
            "/sys/module/sspm_v3/holders/ged/parameters/is_GED_KPI_enabled",
            "1",
        ),
        ("/proc/gpufreq/gpufreq_opp_freq", "0"),
        ("/proc/gpufreqv2/fix_target_opp_index", "-1"),
    ] {
        push(&mut writes, path, value);
    }
    let min_opp = if root.exists("/proc/gpufreqv2") {
        root.read("/proc/gpufreqv2/gpu_working_opp_table")
            .and_then(|text| mtk_min_opp_index(&text))
    } else {
        root.read("/proc/gpufreq/gpufreq_opp_dump")
            .and_then(|text| mtk_min_opp_index(&text))
    };
    if let Some(min_opp) = min_opp {
        push(
            &mut writes,
            "/sys/kernel/ged/hal/custom_boost_gpu_freq",
            &min_opp,
        );
    }
    if root.exists("/proc/gpufreq/gpufreq_power_limited") {
        for setting in [
            "ignore_batt_oc",
            "ignore_batt_percent",
            "ignore_low_batt",
            "ignore_thermal_protect",
            "ignore_pbm_limited",
        ] {
            push(
                &mut writes,
                "/proc/gpufreq/gpufreq_power_limited",
                &format!("{setting} 0"),
            );
        }
    }
    if let Some(node) = first_mali_node(root) {
        let policy = mali_normal_policy(root, &node);
        push(&mut writes, &format!("{node}/power_policy"), &policy);
    }
    push(
        &mut writes,
        "/proc/mtk_batoc_throttling/battery_oc_protect_stop",
        "stop 0",
    );
    if !options.disable_ddr {
        for dir in root.glob("/sys/devices/platform/*.dvfsrc") {
            let base = root.device_path(&dir);
            push(
                &mut writes,
                &format!("{base}/helio-dvfsrc/dvfsrc_req_ddr_opp"),
                "-1",
            );
        }
        push(
            &mut writes,
            "/sys/kernel/helio-dvfsrc/dvfsrc_force_vcore_dvfs_opp",
            "-1",
        );
        writes.extend(devfreq_writes(
            root,
            "/sys/class/devfreq/mtk-dvfsrc-devfreq",
            DevfreqMode::Unlock,
            false,
        ));
    }
    push(&mut writes, "/sys/kernel/eara_thermal/enable", "1");
    writes
}

fn snapdragon_normal_writes(root: &SysfsRoot, options: &ProfilerOptions) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    if !options.disable_ddr {
        for pattern in [
            "/sys/class/devfreq/*memlat*",
            "/sys/class/devfreq/*latfloor*",
            "/sys/class/devfreq/*ddr-lat*",
        ] {
            writes.extend(devfreq_glob_writes(
                root,
                pattern,
                DevfreqMode::Unlock,
                false,
            ));
        }
        for component in ["DDR", "LLCC", "L3"] {
            let node = format!("/sys/devices/system/cpu/bus_dcvs/{component}");
            writes.extend(devfreq_writes(root, &node, DevfreqMode::Unlock, true));
        }
    }
    writes.extend(devfreq_writes(
        root,
        "/sys/class/kgsl/kgsl-3d0/devfreq",
        DevfreqMode::Unlock,
        false,
    ));
    writes.extend(kgsl_pwrlevel_writes(root, false));
    push(&mut writes, "/sys/class/kgsl/kgsl-3d0/bus_split", "1");
    push(&mut writes, "/sys/class/kgsl/kgsl-3d0/force_clk_on", "0");
    writes
}

fn tegra_normal_writes(root: &SysfsRoot) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    let node = "/sys/kernel/tegra_gpu";
    if !root.exists(node) {
        return writes;
    }
    let available = read_available(root, &format!("{node}/available_frequencies"));
    if let Some(max) = max_frequency(&available) {
        push(
            &mut writes,
            &format!("{node}/gpu_cap_rate"),
            &max.to_string(),
        );
    }
    if let Some(min) = min_frequency(&available) {
        push(
            &mut writes,
            &format!("{node}/gpu_floor_rate"),
            &min.to_string(),
        );
    }
    writes
}

fn exynos_normal_writes(root: &SysfsRoot, options: &ProfilerOptions) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    if let Some(available) = exynos_gpu_freq_node(root) {
        writes.extend(exynos_gpu_writes(
            root,
            max_frequency(&available),
            min_frequency(&available),
        ));
    }
    if let Some(node) = first_mali_node(root) {
        let policy = mali_normal_policy(root, &node);
        push(&mut writes, &format!("{node}/power_policy"), &policy);
    }
    if !options.disable_ddr {
        writes.extend(devfreq_glob_writes(
            root,
            "/sys/class/devfreq/*devfreq_mif*",
            DevfreqMode::Unlock,
            false,
        ));
    }
    writes
}

fn unisoc_normal_writes(root: &SysfsRoot) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    if let Some(node) = unisoc_gpu_node(root) {
        writes.extend(devfreq_writes(root, &node, DevfreqMode::Unlock, false));
    }
    writes
}

fn tensor_normal_writes(root: &SysfsRoot, options: &ProfilerOptions) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    if let Some(node) = tensor_mali_node(root) {
        let available = read_available(root, &format!("{node}/available_frequencies"));
        if let Some(max) = max_frequency(&available) {
            push(
                &mut writes,
                &format!("{node}/scaling_max_freq"),
                &max.to_string(),
            );
        }
        if let Some(min) = min_frequency(&available) {
            push(
                &mut writes,
                &format!("{node}/scaling_min_freq"),
                &min.to_string(),
            );
        }
        let policy = mali_normal_policy(root, &node);
        push(&mut writes, &format!("{node}/power_policy"), &policy);
    }
    if !options.disable_ddr {
        writes.extend(devfreq_glob_writes(
            root,
            "/sys/class/devfreq/*devfreq_mif*",
            DevfreqMode::Unlock,
            false,
        ));
    }
    writes
}

fn mediatek_powersave_writes(root: &SysfsRoot) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    push(&mut writes, "/proc/cpufreq/cpufreq_power_mode", "1");
    if root.exists("/proc/gpufreqv2") {
        if let Some(text) = root.read("/proc/gpufreqv2/gpu_working_opp_table") {
            if let Some(index) = mtk_min_opp_index(&text) {
                push(&mut writes, "/proc/gpufreqv2/fix_target_opp_index", &index);
            }
        }
    } else if let Some(text) = root.read("/proc/gpufreq/gpufreq_opp_dump") {
        if let Some(freq) = last_opp_freq(&text) {
            push(&mut writes, "/proc/gpufreq/gpufreq_opp_freq", &freq);
        }
    }
    if let Some(node) = first_mali_node(root) {
        push(
            &mut writes,
            &format!("{node}/power_policy"),
            "coarse_demand",
        );
    }
    writes
}

fn snapdragon_powersave_writes(root: &SysfsRoot, options: &ProfilerOptions) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    if !options.qcom_no_gpu_powersave {
        writes.extend(devfreq_writes(
            root,
            "/sys/class/kgsl/kgsl-3d0/devfreq",
            DevfreqMode::Min,
            false,
        ));
    }
    writes
}

fn exynos_powersave_writes(root: &SysfsRoot) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    if let Some(available) = exynos_gpu_freq_node(root) {
        let min = min_frequency(&available);
        writes.extend(exynos_gpu_writes(root, min, min));
    }
    if let Some(node) = first_mali_node(root) {
        push(
            &mut writes,
            &format!("{node}/power_policy"),
            "coarse_demand",
        );
    }
    writes
}

fn unisoc_powersave_writes(root: &SysfsRoot) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    if let Some(node) = unisoc_gpu_node(root) {
        writes.extend(devfreq_writes(root, &node, DevfreqMode::Min, false));
    }
    writes
}

fn tensor_powersave_writes(root: &SysfsRoot) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    if let Some(node) = tensor_mali_node(root) {
        let available = read_available(root, &format!("{node}/available_frequencies"));
        if let Some(min) = min_frequency(&available) {
            push(
                &mut writes,
                &format!("{node}/scaling_min_freq"),
                &min.to_string(),
            );
            push(
                &mut writes,
                &format!("{node}/scaling_max_freq"),
                &min.to_string(),
            );
        }
        push(
            &mut writes,
            &format!("{node}/power_policy"),
            "coarse_demand",
        );
    }
    writes
}

fn tegra_powersave_writes(root: &SysfsRoot) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    let node = "/sys/kernel/tegra_gpu";
    if !root.exists(node) {
        return writes;
    }
    let available = read_available(root, &format!("{node}/available_frequencies"));
    if let Some(min) = min_frequency(&available) {
        push(
            &mut writes,
            &format!("{node}/gpu_floor_rate"),
            &min.to_string(),
        );
        push(
            &mut writes,
            &format!("{node}/gpu_cap_rate"),
            &min.to_string(),
        );
    }
    writes
}

#[cfg(test)]
mod tests {
    use super::{
        detect_soc, max_frequency, mid_frequency, min_frequency, parse_frequencies, plan,
        ProfileRequest, ProfilerOptions, Soc, SocDetection,
    };
    use crate::sysfs::{apply_plan, SysfsRoot};
    use std::fs;

    fn temp_root(name: &str) -> (SysfsRoot, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("encore-profiler-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(fs::create_dir_all(&dir).is_ok());
        (SysfsRoot::under(&dir), dir)
    }

    fn write_file(root: &SysfsRoot, path: &str, content: &str) {
        let resolved = root.resolve(path);
        let parent = resolved
            .parent()
            .map_or_else(|| root.resolve("/"), std::path::Path::to_path_buf);
        assert!(fs::create_dir_all(&parent).is_ok());
        assert!(fs::write(&resolved, content).is_ok());
    }

    #[test]
    fn frequency_selection_matches_shell_helpers() {
        let freqs = parse_frequencies("100 300 200");
        assert_eq!(max_frequency(&freqs), Some(300));
        assert_eq!(min_frequency(&freqs), Some(100));
        assert_eq!(mid_frequency(&freqs), Some(200));
        let even = parse_frequencies("100 200 300 400");
        assert_eq!(mid_frequency(&even), Some(300));
        assert_eq!(mid_frequency(&[]), None);
    }

    #[test]
    fn soc_detection_follows_install_script_order() {
        let mut input = SocDetection {
            existing_paths: vec!["/sys/class/kgsl/kgsl-3d0/devfreq".to_owned()],
            ..SocDetection::default()
        };
        assert_eq!(detect_soc(&input), Soc::Snapdragon);
        input = SocDetection {
            existing_paths: vec!["/sys/kernel/ged/hal".to_owned()],
            ..SocDetection::default()
        };
        assert_eq!(detect_soc(&input), Soc::MediaTek);
        input = SocDetection {
            model: "SM-G990".to_owned(),
            ..SocDetection::default()
        };
        assert_eq!(detect_soc(&input), Soc::Snapdragon);
        input = SocDetection {
            model: "unknown-device".to_owned(),
            ..SocDetection::default()
        };
        assert_eq!(detect_soc(&input), Soc::Unknown);
        assert_eq!(Soc::from_code(6), Some(Soc::Tegra));
        assert_eq!(Soc::from_code(7), None);
    }

    #[test]
    fn perfcommon_plan_contains_kernel_and_network_constants() {
        let (root, dir) = temp_root("perfcommon");
        write_file(
            &root,
            "/proc/sys/net/ipv4/tcp_available_congestion_control",
            "cubic bbr\n",
        );
        let writes = plan(
            &root,
            ProfileRequest::PerfCommon,
            Soc::Unknown,
            &ProfilerOptions::default(),
        );
        assert!(writes.contains(&("/proc/sys/kernel/panic".to_owned(), "0".to_owned())));
        assert!(writes.contains(&(
            "/proc/sys/net/ipv4/tcp_congestion_control".to_owned(),
            "bbr".to_owned()
        )));
        assert!(writes.contains(&(
            "/proc/sys/kernel/sched_lib_mask_force".to_owned(),
            "255".to_owned()
        )));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapdragon_performance_plan_applies_to_fake_sysfs() {
        let (root, dir) = temp_root("snapdragon");
        write_file(
            &root,
            "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor",
            "schedutil\n",
        );
        write_file(
            &root,
            "/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq",
            "2000000\n",
        );
        write_file(
            &root,
            "/sys/devices/system/cpu/cpu0/cpufreq/scaling_available_frequencies",
            "1000000 2000000\n",
        );
        write_file(
            &root,
            "/sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq",
            "1000000\n",
        );
        write_file(
            &root,
            "/sys/class/kgsl/kgsl-3d0/devfreq/available_frequencies",
            "100 200\n",
        );
        write_file(&root, "/sys/class/kgsl/kgsl-3d0/devfreq/max_freq", "100\n");
        write_file(&root, "/sys/class/kgsl/kgsl-3d0/devfreq/min_freq", "100\n");
        write_file(&root, "/sys/class/kgsl/kgsl-3d0/num_pwrlevels", "4\n");
        let options = ProfilerOptions::default();
        let writes = plan(
            &root,
            ProfileRequest::Performance { lite: false },
            Soc::Snapdragon,
            &options,
        );
        assert!(writes.contains(&(
            "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor".to_owned(),
            "performance".to_owned()
        )));
        assert!(writes.contains(&(
            "/sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq".to_owned(),
            "2000000".to_owned()
        )));
        assert!(writes.contains(&(
            "/sys/class/kgsl/kgsl-3d0/devfreq/max_freq".to_owned(),
            "200".to_owned()
        )));
        let summary = apply_plan(&root, &writes);
        assert_eq!(summary.rejected, 0);
        assert!(summary.applied >= 3);
        assert_eq!(
            fs::read_to_string(
                root.resolve("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
            )
            .unwrap_or_default(),
            "performance\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn powersave_plan_is_balance_plus_powersave_overrides() {
        let (root, dir) = temp_root("powersave");
        write_file(
            &root,
            "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor",
            "schedutil\n",
        );
        let options = ProfilerOptions {
            powersave_gov: "powersave".to_owned(),
            ..ProfilerOptions::default()
        };
        let writes = plan(&root, ProfileRequest::Powersave, Soc::Unknown, &options);
        assert!(writes.contains(&(
            "/proc/sys/vm/vfs_cache_pressure".to_owned(),
            "120".to_owned()
        )));
        assert!(writes.contains(&(
            "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor".to_owned(),
            "powersave".to_owned()
        )));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn lite_performance_uses_middle_minimum_frequency() {
        let (root, dir) = temp_root("lite");
        write_file(
            &root,
            "/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq",
            "3000000\n",
        );
        write_file(
            &root,
            "/sys/devices/system/cpu/cpu0/cpufreq/scaling_available_frequencies",
            "1000000 2000000 3000000\n",
        );
        let writes = plan(
            &root,
            ProfileRequest::Performance { lite: true },
            Soc::Unknown,
            &ProfilerOptions::default(),
        );
        assert!(writes.contains(&(
            "/sys/devices/system/cpu/cpu0/cpufreq/scaling_min_freq".to_owned(),
            "2000000".to_owned()
        )));
        let _ = fs::remove_dir_all(&dir);
    }
}
