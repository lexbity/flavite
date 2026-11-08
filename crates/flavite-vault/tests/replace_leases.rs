//! RFC-0006 S08 leases (§6.9/D-10): the non-replacing source-lease
//! registry and Session-level refusal of mutating commands on leased
//! sources (edit/save/rename), with other sources staying editable.

use flavite_buffer::BufferManager;
use flavite_fs::{Fs, MemFs};
use flavite_index::{IndexActor, open_or_init};
use flavite_vault::channels::ChannelSet;
use flavite_vault::session::{
    CommandResult, OpenPolicy, Session, SessionBuilder, SessionCommand, SessionResult,
};
use flavite_vault::source_leases::{LeaseOwner, SourceLeaseRegistry};
use flavite_vault::types::{VaultConfig, VaultIdentity};
use flavite_vault::{VaultActorBuilder, VaultActorHandle};
use std::path::PathBuf;
use std::time::Duration;

fn harness() -> (Session, MemFs, PathBuf) {
    let fs = MemFs::new();
    let root = PathBuf::from("/test/leases");
    fs.create_dir_all(&root).unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let db = db_dir.path().join("index.sqlite3");
    drop(open_or_init(&db).unwrap());
    let data_dir = tempfile::tempdir().unwrap();
    let identity = VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![1]);
    let actor: VaultActorHandle = VaultActorBuilder::new()
        .with_fs(Box::new(fs.clone()) as Box<dyn Fs + Send + Sync>)
        .with_identity(identity.clone())
        .with_index(IndexActor::new(&db, 1).unwrap())
        .build()
        .unwrap()
        .start()
        .unwrap();
    let channels = ChannelSet::new();
    let session = SessionBuilder::new()
        .with_identity(identity)
        .with_actor(actor)
        .with_buffer_manager(BufferManager::new())
        .with_pulse(channels.pulse)
        .with_notice(channels.notice)
        .with_config(VaultConfig {
            autosave_idle_interval: Duration::from_secs(3600),
            ..VaultConfig::default()
        })
        .with_data_dir(data_dir.path())
        .build()
        .unwrap();
    (session, fs, root)
}

fn wait_terminal(session: &Session, result: SessionResult) -> SessionResult {
    match result {
        SessionResult::Pending { request_id } => session.wait_for_operation(request_id),
        other => other,
    }
}

#[test]
fn registry_is_non_replacing_and_multi_source_ordered() {
    let registry = SourceLeaseRegistry::new();
    let a = registry
        .acquire(b"a.md".to_vec(), LeaseOwner::Replace)
        .unwrap();
    assert!(
        registry
            .acquire(b"a.md".to_vec(), LeaseOwner::Save)
            .is_err()
    );
    let multi = registry.acquire_many(&[
        (b"b.md".to_vec(), LeaseOwner::Replace),
        (b"c.md".to_vec(), LeaseOwner::Replace),
    ]);
    assert!(multi.is_ok());
    // All-or-none: conflict on d releases b/c (already held a stays).
    let conflict = registry.acquire_many(&[
        (b"d.md".to_vec(), LeaseOwner::Edit),
        (b"a.md".to_vec(), LeaseOwner::Edit),
    ]);
    assert!(conflict.is_err());
    assert!(registry.is_leased(b"d.md").is_none());
    assert!(registry.is_leased(b"a.md").is_some());
    registry.release(&a);
    assert!(registry.is_leased(b"a.md").is_none());
    // Stale release is a no-op.
    let stale = registry
        .acquire(b"e.md".to_vec(), LeaseOwner::Replace)
        .unwrap();
    let other = registry.acquire(b"e.md".to_vec(), LeaseOwner::Save);
    assert!(other.is_err());
    registry.release(
        &SourceLeaseRegistry::new()
            .acquire(b"e.md".to_vec(), LeaseOwner::Edit)
            .unwrap(),
    );
    let _ = stale;
}

#[test]
fn session_refuses_save_edit_and_rename_on_a_leased_source() {
    let (session, fs, root) = harness();
    let path_a = root.join("a.md");
    let path_b = root.join("b.md");
    fs.write_new(&path_a, b"alpha").unwrap();
    fs.write_new(&path_b, b"beta").unwrap();
    // Reconcile so buffers open against a real disk base.
    let _ = wait_terminal(
        &session,
        session.execute(SessionCommand::Reconcile {
            scope: flavite_vault::types::ReconcileScope::Full,
            mode: flavite_vault::types::ReconcileMode::VerifyContent,
        }),
    );
    let open_a = wait_terminal(
        &session,
        session.execute(SessionCommand::OpenNote {
            path: path_a.clone(),
            policy: OpenPolicy::CreateIfMissing,
        }),
    );
    let (buffer_id, opened_revision) = match open_a {
        SessionResult::Success(CommandResult::NoteOpened {
            buffer_id,
            revision,
        }) => (buffer_id, revision),
        other => panic!("open failed {other:?}"),
    };
    let _ = wait_terminal(
        &session,
        session.execute(SessionCommand::OpenNote {
            path: path_b.clone(),
            policy: OpenPolicy::CreateIfMissing,
        }),
    );
    // Lease a.md.
    let _lease = session
        .source_leases()
        .acquire(b"a.md".to_vec(), LeaseOwner::Replace)
        .unwrap();
    // Save refused.
    let save = session.execute(SessionCommand::SaveNote {
        path: path_a.clone(),
    });
    assert!(
        matches!(
            save,
            SessionResult::ActionRequired {
                code: "replacement-in-progress",
                ..
            }
        ),
        "{save:?}"
    );
    // Edit refused.
    let edit = session.execute(SessionCommand::ApplyEdits {
        buffer_id,
        revision: opened_revision,
        ops: vec![flavite_buffer::undo::EditOp::new(
            flavite_syntax::text::Span::new(
                flavite_syntax::text::TextByte(0),
                flavite_syntax::text::TextByte(0),
            ),
            "x".into(),
        )],
        undo_policy: flavite_buffer::undo::UndoPolicy::Isolated,
        editor: None,
    });
    assert!(
        matches!(
            edit,
            SessionResult::ActionRequired {
                code: "replacement-in-progress",
                ..
            }
        ),
        "{edit:?}"
    );
    // Rename of the leased source refused; other sources stay editable.
    let rename = session.execute(SessionCommand::RenameNote {
        from: path_a.clone(),
        to: root.join("a2.md"),
        strategy: None,
    });
    assert!(
        matches!(
            rename,
            SessionResult::ActionRequired {
                code: "replacement-in-progress",
                ..
            }
        ),
        "{rename:?}"
    );
    let save_b = session.execute(SessionCommand::SaveNote {
        path: path_b.clone(),
    });
    assert!(
        !matches!(save_b, SessionResult::ActionRequired { .. }),
        "{save_b:?}"
    );
}
