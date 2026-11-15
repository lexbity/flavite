//! Profile facade tests (FEUX-0003 X06).
//!
//! These tests verify the Flavite facade methods for profile operations.

use flavite_api::{ApplyProfileResult, Flavite, FlaviteError};
use flavite_fs::MemFs;

#[test]
fn profiles_list_returns_empty_for_new_vault() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut flavite =
        Flavite::open_root(temp_dir.path().to_path_buf(), Default::default()).unwrap();

    // Replace the filesystem with MemFs for hermetic testing
    let fs = MemFs::new();
    flavite.set_vault_store_fs_for_test(fs);

    let profiles = flavite.profiles_list().unwrap();
    assert!(profiles.is_empty());
}

#[test]
fn profiles_create_from_current_creates_profile() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut flavite =
        Flavite::open_root(temp_dir.path().to_path_buf(), Default::default()).unwrap();

    // Replace the filesystem with MemFs for hermetic testing
    let fs = MemFs::new();
    flavite.set_vault_store_fs_for_test(fs);

    // Create a profile from current settings
    let profile = flavite
        .profiles_create_from_current("test profile")
        .unwrap();

    assert_eq!(profile.name(), "test profile");

    // Verify it was created
    let profiles = flavite.profiles_list().unwrap();
    assert_eq!(profiles.len(), 1);
}

#[test]
fn profiles_create_and_apply_roundtrip() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut flavite =
        Flavite::open_root(temp_dir.path().to_path_buf(), Default::default()).unwrap();

    // Replace the filesystem with MemFs for hermetic testing
    let fs = MemFs::new();
    flavite.set_vault_store_fs_for_test(fs);

    // Modify a setting first
    flavite
        .settings_set(
            "editor.font_size",
            flavite_settings::value::SettingValue::Int(18),
        )
        .unwrap();

    // Create a profile from current settings
    let profile = flavite
        .profiles_create_from_current("modified settings")
        .unwrap();
    let profile_id = profile.profile_id().to_string();

    // Reset the setting back to default
    flavite
        .settings_set(
            "editor.font_size",
            flavite_settings::value::SettingValue::Int(14),
        )
        .unwrap();

    // Verify the setting was reset
    let resolved = flavite.settings_read().unwrap();
    let font_size = resolved.resolved.get("editor.font_size").unwrap();
    assert_eq!(font_size, &flavite_settings::value::SettingValue::Int(14));

    // Apply the profile
    let result: ApplyProfileResult = flavite.profiles_apply(&profile_id).unwrap();

    // Verify the setting was changed back
    let resolved = flavite.settings_read().unwrap();
    let font_size = resolved.resolved.get("editor.font_size").unwrap();
    assert_eq!(font_size, &flavite_settings::value::SettingValue::Int(18));

    // Verify the result counts
    assert!(result.changed > 0 || !result.warnings.is_empty());
}

#[test]
fn profiles_apply_schema_newer_rejected() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut flavite =
        Flavite::open_root(temp_dir.path().to_path_buf(), Default::default()).unwrap();

    // Replace the filesystem with MemFs for hermetic testing
    let fs = MemFs::new();
    flavite.set_vault_store_fs_for_test(fs);

    // Create a profile with schema version 3 (newer than supported)
    let _profile = flavite
        .profiles()
        .create(
            "schema 3",
            b"schema = 3\n[appearance]\nkeyboard_layout = \"qwerty\"",
        )
        .unwrap();
    let profiles = flavite.profiles_list().unwrap();
    let profile_id = profiles[0].profile_id().to_string();

    // Try to apply it - should be rejected
    let result = flavite.profiles_apply(&profile_id);
    assert!(matches!(result, Err(FlaviteError::ProfilesSchemaNewer)));
}

#[test]
fn profiles_delete_removes_profile() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut flavite =
        Flavite::open_root(temp_dir.path().to_path_buf(), Default::default()).unwrap();

    // Replace the filesystem with MemFs for hermetic testing
    let fs = MemFs::new();
    flavite.set_vault_store_fs_for_test(fs);

    // Create a profile
    let profile = flavite.profiles_create_from_current("to delete").unwrap();
    let profile_id = profile.profile_id().to_string();

    // Verify it exists
    assert_eq!(flavite.profiles_list().unwrap().len(), 1);

    // Delete it
    flavite.profiles_delete(&profile_id).unwrap();

    // Verify it's gone
    assert_eq!(flavite.profiles_list().unwrap().len(), 0);
}

#[test]
fn profiles_read_bytes_returns_content() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut flavite =
        Flavite::open_root(temp_dir.path().to_path_buf(), Default::default()).unwrap();

    // Replace the filesystem with MemFs for hermetic testing
    let fs = MemFs::new();
    flavite.set_vault_store_fs_for_test(fs);

    // Create a profile
    let profile = flavite.profiles_create_from_current("read test").unwrap();
    let profile_id = profile.profile_id().to_string();

    // Read the bytes
    let bytes = flavite.profiles_read_bytes(&profile_id).unwrap();

    // Verify it's valid TOML
    assert!(!bytes.is_empty());
    let content = String::from_utf8(bytes.clone()).unwrap();
    assert!(content.starts_with("schema"));
}
