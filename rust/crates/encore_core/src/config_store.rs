//! File-backed configuration store, mirroring `EncoreConfigStore`.
//!
//! Verified from `jni/EncoreConfigStore.cpp`:
//!
//! - If the config file is missing, or cannot be parsed as JSON, the
//!   daemon creates a default config and saves it.
//! - If the JSON root is not an object, loading returns `false`
//!   without changing the stored config.
//! - A default config uses the governor read from the default-governor
//!   file, falling back to `"schedutil"` when that file is missing or
//!   empty.
//! - `parse_config` starts from default preferences but empty governor
//!   strings, so a partial config leaves an absent governor empty.
//!   That quirk is preserved here, because it is the C++ behaviour
//!   (see [`crate::config`]).
//!
//! Two deliberate differences from C++, both required by the task
//! guide:
//!
//! - Saving uses [`crate::atomic_write::write_atomic`] with explicit
//!   `0644` permissions, instead of truncating `fopen` in place
//!   (analysis finding 4).
//! - The store keeps its path and its current config together, so a
//!   reload cannot silently point at a different file.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::atomic_write::{write_atomic, AtomicWriteError};
use crate::config::{parse_config, render_config, ConfigData, ConfigError, FALLBACK_CPU_GOVERNOR};

/// An error from loading or saving a configuration file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigStoreError {
    /// The config text was not a JSON object, so the stored config
    /// was left unchanged.
    #[error("config root is not a JSON object")]
    RootNotObject,
    /// The config text could not be parsed as JSON. This is only
    /// returned by the pure parse step; `load` turns it into a
    /// default config instead, mirroring C++.
    #[error("invalid JSON: {0}")]
    InvalidJson(String),
    /// Serialising the config to JSON failed.
    #[error("failed to render config: {0}")]
    Render(serde_json::Error),
    /// An underlying I/O operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// An atomic write failed.
    #[error("atomic write failed: {0}")]
    AtomicWrite(#[from] AtomicWriteError),
}

impl From<ConfigError> for ConfigStoreError {
    fn from(error: ConfigError) -> Self {
        match error {
            ConfigError::InvalidJson(message) => ConfigStoreError::InvalidJson(message),
            ConfigError::RootNotObject => ConfigStoreError::RootNotObject,
        }
    }
}

/// Read the default CPU governor, mirroring
/// `EncoreConfigStore::read_default_cpu_governor`.
///
/// The first line of the file is used, without its newline. If the
/// file is missing or has no first line, the fallback governor is
/// returned.
#[must_use]
pub fn read_default_governor(path: &Path) -> String {
    let Ok(text) = fs::read_to_string(path) else {
        return FALLBACK_CPU_GOVERNOR.to_owned();
    };
    match text.lines().next() {
        Some(line) => line.to_owned(),
        None => FALLBACK_CPU_GOVERNOR.to_owned(),
    }
}

/// A configuration file on disk, with its current in-memory config.
#[derive(Debug)]
pub struct ConfigStore {
    /// Path to `config.json`.
    path: PathBuf,
    /// Path to the default-governor file, used for default configs.
    default_governor_path: PathBuf,
    /// The current config. Readers clone it; writers replace it whole,
    /// so no caller ever holds a reference into a mutable config.
    config: RwLock<ConfigData>,
}

impl ConfigStore {
    /// Load a config from `path`, mirroring `EncoreConfigStore::load_config`.
    ///
    /// A missing file, or a file that cannot be parsed as JSON, makes
    /// a default config (from the default-governor file, or the
    /// fallback governor) and saves it. A non-object root returns an
    /// error and leaves the store holding the default config, because
    /// there is no earlier config to keep at construction time; use
    /// [`ConfigStore::reload`] on an existing store to keep the old
    /// config on a non-object root.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigStoreError`] if a non-object root is found, or
    /// if saving a default config fails.
    pub fn load(path: &Path, default_governor_path: &Path) -> Result<Self, ConfigStoreError> {
        let governor = read_default_governor(default_governor_path);
        let store = Self {
            path: path.to_owned(),
            default_governor_path: default_governor_path.to_owned(),
            config: RwLock::new(ConfigData::default_with_governor(&governor)),
        };
        match fs::read_to_string(path) {
            Ok(text) => match parse_config(&text) {
                Ok(config) => store.set(config),
                Err(ConfigError::InvalidJson(_)) => {
                    store.create_default_and_save()?;
                }
                Err(ConfigError::RootNotObject) => {
                    return Err(ConfigStoreError::RootNotObject);
                }
            },
            Err(_) => {
                store.create_default_and_save()?;
            }
        }
        Ok(store)
    }

    /// Reload the config from the store's path, mirroring `reload`.
    ///
    /// A missing file or an unparsable file creates and saves a
    /// default config, as in C++. A non-object root returns an error
    /// and keeps the previously stored config.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigStoreError`] if a non-object root is found, or
    /// if saving a default config fails.
    pub fn reload(&self) -> Result<(), ConfigStoreError> {
        match fs::read_to_string(&self.path) {
            Ok(text) => match parse_config(&text) {
                Ok(config) => {
                    self.set(config);
                    Ok(())
                }
                Err(ConfigError::InvalidJson(_)) => self.create_default_and_save(),
                Err(ConfigError::RootNotObject) => Err(ConfigStoreError::RootNotObject),
            },
            Err(_) => self.create_default_and_save(),
        }
    }

    /// Save the current config to the store's path, atomically.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigStoreError`] if rendering or writing fails.
    pub fn save(&self) -> Result<(), ConfigStoreError> {
        let config = self.get();
        let rendered = render_config(&config).map_err(ConfigStoreError::Render)?;
        write_atomic(&self.path, rendered.as_bytes(), 0o644)?;
        Ok(())
    }

    /// The path this store loads from and saves to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A clone of the current config.
    #[must_use]
    pub fn get(&self) -> ConfigData {
        match self.config.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Replace the current config in memory. Call [`ConfigStore::save`]
    /// to persist it.
    pub fn set(&self, config: ConfigData) {
        match self.config.write() {
            Ok(mut guard) => *guard = config,
            Err(poisoned) => *poisoned.into_inner() = config,
        }
    }

    /// Build a default config from the default-governor file, store
    /// it, and save it.
    fn create_default_and_save(&self) -> Result<(), ConfigStoreError> {
        let governor = read_default_governor(&self.default_governor_path);
        let default = ConfigData::default_with_governor(&governor);
        self.set(default);
        self.save()
    }
}

#[cfg(test)]
mod tests {
    use super::{read_default_governor, ConfigStore};
    use crate::config::FALLBACK_CPU_GOVERNOR;
    use std::fs;

    fn temp_dir_for(test_name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("encore-config-{test_name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(fs::create_dir_all(&dir).is_ok());
        dir
    }

    #[test]
    fn missing_file_creates_default_with_fallback_governor() -> Result<(), super::ConfigStoreError>
    {
        let dir = temp_dir_for("missing");
        let path = dir.join("config.json");
        let governor_path = dir.join("default_cpu_gov");
        let store = ConfigStore::load(&path, &governor_path)?;
        assert_eq!(store.get().cpu_governor.balance, FALLBACK_CPU_GOVERNOR);
        assert_eq!(store.get().preferences.log_level, 4);
        assert!(path.exists());
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn missing_governor_file_falls_back_and_present_file_is_used() {
        let dir = temp_dir_for("governor");
        assert_eq!(
            read_default_governor(&dir.join("missing")),
            FALLBACK_CPU_GOVERNOR
        );
        let governor_path = dir.join("default_cpu_gov");
        assert!(fs::write(&governor_path, "performance\n").is_ok());
        assert_eq!(read_default_governor(&governor_path), "performance");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_and_reload_round_trips() -> Result<(), super::ConfigStoreError> {
        let dir = temp_dir_for("roundtrip");
        let path = dir.join("config.json");
        let governor_path = dir.join("default_cpu_gov");
        let store = ConfigStore::load(&path, &governor_path)?;
        let mut config = store.get();
        config.preferences.enforce_lite_mode = true;
        config.cpu_governor.balance = "performance".to_owned();
        store.set(config);
        store.save()?;
        store.reload()?;
        assert!(store.get().preferences.enforce_lite_mode);
        assert_eq!(store.get().cpu_governor.balance, "performance");
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn invalid_json_creates_default_and_non_object_keeps_old() -> Result<(), super::ConfigStoreError>
    {
        let dir = temp_dir_for("invalid");
        let path = dir.join("config.json");
        let governor_path = dir.join("default_cpu_gov");
        assert!(fs::write(&path, "{not json").is_ok());
        let store = ConfigStore::load(&path, &governor_path)?;
        assert_eq!(store.get().cpu_governor.balance, FALLBACK_CPU_GOVERNOR);
        assert!(fs::write(&path, "[1, 2]").is_ok());
        assert!(store.reload().is_err());
        assert_eq!(store.get().cpu_governor.balance, FALLBACK_CPU_GOVERNOR);
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }
}
