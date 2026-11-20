//! RFC-0007 FR-9 / D-13 — `[vault]` settings reach the backend.
//!
//! These tests exercise the production `Flavite::open_root` path: the
//! resolved vault document is folded into the `VaultConfig` before the
//! actor is built, and `vault.ignore_globs` is applied by the scan
//! pipeline (with the hardcoded `.flavite` exclusion preserved).

use flavite_api::{Flavite, Query, VaultConfig};

fn write_note(root: &std::path::Path, relative: &str, tags: &str, body: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        path,
        format!("---\ntags: [{tags}]\n---\n# {body}\n"),
    )
    .expect("write note");
}

#[test]
fn ignore_globs_exclude_notes_from_index_on_reopen() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("vault");
    std::fs::create_dir_all(&root).expect("vault root");
    write_note(&root, "keep.md", "keep", "keep");
    write_note(&root, "scratch/drop.md", "scratch", "drop");

    // First open: the initial scan indexes both notes.
    {
        let flavite = Flavite::open_root(root.clone(), VaultConfig::default())
            .expect("first open");
        let keep = flavite
            .query_with_limit(&Query::parse("tag:keep").expect("q"), 10)
            .expect("query keep");
        assert_eq!(keep.notes.len(), 1);
        let scratch = flavite
            .query_with_limit(&Query::parse("tag:scratch").expect("q"), 10)
            .expect("query scratch");
        assert_eq!(scratch.notes.len(), 1, "indexed before the glob takes effect");

        // Set `vault.ignore_globs` (OnRestart): takes effect on reopen.
        flavite
            .settings_set(
                "vault.ignore_globs",
                flavite_settings::value::SettingValue::List(vec!["scratch/**".into()]),
            )
            .expect("set ignore_globs");
    }

    // Reopen: the mapped config reaches the actor, and the initial scan
    // applies the glob; the excluded note leaves the catalog.
    {
        let flavite = Flavite::open_root(root.clone(), VaultConfig::default())
            .expect("second open");
        let scratch = flavite
            .query_with_limit(&Query::parse("tag:scratch").expect("q"), 10)
            .expect("query scratch");
        assert!(
            scratch.notes.is_empty(),
            "glob-excluded note must leave the catalog on reopen"
        );
        let keep = flavite
            .query_with_limit(&Query::parse("tag:keep").expect("q"), 10)
            .expect("query keep");
        assert_eq!(keep.notes.len(), 1);

        // The hardcoded `.flavite` exclusion is intact: the vault's own
        // state remains readable and writable with the glob active.
        let stored = flavite.read_stored_settings().expect("read").expect("doc");
        assert!(!stored.is_read_only());
        assert!(!stored.warnings.iter().any(|w| w.contains("unparseable")));
        let _ = flavite.settings_read().expect("resolved");
        let _ = flavite
            .settings_set(
                "vault.verification_interval_mins",
                flavite_settings::value::SettingValue::Int(15),
            )
            .expect("settings still writable");
    }
}

#[test]
fn default_settings_produce_default_config_behavior() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("vault");
    std::fs::create_dir_all(&root).expect("vault root");
    write_note(&root, "a.md", "keep", "a");

    // Open twice with default settings; the doc is created on first open
    // and everything stays functional on the second (identity case).
    {
        let flavite = Flavite::open_root(root.clone(), VaultConfig::default())
            .expect("first open");
        let all = flavite
            .query_with_limit(&Query::parse("tag:keep").expect("q"), 10)
            .expect("query");
        assert_eq!(all.notes.len(), 1);
    }
    {
        let flavite = Flavite::open_root(root.clone(), VaultConfig::default())
            .expect("second open");
        let all = flavite
            .query_with_limit(&Query::parse("tag:keep").expect("q"), 10)
            .expect("query");
        assert_eq!(all.notes.len(), 1);
    }
}
