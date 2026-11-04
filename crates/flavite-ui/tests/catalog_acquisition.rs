//! RFC-0004 S5 — catalog acquisition off the host turn (W-D, D9, R5).
//!
//! Through the real app stack: acquisition never blocks a turn, `Loading`
//! keeps serving the previous snapshot (no picker flicker), the Ready
//! status carries honest provenance, and the Notes/picker surfaces see a
//! usable catalog after reconcile.

use flavite_fs::Fs;
use flavite_ui::app::{CatalogStatus, Message};

#[path = "support/graph_fixture.rs"]
mod graph_fixture;
use graph_fixture::*;

#[test]
fn startup_reconcile_and_refresh_acquire_the_catalog_without_flicker() {
    let (mut app, _fs, _cache) = fixture();
    // Ensure the vault is reconciled so the index holds every note, then
    // drive the pump: tickets settle through Message::Pump only.
    let _ = app.update(Message::RunCoreCommand("vault.reconcile"));
    wait_for(&mut app, "reconcile drained", |app| {
        app.pending_operations() == 0
    });
    // Explicit refresh trigger (same as every existing UI integration
    // pattern): reconcile completion pulses are best-effort `try_send`, so
    // hosts drive convergence through an admitted ticket, never a blocked
    // turn. A startup ticket may have served an earlier generation.
    let _ = app.update(Message::RefreshCatalog);
    wait_for(&mut app, "catalog ready with all notes", |app| {
        matches!(
            app.catalog_status(),
            CatalogStatus::Ready {
                truncated: false,
                ..
            }
        ) && app.catalog().len() == 3
    });
    let CatalogStatus::Ready {
        epoch, generation, ..
    } = app.catalog_status().clone()
    else {
        unreachable!("matched above");
    };
    assert_eq!(epoch, 1);
    assert!(generation >= 3, "three applied notes carry generations");
    assert_eq!(app.catalog_error(), None);

    // Explicit refresh flips the status synchronously (an admission, not a
    // read) while the ready snapshot keeps serving until the new page
    // lands — the no-flicker invariant.
    let _ = app.update(Message::RefreshCatalog);
    assert_eq!(*app.catalog_status(), CatalogStatus::Loading);
    assert_eq!(app.catalog().len(), 3, "Loading never clears the snapshot");
    assert_eq!(app.note_candidates().all().len(), 3);
    wait_for(&mut app, "refreshed catalog ready", |app| {
        matches!(app.catalog_status(), CatalogStatus::Ready { .. })
    });
    assert_eq!(app.catalog().len(), 3);
}

#[test]
fn vault_changed_pulses_coalesce_and_converge_to_a_fresh_page() {
    let (mut app, fs, _cache) = fixture();
    let _ = app.update(Message::RunCoreCommand("vault.reconcile"));
    wait_for(&mut app, "catalog settled", |app| {
        matches!(app.catalog_status(), CatalogStatus::Ready { .. })
    });
    // A new note lands and reconciles; an explicit refresh converges to the
    // fresh page without ever going blank, and coalescing (never two live
    // host-turn reads) is covered by the app state-machine unit tests.
    fs.write_new(
        std::path::Path::new("/test/context-routing/late.md"),
        b"Late arrival.\n",
    )
    .expect("write late note");
    let _ = app.update(Message::RunCoreCommand("vault.reconcile"));
    wait_for(&mut app, "reconcile drained", |app| {
        app.pending_operations() == 0
    });
    let _ = app.update(Message::RefreshCatalog);
    // The admitted ticket lands as Loading; whatever page was served last
    // keeps serving until the fresh page replaces it wholesale
    // (provenance-stamped, never cleared by an in-flight refresh).
    let served_before = app.catalog().len();
    let _ = app.update(Message::Pump);
    assert_eq!(
        app.catalog().len(),
        served_before,
        "a ticket in flight never clears the served snapshot"
    );
    wait_for(&mut app, "refresh converged", |app| {
        matches!(
            app.catalog_status(),
            CatalogStatus::Ready {
                truncated: false,
                ..
            }
        ) && app.catalog().len() == 4
    });
}
