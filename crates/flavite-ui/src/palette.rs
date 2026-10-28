//! Registry-backed command palette model.
//!
//! Matching is in-memory nucleo fuzzy scoring (§4.7): the registry list is
//! small, so filtering happens on the UI thread without touching the database.

use flavite_api::{CommandSpec, Flavite};
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher};
use std::sync::{LazyLock, Mutex};

/// Shared nucleo matcher: reusing one instance avoids re-allocating its
/// internal slab on every keystroke.
static MATCHER: LazyLock<Mutex<Matcher>> =
    LazyLock::new(|| Mutex::new(Matcher::new(Config::DEFAULT)));

/// Filter registered commands by title, id, or declared keybinding, ranked by
/// nucleo fuzzy score. An empty or whitespace query preserves registry order.
#[must_use]
pub fn matching_commands(flavite: &Flavite, query: &str) -> Vec<CommandSpec> {
    matching_specs(flavite.command_registry().specs(), query)
}

/// Match and rank a stable command-spec snapshot, which also keeps palette
/// behavior easy to test.
#[must_use]
pub fn matching_specs(specs: Vec<CommandSpec>, query: &str) -> Vec<CommandSpec> {
    let needle = query.trim();
    if needle.is_empty() {
        return specs;
    }
    // One haystack folds title, id, and keybinding so "ctrl", "save", or
    // "note.open" all find their command.
    let texts: Vec<String> = specs.iter().map(candidate_text).collect();
    let pattern = Pattern::parse(needle, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = MATCHER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    pattern
        .match_list(texts.iter().map(String::as_str), &mut matcher)
        .into_iter()
        .filter_map(|(text, _)| {
            texts
                .iter()
                .position(|candidate| candidate == text)
                .map(|index| specs[index].clone())
        })
        .collect()
}

/// The searchable haystack for one command: title, id, and rendered chord.
fn candidate_text(spec: &CommandSpec) -> String {
    let mut text = format!("{} {}", spec.title, spec.id);
    if let Some(chord) = keybinding(spec) {
        text.push(' ');
        text.push_str(&chord);
    }
    text
}

/// Render the command's declared keybinding for display.
#[must_use]
pub fn keybinding(spec: &CommandSpec) -> Option<String> {
    spec.keys.first().map(|chord| {
        let mut parts = Vec::new();
        if chord.modifiers & flavite_api::command::modifiers::CTRL != 0 {
            parts.push("Ctrl");
        }
        if chord.modifiers & flavite_api::command::modifiers::SHIFT != 0 {
            parts.push("Shift");
        }
        if chord.modifiers & flavite_api::command::modifiers::ALT != 0 {
            parts.push("Alt");
        }
        if chord.modifiers & flavite_api::command::modifiers::SUPER != 0 {
            parts.push("Super");
        }
        parts.push(chord.key.as_str());
        parts.join("+")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_specs() -> Vec<CommandSpec> {
        static SAVE: std::sync::LazyLock<[flavite_api::Chord; 1]> =
            std::sync::LazyLock::new(|| {
                [flavite_api::Chord::new(
                    flavite_api::command::modifiers::CTRL,
                    "S",
                )]
            });
        vec![
            CommandSpec::new(
                "note.open",
                "Open note",
                flavite_api::ArgSchema::typed::<std::path::PathBuf>(),
            ),
            CommandSpec::new(
                "note.close",
                "Close note",
                flavite_api::ArgSchema::typed::<std::path::PathBuf>(),
            ),
            CommandSpec::with_availability(
                "note.save",
                "Save note",
                flavite_api::ArgSchema::typed::<std::path::PathBuf>(),
                None,
                &SAVE[..],
            ),
            CommandSpec::new(
                "vault.reconcile",
                "Refresh vault",
                flavite_api::ArgSchema::typed::<serde_json::Value>(),
            ),
        ]
    }

    #[test]
    fn keybinding_renders_declared_chord() {
        let spec = &sample_specs()[2];
        assert_eq!(keybinding(spec).as_deref(), Some("Ctrl+S"));
    }

    #[test]
    fn empty_query_keeps_snapshot_order() {
        let ranked = matching_specs(sample_specs(), "   ");
        let ids: Vec<&str> = ranked.iter().map(|spec| spec.id).collect();
        assert_eq!(
            ids,
            ["note.open", "note.close", "note.save", "vault.reconcile"]
        );
    }

    #[test]
    fn fuzzy_query_matches_and_ranks_by_title() {
        let ranked = matching_specs(sample_specs(), "save");
        assert_eq!(ranked[0].id, "note.save");
        assert!(!ranked.is_empty());
    }

    #[test]
    fn keybinding_text_is_searchable() {
        let ranked = matching_specs(sample_specs(), "ctrl+s");
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].id, "note.save");
    }

    #[test]
    fn unmatched_queries_return_nothing() {
        assert!(matching_specs(sample_specs(), "zzzz").is_empty());
    }
}
