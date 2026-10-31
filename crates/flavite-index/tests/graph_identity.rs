//! A retired dense-cache identity must never identify a later note in its epoch.
use flavite_index::IndexActor;

#[test]
fn deleting_the_highest_id_and_reopening_does_not_reuse_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.sqlite");
    let mut actor = IndexActor::new(&db, 1).unwrap();
    actor
        .apply_saved_note(b"first.md", "first.md", b"First", 1, 1)
        .unwrap();
    actor
        .apply_saved_note(b"old.md", "old.md", b"Old", 1, 1)
        .unwrap();
    let old: i64 = actor
        .writer()
        .conn()
        .query_row("SELECT note_id FROM notes WHERE path='old.md'", [], |r| {
            r.get(0)
        })
        .unwrap();
    actor.remove_note_path(b"old.md").unwrap();
    drop(actor);
    let mut actor = IndexActor::new(&db, 1).unwrap();
    actor
        .apply_saved_note(b"new.md", "new.md", b"New", 2, 2)
        .unwrap();
    let new: i64 = actor
        .writer()
        .conn()
        .query_row("SELECT note_id FROM notes WHERE path='new.md'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(new > old);
    actor
        .apply_saved_note(b"new.md", "new.md", b"Updated", 3, 3)
        .unwrap();
    let updated: i64 = actor
        .writer()
        .conn()
        .query_row("SELECT note_id FROM notes WHERE path='new.md'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(updated, new, "refresh preserves the entity");
}

#[test]
fn unindexed_source_uses_canonical_same_directory_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.sqlite");
    let mut actor = IndexActor::new(&db, 1).unwrap();
    for path in ["a/Idea.md", "z/Idea.md"] {
        actor
            .apply_saved_note(path.as_bytes(), path, b"Idea", 1, 1)
            .unwrap();
    }
    let catalog = flavite_index::catalog::ResolverCatalog::load(actor.writer().conn()).unwrap();
    let result = catalog.resolve_at_path(
        "idea",
        "Idea",
        false,
        Some(std::path::Path::new("z/Unsaved.md")),
    );
    assert_eq!(result.state, flavite_index::schema::ResState::Ambiguous);
    assert_eq!(result.note.unwrap().path_native, b"z/Idea.md");
}
