//! File-backed `module.prop` store, mirroring `ModuleProperty`.
//!
//! Verified from `jni/include/ModuleProperty.hpp`:
//!
//! - `Get` reads the file and throws if it cannot be opened.
//! - `Change` merges the requested keys over the existing ones, if
//!   the file exists, and writes the result in key order.
//!
//! One deliberate difference from C++, required by the task guide:
//! writing uses [`crate::atomic_write::write_atomic`] with explicit
//! `0644` permissions, instead of truncating `ofstream` in place
//! (analysis finding 4: a kill mid-write could leave `module.prop`
//! empty or corrupt).
//!
//! This module also carries `get_module_version`, mirroring
//! `get_module_version` in `jni/Main.cpp`: the first line starting
//! with `version=` wins, and surrounding double quotes are stripped
//! only when both the first and last characters are quotes.

use std::fs;
use std::io;
use std::path::Path;

use crate::atomic_write::{write_atomic, AtomicWriteError};
use crate::module_prop::{change, parse, ModuleProperty};

/// An error from reading or writing `module.prop`.
#[derive(Debug, thiserror::Error)]
pub enum ModulePropStoreError {
    /// An underlying I/O operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    /// An atomic write failed.
    #[error("atomic write failed: {0}")]
    AtomicWrite(#[from] AtomicWriteError),
}

/// Read all properties from `module.prop`, in file order.
///
/// # Errors
///
/// Returns [`ModulePropStoreError`] if the file cannot be opened or
/// read, mirroring the C++ `Get` throwing on open failure.
pub fn load_module_prop(path: &Path) -> Result<Vec<ModuleProperty>, ModulePropStoreError> {
    let text = fs::read_to_string(path)?;
    Ok(parse(&text))
}

/// Merge `changes` over the existing `module.prop` at `path`, and
/// write the result atomically, mirroring `ModuleProperty::Change`.
///
/// A missing file is treated as empty, as in C++, which only reads
/// the existing file when it exists. Other read errors are returned.
///
/// # Errors
///
/// Returns [`ModulePropStoreError`] if reading or writing fails.
pub fn change_module_prop(
    path: &Path,
    changes: &[ModuleProperty],
) -> Result<(), ModulePropStoreError> {
    let existing = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let rendered = change(&existing, changes);
    write_atomic(path, rendered.as_bytes(), 0o644)?;
    Ok(())
}

/// Read the module version from `module.prop`, or `"unknown"`.
///
/// The first `version=` line wins. Surrounding double quotes are
/// stripped only when the value starts and ends with a quote, as in
/// C++; a value with only one quote is returned as-is.
#[must_use]
pub fn get_module_version(path: &Path) -> String {
    let Ok(text) = fs::read_to_string(path) else {
        return "unknown".to_owned();
    };
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("version=") {
            if let Some(stripped) = value
                .strip_prefix('"')
                .and_then(|inner| inner.strip_suffix('"'))
            {
                return stripped.to_owned();
            }
            return value.to_owned();
        }
    }
    "unknown".to_owned()
}

#[cfg(test)]
mod tests {
    use super::{change_module_prop, get_module_version, load_module_prop};
    use crate::module_prop::ModuleProperty;
    use std::fs;

    fn temp_dir_for(test_name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("encore-prop-{test_name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(fs::create_dir_all(&dir).is_ok());
        dir
    }

    fn prop(key: &str, value: &str) -> ModuleProperty {
        ModuleProperty {
            key: key.to_owned(),
            value: value.to_owned(),
        }
    }

    #[test]
    fn change_creates_missing_file_and_merges_sorted() {
        let dir = temp_dir_for("change");
        let path = dir.join("module.prop");
        assert!(change_module_prop(&path, &[prop("version", "5.2.3")]).is_ok());
        assert_eq!(
            fs::read_to_string(&path).unwrap_or_default(),
            "version=5.2.3\n"
        );
        assert!(change_module_prop(&path, &[prop("id", "encore")]).is_ok());
        assert_eq!(
            fs::read_to_string(&path).unwrap_or_default(),
            "id=encore\nversion=5.2.3\n"
        );
        let loaded = load_module_prop(&path).unwrap_or_default();
        assert_eq!(loaded.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_file_is_an_error() {
        let dir = temp_dir_for("missing");
        assert!(load_module_prop(&dir.join("module.prop")).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_strips_quotes_only_when_both_present() {
        let dir = temp_dir_for("version");
        let path = dir.join("module.prop");
        assert!(fs::write(&path, "id=encore\nversion=\"5.2.2\"\n").is_ok());
        assert_eq!(get_module_version(&path), "5.2.2");
        assert!(fs::write(&path, "version=\"5.2.2\n").is_ok());
        assert_eq!(get_module_version(&path), "\"5.2.2");
        assert!(fs::write(&path, "id=encore\n").is_ok());
        assert_eq!(get_module_version(&path), "unknown");
        assert_eq!(get_module_version(&dir.join("missing")), "unknown");
        let _ = fs::remove_dir_all(&dir);
    }
}
