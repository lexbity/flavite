//! RFC-0004 S2 — the resolution-equality gate (AC-1, FR-2, §4.2).
//!
//! After any mutation sequence, stored link states (`dst_note`, `dst_att`,
//! `res_state` for every `(src, ord)`) and `id_conflict` flags MUST equal
//! the states produced by a from-scratch `re_resolve_all` + full-table
//! `id_conflict` recompute over the same entity tables. The oracle runs in
//! a rolled-back transaction on the live writer connection, so every
//! intermediate state is checked, not just the final one. Sequences drive
//! the real pool apply paths (`apply_saved_note`, `apply_parsed_note`,
//! `apply_unparsed_note`, `apply_saved_attachment`, `remove_note_path`,
//! `remove_attachment_path`) through the shared spine with a lowered
//! backstop cadence, so targeted passes, backstop walks, and overlay folds
//! all interleave inside every case.

use flavite_index::resolver::Resolver;
use flavite_index::{IndexActor, open_or_init};
use proptest::prelude::*;
use rusqlite::{Connection, OptionalExtension};
use std::collections::BTreeMap;
use std::fmt::Write;

const NOTE_SLOTS: u8 = 6;
const ATT_SLOTS: u8 = 4;

type LinkState = (i64, i32, Option<i64>, Option<i64>, String);
type ConflictState = (i64, String, Option<String>, i64);

fn dump_state(conn: &Connection) -> (Vec<LinkState>, Vec<ConflictState>) {
    let links = {
        let mut statement = conn
            .prepare("SELECT src, ord, dst_note, dst_att, res_state FROM links ORDER BY src, ord")
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<LinkState>, _>>()
            .unwrap()
    };
    let notes = {
        let mut statement = conn
            .prepare(
                "SELECT note_id, path_key, stable_id_key, id_conflict FROM notes ORDER BY note_id",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<Result<Vec<ConflictState>, _>>()
            .unwrap()
    };
    (links, notes)
}

/// Run the from-scratch strategy (full `re_resolve_all` + full-table
/// `id_conflict` recompute) inside a transaction that is rolled back, and
/// assert the stored state already equals it.
fn check_oracle(index: &IndexActor) {
    let conn = index.writer().conn();
    let before = dump_state(conn);
    let tx = conn.unchecked_transaction().unwrap();
    let mut resolver = Resolver::new(&tx);
    resolver.re_resolve_all().unwrap();
    // The pre-S2 full-table statement, verbatim: the oracle of record for
    // conflict flags.
    tx.execute(
        "UPDATE notes SET id_conflict=CASE WHEN stable_id_key IS NOT NULL AND path_key<>(SELECT MIN(n2.path_key) FROM notes n2 WHERE n2.stable_id_key=notes.stable_id_key) THEN 1 ELSE 0 END",
        [],
    )
    .unwrap();
    let after = dump_state(&tx);
    tx.rollback().unwrap();
    assert_eq!(
        before.0, after.0,
        "stored link states differ from from-scratch re-resolution"
    );
    assert_eq!(
        before.1, after.1,
        "stored id_conflict flags differ from from-scratch recompute"
    );
}

fn note_path(slot: u8, variant: u8) -> String {
    let dirs = ["", "folder/", "deep/in/", "other/"];
    format!("{}n{slot}.md", dirs[usize::from(variant % 4)])
}

fn att_path(slot: u8, variant: u8) -> String {
    if variant.is_multiple_of(2) {
        format!("a{slot}.png")
    } else {
        format!("pics/a{slot}.png")
    }
}

/// Deterministic body for one upsert: frontmatter id/alias churn (creating
/// and resolving id conflicts), plus links through every target spelling
/// the scanner stores — bare stem, `.md` suffix, directory-qualified minus-
/// `.md` path, full path, alias text, `id:` form, and a non-`.md`-extension
/// directory stem form that only the stored-stem fetch arm can reach.
fn note_body(slot: u8, target: u8, variant: u8) -> Vec<u8> {
    let mut spellings = vec![
        format!("[[n{}]]", target % NOTE_SLOTS),
        format!("[[n{}.md]]", target % NOTE_SLOTS),
        format!("[[folder/n{}]]", target % NOTE_SLOTS),
        format!("[[folder/n{}.md]]", target % NOTE_SLOTS),
        format!("[[Al{}-{}]]", target % NOTE_SLOTS, target % 4),
        format!("[[id:I{}]]", target % 3),
        format!("[[folder/n{}.png]]", target % NOTE_SLOTS),
        format!("![[a{}.png]]", target % ATT_SLOTS),
        format!("![[pics/a{}.png]]", target % ATT_SLOTS),
    ];
    let mut text = String::new();
    if variant % 4 >= 2 {
        let _ = write!(
            text,
            "---\nid: I{v}\naliases: [Al{s}-{v}]\n---\n",
            v = target % 3,
            s = slot
        );
    }
    let _ = writeln!(text, "# Note {slot}");
    for index in 0..=(variant % 4) {
        let slot_for_link = (slot + index) % NOTE_SLOTS;
        let target_for_link = (target + index) % NOTE_SLOTS;
        let spelling = spellings
            .remove(usize::from((slot_for_link + target_for_link + index) % 9) % spellings.len());
        let _ = writeln!(text, "link-{index} {spelling}");
    }
    let _ = writeln!(text, "body-{slot}-{variant} #tag{}", target % 3);
    text.into_bytes()
}

#[derive(Clone, Copy, Debug)]
struct Op {
    action: u8,
    slot: u8,
    target: u8,
    variant: u8,
}

fn drive(index: &mut IndexActor, files: &mut BTreeMap<u8, (String, Vec<u8>)>, op: Op) {
    let slot = op.slot % NOTE_SLOTS;
    let path = note_path(slot, op.variant);
    match op.action % 8 {
        0 | 1 => {
            let bytes = note_body(slot, op.target, op.variant);
            index
                .apply_saved_note(path.as_bytes(), &path, &bytes, 1, 1)
                .unwrap();
            files.insert(slot, (path, bytes));
        }
        2 => {
            if files.remove(&slot).is_some() {
                index.remove_note_path(path.as_bytes()).unwrap();
            }
        }
        3 => {
            // Rename: remove the old spelling, re-apply the same bytes under
            // a new directory/path variant (D3 pre-image exercises renames).
            if let Some((old_path, bytes)) = files.remove(&slot) {
                index.remove_note_path(old_path.as_bytes()).unwrap();
                index
                    .apply_saved_note(path.as_bytes(), &path, &bytes, 1, 1)
                    .unwrap();
                files.insert(slot, (path, bytes));
            }
        }
        4 => {
            let att = att_path(op.slot % ATT_SLOTS, op.variant);
            let size = u64::from(op.target % 90) + 8;
            let bytes = vec![op.variant; usize::try_from(size).unwrap()];
            index
                .apply_saved_attachment(att.as_bytes(), &att, Some(&bytes), size, 1, 1, true)
                .unwrap();
        }
        5 => {
            index
                .remove_attachment_path(att_path(op.slot % ATT_SLOTS, op.variant).as_bytes())
                .unwrap();
        }
        6 => {
            // Oversized listing: the unparsed path without materializing
            // 32 MiB; same shape as the scan worker's MAX_NOTE_BYTES route.
            index
                .apply_unparsed_note(path.as_bytes(), &path, 4_000_000_000, 1, 1)
                .unwrap();
            files.insert(slot, (path, Vec::new()));
        }
        _ => {
            // Unparseable bytes: the degraded title-only path.
            index
                .apply_saved_note(path.as_bytes(), &path, &[0xff, 0xfe, 0xff], 1, 1)
                .unwrap();
            files.insert(slot, (path, Vec::new()));
        }
    }
    check_oracle(index);
}

fn actor_with_backstop(dir: &std::path::Path, interval: u64) -> IndexActor {
    let db = dir.join("index.sqlite3");
    drop(open_or_init(&db).unwrap());
    IndexActor::with_resolver_backstop(&db, 1, interval).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 5_000,
        ..ProptestConfig::default()
    })]

    /// AC-1: >= 5,000 randomized op sequences; incremental stored state
    /// equals from-scratch full re-resolution after every single op.
    /// Backstop interval 7 interleaves whole-vault walks with targeted
    /// passes inside each sequence.
    #[test]
    fn incremental_equals_full_resolution(
        ops in prop::collection::vec(
            (0u8..8, 0u8..NOTE_SLOTS, 0u8..7, 0u8..8),
            4..15,
        ),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut index = actor_with_backstop(dir.path(), 7);
        let mut files = BTreeMap::new();
        for (action, slot, target, variant) in ops {
            drive(
                &mut index,
                &mut files,
                Op {
                    action,
                    slot,
                    target,
                    variant,
                },
            );
        }
    }
}

/// The seeded deterministic corpus (§6.2 "Tests/Validation"): ten trap
/// families × three variant sequences = 30 seeded cases. Each sequence
/// drives the real apply paths with the oracle checked after every op.
fn scenario(ops: &[Op]) {
    let dir = tempfile::tempdir().unwrap();
    let mut index = actor_with_backstop(dir.path(), 7);
    let mut files = BTreeMap::new();
    for op in ops {
        drive(&mut index, &mut files, *op);
    }
}

fn op(action: u8, slot: u8, target: u8, variant: u8) -> Op {
    Op {
        action,
        slot,
        target,
        variant,
    }
}

#[test]
fn seeded_rename_with_alias() {
    for variant in 0..3u8 {
        scenario(&[
            op(0, 1, 0, 2),
            op(1, 0, 1, 2),
            op(3, 1, 1, 2 + variant),
            op(2, 0, 0, 0),
            op(0, 0, 1, 3),
        ]);
    }
}

#[test]
fn seeded_id_duplication_arrival() {
    for target in 0..3u8 {
        scenario(&[
            op(0, 0, target, 2),
            op(0, 1, target, 2),
            op(0, 2, target, 3),
            op(2, 0, 0, 0),
            op(0, 3, target, 2),
        ]);
    }
}

#[test]
fn seeded_self_link() {
    for slot in 0..3u8 {
        scenario(&[
            op(0, slot, slot, 0),
            op(1, slot, slot, 1),
            op(3, slot, slot, 2),
        ]);
    }
}

#[test]
fn seeded_embed_flip_on_attachment() {
    for variant in 0..3u8 {
        scenario(&[
            op(0, 0, variant, 0),
            op(4, variant, variant, variant),
            op(1, 0, variant, 1),
            op(5, variant, variant, variant),
            op(4, variant, variant, variant + 1),
        ]);
    }
}

#[test]
fn seeded_stem_fallback_arrival() {
    for extra in 0..3u8 {
        scenario(&[
            op(0, 3 + extra, 0, 0),
            op(0, 4, extra, 0),
            op(0, 0, 3 + extra, 1),
            op(3, 0, 0, 2),
            op(0, 0, extra, 0),
        ]);
    }
}

#[test]
fn seeded_alias_churn() {
    for variant in 2..5u8 {
        scenario(&[
            op(0, 0, 0, 2),
            op(0, 1, 0, 0),
            op(1, 1, 0, variant % 4),
            op(2, 0, 0, 0),
            op(0, 0, 0, variant % 4),
        ]);
    }
}

#[test]
fn seeded_parsed_to_unparsed_roundtrip() {
    for variant in 0..3u8 {
        scenario(&[
            op(0, 0, 1, 2),
            op(0, 1, 2, 2),
            op(6, 1, 0, variant % 2),
            op(0, 2, 1, variant + 1),
            op(1, 1, 2, 2),
        ]);
    }
}

#[test]
fn seeded_degraded_note_occupies_target() {
    for slot in 0..3u8 {
        scenario(&[
            op(0, slot, 0, 0),
            op(7, slot, slot, 0),
            op(0, (slot + 1) % NOTE_SLOTS, slot, 0),
            op(1, slot, 0, 1),
        ]);
    }
}

#[test]
fn seeded_id_edit_flip() {
    for target in 0..3u8 {
        scenario(&[
            op(0, 0, 0, 2),
            op(0, 1, 1, 2),
            op(0, 2, target, 1),
            op(1, 0, 1, 2),
            op(1, 1, 0, 2),
        ]);
    }
}

#[test]
fn seeded_attachment_ambiguity() {
    for variant in 0..3u8 {
        scenario(&[
            op(0, 0, 0, variant % 2),
            op(4, 0, 0, 0),
            op(4, 0, 0, 1),
            op(1, 0, 0, 1),
            op(5, 0, 0, variant % 3),
        ]);
    }
}

/// S2 rollback test (§6.2.4): a mid-apply failure must leave the base
/// catalog untouched, drop the overlay, and keep equality across the
/// failure. The fault hook is a SQLite trigger that aborts the targeted
/// pass's conditional UPDATE (the second statement family the apply makes).
#[test]
fn rollback_keeps_base_overlay_and_equality() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = actor_with_backstop(dir.path(), 4096);
    // A source note with an unresolved link, then arm the fault.
    index
        .apply_saved_note(b"source.md", "source.md", b"[[target]]", 1, 1)
        .unwrap();
    check_oracle(&index);
    let before = index.resolver_diagnostics();
    let generation_before = index.current_version().unwrap().1;
    index
        .writer()
        .conn()
        .execute_batch(
            "CREATE TRIGGER fail_resolution BEFORE UPDATE ON links BEGIN SELECT RAISE(ABORT, 'forced resolver failure'); END;",
        )
        .unwrap();
    assert!(
        index
            .apply_saved_note(b"target.md", "target.md", b"# Target", 2, 2)
            .is_err()
    );
    let after_failure = index.resolver_diagnostics();
    // Base catalog, overlay fold, and counters are all commit-only: none of
    // them moved across the rolled-back transaction.
    assert_eq!(after_failure, before);
    assert_eq!(index.current_version().unwrap().1, generation_before);
    let state: String = index
        .writer()
        .conn()
        .query_row(
            "SELECT res_state FROM links WHERE dst_raw='target'",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap()
        .expect("source link row present");
    assert_eq!(state, "unresolved", "failed apply must not persist flips");
    index
        .writer()
        .conn()
        .execute_batch("DROP TRIGGER fail_resolution")
        .unwrap();
    // Next apply is correct: equality holds across the failure.
    index
        .apply_saved_note(b"target.md", "target.md", b"# Target", 2, 2)
        .unwrap();
    check_oracle(&index);
    let progressed = index.resolver_diagnostics();
    assert_eq!(
        progressed.generations_since_full,
        before.generations_since_full + 1
    );
    let flip_logged: i64 = index
        .writer()
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM changes WHERE kind='outgoing' AND entity_id=(SELECT note_id FROM notes WHERE path='source.md')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        flip_logged, 1,
        "the committed flip must log outgoing(source)"
    );
}

/// S2 short-circuit test (D4): a tags/properties-only edit (identity
/// fields unchanged, no fresh resolvable links) emits no `outgoing` rows
/// and skips the affected pass; a link-adding edit still resolves the fresh
/// own-outgoing links. Link states keep satisfying equality either way.
#[test]
fn identity_relevance_short_circuit() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = actor_with_backstop(dir.path(), 4096);
    index
        .apply_saved_note(b"a.md", "a.md", b"---\ntags: [one]\n---\nplain body", 1, 1)
        .unwrap();
    check_oracle(&index);
    let baseline = index.resolver_diagnostics();
    let cursor = flavite_index::ChangeLogReader::new(index.writer().conn())
        .current_cursor()
        .unwrap();

    // Tags + properties only: same path, stem, aliases, id, no links.
    index
        .apply_saved_note(
            b"a.md",
            "a.md",
            b"---\ntags: [two, three]\nrating: 7\n---\nplain body",
            2,
            2,
        )
        .unwrap();
    let after_edit = index.resolver_diagnostics();
    assert_eq!(
        after_edit.resolve_targeted_total, baseline.resolve_targeted_total,
        "identity-irrelevant edit must skip the affected pass"
    );
    assert_eq!(
        after_edit.affected_links_last, baseline.affected_links_last,
        "skipped pass must not visit candidates"
    );
    let outgoing_rows: i64 = index
        .writer()
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM changes WHERE generation > ?1 AND kind='outgoing'",
            [i64::try_from(cursor.generation).unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(outgoing_rows, 0, "no flip ⇒ no outgoing rows");
    check_oracle(&index);

    // Adding a resolvable link: the fresh own-outgoing keys are resolved in
    // the same pass even though the identity is still unchanged.
    index
        .apply_saved_note(
            b"a.md",
            "a.md",
            b"---\ntags: [two, three]\nrating: 7\n---\nplain body [[b]]",
            3,
            3,
        )
        .unwrap();
    let after_link = index.resolver_diagnostics();
    assert_eq!(
        after_link.resolve_targeted_total,
        baseline.resolve_targeted_total + 1,
        "fresh outgoing keys must run the targeted pass"
    );
    check_oracle(&index);
}

/// D13 backstop firing at a lowered cadence (test-only builder): the
/// whole-vault walk runs exactly when `generations_since_full` reaches the
/// configured interval, resets the counter, and keeps equality.
#[test]
fn backstop_fires_at_configured_interval() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = actor_with_backstop(dir.path(), 2);
    for round in 0..6u64 {
        index
            .apply_saved_note(
                format!("n{round}.md").as_bytes(),
                &format!("n{round}.md"),
                format!("# Note {round} [[n0]]").as_bytes(),
                1,
                1,
            )
            .unwrap();
        check_oracle(&index);
    }
    let diagnostics = index.resolver_diagnostics();
    // Cadence 2 fires on applies #3 and #5 (walkthrough in the spine):
    // generations_since_full cycles 0→1→2→(full)→1→2→(full)→1→2.
    assert_eq!(diagnostics.resolve_full_total, 2);
    assert!(diagnostics.generations_since_full <= 2);
    assert!(diagnostics.resolve_targeted_total >= 4);
}
