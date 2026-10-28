//! Configuration parsing and rendering, mirroring `EncoreConfigStore`.
//!
//! Verified from `jni/EncoreConfigStore.cpp` and
//! `jni/EncoreConfigStore.hpp`:
//!
//! - Preferences default to `enforce_lite_mode = false`,
//!   `use_device_mitigation = false`, `disable_tweaks = false`,
//!   `log_level = 4`.
//! - A missing or unparsable config file makes the C++ daemon create a
//!   default config; a non-object root returns `false` without changing
//!   the stored config.
//! - `create_default_config` uses the governor read from
//!   `default_cpu_gov`, falling back to `"schedutil"` when that file is
//!   missing or empty.
//! - `parse_config` starts from default preferences, but with empty
//!   CPU governor strings, and only fills fields that are present with
//!   the expected type. A partial config therefore leaves an absent
//!   governor empty, rather than restoring the default governor.
//!   That quirk is preserved here and marked below, instead of being
//!   silently "fixed" in Stage 1.
//!
//! // TODO(verify): confirm with upstream whether an empty governor
//! after a partial config is intentional, or whether Stage 2 should
//! fall back to the default governor in that case.

use serde_json::{json, Value};

/// Fallback CPU governor, as in `EncoreConfigStore::read_default_cpu_governor`.
pub const FALLBACK_CPU_GOVERNOR: &str = "schedutil";

/// User preferences from `config.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preferences {
    /// Force lite mode for all games.
    pub enforce_lite_mode: bool,
    /// Apply device mitigation rules.
    pub use_device_mitigation: bool,
    /// Skip all tweaks.
    pub disable_tweaks: bool,
    /// Log level, as in the C++ default of `4`.
    pub log_level: i32,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            enforce_lite_mode: false,
            use_device_mitigation: false,
            disable_tweaks: false,
            log_level: 4,
        }
    }
}

/// CPU governor choices from `config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CpuGovernor {
    /// Governor used for the balance profile.
    pub balance: String,
    /// Governor used for the powersave profile.
    pub powersave: String,
}

/// The full configuration, as in `EncoreConfigStore::ConfigData`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigData {
    /// User preferences.
    pub preferences: Preferences,
    /// CPU governor choices.
    pub cpu_governor: CpuGovernor,
}

impl ConfigData {
    /// Build the default configuration, as in
    /// `EncoreConfigStore::create_default_config`.
    #[must_use]
    pub fn default_with_governor(default_governor: &str) -> Self {
        Self {
            preferences: Preferences::default(),
            cpu_governor: CpuGovernor {
                balance: default_governor.to_owned(),
                powersave: default_governor.to_owned(),
            },
        }
    }
}

/// An error from configuration parsing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The JSON text could not be parsed.
    #[error("invalid JSON: {0}")]
    InvalidJson(String),
    /// The JSON root was not an object.
    #[error("config root is not a JSON object")]
    RootNotObject,
}

/// Parse `config.json` text, following `EncoreConfigStore::parse_config`.
///
/// Fields that are absent, or present with the wrong type, keep their
/// defaults from [`Preferences::default`] and from empty governor
/// strings. See the module documentation for the empty-governor quirk.
///
/// # Errors
///
/// Returns [`ConfigError`] if the text is not valid JSON, or if the
/// JSON root is not an object.
pub fn parse_config(text: &str) -> Result<ConfigData, ConfigError> {
    let value: Value =
        serde_json::from_str(text).map_err(|error| ConfigError::InvalidJson(error.to_string()))?;
    let Some(object) = value.as_object() else {
        return Err(ConfigError::RootNotObject);
    };
    let mut config = ConfigData {
        preferences: Preferences::default(),
        cpu_governor: CpuGovernor::default(),
    };
    if let Some(prefs) = object.get("preferences").and_then(Value::as_object) {
        if let Some(value) = prefs.get("enforce_lite_mode").and_then(Value::as_bool) {
            config.preferences.enforce_lite_mode = value;
        }
        if let Some(value) = prefs.get("use_device_mitigation").and_then(Value::as_bool) {
            config.preferences.use_device_mitigation = value;
        }
        if let Some(value) = prefs.get("disable_tweaks").and_then(Value::as_bool) {
            config.preferences.disable_tweaks = value;
        }
        if let Some(value) = prefs.get("log_level").and_then(Value::as_i64) {
            // Clamp to the `i32` range first, so the conversion below
            // cannot fail, panic, or wrap, and matches the C++ `int`
            // field on a 32-bit-or-larger int.
            let clamped = value.clamp(i64::from(i32::MIN), i64::from(i32::MAX));
            if let Ok(level) = i32::try_from(clamped) {
                config.preferences.log_level = level;
            }
        }
    }
    if let Some(governor) = object.get("cpu_governor").and_then(Value::as_object) {
        if let Some(value) = governor.get("balance").and_then(Value::as_str) {
            value.clone_into(&mut config.cpu_governor.balance);
        }
        if let Some(value) = governor.get("powersave").and_then(Value::as_str) {
            value.clone_into(&mut config.cpu_governor.powersave);
        }
    }
    Ok(config)
}

/// Render a configuration as pretty JSON with a 2-space indent,
/// matching `EncoreConfigStore::save_config`.
///
/// # Errors
///
/// Returns a [`serde_json::Error`] if serialisation fails. In practice
/// this cannot fail for the plain string, boolean, and integer fields
/// used here, but the error is propagated instead of being hidden.
pub fn render_config(config: &ConfigData) -> Result<String, serde_json::Error> {
    let value = json!({
        "preferences": {
            "enforce_lite_mode": config.preferences.enforce_lite_mode,
            "use_device_mitigation": config.preferences.use_device_mitigation,
            "disable_tweaks": config.preferences.disable_tweaks,
            "log_level": config.preferences.log_level,
        },
        "cpu_governor": {
            "balance": config.cpu_governor.balance,
            "powersave": config.cpu_governor.powersave,
        },
    });
    serde_json::to_string_pretty(&value)
}

#[cfg(test)]
mod tests {
    use super::{parse_config, render_config, ConfigData, FALLBACK_CPU_GOVERNOR};

    #[test]
    fn default_config_uses_the_given_governor() {
        let config = ConfigData::default_with_governor(FALLBACK_CPU_GOVERNOR);
        assert_eq!(config.cpu_governor.balance, "schedutil");
        assert_eq!(config.cpu_governor.powersave, "schedutil");
        assert_eq!(config.preferences.log_level, 4);
        assert!(!config.preferences.disable_tweaks);
    }

    #[test]
    fn parses_full_config_and_ignores_wrong_types() {
        let config = parse_config(
            r#"{"preferences": {"enforce_lite_mode": true, "log_level": "high"}, "cpu_governor": {"balance": "performance", "powersave": "powersave"}}"#,
        )
        .unwrap_or_else(|_| ConfigData::default_with_governor(FALLBACK_CPU_GOVERNOR));
        assert!(config.preferences.enforce_lite_mode);
        assert_eq!(config.preferences.log_level, 4);
        assert_eq!(config.cpu_governor.balance, "performance");
        assert_eq!(config.cpu_governor.powersave, "powersave");
    }

    #[test]
    fn partial_config_leaves_absent_governor_empty_like_cpp() {
        let config = parse_config(r#"{"preferences": {}}"#)
            .unwrap_or_else(|_| ConfigData::default_with_governor(FALLBACK_CPU_GOVERNOR));
        assert_eq!(config.cpu_governor.balance, "");
        assert_eq!(config.cpu_governor.powersave, "");
    }

    #[test]
    fn renders_pretty_json_with_two_space_indent() {
        let config = ConfigData::default_with_governor("schedutil");
        let rendered = render_config(&config).unwrap_or_default();
        assert!(rendered.contains("\"log_level\": 4"));
        assert!(rendered.contains("\"balance\": \"schedutil\""));
    }
}
