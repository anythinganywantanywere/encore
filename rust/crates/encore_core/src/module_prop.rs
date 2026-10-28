//! `module.prop` parsing and merging.
//!
//! Verified from `jni/include/ModuleProperty.hpp`:
//!
//! - Lines that are empty or start with `#` are skipped.
//! - Other lines are split at the first `=`; lines without `=` are
//!   skipped by `Get` and, in `Change`, are also skipped because the
//!   existing-file loop only stores lines that contain `=`.
//! - `Change` merges the requested keys over the existing ones and
//!   writes the result in key order, because it stores them in a
//!   `std::map`.
//!
//! This module is the pure text transformation only. The C++ version
//! writes with `ofstream` truncation; the Rust caller must write the
//! rendered text with [`crate::atomic_write::write_atomic`] instead.
//! That separation is required by the task guide (section 4): pure
//! logic separate from I/O, so it can be tested without a device.

use std::collections::BTreeMap;

/// A single `key=value` property from `module.prop`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleProperty {
    /// The property key, before the first `=`.
    pub key: String,
    /// The property value, after the first `=`.
    pub value: String,
}

/// Parse `module.prop` text into properties, in file order.
///
/// # Panics
///
/// This function does not panic: it uses `split_once`, which returns
/// `None` for lines without `=`, and those lines are skipped.
#[must_use]
pub fn parse(text: &str) -> Vec<ModuleProperty> {
    let mut properties = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            properties.push(ModuleProperty {
                key: key.to_owned(),
                value: value.to_owned(),
            });
        }
    }
    properties
}

/// Render properties in key order, one `key=value` line each.
///
/// The C++ version writes in `std::map` order, which is lexicographic
/// key order; `BTreeMap` preserves that order here.
#[must_use]
pub fn render(properties: &[ModuleProperty]) -> String {
    let mut map = BTreeMap::new();
    for property in properties {
        map.insert(property.key.as_str(), property.value.as_str());
    }
    let mut out = String::new();
    for (key, value) in map {
        out.push_str(key);
        out.push('=');
        out.push_str(value);
        out.push('\n');
    }
    out
}

/// Merge `changes` over `existing_text`, in the same way as
/// `ModuleProperty::Change`, and render the result.
///
/// Existing keys not mentioned in `changes` are kept. Keys mentioned
/// in `changes` are replaced, or added if absent.
#[must_use]
pub fn change(existing_text: &str, changes: &[ModuleProperty]) -> String {
    let mut properties = parse(existing_text);
    properties.extend(changes.iter().cloned());
    render(&properties)
}

#[cfg(test)]
mod tests {
    use super::{change, parse, render, ModuleProperty};

    fn prop(key: &str, value: &str) -> ModuleProperty {
        ModuleProperty {
            key: key.to_owned(),
            value: value.to_owned(),
        }
    }

    #[test]
    fn parses_and_skips_comments_and_lines_without_equals() {
        let parsed = parse("# comment\n\nid=encore\nname=Encore\nbadline\n");
        assert_eq!(parsed, vec![prop("id", "encore"), prop("name", "Encore")]);
    }

    #[test]
    fn splits_at_the_first_equals_only() {
        let parsed = parse("description=[ok] a=b\n");
        assert_eq!(parsed, vec![prop("description", "[ok] a=b")]);
    }

    #[test]
    fn change_keeps_unmentioned_keys_and_sorts() {
        let changed = change("version=5.2.2\nid=encore\n", &[prop("version", "5.2.3")]);
        assert_eq!(changed, "id=encore\nversion=5.2.3\n");
    }

    #[test]
    fn render_is_sorted_and_ends_with_newline_per_entry() {
        assert_eq!(render(&[prop("b", "2"), prop("a", "1")]), "a=1\nb=2\n");
    }
}
