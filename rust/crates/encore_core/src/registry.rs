//! Snapshot game registry, mirroring `GameRegistry`.
//!
//! This is the Stage 2 fix for analysis finding 1. The C++ registry
//! kept one mutable vector behind a mutex and handed out a raw pointer
//! into it (`find_game_ptr`). The mutex was released before the caller
//! used the pointer, and a concurrent `update_gamelist` (from the
//! inotify thread) could clear and re-sort that vector while a profile
//! was being applied, leaving a dangling pointer.
//!
//! The Rust replacement never hands out a reference into a mutable
//! collection:
//!
//! - The registry holds one `Arc<Vec<GameEntry>>` behind a read-write
//!   lock.
//! - `snapshot` clones the `Arc` (cheap) and returns it. A reader that
//!   holds a snapshot keeps that whole list alive, even if another
//!   thread replaces the registry afterwards.
//! - `replace` swaps the whole `Arc` for a new, sorted list. It never
//!   mutates a list that a snapshot may still be reading.
//! - `find` snapshots first, then searches the snapshot, and returns
//!   an owned `GameEntry` copy, never a reference into the registry.
//!
//! Verified from `jni/base/GameRegistry/GameRegistry.cpp`:
//!
//! - Entries are validated only by rejecting empty package names.
//! - `update_gamelist` sorts by package name, with no deduplication.
//!   A list built from JSON cannot contain duplicate keys anyway,
//!   because the JSON object parser keeps one value per key, but a
//!   list built directly may contain duplicates; `find` then returns
//!   the first match, as the C++ `lower_bound` search does.
//! - A failed load does not replace the old list. That is why the
//!   parsing functions in [`crate::gamelist`] return a result, and the
//!   caller only calls `replace` on success.

use std::sync::{Arc, RwLock};

use crate::gamelist::{parse_gamelist, GameEntry, GamelistError};

/// A thread-safe game registry holding one immutable snapshot at a time.
#[derive(Debug, Default)]
pub struct GameRegistry {
    /// The current snapshot. Readers clone this `Arc`; writers replace it.
    current: RwLock<Arc<Vec<GameEntry>>>,
}

impl GameRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            current: RwLock::new(Arc::new(Vec::new())),
        }
    }

    /// Create a registry from entries, sorted by package name.
    ///
    /// Empty package names are dropped, mirroring
    /// `GameRegistry::validate_game_entry`.
    #[must_use]
    pub fn from_entries(entries: Vec<GameEntry>) -> Self {
        let registry = Self::new();
        registry.replace(entries);
        registry
    }

    /// Take a snapshot of the current game list.
    ///
    /// The snapshot stays valid even if `replace` is called afterwards,
    /// because it owns its own `Arc` to the old list.
    #[must_use]
    pub fn snapshot(&self) -> Arc<Vec<GameEntry>> {
        match self.current.read() {
            Ok(guard) => Arc::clone(&guard),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// Replace the whole registry with a new, sorted list.
    ///
    /// Empty package names are dropped. This swaps the `Arc`; it does
    /// not mutate any list a snapshot may still hold.
    pub fn replace(&self, entries: Vec<GameEntry>) {
        let mut kept: Vec<GameEntry> = entries
            .into_iter()
            .filter(|entry| !entry.package_name.is_empty())
            .collect();
        kept.sort_by(|a, b| a.package_name.cmp(&b.package_name));
        let next = Arc::new(kept);
        match self.current.write() {
            Ok(mut guard) => *guard = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
    }

    /// Replace the registry from `gamelist.json` text.
    ///
    /// On a parse error the old registry is left untouched, mirroring
    /// the C++ caller, which returns `false` without replacing the list.
    ///
    /// # Errors
    ///
    /// Returns [`GamelistError`] if the text is not valid JSON, or if
    /// the JSON root is not an object.
    pub fn load_from_text(&self, text: &str) -> Result<(), GamelistError> {
        let entries = parse_gamelist(text)?;
        self.replace(entries);
        Ok(())
    }

    /// Find a game by package name, returning an owned copy.
    ///
    /// The copy is safe to use after the registry is replaced, because
    /// it does not borrow from any snapshot.
    #[must_use]
    pub fn find(&self, package_name: &str) -> Option<GameEntry> {
        find_in_snapshot(&self.snapshot(), package_name)
    }

    /// Check whether a package is registered as a game.
    #[must_use]
    pub fn is_registered(&self, package_name: &str) -> bool {
        self.find(package_name).is_some()
    }

    /// The number of registered games in the current snapshot.
    #[must_use]
    pub fn len(&self) -> usize {
        self.snapshot().len()
    }

    /// Whether the current snapshot contains no games.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// All package names in the current snapshot, in sorted order.
    #[must_use]
    pub fn package_names(&self) -> Vec<String> {
        self.snapshot()
            .iter()
            .map(|entry| entry.package_name.clone())
            .collect()
    }
}

/// Find a game in a snapshot, returning an owned copy.
///
/// The snapshot is already sorted, so a binary search is used, as in
/// the C++ `lower_bound` search.
#[must_use]
pub fn find_in_snapshot(snapshot: &[GameEntry], package_name: &str) -> Option<GameEntry> {
    let index = snapshot.partition_point(|entry| entry.package_name.as_str() < package_name);
    let entry = snapshot.get(index)?;
    if entry.package_name == package_name {
        Some(entry.clone())
    } else {
        None
    }
}

/// Build a gamelist from base-file text, mirroring
/// `GameRegistry::populate_from_base`.
///
/// Each line is trimmed of trailing spaces, tabs, carriage returns,
/// and newlines only, as in C++; leading whitespace is kept. Empty
/// lines are skipped. A line is kept only when `is_installed` returns
/// `true` for it. On a device that check is
/// `access("/data/data/<package>", F_OK) == 0`; callers pass their own
/// check so this pure function can be tested without a device, and so
/// the binary can point it at a different data directory for testing.
///
/// The returned entries are in base-file order, not sorted: the C++
/// version writes the JSON object in insertion order. Sorting happens
/// on load, in [`GameRegistry::replace`], not here.
#[must_use]
pub fn populate_from_base(base_text: &str, is_installed: impl Fn(&str) -> bool) -> Vec<GameEntry> {
    let mut entries = Vec::new();
    for line in base_text.lines() {
        let package_name = line.trim_end_matches([' ', '\t', '\r', '\n']);
        if package_name.is_empty() {
            continue;
        }
        if is_installed(package_name) {
            entries.push(GameEntry {
                package_name: package_name.to_owned(),
                lite_mode: false,
                enable_dnd: false,
            });
        }
    }
    entries
}

/// Render a gamelist as pretty JSON with a 2-space indent, in the
/// order given, mirroring `GameRegistry::populate_from_base`.
///
/// Rendering is done by hand, rather than through a JSON map, so the
/// entry order is preserved: the default `serde_json` map sorts keys,
/// which would silently reorder a base file. Package names are escaped
/// with `serde_json` string serialisation, so a name containing a
/// quote or control character cannot break the JSON structure.
#[must_use]
pub fn render_gamelist(entries: &[GameEntry]) -> String {
    if entries.is_empty() {
        return "{}".to_owned();
    }
    let mut out = String::new();
    out.push_str("{\n");
    let last = entries.len() - 1;
    for (index, entry) in entries.iter().enumerate() {
        let name = serde_json::to_string(&entry.package_name).unwrap_or_default();
        out.push_str("  ");
        out.push_str(&name);
        out.push_str(": {\n");
        out.push_str("    \"lite_mode\": ");
        out.push_str(if entry.lite_mode { "true" } else { "false" });
        out.push_str(",\n");
        out.push_str("    \"enable_dnd\": ");
        out.push_str(if entry.enable_dnd { "true" } else { "false" });
        out.push_str("\n  }");
        if index == last {
            out.push('\n');
        } else {
            out.push_str(",\n");
        }
    }
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use super::{find_in_snapshot, populate_from_base, render_gamelist, GameRegistry};
    use crate::gamelist::GameEntry;

    fn entry(name: &str, lite_mode: bool, enable_dnd: bool) -> GameEntry {
        GameEntry {
            package_name: name.to_owned(),
            lite_mode,
            enable_dnd,
        }
    }

    #[test]
    fn snapshot_survives_replace() {
        let registry = GameRegistry::from_entries(vec![entry("com.a", false, false)]);
        let old_snapshot = registry.snapshot();
        registry.replace(vec![entry("com.b", true, true)]);
        assert_eq!(old_snapshot.len(), 1);
        assert_eq!(
            old_snapshot.first().map(|game| game.package_name.as_str()),
            Some("com.a")
        );
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.package_names(), vec!["com.b".to_owned()]);
        assert_eq!(
            find_in_snapshot(&old_snapshot, "com.a"),
            Some(entry("com.a", false, false))
        );
    }

    #[test]
    fn find_returns_owned_copy_and_missing_is_none() {
        let registry = GameRegistry::from_entries(vec![
            entry("com.z", false, false),
            entry("com.a", true, false),
        ]);
        assert_eq!(registry.find("com.a"), Some(entry("com.a", true, false)));
        assert_eq!(registry.find("com.missing"), None);
        assert!(registry.is_registered("com.z"));
        assert!(!registry.is_registered("com.missing"));
    }

    #[test]
    fn replace_sorts_and_drops_empty_names() {
        let registry = GameRegistry::new();
        registry.replace(vec![
            entry("com.z", false, false),
            entry("", false, false),
            entry("com.a", false, true),
        ]);
        assert_eq!(
            registry.package_names(),
            vec!["com.a".to_owned(), "com.z".to_owned()]
        );
    }

    #[test]
    fn load_from_text_rejects_bad_json_without_replacing() {
        let registry = GameRegistry::from_entries(vec![entry("com.a", false, false)]);
        assert!(registry.load_from_text("{not json").is_err());
        assert_eq!(registry.package_names(), vec!["com.a".to_owned()]);
        assert!(registry
            .load_from_text(r#"{"com.b": {"lite_mode": true}}"#)
            .is_ok());
        assert_eq!(registry.package_names(), vec!["com.b".to_owned()]);
    }

    #[test]
    fn populate_keeps_base_order_and_trims_trailing_whitespace() {
        let entries = populate_from_base("com.z.game  \n\ncom.a.game\t\ncom.missing\n", |name| {
            name != "com.missing"
        });
        assert_eq!(
            entries,
            vec![
                entry("com.z.game", false, false),
                entry("com.a.game", false, false)
            ]
        );
    }

    #[test]
    fn render_preserves_order_and_pretty_format() {
        let rendered = render_gamelist(&[entry("com.z", false, false), entry("com.a", true, true)]);
        assert_eq!(
            rendered,
            "{\n  \"com.z\": {\n    \"lite_mode\": false,\n    \"enable_dnd\": false\n  },\n  \"com.a\": {\n    \"lite_mode\": true,\n    \"enable_dnd\": true\n  }\n}"
        );
        assert_eq!(render_gamelist(&[]), "{}");
        assert_eq!(
            render_gamelist(&[entry("com.a", false, false)]),
            "{\n  \"com.a\": {\n    \"lite_mode\": false,\n    \"enable_dnd\": false\n  }\n}"
        );
    }
}
