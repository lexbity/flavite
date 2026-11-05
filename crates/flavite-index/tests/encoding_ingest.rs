//! RFC-0006 S00 — encoding-aware index ingestion (AC-26).
//!
//! Real SQLite ingestion over raw saved bytes: UTF-16 notes decode once under
//! the central codec contract, keep typed encoding/BOM/decode-status metadata,
//! and store SOURCE-byte link spans (never the UTF-8 text offsets). Malformed
//! bytes are a failed/unparsed row — never an empty success. The 3.1-era cache
//! fixture rebuilds safely into the encoding-aware 4.0 layout.

use flavite_index::{IndexActor, open_or_init};
use rusqlite::{Connection, OptionalExtension};
use tempfile::tempdir;

/// Independent std-only UTF-16LE+BOM reference encoder.
fn reference_utf16le_with_bom(text: &str) -> Vec<u8> {
    let mut out = vec![0xFF_u8, 0xFE];
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

fn notes_row(conn: &Connection, path_native: &[u8]) -> (String, i64, String, i64, i64, i64) {
    conn.query_row(
        "SELECT encoding, bom, decode_status, parse_ok, generation, note_id
         FROM notes WHERE path_native = ?1",
        [path_native],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )
    .expect("notes row")
}

#[test]
fn utf16le_bom_note_ingests_source_spans_and_decoded_body() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("index.sqlite3");
    drop(open_or_init(&db).unwrap());
    let text = "# Doc\n\nSee [[target]] now.\n";
    let bytes = reference_utf16le_with_bom(text);
    let mut actor = IndexActor::new(&db, 1).unwrap();
    let (epoch, generation) = actor
        .apply_saved_note(b"doc.md", "doc.md", &bytes, 1, 1)
        .expect("utf16 note ingests");
    assert!(epoch >= 1 && generation >= 1);

    // Typed encoding metadata on the notes row.
    let (encoding, bom, decode_status, parse_ok, _generation, note_id) =
        notes_row(actor.writer().conn(), b"doc.md");
    assert_eq!(encoding, "utf16le");
    assert_eq!(bom, 1);
    assert_eq!(decode_status, "ok");
    assert_eq!(parse_ok, 1);

    // The link's stored span is in SOURCE bytes: every character before the
    // link is ASCII, so the source offset is exactly BOM + 2x the UTF-8 text
    // offset — and decisively NOT the UTF-8 text offsets.
    let text_start = i64::try_from(text.find("[[target]]").unwrap()).unwrap();
    let text_end = text_start + i64::try_from("[[target]]".len()).unwrap();
    let (span_start, span_end): (i64, i64) = actor
        .writer()
        .conn()
        .query_row(
            "SELECT span_start, span_end FROM links WHERE src = ?1",
            [note_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("links row");
    assert_eq!(span_start, 2 + 2 * text_start, "source span start");
    assert_eq!(
        span_end,
        span_start + 2 * (text_end - text_start),
        "source span end"
    );
    assert!(
        span_start > text_start && span_end > text_end,
        "source spans must not be the UTF-8 text offsets ({span_start},{span_end}) vs ({text_start},{text_end})"
    );

    // Search text is the decoded UTF-8 layer, not raw bytes.
    let body: String = actor
        .writer()
        .conn()
        .query_row(
            "SELECT body_text FROM search_docs WHERE note_id = ?1",
            [note_id],
            |row| row.get(0),
        )
        .expect("search doc");
    assert_eq!(body, text);
}

#[test]
fn utf16be_bom_note_ingests_with_matching_metadata() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("index.sqlite3");
    drop(open_or_init(&db).unwrap());
    let text = "# Be\n\nLink to [[other]]\n";
    let mut bytes = vec![0xFE_u8, 0xFF];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    let mut actor = IndexActor::new(&db, 1).unwrap();
    actor
        .apply_saved_note(b"be.md", "be.md", &bytes, 3, 3)
        .expect("utf16be note ingests");
    let (encoding, bom, decode_status, parse_ok, _, _) = notes_row(actor.writer().conn(), b"be.md");
    assert_eq!(encoding, "utf16be");
    assert_eq!(bom, 1);
    assert_eq!(decode_status, "ok");
    assert_eq!(parse_ok, 1);
}

/// Malformed bytes are a typed failed/unparsed row: undecodable, unparsed,
/// unknown encoding — never an empty success (§6.10).
#[test]
fn malformed_bytes_ingest_as_undecodable_not_empty_success() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("index.sqlite3");
    drop(open_or_init(&db).unwrap());
    let malformed: &[u8] = b"\xff\xfe\x00";
    let mut actor = IndexActor::new(&db, 1).unwrap();
    let (epoch, generation) = actor
        .apply_saved_note(b"bad.md", "bad.md", malformed, 5, 5)
        .expect("malformed input is still an indexed row, not an error");
    assert!(epoch >= 1 && generation >= 1);

    let (encoding, bom, decode_status, parse_ok, _, note_id) =
        notes_row(actor.writer().conn(), b"bad.md");
    assert_eq!(encoding, "unknown");
    assert_eq!(bom, 0);
    assert_eq!(decode_status, "undecodable");
    assert_eq!(parse_ok, 0);

    // No search body and no links for an undecodable source.
    let body: Option<String> = actor
        .writer()
        .conn()
        .query_row(
            "SELECT body_text FROM search_docs WHERE note_id = ?1",
            [note_id],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    assert!(
        body.is_none(),
        "undecodable source is never silently indexed"
    );
    let links: i64 = actor
        .writer()
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM links WHERE src = ?1",
            [note_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(links, 0);
}

/// An encoding-only external change (BOM added to identical text) is not a
/// fast path: the content hash changes, the encoding columns update and the
/// generation advances.
#[test]
fn encoding_only_change_invalidates_fast_path_and_updates_columns() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("index.sqlite3");
    drop(open_or_init(&db).unwrap());
    let with_bom: &[u8] = b"\xef\xbb\xbfsame text\n";
    let bom_less: &[u8] = b"same text\n";
    assert_ne!(with_bom, bom_less);

    let mut actor = IndexActor::new(&db, 1).unwrap();
    actor
        .apply_saved_note(b"e.md", "e.md", with_bom, 10, 10)
        .expect("first apply");
    let (_, _, _, _, generation_before, note_id) = notes_row(actor.writer().conn(), b"e.md");
    let hash_before: Vec<u8> = actor
        .writer()
        .conn()
        .query_row(
            "SELECT content_hash FROM notes WHERE note_id = ?1",
            [note_id],
            |row| row.get(0),
        )
        .unwrap();

    actor
        .apply_saved_note(b"e.md", "e.md", bom_less, 11, 11)
        .expect("second apply");

    let (encoding, bom, decode_status, _, generation_after, _) =
        notes_row(actor.writer().conn(), b"e.md");
    assert_eq!(encoding, "utf8");
    assert_eq!(bom, 0, "BOM-less reinterpretation updates the bom column");
    assert_eq!(decode_status, "ok");
    assert!(generation_after > generation_before, "generation advances");

    let hash_after: Vec<u8> = actor
        .writer()
        .conn()
        .query_row(
            "SELECT content_hash FROM notes WHERE note_id = ?1",
            [note_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(
        hash_before, hash_after,
        "an encoding-only change is not a content fast path"
    );
}

/// The old 3.1-era cache (no encoding columns, no codec marker) rebuilds
/// safely into the encoding-aware 4.0 layout instead of being reinterpreted.
#[test]
fn old_schema_cache_rebuilds_with_encoding_columns() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("old.sqlite3");
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        conn.execute_batch("INSERT INTO meta(key, value) VALUES ('schema_version', '3.1');")
            .unwrap();
        // The ten table names the compatibility check counts; their shape is
        // deliberately the old one (no encoding/bom/decode_status columns).
        for table in [
            "CREATE TABLE notes(id INTEGER PRIMARY KEY, path TEXT)",
            "CREATE TABLE attachments(id INTEGER PRIMARY KEY)",
            "CREATE TABLE aliases(id INTEGER)",
            "CREATE TABLE links(id INTEGER)",
            "CREATE TABLE tags(id INTEGER)",
            "CREATE TABLE properties(id INTEGER)",
            "CREATE TABLE ext_facts(id INTEGER)",
            "CREATE TABLE search_docs(id INTEGER)",
            "CREATE TABLE changes(id INTEGER)",
        ] {
            conn.execute_batch(table).unwrap();
        }
    }

    // Rebuild is invisible to the caller: open_or_init just succeeds.
    drop(open_or_init(&db).expect("rebuild must not error"));

    let conn = Connection::open(&db).unwrap();
    let schema_version: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(schema_version, "4.0");
    let codec_version: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'codec_version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        codec_version,
        flavite_encoding::CODEC_VERSION.to_string(),
        "codec marker is recorded on the rebuilt cache"
    );

    let mut statement = conn.prepare("PRAGMA table_info(notes)").unwrap();
    let columns: Vec<String> = statement
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for required in ["encoding", "bom", "decode_status"] {
        assert!(
            columns.iter().any(|name| name == required),
            "rebuilt notes table carries {required}: {columns:?}"
        );
    }
}
