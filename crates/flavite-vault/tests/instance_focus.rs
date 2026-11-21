//! Coverage for single-instance advisory locks and focus IPC (instance.rs):
//! exclusive create, release/drop, server↔client FOCUS roundtrip, no-
//! instance refusal, and instance-running detection.

use flavite_vault::instance::{
    AdvisoryLock, FocusIpc, InstanceError, InstanceManager, is_instance_running,
};
use flavite_vault::types::VaultIdentity;

#[test]
fn advisory_lock_is_exclusive_and_release_removes_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("locks/vault.lock");
    assert!(!AdvisoryLock::exists(&path));

    let lock = AdvisoryLock::acquire(&path).expect("first acquire");
    assert!(AdvisoryLock::exists(&path));
    let second = AdvisoryLock::acquire(&path);
    match second {
        Err(InstanceError::AlreadyLocked) => {}
        Err(InstanceError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        other => panic!("expected a lock conflict, got {other:?}"),
    }

    lock.release();
    assert!(!AdvisoryLock::exists(&path));
    // A fresh acquire works after release.
    let _relock = AdvisoryLock::acquire(&path).expect("re-acquire");
}

#[test]
fn advisory_lock_drop_releases_the_lock_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drop.lock");
    {
        let _lock = AdvisoryLock::acquire(&path).expect("acquire");
        assert!(AdvisoryLock::exists(&path));
    }
    assert!(!AdvisoryLock::exists(&path), "drop removes the lock file");
}

#[test]
fn focus_ipc_server_client_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("focus.sock");
    let server = FocusIpc::new(path.clone(), true).expect("server");
    // The server cannot send to itself.
    let err = server.send_focus().expect_err("server send refuses");
    assert!(err.to_string().contains("Server cannot send to itself"));

    let client = FocusIpc::new(path, false).expect("client");
    // A client that is not a server never reports messages.
    assert!(client.check_focus().unwrap().is_empty());
    client.send_focus().expect("focus sent");

    // Server-side non-blocking accept observes the FOCUS message.
    let messages = server.check_focus().expect("check");
    assert_eq!(messages, vec!["FOCUS".to_string()]);
    // Draining again yields nothing.
    assert!(server.check_focus().unwrap().is_empty());
}

#[test]
fn focus_ipc_client_without_server_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing.sock");
    let client = FocusIpc::new(path, false).expect("client");
    let err = client.send_focus().expect_err("no server listening");
    assert!(!err.to_string().is_empty());
}

#[test]
fn instance_manager_focus_and_running_detection() {
    let dir = tempfile::tempdir().unwrap();
    let identity = VaultIdentity::new(
        uuid::Uuid::new_v4(),
        dir.path().to_path_buf(),
        vec![1, 2, 3],
    );
    // Nothing exists yet.
    assert!(!is_instance_running(&identity));

    // The identity tree that hosts the IPC endpoint must exist for the
    // server socket to bind (production creates it during identity init).
    let ipc_path = flavite_vault::identity::instance_ipc_path(&identity.root, identity.uuid);
    std::fs::create_dir_all(ipc_path.parent().unwrap()).unwrap();

    let manager = InstanceManager::new(identity.clone()).expect("manager");
    // The manager holds the IPC endpoint, so focusing succeeds.
    assert!(InstanceManager::try_focus_existing(&identity).is_ok());
    // The server observes the focus message from the real client.
    let messages = manager.check_focus().expect("check focus");
    assert_eq!(messages, vec!["FOCUS".to_string()]);

    // A fresh identity has no endpoint: focus refuses.
    let other = VaultIdentity::new(uuid::Uuid::new_v4(), dir.path().to_path_buf(), vec![9]);
    let err = InstanceManager::try_focus_existing(&other).expect_err("no instance");
    assert!(err.to_string().contains("No existing instance"));

    // The uuid lock alone reports an instance as running.
    let uuid_lock = flavite_vault::identity::uuid_lock_path(&identity.root, identity.uuid);
    let _lock = AdvisoryLock::acquire(&uuid_lock).expect("uuid lock");
    assert!(is_instance_running(&identity));
}
