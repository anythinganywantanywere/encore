//! Device-mitigation rules, from `jni/DeviceMitigationStore.cpp`.
//!
//! Verified facts:
//!
//! - The file is a JSON object with an optional `default`
//!   object (a string array `items`) and an optional
//!   `device_rules` object. Each rule has a `name`,
//!   `description`, `filter_type` (`all` or `any`),
//!   a string array `items`, and a `filter_condition`
//!   object mapping a device-info key (`soc`, `model`, or
//!   `uname`) to an `operator` (`match`, `contains`, or
//!   `regex`) and a `value`. Non-string items, non-object
//!   rules, and non-object conditions are ignored, as in
//!   C++.
//! - Device info, from `DeviceInfo.cpp`: `soc` is
//!   `/proc/device-tree/model` (first line, NUL-truncated,
//!   whitespace-trimmed, or `"Unknown"`), `model` is the
//!   `ro.product.model` system property (or `"Unknown"`),
//!   and `uname` is the kernel release from `uname(2)`
//!   (or `"Unknown"` on failure).
//! - `get_mitigation_items` re-evaluates device rules
//!   against current device info, and adds the default
//!   items only when `use_device_mitigation` is true.
//!   `get_cached_mitigation_items`, which the profiler
//!   actually uses, returns the items cached at load time
//!   (rules evaluated once during parsing) plus the
//!   default items under the same flag. Both are provided
//!   here, with the cached form used by default.
//! - Filter evaluation, exactly as in C++: an unknown
//!   `filter_type` matches nothing; for `all`, a missing
//!   condition or a failed condition fails the rule, and
//!   all conditions must match; for `any`, the first
//!   matching condition matches the rule, and a rule with
//!   no matching condition fails. An unknown operator
//!   fails that condition, and an invalid regex fails
//!   that condition (logged in C++, returned as a
//!   non-match here).
//! - The shipped default file (`config/device_mitigation.json`)
//!   contains only default items
//!   `DISABLE_DDR_TWEAK`, `NO_PERFORMANCE_CPUGOV`, and
//!   `QCOM_NO_GPU_POWERSAVE`, and no device rules. Those
//!   three names map directly to the profiler options in
//!   [`crate::profiler::ProfilerOptions`], via
//!   [`MitigationItems::to_profiler_flags`]. Other items
//!   are kept in the set, as C++ keeps them as
//!   `ENCORE_<ITEM>` environment variables, but the shell
//!   profiler only reads those three.
//!
//! Deliberate difference: the C++ store loads from a path
//! and holds a mutex; this module parses text and returns
//! an owned value, so it can be tested without a device
//! and without a global lock. The file-backed loading
//! (missing file, parse error, non-object root all fail)
//! is the same shape as Stage 4's `is_json_object` check,
//! extended to full parsing here.

use crate::profiler::ProfilerOptions;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Device info used to evaluate mitigation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    /// `SoC` model string.
    pub soc: String,
    /// Device model string.
    pub model: String,
    /// Kernel release string.
    pub uname: String,
}

impl DeviceInfo {
    /// The value for a condition key, or `None` when the
    /// key is not one of `soc`, `model`, or `uname`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        match key {
            "soc" => Some(&self.soc),
            "model" => Some(&self.model),
            "uname" => Some(&self.uname),
            _ => None,
        }
    }
}

/// One filter condition in a device rule.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Condition {
    /// `match`, `contains`, or `regex`.
    pub op: String,
    /// The expected value or pattern.
    pub value: String,
}

/// One device rule.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceRule {
    /// Rule name, from the rule object itself.
    pub name: String,
    /// Human-readable description, if present.
    pub description: String,
    /// `all` or `any`.
    pub filter_type: String,
    /// Items applied when the rule matches.
    pub items: BTreeSet<String>,
    /// Conditions, keyed by device-info key.
    pub filter_condition: BTreeMap<String, Condition>,
}

/// A parsed mitigation file, with cached items as C++
/// computes them at load time.
#[derive(Debug, Clone, Default)]
pub struct MitigationData {
    /// Default items, added only when the config flag is on.
    pub default_items: BTreeSet<String>,
    /// Device rules, keyed by the JSON member name.
    pub device_rules: BTreeMap<String, DeviceRule>,
    /// Items from rules that matched at load time.
    pub cached_items: BTreeSet<String>,
}

/// The error for a mitigation file that cannot be parsed
/// as a mitigation object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MitigationError {
    /// The text is not valid JSON.
    #[error("mitigation file is not valid JSON")]
    InvalidJson,
    /// The JSON root is not an object.
    #[error("mitigation file root is not an object")]
    RootNotObject,
}

fn string_items(value: &Value) -> BTreeSet<String> {
    let mut items = BTreeSet::new();
    if let Some(array) = value.as_array() {
        for item in array {
            if let Some(text) = item.as_str() {
                items.insert(text.to_owned());
            }
        }
    }
    items
}

/// Parse mitigation text, evaluating rules once against
/// `device_info` to fill the cached items, as C++
/// `parse_config` does.
///
/// # Errors
///
/// Returns [`MitigationError`] when the text is not valid
/// JSON or its root is not an object.
pub fn parse_mitigation(
    text: &str,
    device_info: &DeviceInfo,
) -> Result<MitigationData, MitigationError> {
    let doc: Value = serde_json::from_str(text).map_err(|_| MitigationError::InvalidJson)?;
    let root = doc.as_object().ok_or(MitigationError::RootNotObject)?;
    let mut data = MitigationData::default();
    if let Some(default_obj) = root.get("default").and_then(Value::as_object) {
        if let Some(items) = default_obj.get("items") {
            data.default_items = string_items(items);
        }
    }
    if let Some(rules_obj) = root.get("device_rules").and_then(Value::as_object) {
        for (member_name, rule_value) in rules_obj {
            let Some(rule_obj) = rule_value.as_object() else {
                continue;
            };
            let mut rule = DeviceRule::default();
            if let Some(name) = rule_obj.get("name").and_then(Value::as_str) {
                name.clone_into(&mut rule.name);
            }
            if let Some(description) = rule_obj.get("description").and_then(Value::as_str) {
                description.clone_into(&mut rule.description);
            }
            if let Some(filter_type) = rule_obj.get("filter_type").and_then(Value::as_str) {
                filter_type.clone_into(&mut rule.filter_type);
            }
            if let Some(items) = rule_obj.get("items") {
                rule.items = string_items(items);
            }
            if let Some(cond_obj) = rule_obj.get("filter_condition").and_then(Value::as_object) {
                for (cond_name, cond_value) in cond_obj {
                    let Some(cond_value) = cond_value.as_object() else {
                        continue;
                    };
                    let mut condition = Condition::default();
                    if let Some(op) = cond_value.get("operator").and_then(Value::as_str) {
                        op.clone_into(&mut condition.op);
                    }
                    if let Some(value) = cond_value.get("value").and_then(Value::as_str) {
                        value.clone_into(&mut condition.value);
                    }
                    rule.filter_condition.insert(cond_name.clone(), condition);
                }
            }
            if matches_rule(&rule, device_info) {
                data.cached_items.extend(rule.items.iter().cloned());
            }
            data.device_rules.insert(member_name.clone(), rule);
        }
    }
    Ok(data)
}

/// Whether a rule matches device info, following the
/// C++ `all`/`any` logic exactly.
#[must_use]
pub fn matches_rule(rule: &DeviceRule, device_info: &DeviceInfo) -> bool {
    let is_all = rule.filter_type == "all";
    let is_any = rule.filter_type == "any";
    if !is_all && !is_any {
        return false;
    }
    let mut matched = 0usize;
    let total = rule.filter_condition.len();
    for (key, condition) in &rule.filter_condition {
        let Some(actual) = device_info.get(key) else {
            if is_all {
                return false;
            }
            continue;
        };
        if check_condition(condition, actual) {
            matched += 1;
            if is_any {
                return true;
            }
        } else if is_all {
            return false;
        }
    }
    if is_all {
        matched == total
    } else {
        matched > 0
    }
}

/// Whether one condition matches a device value.
#[must_use]
pub fn check_condition(condition: &Condition, actual: &str) -> bool {
    match condition.op.as_str() {
        "match" => actual == condition.value,
        "contains" => actual.contains(&condition.value),
        "regex" => match regex::Regex::new(&condition.value) {
            Ok(pattern) => pattern.is_match(actual),
            Err(_) => false,
        },
        _ => false,
    }
}

/// A set of mitigation items, with the profiler flags
/// they imply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MitigationItems {
    /// All items in the set.
    pub items: BTreeSet<String>,
}

impl MitigationItems {
    /// The profiler flags implied by the known item names.
    #[must_use]
    pub fn to_profiler_flags(&self) -> (bool, bool, bool) {
        (
            self.items.contains("DISABLE_DDR_TWEAK"),
            self.items.contains("NO_PERFORMANCE_CPUGOV"),
            self.items.contains("QCOM_NO_GPU_POWERSAVE"),
        )
    }

    /// Apply the known flags to profiler options, keeping
    /// the governors and other options unchanged.
    #[must_use]
    pub fn apply_to_options(&self, options: &ProfilerOptions) -> ProfilerOptions {
        let (disable_ddr, no_performance_gov, qcom_no_gpu_powersave) = self.to_profiler_flags();
        ProfilerOptions {
            disable_ddr,
            no_performance_gov,
            qcom_no_gpu_powersave,
            balanced_gov: options.balanced_gov.clone(),
            powersave_gov: options.powersave_gov.clone(),
        }
    }
}

impl MitigationData {
    /// The cached items, as the profiler uses them, plus
    /// default items when `use_device_mitigation` is true.
    #[must_use]
    pub fn cached_mitigation_items(&self, use_device_mitigation: bool) -> MitigationItems {
        let mut items = self.cached_items.clone();
        if use_device_mitigation {
            items.extend(self.default_items.iter().cloned());
        }
        MitigationItems { items }
    }

    /// Re-evaluated items, as `get_mitigation_items` does,
    /// plus default items when the flag is true.
    #[must_use]
    pub fn evaluated_mitigation_items(
        &self,
        device_info: &DeviceInfo,
        use_device_mitigation: bool,
    ) -> MitigationItems {
        let mut items = BTreeSet::new();
        for rule in self.device_rules.values() {
            if matches_rule(rule, device_info) {
                items.extend(rule.items.iter().cloned());
            }
        }
        if use_device_mitigation {
            items.extend(self.default_items.iter().cloned());
        }
        MitigationItems { items }
    }
}

#[cfg(test)]
mod tests {
    use super::{check_condition, parse_mitigation, Condition, DeviceInfo};

    fn info() -> DeviceInfo {
        DeviceInfo {
            soc: "SM8650".to_owned(),
            model: "Pixel 9".to_owned(),
            uname: "6.1.0-android".to_owned(),
        }
    }

    #[test]
    fn default_items_only_with_flag() {
        let text = r#"{"default": {"items": ["DISABLE_DDR_TWEAK"]}, "device_rules": {}}"#;
        let result = parse_mitigation(text, &info());
        assert!(result.is_ok());
        let Ok(data) = result else {
            return;
        };
        assert!(data.cached_mitigation_items(false).items.is_empty());
        assert!(data
            .cached_mitigation_items(true)
            .items
            .contains("DISABLE_DDR_TWEAK"));
        let (disable_ddr, _, _) = data.cached_mitigation_items(true).to_profiler_flags();
        assert!(disable_ddr);
    }

    #[test]
    fn rules_follow_all_any_and_operators() {
        let text = r#"{
            "device_rules": {
                "all_rule": {
                    "name": "all_rule", "filter_type": "all",
                    "items": ["A"],
                    "filter_condition": {
                        "soc": {"operator": "match", "value": "SM8650"},
                        "model": {"operator": "contains", "value": "Pixel"}
                    }
                },
                "any_rule": {
                    "name": "any_rule", "filter_type": "any",
                    "items": ["B"],
                    "filter_condition": {
                        "soc": {"operator": "match", "value": "other"},
                        "uname": {"operator": "regex", "value": "^6\\.1"}
                    }
                },
                "bad_rule": {
                    "name": "bad_rule", "filter_type": "all",
                    "items": ["C"],
                    "filter_condition": {
                        "soc": {"operator": "match", "value": "SM8650"},
                        "missing_key": {"operator": "match", "value": "x"}
                    }
                },
                "unknown_op": {
                    "name": "unknown_op", "filter_type": "any",
                    "items": ["D"],
                    "filter_condition": {
                        "soc": {"operator": "bogus", "value": "SM8650"}
                    }
                }
            }
        }"#;
        let result = parse_mitigation(text, &info());
        assert!(result.is_ok());
        let Ok(data) = result else {
            return;
        };
        let items = data.cached_mitigation_items(false).items;
        assert!(items.contains("A"));
        assert!(items.contains("B"));
        assert!(!items.contains("C"));
        assert!(!items.contains("D"));
        assert!(!check_condition(
            &Condition {
                op: "regex".to_owned(),
                value: "([invalid".to_owned()
            },
            "x"
        ));
    }

    #[test]
    fn parse_rejects_invalid_and_non_object() {
        assert!(parse_mitigation("not json", &info()).is_err());
        assert!(parse_mitigation("[1, 2]", &info()).is_err());
    }
}
