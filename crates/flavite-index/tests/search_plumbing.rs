//! RFC-0004 S4 — search-plumbing tests (§6.3, FR-7, AC-5 precursor).
//!
//! * frozen-corpus equality: the pre-S4 assembly algorithm (per-row SQL
//!   lookup + `build_excerpt`) is preserved verbatim as a test-local
//!   reference and must produce identical ids, order, and excerpts to the
//!   batched implementation on the same corpus;
//! * the `has_more` truth table (T = limit−1, limit, limit+1) — the §4.4
//!   sentinel is now a pool-typed field, so a pre-S4 build does not even
//!   compile this file (that is the bug-fix proof at the contract level);
//! * 512-chunk batch boundaries at 513 and 1,024 requested rows;
//! * `SearchDocCache::get_many` equality at 99/100/101/2,000 ids against
//!   per-id `get` (the C11 full-table-scan branch is gone).

use flavite_index::search_docs::SearchDocCache;
use flavite_index::{IndexActor, Query, open_or_init};
use rusqlite::Connection;
use tempfile::tempdir;

/// Excerpt budget from `pool.rs` (unchanged by S4; mirrored for the
/// reference implementation below).
const EXCERPT_MAX_CHARS: usize = 160;

fn reference_find_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let needle_chars: Vec<char> = needle.to_lowercase().chars().collect();
    if needle_chars.is_empty() {
        return None;
    }
    let haystack_chars: Vec<(usize, char)> = haystack.char_indices().collect();
    for start in 0..haystack_chars.len() {
        let mut matched = true;
        for (offset, expected) in needle_chars.iter().enumerate() {
            match haystack_chars.get(start + offset) {
                Some((_, actual)) if actual.to_lowercase().eq(expected.to_lowercase()) => {}
                _ => {
                    matched = false;
                    break;
                }
            }
        }
        if matched {
            return Some(haystack_chars[start].0);
        }
    }
    None
}

fn reference_build_excerpt(body: Option<&str>, terms: &[String]) -> String {
    let Some(body) = body.filter(|body| !body.is_empty()) else {
        return String::new();
    };
    let Some(anchor) = terms
        .iter()
        .filter_map(|term| {
            if term.is_empty() {
                return None;
            }
            reference_find_case_insensitive(body, term)
        })
        .min()
    else {
        return body.chars().take(EXCERPT_MAX_CHARS).collect();
    };
    let anchor = anchor.min(body.len());
    let before_anchor = &body[..anchor];
    let context = EXCERPT_MAX_CHARS / 3;
    let total = before_anchor.chars().count();
    let skip = total.saturating_sub(context);
    let start = before_anchor
        .char_indices()
        .nth(skip)
        .map_or(0, |(index, _)| index);
    let excerpt = &body[start..];
    excerpt.chars().take(EXCERPT_MAX_CHARS).collect()
}

/// The pre-S4 assembly: one query per hit id, old excerpt logic.
fn reference_assemble(
    conn: &Connection,
    note_ids: &[i64],
    terms: &[String],
) -> Vec<(i64, String, String, String)> {
    let mut rows = Vec::with_capacity(note_ids.len());
    for note_id in note_ids {
        let row = conn
            .query_row(
                "SELECT n.note_id, n.path, n.title, d.body_text \
                 FROM notes n LEFT JOIN search_docs d ON d.note_id = n.note_id \
                 WHERE n.note_id=?1",
                [note_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .ok();
        if let Some((note_id, path, title, body)) = row {
            let excerpt = reference_build_excerpt(body.as_deref(), terms);
            rows.push((note_id, path, title, excerpt));
        }
    }
    rows
}

fn indexed_index(count: usize, token: &str) -> (tempfile::TempDir, IndexActor) {
    let dir = tempdir().unwrap();
    let db = dir.path().join("index.sqlite3");
    drop(open_or_init(&db).unwrap());
    let mut actor = IndexActor::new(&db, 1).unwrap();
    for index in 0..count {
        let path = format!("note{index:05}.md");
        let body = format!(
            "# Note {index}\n\n{token} filler words unique{index}\n\
             tail {token} with additional context words to extend the body past the excerpt window boundary padding padding\n"
        );
        actor
            .apply_saved_note(path.as_bytes(), &path, body.as_bytes(), 1, 1)
            .unwrap();
    }
    (dir, actor)
}

#[test]
fn frozen_corpus_matches_the_old_assembly_algorithm() {
    let (_dir, mut actor) = indexed_index(300, "s4token");
    for (query_text, limit) in [("s4token", 300usize), ("s4token", 120), ("unique7", 10)] {
        let terms: Vec<String> = query_text.split_whitespace().map(str::to_owned).collect();
        // Hit order is backend truth on both sides. The page returns at
        // most `limit` rows (the +1 probe drives `has_more` alone), so the
        // reference assembles the same id window.
        let ids = actor
            .evaluate_saved_search(
                &Query::Text(flavite_index::query::TextLeaf::all_fields(query_text)),
                limit.saturating_add(1),
            )
            .map(|p| p.items.into_iter().map(|r| r.note_id).collect::<Vec<_>>())
            .unwrap();
        let window = &ids[..ids.len().min(limit)];
        let expected = reference_assemble(actor.writer().conn(), window, &terms);
        let page = actor
            .evaluate_saved_search(
                &Query::Text(flavite_index::query::TextLeaf::all_fields(query_text)),
                limit,
            )
            .unwrap();
        assert_eq!(
            page.items
                .iter()
                .map(|row| (
                    row.note_id,
                    row.path.clone(),
                    row.title.clone(),
                    row.excerpt.clone()
                ))
                .collect::<Vec<_>>(),
            expected,
            "ids/order/excerpts must equal the pre-S4 reference for {query_text:?} limit {limit}"
        );
    }
}

#[test]
fn has_more_truth_table() {
    let total = 25usize;
    let (_dir, mut actor) = indexed_index(total, "tabletoken");
    for limit in [total - 1, total, total + 1] {
        let page = actor
            .evaluate_saved_search(
                &Query::Text(flavite_index::query::TextLeaf::all_fields("tabletoken")),
                limit,
            )
            .unwrap();
        assert_eq!(
            page.items.len(),
            total.min(limit),
            "items are the requested page at T={total} L={limit}"
        );
        assert_eq!(
            page.has_more,
            total > limit,
            "has_more is true exactly when a limit+1'th row exists (T={total} L={limit})"
        );
        // Scored for text queries (D11: Some, relative-only); ordering is
        // score-descendant, so scores must be non-increasing.
        let scores = page
            .items
            .iter()
            .map(|row| row.score.expect("text pages carry relative scores"))
            .collect::<Vec<_>>();
        assert!(
            scores.windows(2).all(|pair| pair[0] >= pair[1]),
            "hit order must be score-descendant: {scores:?}"
        );
    }
}

#[test]
fn structural_only_pages_have_no_scores_and_a_honest_sentinel() {
    let (_dir, mut actor) = indexed_index(6, "anytext");
    for index in 0..6 {
        let path = format!("tagged{index}.md");
        actor
            .apply_saved_note(
                path.as_bytes(),
                &path,
                b"---\ntags: [s4tag]\n---\nbody s4tagged\n",
                2,
                2,
            )
            .unwrap();
    }
    let query = Query::Tag {
        name: "s4tag".into(),
        hierarchical: false,
    };
    for limit in [5usize, 6, 7] {
        let page = actor.evaluate_saved_search(&query, limit).unwrap();
        let tagged = 6; // tagged0..5 (the six generic notes carry no tag).
        assert_eq!(page.items.len(), tagged.min(limit));
        assert_eq!(page.has_more, tagged > limit);
        assert!(
            page.items.iter().all(|row| row.score.is_none()),
            "structural-only pages carry no relevance score (D11)"
        );
    }
}

#[test]
fn batched_assembly_crosses_512_row_chunks() {
    let total = 1_300usize;
    let (_dir, mut actor) = indexed_index(total, "chunktoken");
    for limit in [513usize, 1_024] {
        let ids = actor
            .evaluate_saved_search(
                &Query::Text(flavite_index::query::TextLeaf::all_fields("chunktoken")),
                limit.saturating_add(1),
            )
            .map(|page| {
                page.items
                    .into_iter()
                    .map(|row| row.note_id)
                    .collect::<Vec<_>>()
            })
            .unwrap();
        assert_eq!(ids.len(), limit + 1, "probe row must be fetched");
        let page = actor
            .evaluate_saved_search(
                &Query::Text(flavite_index::query::TextLeaf::all_fields("chunktoken")),
                limit,
            )
            .unwrap();
        assert_eq!(page.items.len(), limit);
        assert!(page.has_more);
        assert_eq!(
            page.items.iter().map(|row| row.note_id).collect::<Vec<_>>(),
            ids[..limit],
            "batched assembly preserves hit order across chunk boundaries"
        );
    }
}

#[test]
fn get_many_chunks_equal_per_id_gets() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("index.sqlite3");
    let mut conn = open_or_init(&db).unwrap();
    {
        let tx = conn.transaction().unwrap();
        for id in 1..=2_000i64 {
            tx.execute(
                "INSERT INTO notes(note_id,path,path_native,path_key,stem_key,title,size,mtime_ns,scan_ns,generation) VALUES(?1,?2,?2,?2,?2,?2,0,0,0,0)",
                rusqlite::params![id, format!("n{id}.md")],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    let mut cache = SearchDocCache::new(conn);
    for id in 1..=2_000i64 {
        if id == 777 {
            continue; // deliberate hole across chunks.
        }
        let seed = u8::try_from(id % 251).unwrap_or(0);
        let content_hash: Vec<u8> = (0..32u8).map(|byte| seed ^ byte).collect();
        cache.set(id, &content_hash, &format!("body {id}")).unwrap();
    }
    for requested in [99usize, 100, 101, 2_000] {
        let requested_id = i64::try_from(requested).unwrap();
        let ids = (1..=requested_id).collect::<Vec<i64>>();
        let many = cache.get_many(&ids).unwrap();
        assert_eq!(
            many.len(),
            if requested >= 777 {
                requested - 1
            } else {
                requested
            },
            "requested {requested}"
        );
        for id in ids {
            if id == 777 {
                continue;
            }
            let one = cache
                .get(id)
                .unwrap()
                .unwrap_or_else(|| panic!("per-id get must find {id}"));
            let batched = many
                .get(&id)
                .unwrap_or_else(|| panic!("get_many must find {id}"));
            assert_eq!(batched.note_id, one.note_id);
            assert_eq!(batched.body_text, one.body_text);
            assert_eq!(batched.content_hash, one.content_hash);
        }
    }
    assert!(cache.get_many(&[]).unwrap().is_empty());
}
