use flavite_index::{IndexActor, open_or_init};
use proptest::prelude::*;

proptest! {
    #[test]
    fn saved_multibyte_notes_always_have_boundary_safe_previews(tail in ".{0,1024}") {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite3");
        drop(open_or_init(&db).unwrap());
        let mut index = IndexActor::new(&db, 1).unwrap();
        let body = format!("# Preview\n{}{}", "界e\u{301}😀".repeat(260), tail);
        index
            .apply_saved_note(b"multibyte.md", "multibyte.md", body.as_bytes(), 1, 1)
            .unwrap();
        let preview: String = index
            .writer()
            .conn()
            .query_row(
                "SELECT preview FROM notes WHERE path_native=?1",
                [b"multibyte.md"],
                |row| row.get(0),
            )
            .unwrap();
        prop_assert!(preview.len() <= 1024);
        prop_assert!(preview.is_char_boundary(preview.len()));
    }
}
