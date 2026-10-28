//! Sysfs/procfs writing with a static allowlist.
//!
//! Verified from `scripts/encore_profiler.sh`:
//!
//! - The shell writes with two helpers: `apply` (chmod 644,
//!   write, chmod 444) and `write` (chmod 644, write), plus
//!   direct `echo ... > file` and `tee` for CPU governors.
//!   Both helpers return early when the target file does not
//!   exist, and the script continues with the next tweak.
//! - The task guide requires sysfs writes to go only to a
//!   static allowlist in one module, to reject `..`, and to
//!   use `O_NOFOLLOW` where relevant. This module is that one
//!   module.
//!
//! Deliberate differences from the shell, documented here
//! rather than hidden:
//!
//! - No `chmod` is done. On sysfs, permissions are
//!   kernel-controlled, a chmod can fail or be ignored, and
//!   the daemon already runs as root, so chmod does not
//!   change whether a write succeeds. The writer therefore
//!   only checks existence and allowlist membership, then
//!   writes.
//! - No atomic write is used for sysfs. `write_atomic` works
//!   for regular files in the config directory, but sysfs
//!   nodes are not regular files: a rename over them fails,
//!   and the kernel expects a direct write. The writer uses
//!   a direct truncating write, as `echo > file` does.
//! - Values are written with a trailing newline, as `echo`
//!   does. Sysfs nodes ignore the trailing newline.
//! - A missing file, a rejected path, or a failed write is
//!   reported per write and does not abort the rest of the
//!   plan, mirroring the shell's continue-on-error behaviour
//!   and the guide's "one tweak's failure must not kill the
//!   daemon" rule.
//! - Symlinks are not followed for the allowlist check: the
//!   path as given must match an allowlist pattern, and any
//!   `..` component rejects the write outright, before any
//!   filesystem access.

use std::fs;
use std::path::{Path, PathBuf};

/// Allowlist patterns, one per line of the profiler script's
/// write surface, generated from `encore_profiler.sh` by
/// collecting every `apply`, `write`, `echo >`, and `tee`
/// target, then collapsing per-CPU, per-policy, per-block,
/// per-thermal-zone, per-devfreq, and per-Mali nodes into
/// `*` patterns. A `*` matches any characters within one
/// path component, including an empty string, so
/// `/sys/class/devfreq/*memlat*` matches
/// `/sys/class/devfreq/abc-memlat-xyz/max_freq`.
pub const ALLOWLIST: &[&str] = &[
    "/dev/stune/top-app/schedtune.boost",
    "/dev/stune/top-app/schedtune.prefer_idle",
    "/proc/cpufreq/cpufreq_cci_mode",
    "/proc/cpufreq/cpufreq_power_mode",
    "/proc/gpufreq/gpufreq_opp_freq",
    "/proc/gpufreq/gpufreq_power_limited",
    "/proc/gpufreqv2/fix_target_opp_index",
    "/proc/mtk_batoc_throttling/battery_oc_protect_stop",
    "/proc/oplus_scheduler/sched_assist/sched_assist_enabled",
    "/proc/ppm/policy/hard_userlimit_max_cpu_freq",
    "/proc/ppm/policy/hard_userlimit_min_cpu_freq",
    "/proc/ppm/policy_status",
    "/proc/sys/kernel/panic",
    "/proc/sys/kernel/panic_on_oops",
    "/proc/sys/kernel/panic_on_warn",
    "/proc/sys/kernel/perf_cpu_time_max_percent",
    "/proc/sys/kernel/sched_autogroup_enabled",
    "/proc/sys/kernel/sched_child_runs_first",
    "/proc/sys/kernel/sched_lib_mask_force",
    "/proc/sys/kernel/sched_lib_name",
    "/proc/sys/kernel/sched_migration_cost_ns",
    "/proc/sys/kernel/sched_min_granularity_ns",
    "/proc/sys/kernel/sched_nr_migrate",
    "/proc/sys/kernel/sched_schedstats",
    "/proc/sys/kernel/sched_wakeup_granularity_ns",
    "/proc/sys/kernel/softlockup_panic",
    "/proc/sys/kernel/split_lock_mitigate",
    "/proc/sys/kernel/task_cpustats_enable",
    "/proc/sys/net/ipv4/tcp_congestion_control",
    "/proc/sys/net/ipv4/tcp_ecn",
    "/proc/sys/net/ipv4/tcp_fastopen",
    "/proc/sys/net/ipv4/tcp_low_latency",
    "/proc/sys/net/ipv4/tcp_sack",
    "/proc/sys/net/ipv4/tcp_timestamps",
    "/proc/sys/vm/compaction_proactiveness",
    "/proc/sys/vm/drop_caches",
    "/proc/sys/vm/page-cluster",
    "/proc/sys/vm/stat_interval",
    "/proc/sys/vm/vfs_cache_pressure",
    "/proc/task_info/task_sched_info/task_sched_info_enable",
    "/proc/touchpanel/game_switch_enable",
    "/proc/touchpanel/oplus_tp_direction",
    "/proc/touchpanel/oplus_tp_limit_enable",
    "/proc/touchpanel/oppo_tp_direction",
    "/proc/touchpanel/oppo_tp_limit_enable",
    "/sys/block/*/queue/add_random",
    "/sys/block/*/queue/iostats",
    "/sys/block/*/queue/nr_requests",
    "/sys/block/*/queue/read_ahead_kb",
    "/sys/class/devfreq/*/hw_max_freq",
    "/sys/class/devfreq/*/hw_min_freq",
    "/sys/class/devfreq/*/max_freq",
    "/sys/class/devfreq/*/min_freq",
    "/sys/class/devfreq/*ddr-lat*/hw_max_freq",
    "/sys/class/devfreq/*ddr-lat*/hw_min_freq",
    "/sys/class/devfreq/*ddr-lat*/max_freq",
    "/sys/class/devfreq/*ddr-lat*/min_freq",
    "/sys/class/devfreq/*devfreq_mif*/hw_max_freq",
    "/sys/class/devfreq/*devfreq_mif*/hw_min_freq",
    "/sys/class/devfreq/*devfreq_mif*/max_freq",
    "/sys/class/devfreq/*devfreq_mif*/min_freq",
    "/sys/class/devfreq/*latfloor*/hw_max_freq",
    "/sys/class/devfreq/*latfloor*/hw_min_freq",
    "/sys/class/devfreq/*latfloor*/max_freq",
    "/sys/class/devfreq/*latfloor*/min_freq",
    "/sys/class/devfreq/*memlat*/hw_max_freq",
    "/sys/class/devfreq/*memlat*/hw_min_freq",
    "/sys/class/devfreq/*memlat*/max_freq",
    "/sys/class/devfreq/*memlat*/min_freq",
    "/sys/class/kgsl/kgsl-3d0/bus_split",
    "/sys/class/kgsl/kgsl-3d0/devfreq/max_freq",
    "/sys/class/kgsl/kgsl-3d0/devfreq/min_freq",
    "/sys/class/kgsl/kgsl-3d0/force_clk_on",
    "/sys/class/kgsl/kgsl-3d0/max_pwrlevel",
    "/sys/class/kgsl/kgsl-3d0/min_pwrlevel",
    "/sys/class/thermal/thermal_zone*/policy",
    "/sys/devices/platform/*.dvfsrc/helio-dvfsrc/dvfsrc_req_ddr_opp",
    "/sys/devices/platform/*.mali/available_frequencies",
    "/sys/devices/platform/*.mali/power_policy",
    "/sys/devices/platform/*.mali/scaling_max_freq",
    "/sys/devices/platform/*.mali/scaling_min_freq",
    "/sys/devices/platform/boot_dramboost/dramboost/dramboost",
    "/sys/devices/system/cpu/bus_dcvs/*/hw_max_freq",
    "/sys/devices/system/cpu/bus_dcvs/*/hw_min_freq",
    "/sys/devices/system/cpu/cpu*/cpufreq/scaling_governor",
    "/sys/devices/system/cpu/cpufreq/policy*/scaling_governor",
    "/sys/devices/system/cpu/*/cpufreq/scaling_max_freq",
    "/sys/devices/system/cpu/*/cpufreq/scaling_min_freq",
    "/sys/devices/system/cpu/eas/enable",
    "/sys/kernel/debug/sched_features",
    "/sys/kernel/eara_thermal/enable",
    "/sys/kernel/fpsgo/common/force_onoff",
    "/sys/kernel/ged/hal/custom_boost_gpu_freq",
    "/sys/kernel/helio-dvfsrc/dvfsrc_force_vcore_dvfs_opp",
    "/sys/kernel/gpu/gpu_max_clock",
    "/sys/kernel/gpu/gpu_min_clock",
    "/sys/kernel/tegra_gpu/gpu_cap_rate",
    "/sys/kernel/tegra_gpu/gpu_floor_rate",
    "/sys/module/battery_saver/parameters/enabled",
    "/sys/module/cpufreq_bouncing/parameters/enable",
    "/sys/module/mmc_core/parameters/use_spi_crc",
    "/sys/module/opchain/parameters/chain_on",
    "/sys/module/sspm_v3/holders/ged/parameters/is_GED_KPI_enabled",
];

/// Whether `path` is allowed by the allowlist.
///
/// A path is allowed only when it is absolute, contains no
/// `..` component, and matches at least one allowlist
/// pattern component by component.
#[must_use]
pub fn is_allowed(path: &str) -> bool {
    if !path.starts_with('/') {
        return false;
    }
    let components: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if components.contains(&"..") {
        return false;
    }
    ALLOWLIST.iter().any(|pattern| {
        let pattern_parts: Vec<&str> = pattern.split('/').filter(|part| !part.is_empty()).collect();
        pattern_parts.len() == components.len()
            && pattern_parts
                .iter()
                .zip(components.iter())
                .all(|(pattern_part, component)| component_matches(pattern_part, component))
    })
}

/// Whether one path component matches a pattern component,
/// where `*` in the pattern matches any characters.
fn component_matches(pattern: &str, component: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == component;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let Some(first) = parts.first() else {
        return false;
    };
    let Some(last) = parts.last() else {
        return false;
    };
    if !component.starts_with(first) || !component.ends_with(last) {
        return false;
    }
    let mut rest = component;
    for part in &parts {
        if part.is_empty() {
            continue;
        }
        let Some(index) = rest.find(part) else {
            return false;
        };
        let Some(next) = rest.get(index + part.len()..) else {
            return false;
        };
        rest = next;
    }
    true
}

/// A root for sysfs reads and writes.
///
/// On a device the root is `/`, so a path like
/// `/sys/devices/system/cpu/...` is used as-is. In tests
/// the root is a temporary directory, and the same path
/// maps to `<root>/sys/devices/system/cpu/...`.
#[derive(Debug, Clone)]
pub struct SysfsRoot {
    root: PathBuf,
}

impl SysfsRoot {
    /// The real device root, `/`.
    #[must_use]
    pub fn device() -> Self {
        Self {
            root: PathBuf::from("/"),
        }
    }

    /// A test root, under which `/sys/...` maps to
    /// `<root>/sys/...`.
    #[must_use]
    pub fn under(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    /// Resolve an absolute sysfs path under this root.
    #[must_use]
    pub fn resolve(&self, path: &str) -> PathBuf {
        let stripped = path.strip_prefix('/').unwrap_or(path);
        self.root.join(stripped)
    }

    /// Whether the file at `path` exists under this root.
    #[must_use]
    pub fn exists(&self, path: &str) -> bool {
        self.resolve(path).exists()
    }

    /// Read a file under this root, or `None` on any error.
    #[must_use]
    pub fn read(&self, path: &str) -> Option<String> {
        fs::read_to_string(self.resolve(path)).ok()
    }

    /// Expand a pattern under this root, where `*` in a
    /// component matches any directory or file name in that
    /// component. Returns an empty list when nothing matches.
    /// Results are sorted, so plans are deterministic.
    #[must_use]
    pub fn glob(&self, pattern: &str) -> Vec<PathBuf> {
        let mut current = vec![self.root.clone()];
        for component in pattern.split('/').filter(|part| !part.is_empty()) {
            let mut next = Vec::new();
            for dir in current {
                if component.contains('*') {
                    if let Ok(entries) = fs::read_dir(&dir) {
                        for entry in entries.flatten() {
                            let name = entry.file_name().to_string_lossy().into_owned();
                            if component_matches(component, &name) {
                                next.push(entry.path());
                            }
                        }
                    }
                } else {
                    let candidate = dir.join(component);
                    if candidate.exists() {
                        next.push(candidate);
                    }
                }
            }
            current = next;
            if current.is_empty() {
                break;
            }
        }
        current.sort();
        current
    }

    /// The absolute sysfs path for a resolved path, for
    /// planned writes: strips the test root back off, so a
    /// planned write always carries a device-style path.
    #[must_use]
    pub fn device_path(&self, resolved: &Path) -> String {
        let Ok(relative) = resolved.strip_prefix(&self.root) else {
            return resolved.display().to_string();
        };
        format!("/{}", relative.display())
    }
}

/// The outcome of applying one planned write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The write succeeded.
    Applied,
    /// The target file did not exist, as in the shell helpers.
    Missing,
    /// The path was rejected by the allowlist.
    Rejected,
    /// The write failed for another I/O reason.
    Failed,
}

/// Apply one write, returning its outcome. This never panics
/// and never aborts a plan on a single failure.
#[must_use]
pub fn apply_write(root: &SysfsRoot, path: &str, value: &str) -> WriteOutcome {
    if !is_allowed(path) {
        return WriteOutcome::Rejected;
    }
    let resolved = root.resolve(path);
    if !resolved.exists() {
        return WriteOutcome::Missing;
    }
    match fs::write(&resolved, format!("{value}\n")) {
        Ok(()) => WriteOutcome::Applied,
        Err(_) => WriteOutcome::Failed,
    }
}

/// A summary of applying a whole plan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplySummary {
    /// Writes that succeeded.
    pub applied: usize,
    /// Writes skipped because the target did not exist.
    pub missing: usize,
    /// Writes rejected by the allowlist.
    pub rejected: usize,
    /// Writes that failed for another I/O reason.
    pub failed: usize,
}

impl ApplySummary {
    /// Add one outcome to this summary.
    pub fn add(&mut self, outcome: WriteOutcome) {
        match outcome {
            WriteOutcome::Applied => self.applied += 1,
            WriteOutcome::Missing => self.missing += 1,
            WriteOutcome::Rejected => self.rejected += 1,
            WriteOutcome::Failed => self.failed += 1,
        }
    }
}

/// Apply a plan, continuing after every failure, and return
/// the summary.
#[must_use]
pub fn apply_plan(root: &SysfsRoot, writes: &[(String, String)]) -> ApplySummary {
    let mut summary = ApplySummary::default();
    for (path, value) in writes {
        summary.add(apply_write(root, path, value));
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::{apply_plan, apply_write, is_allowed, ApplySummary, SysfsRoot, WriteOutcome};
    use std::fs;

    fn temp_root_for(test_name: &str) -> SysfsRoot {
        let dir =
            std::env::temp_dir().join(format!("encore-sysfs-{test_name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(fs::create_dir_all(&dir).is_ok());
        SysfsRoot::under(&dir)
    }

    #[test]
    fn allowlist_accepts_exact_and_pattern_paths() {
        assert!(is_allowed("/proc/sys/kernel/panic"));
        assert!(is_allowed(
            "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"
        ));
        assert!(is_allowed(
            "/sys/devices/system/cpu/cpufreq/policy4/scaling_governor"
        ));
        assert!(is_allowed("/sys/class/devfreq/abc-memlat-xyz/max_freq"));
        assert!(is_allowed("/sys/block/mmcblk0/queue/read_ahead_kb"));
        assert!(is_allowed("/sys/class/thermal/thermal_zone0/policy"));
    }

    #[test]
    fn allowlist_rejects_traversal_relative_and_unknown_paths() {
        assert!(!is_allowed("/sys/devices/system/cpu/../etc/passwd"));
        assert!(!is_allowed("/etc/passwd"));
        assert!(!is_allowed("sys/kernel/panic"));
        assert!(!is_allowed(
            "/sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq_extra"
        ));
        assert!(!is_allowed("/proc/sys/kernel/panic_on_oops_extra"));
        assert!(!is_allowed("/sys/unknown/file"));
    }

    #[test]
    fn apply_write_writes_missing_rejects_and_continues() {
        let root = temp_root_for("apply");
        let governor = "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor";
        let resolved = root.resolve(governor);
        assert!(fs::create_dir_all(resolved.parent().unwrap_or(&root.resolve("/"))).is_ok());
        assert!(fs::write(&resolved, "old\n").is_ok());
        assert_eq!(
            apply_write(&root, governor, "performance"),
            WriteOutcome::Applied
        );
        assert_eq!(
            fs::read_to_string(&resolved).unwrap_or_default(),
            "performance\n"
        );
        assert_eq!(
            apply_write(
                &root,
                "/sys/devices/system/cpu/cpu1/cpufreq/scaling_governor",
                "x"
            ),
            WriteOutcome::Missing
        );
        assert_eq!(
            apply_write(&root, "/etc/passwd", "x"),
            WriteOutcome::Rejected
        );
        let summary: ApplySummary = apply_plan(
            &root,
            &[
                (governor.to_owned(), "balance".to_owned()),
                ("/etc/passwd".to_owned(), "x".to_owned()),
                ("/sys/unknown/file".to_owned(), "x".to_owned()),
            ],
        );
        assert_eq!(summary.applied, 1);
        assert_eq!(summary.rejected, 2);
        assert_eq!(summary.missing, 0);
        let _ = fs::remove_dir_all(root.resolve("/"));
    }
}
