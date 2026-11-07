//! XPI themes — typed theme commands through the Flavite facade
//! (`theme.install`, `theme.list`, `theme.uninstall`), the Themes
//! capability gate, and the `theme_package` preference key.
//!
//! The stack is assembled over an `OsFs` vault at a per-test tempdir with
//! a hermetic device data directory (the theme store must never touch
//! real user state — the same shape as `note_encoding.rs`), and every
//! outcome below is a real typed core result.

use flavite_api::{
    ExecutionResult, Flavite, Invocation, LocalResult, Origin, RejectionReason, ThemeStore,
    VaultConfig,
};
use flavite_fs::OsFs;
use flavite_vault::actor::VaultActorBuilder;
use flavite_vault::channels::ChannelSet;
use flavite_vault::session::{Session, SessionBuilder};
use std::io::{Cursor, Write};
use std::path::PathBuf;
use std::time::Duration;

struct Fixture {
    flavite: Flavite,
    /// Hermetic device data directory backing the theme store.
    data: PathBuf,
    #[allow(dead_code)]
    temp: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("vault");
    std::fs::create_dir_all(&root).expect("vault root");
    let data = temp.path().join("data");
    std::fs::create_dir_all(&data).expect("data dir");
    let cache = temp.path().join("cache");
    std::fs::create_dir_all(&cache).expect("cache dir");
    let db = cache.join("theme-index.sqlite3");
    drop(flavite_index::open_or_init(&db).expect("index database"));
    let identity =
        flavite_api::VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![3, 1, 4]);
    let actor = VaultActorBuilder::new()
        .with_fs(Box::new(OsFs::new()) as Box<dyn flavite_fs::Fs + Send + Sync>)
        .with_identity(identity.clone())
        .with_index(flavite_index::IndexActor::new(&db, 1).expect("index actor"))
        .build()
        .expect("actor")
        .start()
        .expect("actor start");
    let channels = ChannelSet::new();
    let config = VaultConfig {
        autosave_idle_interval: Duration::from_secs(3600),
        ..VaultConfig::default()
    };
    let session: Session = SessionBuilder::new()
        .with_identity(identity.clone())
        .with_actor(actor)
        .with_buffer_manager(flavite_buffer::manager::BufferManager::new())
        .with_pulse(channels.pulse)
        .with_notice(channels.notice)
        .with_config(config.clone())
        .with_data_dir(&data)
        .build()
        .expect("session");
    let mut flavite = Flavite::open(identity, session, config).expect("facade");
    flavite.set_graph_db_path_for_test(db);
    flavite.set_theme_store_for_test(ThemeStore::at_root(
        Box::new(OsFs::new()),
        data.join("flavite").join("themes"),
    ));
    Fixture {
        flavite,
        data,
        temp,
    }
}

const MANIFEST: &str = r##"{
    "manifest_version": 2,
    "name": "Command Fixture",
    "browser_specific_settings": { "gecko": { "id": "commands@host" } },
    "theme": { "colors": { "frame": "#101010", "tab_background_text": "#eeeeee" } }
}"##;

fn package_bytes() -> Vec<u8> {
    let mut buffer = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("manifest.json", options).expect("entry");
        zip.write_all(MANIFEST.as_bytes()).expect("bytes");
        zip.finish().expect("finish");
    }
    buffer.into_inner()
}

/// Write the fixture package to a real file for `theme.install`.
fn package_file(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("fixture.xpi");
    std::fs::write(&path, package_bytes()).expect("package file");
    path
}

fn invoke(flavite: &Flavite, id: &'static str, args: serde_json::Value) -> ExecutionResult {
    flavite.execute(Invocation::new(
        flavite.command_registry().next_request_id(),
        id,
        args,
        Origin::Gui,
    ))
}

#[test]
fn install_list_and_uninstall_flow_through_the_command_bus() {
    let fixture = fixture();
    let path = package_file(fixture.temp.path());

    let result = invoke(
        &fixture.flavite,
        "theme.install",
        serde_json::json!({ "path": path.display().to_string() }),
    );
    let ExecutionResult::Local(LocalResult::Success(success)) = result else {
        panic!("install must succeed: {result:?}");
    };
    assert_eq!(success.command_id, "theme.install");
    assert_eq!(success.data["theme"]["id"].as_str(), Some("commands@host"));
    assert_eq!(
        success.data["theme"]["name"].as_str(),
        Some("Command Fixture")
    );

    let result = invoke(&fixture.flavite, "theme.list", serde_json::json!({}));
    let ExecutionResult::Local(LocalResult::Success(success)) = result else {
        panic!("list must succeed");
    };
    assert_eq!(success.data["themes"].as_array().map(Vec::len), Some(1));

    // The package landed in the hermetic data directory.
    assert!(
        fixture
            .data
            .join("flavite")
            .join("themes")
            .read_dir()
            .expect("themes dir")
            .count()
            > 0
    );

    let result = invoke(
        &fixture.flavite,
        "theme.uninstall",
        serde_json::json!({ "id": "commands@host" }),
    );
    assert!(matches!(
        result,
        ExecutionResult::Local(LocalResult::Success(_))
    ));
    let result = invoke(&fixture.flavite, "theme.list", serde_json::json!({}));
    let ExecutionResult::Local(LocalResult::Success(success)) = result else {
        panic!("list must succeed");
    };
    assert_eq!(success.data["themes"].as_array().map(Vec::len), Some(0));
}

#[test]
fn install_rejects_invalid_args_and_missing_files_with_typed_results() {
    let fixture = fixture();
    // Stray arguments fail at the registry schema.
    let result = invoke(
        &fixture.flavite,
        "theme.install",
        serde_json::json!({ "path": "/tmp/x.xpi", "force": true }),
    );
    assert!(matches!(result, ExecutionResult::Rejected(_)));
    // A missing file is a typed local error, not a panic.
    let result = invoke(
        &fixture.flavite,
        "theme.install",
        serde_json::json!({ "path": "/nonexistent/theme.xpi" }),
    );
    let ExecutionResult::Local(LocalResult::Error(error)) = result else {
        panic!("missing file must be a local error");
    };
    assert_eq!(error.code, "theme_source_unreadable");
    assert!(error.message.contains("stat failed"));
}

#[test]
fn uninstalling_an_unknown_theme_is_a_typed_not_found() {
    let fixture = fixture();
    let result = invoke(
        &fixture.flavite,
        "theme.uninstall",
        serde_json::json!({ "id": "ghost@host" }),
    );
    let ExecutionResult::Local(LocalResult::Error(error)) = result else {
        panic!("unknown id must be a local error");
    };
    assert_eq!(error.code, "theme_not_found");
}

#[test]
fn theme_commands_are_read_only_gated() {
    let mut fixture = fixture();
    fixture.flavite.set_read_only(true);
    let path = package_file(fixture.temp.path());
    // Themes are now vault state, so install/uninstall are gated on read-only vaults.
    // list should still work on read-only vaults.
    let result = invoke(
        &fixture.flavite,
        "theme.install",
        serde_json::json!({ "path": path.display().to_string() }),
    );
    assert!(matches!(
        result,
        ExecutionResult::Local(LocalResult::Error(_))
    ));
    // Verify the error code is theme_read_only
    if let ExecutionResult::Local(LocalResult::Error(error)) = result {
        assert_eq!(error.code, "theme_read_only");
    } else {
        panic!("Expected Local Error result");
    }
    // list should still work on read-only vaults
    let result = invoke(
        &fixture.flavite,
        "theme.list",
        serde_json::json!({}),
    );
    assert!(matches!(
        result,
        ExecutionResult::Local(LocalResult::Success(_))
    ));
}

#[test]
fn plugin_callers_without_the_themes_capability_are_denied() {
    let fixture = fixture();
    let path = package_file(fixture.temp.path());
    let mut invocation = Invocation::new(
        fixture.flavite.command_registry().next_request_id(),
        "theme.install",
        serde_json::json!({ "path": path.display().to_string() }),
        Origin::Plugin,
    );
    invocation = invocation.with_capabilities(vec![flavite_api::Capability::ReadNotes]);
    let result = fixture.flavite.execute(invocation);
    let ExecutionResult::Rejected(rejected) = result else {
        panic!("a plugin without Themes must be denied");
    };
    assert_eq!(rejected.reason, RejectionReason::CapabilityDenied);

    // Granting Themes lets the same plugin through.
    let mut invocation = Invocation::new(
        fixture.flavite.command_registry().next_request_id(),
        "theme.install",
        serde_json::json!({ "path": path.display().to_string() }),
        Origin::Plugin,
    );
    invocation = invocation.with_capabilities(vec![flavite_api::Capability::Themes]);
    let result = fixture.flavite.execute(invocation);
    assert!(matches!(
        result,
        ExecutionResult::Local(LocalResult::Success(_))
    ));
}
