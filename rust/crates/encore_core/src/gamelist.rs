//! Game list parsing, mirroring `GameRegistry::load_from_json`.
//!
//! Verified from `jni/base/GameRegistry/GameRegistry.cpp`:
//!
//! - The file must be a JSON object; a non-object root is rejected.
//! - Each member key is a package name; non-object values are skipped.
//! - Empty package names are skipped.
//! - `lite_mode` and `enable_dnd` default to `false` when absent or
//!   when present but not a boolean.
//! - The resulting list is sorted by package name
//!   (`GameRegistry::update_gamelist`).
//!
//! The C++ version only rejects an empty package name. Strict
//! character validation is a separate, stricter rule in
//! [`crate::package_name`], to be applied in a later stage, not here,
//! so this parser does not silently change Stage 1 behaviour.
//!
//! A parse error in the C++ daemon keeps the old registry (the caller
//! returns `false` and does not replace the list). This pure parser
//! therefore returns an error instead of a partial list.

use serde_json::Value;

/// A single game entry from `gamelist.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameEntry {
    /// The package name, as written in the JSON object key.
    pub package_name: String,
    /// Whether lite mode is enabled for this game.
    pub lite_mode: bool,
    /// Whether Do Not Disturb is enabled for this game.
    pub enable_dnd: bool,
}

/// An error from game list parsing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GamelistError {
    /// The JSON text could not be parsed.
    #[error("invalid JSON: {0}")]
    InvalidJson(String),
    /// The JSON root was not an object.
    #[error("gamelist root is not a JSON object")]
    RootNotObject,
}

/// Parse `gamelist.json` text into a sorted list of game entries.
///
/// # Errors
///
/// Returns [`GamelistError`] if the text is not valid JSON, or if the
/// JSON root is not an object.
pub fn parse_gamelist(text: &str) -> Result<Vec<GameEntry>, GamelistError> {
    let value: Value = serde_json::from_str(text)
        .map_err(|error| GamelistError::InvalidJson(error.to_string()))?;
    let Some(object) = value.as_object() else {
        return Err(GamelistError::RootNotObject);
    };
    let mut entries = Vec::new();
    for (package_name, game_value) in object {
        if package_name.is_empty() {
            continue;
        }
        let Some(game_object) = game_value.as_object() else {
            continue;
        };
        let lite_mode = game_object
            .get("lite_mode")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let enable_dnd = game_object
            .get("enable_dnd")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        entries.push(GameEntry {
            package_name: package_name.clone(),
            lite_mode,
            enable_dnd,
        });
    }
    entries.sort_by(|a, b| a.package_name.cmp(&b.package_name));
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::{parse_gamelist, GameEntry, GamelistError};

    #[test]
    fn parses_entries_with_defaults_and_sorts() {
        let entries = parse_gamelist(
            r#"{"com.z.game": {"lite_mode": true}, "com.a.game": {"enable_dnd": true}}"#,
        )
        .unwrap_or_default();
        assert_eq!(
            entries,
            vec![
                GameEntry {
                    package_name: "com.a.game".to_owned(),
                    lite_mode: false,
                    enable_dnd: true,
                },
                GameEntry {
                    package_name: "com.z.game".to_owned(),
                    lite_mode: true,
                    enable_dnd: false,
                },
            ]
        );
    }

    #[test]
    fn skips_empty_names_non_objects_and_non_booleans() {
        let entries = parse_gamelist(
            r#"{"": {"lite_mode": true}, "com.bad": "not-an-object", "com.ok": {"lite_mode": "yes"}}"#,
        )
        .unwrap_or_default();
        assert_eq!(
            entries,
            vec![GameEntry {
                package_name: "com.ok".to_owned(),
                lite_mode: false,
                enable_dnd: false,
            }]
        );
    }

    #[test]
    fn rejects_invalid_json_and_non_object_roots() {
        assert!(matches!(
            parse_gamelist("{not json"),
            Err(GamelistError::InvalidJson(_))
        ));
        assert_eq!(parse_gamelist("[1, 2]"), Err(GamelistError::RootNotObject));
    }
}
