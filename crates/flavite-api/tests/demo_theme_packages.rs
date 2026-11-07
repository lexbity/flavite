//! Demo theme-package validation: real browser `.xpi` packages converted
//! and stored end-to-end through the hermetic [`ThemeStore`].
//!
//! The fixture packages are deliberately outside the repository
//! (gitignored scratch content — randomly picked browser themes). A run
//! points `FLAVITE_DEMO_THEMES` at a directory of `.xpi` files; without
//! the variable every test returns immediately, so CI never depends on
//! the fixtures.

use flavite_api::{InstalledTheme, ThemeStore};
use flavite_fs::OsFs;
use flavite_theme::ThemeImage;
use std::path::PathBuf;

fn demo_dir() -> Option<(PathBuf, Vec<PathBuf>)> {
    let dir = PathBuf::from(std::env::var_os("FLAVITE_DEMO_THEMES")?);
    let mut packages: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("demo theme directory is readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "xpi"))
        .collect();
    packages.sort();
    assert!(
        !packages.is_empty(),
        "FLAVITE_DEMO_THEMES points at a directory without .xpi files"
    );
    Some((dir, packages))
}

fn store() -> (ThemeStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("data dir");
    (
        ThemeStore::at_root(Box::new(OsFs::new()), dir.path().to_path_buf()),
        dir,
    )
}

#[test]
fn demo_packages_convert_install_and_load() {
    let Some((_, packages)) = demo_dir() else {
        return;
    };
    let (store, _dir) = store();
    let mut ids = Vec::new();
    for package in &packages {
        let bytes = std::fs::read(package).expect("package bytes");
        let installed: InstalledTheme = store
            .install(&bytes)
            .unwrap_or_else(|error| panic!("{} must install: {error}", package.display()));
        ids.push(installed.id.clone());
        // The metadata names the theme; identity is stable across reload.
        assert!(!installed.name.is_empty(), "name is carried");
    }
    assert_eq!(ids.len(), packages.len());
    let listed = store.list().expect("list");
    assert_eq!(listed.len(), ids.len(), "every package lists exactly once");
    for id in &ids {
        let loaded = store.load(id).expect("load");
        assert_eq!(&loaded.installed.id, id);
        // The semantic mapping resolved to at least one slot for every
        // real package (all four demos carry usable colors).
        assert!(
            !loaded.converted.light.semantic.is_empty(),
            "{id} mapped to no flavite surface"
        );
        assert!(
            !loaded
                .converted
                .report
                .warnings
                .iter()
                .any(|w| w.contains("unreadable")),
            "{id}: real packages must not produce unreadable-image warnings"
        );
    }
}

#[test]
fn demo_package_images_decode_within_budgets() {
    let Some((_, packages)) = demo_dir() else {
        return;
    };
    for package in &packages {
        let bytes = std::fs::read(package).expect("package bytes");
        let converted = flavite_api::flavite_theme::convert_xpi(&bytes)
            .unwrap_or_else(|error| panic!("{} must convert: {error}", package.display()));
        // Every declared theme_frame decodes; real browser themes carry
        // the header in additional_backgrounds (v1 drops it without
        // decoding), so this asserts what v1 actually renders.
        if let Some(frame) = &converted.theme_frame {
            assert!(frame.width() > 0 && frame.height() > 0);
            if let ThemeImage::Animated { frames, .. } = frame {
                assert!(!frames.is_empty());
            }
        }
        // The report's animated flag matches the decoded header.
        assert_eq!(
            converted.report.animated_frame,
            converted
                .theme_frame
                .as_ref()
                .is_some_and(ThemeImage::is_animated)
        );
    }
}

/// The stored converted package keeps every original entry (real browser
/// packages ship META-INF signing files that must survive verbatim).
#[test]
fn demo_packages_keep_their_original_entries() {
    let Some((_, packages)) = demo_dir() else {
        return;
    };
    for package in &packages {
        let bytes = std::fs::read(package).expect("package bytes");
        let converted = flavite_api::flavite_theme::convert_xpi(&bytes)
            .unwrap_or_else(|error| panic!("{} must convert: {error}", package.display()));
        let original = zip_entries(&bytes);
        let converted_entries = converted
            .files
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<std::collections::BTreeSet<_>>();
        for name in &original {
            assert!(
                converted_entries.contains(name),
                "{name} from {} is missing after conversion",
                package.display()
            );
        }
        // The manifest is present exactly once (the normalized copy).
        assert_eq!(
            converted
                .files
                .iter()
                .filter(|(name, _)| name == "manifest.json")
                .count(),
            1
        );
    }
}

fn zip_entries(package: &[u8]) -> Vec<String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(package)).expect("zip");
    (0..archive.len())
        .filter_map(|index| {
            archive
                .by_index_raw(index)
                .ok()
                .and_then(|entry| entry.name().ok().map(|name| name.to_string()))
        })
        .collect()
}
