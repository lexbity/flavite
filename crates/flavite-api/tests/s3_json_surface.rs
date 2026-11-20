//! RFC-0011 S3-T3 (FR-9): the JSON command surface is unchanged.
//!
//! `BufferView` (the serialized read surface) keeps its exact shape and
//! carries materialized `String` text; the shared O(1) root stays
//! in-process only. Two checks: (1) a golden for the serialized layout
//! with fixed values, and (2) an end-to-end `get_buffer_view` read from a
//! real vault whose JSON round-trips to the snapshot's fields.

use flavite_api::{Flavite, VaultConfig};
use flavite_buffer::manager::BufferManager;
use flavite_buffer::revision::BufferId;
use flavite_fs::Fs;
use flavite_index::IndexActor;
use flavite_vault::actor::VaultActorBuilder;
use flavite_vault::channels::ChannelSet;
use flavite_vault::session::{Session, SessionCommand, SessionResult};
use std::path::PathBuf;
fn fixture() -> (Flavite, u64, flavite_buffer::revision::Revision, PathBuf, tempfile::TempDir) {
    let fs = flavite_fs::MemFs::new();
    let root = PathBuf::from("/test/s3json");
    fs.create_dir_all(&root).expect("vault root");
    fs.write_new(&root.join("note.md"), b"hello\nworld\n")
        .expect("note");
    let cache = tempfile::tempdir().expect("fixture cache directory");
    let db = cache.path().join("s3json-index.sqlite3");
    drop(flavite_index::open_or_init(&db).expect("index database"));
    let identity = flavite_api::VaultIdentity::new(
        uuid::Uuid::now_v7(),
        root.clone(),
        vec![3, 3, 7],
    );
    let actor = VaultActorBuilder::new()
        .with_fs(Box::new(fs) as Box<dyn Fs + Send + Sync>)
        .with_identity(identity.clone())
        .with_index(IndexActor::new(&db, 1).expect("index actor"))
        .build()
        .expect("vault actor")
        .start()
        .expect("actor start");
    let channels = ChannelSet::new();
    let session = Session::new(
        identity.clone(),
        actor,
        BufferManager::new(),
        channels.pulse,
        channels.notice,
        VaultConfig::default(),
    );
    let opened = session.execute(SessionCommand::OpenNote {
        path: root.join("note.md"),
        policy: flavite_vault::session::OpenPolicy::ExistingOnly,
    });
    let opened = match opened {
        SessionResult::Pending { request_id } => session.wait_for_operation(request_id),
        other => other,
    };
    let (buffer_id, revision) = match opened {
        SessionResult::Success(flavite_vault::session::CommandResult::NoteOpened {
            buffer_id,
            revision,
        }) => (buffer_id, revision),
        other => panic!("open failed: {other:?}"),
    };
    let flavite = Flavite::open(identity, session, VaultConfig::default())
        .expect("flavite facade");
    (flavite, buffer_id.as_u64(), revision, root, cache)
}

#[test]
fn serialized_buffer_view_layout_is_golden() {
    // The exact serialized shape: seven fields, text as a plain String,
    // path vault-relative, source_format flattened to {encoding, bom}.
    let view = flavite_api::BufferView {
        buffer_id: 7,
        revision: 42,
        text: "hello\nworld\n".to_owned(),
        base_hash: vec![1, 2, 3, 4],
        state: flavite_buffer::state::BufferState::Dirty,
        path: PathBuf::from("note.md"),
        source_format: flavite_syntax::SourceFormat::utf8_with_bom(),
    };
    let serialized = serde_json::to_string(&view).expect("serialize BufferView");
    let golden = serde_json::json!({
        "buffer_id": 7,
        "revision": 42,
        "text": "hello\nworld\n",
        "base_hash": [1, 2, 3, 4],
        "state": "dirty",
        "path": "note.md",
        "source_format": { "encoding": "utf8", "bom": true },
    });
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&serialized).expect("parse"),
        golden,
        "serialized BufferView layout drifted from the documented surface"
    );
}

#[test]
fn end_to_end_buffer_view_materializes_and_round_trips() {
    let (flavite, buffer_id, revision, _root, _cache) = fixture();

    let view = flavite
        .get_buffer_view(BufferId::from(buffer_id), None)
        .expect("buffer view");

    // The JSON surface carries materialized text — identical to the live
    // document bytes, at serialization time. (`path` is the buffer's full
    // vault path — the pre-existing surface semantics, unchanged by S3.)
    assert_eq!(view.text, "hello\nworld\n");
    assert_eq!(view.buffer_id, buffer_id);
    assert_eq!(view.revision, revision.as_u64());
    assert_eq!(view.path, _root.join("note.md"));
    assert_eq!(view.state, flavite_buffer::state::BufferState::Clean);
    assert_eq!(view.source_format, flavite_syntax::SourceFormat::utf8());

    // Round-trip: the serialized surface deserializes to the same view.
    let json = serde_json::to_string(&view).expect("serialize BufferView");
    let decoded: flavite_api::BufferView =
        serde_json::from_str(&json).expect("deserialize BufferView");
    assert_eq!(decoded.text, view.text);
    assert_eq!(decoded.buffer_id, view.buffer_id);
    assert_eq!(decoded.revision, view.revision);
    assert_eq!(decoded.path, view.path);
    assert_eq!(decoded.state, view.state);
    assert_eq!(decoded.source_format, view.source_format);
    assert_eq!(decoded.base_hash, view.base_hash);

    // Still exactly the seven documented fields on the wire.
    let value: serde_json::Value = serde_json::from_str(&json).expect("parse value");
    let obj = value.as_object().expect("object");
    let mut keys: Vec<&String> = obj.keys().collect();
    keys.sort();
    assert_eq!(keys, vec!["base_hash", "buffer_id", "path", "revision", "source_format", "state", "text"]);
}
