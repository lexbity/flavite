//! RFC-0004 S6 — the mirror-equivalence gate (AC-3, FR-5).
//!
//! Randomized change operations drive the REAL pool apply paths
//! (`apply_saved_note`, `apply_unparsed_note`, `apply_saved_attachment`,
//! `remove_note_path`, `remove_attachment_path` — through the shared apply
//! spine with its targeted resolution and FR-1 change rows) on a tempdir
//! index. After every delta the mirror-assembled complete view (nodes,
//! edges, occurrences, metadata) must byte-compare against the canonical
//! full consistent read (`full_saved_graph`: read_current + occurrence
//! scans + metadata scan at the same version). Zero diffs, or the gate
//! fails.
//!
//! Scenario tests below encode the known trap shapes the random driver must
//! also survive: rename storms, bulk deletes, stable-id conflict churn,
//! unresolved flip-flops (target created/deleted while others link at it),
//! and a 50-generation burst consumed by ONE synchronize (R10).

use flavite_api::graph_mirror::{GraphMirror, Synchronize, full_saved_graph, parity_findings};
use flavite_index::{IndexActor, open_or_init};
use proptest::prelude::*;
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::fmt::Write as _;

const NOTE_SLOTS: u8 = 6;
const ATT_SLOTS: u8 = 4;

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

/// Deterministic body for one upsert: frontmatter id/alias churn, tags and
/// properties, and links through every spelling the graph must show —
/// resolved notes, attachments as embeds, permanently-unresolved ghosts,
/// and slots that flip unresolved↔resolved as the vault churns.
fn note_body(slot: u8, target: u8, variant: u8) -> Vec<u8> {
    let mut spellings = vec![
        format!("[[n{}]]", target % NOTE_SLOTS),
        format!("[[n{}.md]]", target % NOTE_SLOTS),
        format!("[[folder/n{}]]", target % NOTE_SLOTS),
        format!("[[ghost{}]]", target % 3),
        format!("[[id:I{}]]", target % 3),
        format!("[[Al{}-{}]]", target % NOTE_SLOTS, target % 4),
        format!("![[a{}.png]]", target % ATT_SLOTS),
        format!("![[pics/a{}.png]]", target % ATT_SLOTS),
    ];
    let mut text = String::new();
    if variant % 4 >= 2 {
        let _ = write!(
            &mut text,
            "---\nid: I{v}\naliases: [Al{s}-{v}]\ntags: [x{v}, shared]\nrating: {v}\n---\n",
            v = target % 3,
            s = slot
        );
    }
    let _ = writeln!(text, "# Note {slot}");
    for index in 0..=(variant % 4) {
        let slot_for_link = (slot + index) % NOTE_SLOTS;
        let target_for_link = (target + index) % NOTE_SLOTS;
        let pick = usize::from(slot_for_link + target_for_link + index) % spellings.len();
        let spelling = spellings.remove(pick);
        let _ = writeln!(text, "link-{index} {spelling}");
    }
    let _ = writeln!(text, "body-{slot}-{variant} #tag{t}", t = target % 3);
    text.into_bytes()
}

/// One test harness: real writer actor + consumer read connection + mirror.
struct Harness {
    _dir: tempfile::TempDir,
    actor: IndexActor,
    conn: Connection,
    mirror: GraphMirror,
    files: BTreeMap<u8, (String, Vec<u8>)>,
    atts: BTreeMap<u8, String>,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite3");
        drop(open_or_init(&db).unwrap());
        let actor = IndexActor::new(&db, 1).unwrap();
        let conn = Connection::open(&db).unwrap();
        let mirror = GraphMirror::new(None);
        Self {
            _dir: dir,
            actor,
            conn,
            mirror,
            files: BTreeMap::new(),
            atts: BTreeMap::new(),
        }
    }

    /// Compare the mirror's canonical view against a fresh full consistent
    /// read — byte-equal per AC-3 (the canonical order makes `PartialEq`
    /// the byte comparison; findings make failures readable).
    fn check_view(&mut self, when: &str) {
        let view = self.mirror.view().expect("mirror view");
        let full = full_saved_graph(&self.conn, None).expect("full read");
        let findings = parity_findings(&full, view);
        assert!(findings.is_empty(), "{when} diverged: {findings:?}");
        assert_eq!(view, &full, "{when} not byte-equal");
    }

    /// Drive the mirror to the current version, comparing EVERY produced
    /// canonical view against a fresh full consistent read.
    fn synchronize_and_check(&mut self) {
        let mut guard = 0;
        loop {
            guard += 1;
            assert!(guard < 200, "mirror never caught up");
            match self.mirror.synchronize(&self.conn).expect("synchronize") {
                Synchronize::UpToDate => {
                    self.check_view("up-to-date view");
                    return;
                }
                Synchronize::Delta { .. } => self.check_view("mirror delta"),
                Synchronize::Reset { .. } => {
                    self.mirror.force_reset(&self.conn).expect("force reset");
                    self.check_view("reset rebuild");
                }
            }
        }
    }

    fn apply_note(&mut self, path: String, bytes: Vec<u8>, stamp: i64) {
        self.actor
            .apply_saved_note(path.as_bytes(), &path, &bytes, stamp, stamp)
            .unwrap();
    }

    /// One op of the randomized sequences. `defer` leaves the mirror behind
    /// so the next synchronize spans multiple generations (R10 bursts are
    /// the tested shape, not an escape).
    fn drive(&mut self, op: &Op) {
        let stamp = i64::try_from(self.actor.current_version().unwrap().1 + 1).unwrap();
        let slot = op.slot % NOTE_SLOTS;
        let path = note_path(slot, op.variant);
        match op.action % 8 {
            0 | 1 => {
                let bytes = note_body(slot, op.target, op.variant);
                self.apply_note(path.clone(), bytes.clone(), stamp);
                self.files.insert(slot, (path, bytes));
            }
            2 => {
                if let Some((old_path, _)) = self.files.remove(&slot) {
                    self.actor.remove_note_path(old_path.as_bytes()).unwrap();
                }
            }
            3 => {
                // Rename: remove the old spelling, re-apply under a new
                // directory variant (D3 pre-image, inbound flips, unresolved
                // flip-flops all at once).
                if let Some((old_path, bytes)) = self.files.remove(&slot) {
                    self.actor.remove_note_path(old_path.as_bytes()).unwrap();
                    self.apply_note(path.clone(), bytes, stamp);
                    self.files.insert(slot, (path, Vec::new()));
                }
            }
            4 => {
                let att = att_path(op.slot % ATT_SLOTS, op.variant);
                let size = u64::from(op.target % 90) + 8;
                let bytes = vec![op.variant; usize::try_from(size).unwrap()];
                self.actor
                    .apply_saved_attachment(
                        att.as_bytes(),
                        &att,
                        Some(&bytes),
                        size,
                        stamp,
                        stamp,
                        true,
                    )
                    .unwrap();
                self.atts.insert(op.slot % ATT_SLOTS, att);
            }
            5 => {
                if let Some(att) = self.atts.remove(&(op.slot % ATT_SLOTS)) {
                    self.actor.remove_attachment_path(att.as_bytes()).unwrap();
                }
            }
            6 => {
                // Oversized listing: the unparsed path (no links, degraded
                // metadata shape).
                self.actor
                    .apply_unparsed_note(path.as_bytes(), &path, 4_000_000_000, stamp, stamp)
                    .unwrap();
                self.files.insert(slot, (path, Vec::new()));
            }
            _ => {
                // Unparseable bytes: the degraded title-only path.
                self.apply_note(path.clone(), vec![0xff, 0xfe, 0xff], stamp);
                self.files.insert(slot, (path, Vec::new()));
            }
        }
        if !op.defer {
            self.synchronize_and_check();
        }
    }

    fn metrics_line(&self) -> String {
        let stats = self.mirror.stats();
        format!(
            "deltas={} resets={}",
            stats.deltas_total, stats.resets_total
        )
    }

    /// Churn tripwire: a clean drive must never be mistaken for drift.
    fn assert_no_churn(&self) {
        let stats = self.mirror.stats();
        assert_eq!(
            stats.resets_total, 1,
            "unexpected mirror rebuild — a delta was misclassified as drift: {stats:?}"
        );
        assert_eq!(
            stats.mismatches_total, 0,
            "false drift detected in a clean change sequence: {stats:?}"
        );
    }
}

#[derive(Clone, Debug)]
struct Op {
    action: u8,
    slot: u8,
    target: u8,
    variant: u8,
    defer: bool,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    (
        0u8..8,
        0u8..NOTE_SLOTS,
        0u8..NOTE_SLOTS + 6,
        0u8..16,
        0u8..10,
    )
        .prop_map(|(action, slot, target, variant, defer_roll)| Op {
            action,
            slot,
            target,
            variant,
            // ~20% of ops leave the mirror behind, so synchronizes
            // regularly span multiple generations.
            defer: defer_roll < 2,
        })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 2_000,
        ..ProptestConfig::default()
    })]

    /// AC-3: ≥2,000 randomized change operations; after every delta the
    /// mirror-assembled complete view equals the full consistent read.
    #[test]
    fn mirror_equals_full_read_random_sequences(ops in prop::collection::vec(op_strategy(), 1..25)) {
        let mut harness = Harness::new();
        // Cold mirror must start with a reset (service-start semantics).
        harness.synchronize_and_check();
        assert!(harness.metrics_line().contains("resets=1"), "{}", harness.metrics_line());
        for op in &ops {
            harness.drive(op);
        }
        // Always converge, then compare the settled views one last time.
        harness.synchronize_and_check();
        let view = harness.mirror.view().expect("view");
        let full = full_saved_graph(&harness.conn, None).unwrap();
        prop_assert_eq!(view, &full, "settled view differs");
        prop_assert_eq!(
            view.effective_pre_overlay(),
            full.effective_pre_overlay(),
            "pre-overlay effective batch differs"
        );
        // Churn tripwire: clean sequences must never look like drift. A
        // second reset or ANY mismatch count means a delta was treated as
        // a contract violation (e.g. an edge whose target is created in
        // the same window) — the view would still pass equality after the
        // self-healing rebuild, so equality alone cannot catch it.
        let stats = harness.mirror.stats();
        prop_assert_eq!(
            stats.resets_total,
            1,
            "unexpected mirror rebuilds — a delta window was misclassified as drift"
        );
        prop_assert_eq!(
            stats.mismatches_total,
            0,
            "false drift detected in a clean change sequence"
        );
    }
}

/// Rename storm: every note is moved between directories repeatedly while
/// cross-links and embeds point at the old spellings. Deltas must flip
/// inbound sources (outgoing rows), prune vanished unresolved identities,
/// and re-add them when the rename is undone.
#[test]
fn rename_storm() {
    let mut harness = Harness::new();
    harness.synchronize_and_check();
    for slot in 0..8u8 {
        let path = note_path(slot % NOTE_SLOTS, slot);
        let body = note_body(slot % NOTE_SLOTS, slot % 6, slot);
        let stamp = i64::from(slot) + 1;
        harness.apply_note(path.clone(), body, stamp);
    }
    for round in 1..=20 {
        let slot = u8::try_from(round % 6).unwrap();
        if let Some((old_path, bytes)) = harness.files.remove(&slot) {
            harness.actor.remove_note_path(old_path.as_bytes()).unwrap();
            let new_path = note_path(slot, u8::try_from(round).unwrap());
            let stamp = i64::from(100 + round);
            harness.apply_note(new_path.clone(), bytes, stamp);
            harness.files.insert(slot, (new_path, Vec::new()));
        }
        harness.synchronize_and_check();
    }
    harness.assert_no_churn();
}

/// Bulk delete: create a web of notes/attachments/links, then remove every
/// note; each removal flips inbound links and prunes edges + occurrences +
/// unresolved references.
#[test]
fn bulk_delete() {
    let mut harness = Harness::new();
    harness.synchronize_and_check();
    for slot in 0..NOTE_SLOTS {
        let path = note_path(slot, 2);
        let body = note_body(slot, slot, 2);
        harness.apply_note(path.clone(), body, i64::from(slot) + 1);
        harness.files.insert(slot, (path, Vec::new()));
    }
    for slot in 0..ATT_SLOTS {
        let att = att_path(slot, 0);
        harness
            .actor
            .apply_saved_attachment(att.as_bytes(), &att, Some(&[slot; 12]), 12, 1, 1, true)
            .unwrap();
    }
    harness.synchronize_and_check();
    while let Some((_slot, (path, _))) = harness.files.pop_first() {
        harness.actor.remove_note_path(path.as_bytes()).unwrap();
        harness.synchronize_and_check();
    }
    for slot in 0..ATT_SLOTS {
        harness
            .actor
            .remove_attachment_path(att_path(slot, 0).as_bytes())
            .unwrap();
        harness.synchronize_and_check();
    }
    let snapshot = harness.mirror.view().expect("view");
    assert!(
        snapshot
            .saved
            .nodes
            .iter()
            .all(|n| n.id.kind != flavite_graph::NodeKind::Note)
    );
}

/// Stable-id conflict churn: two notes alternately claim `id: I0`; the
/// resolver's `id_conflict` ranking flips targets between the claimants,
/// and the mirror must follow every `outgoing` flip exactly.
#[test]
fn stable_id_conflict_churn() {
    let mut harness = Harness::new();
    harness.synchronize_and_check();
    for round in 0..12 {
        let holder = u8::try_from(round % 2).unwrap();
        let other = 5 - holder;
        for slot in [holder, other] {
            let stamp = i64::from(10 + round);
            let path = format!("c{slot}.md");
            let body = format!("---\nid: I0\n---\nlinks [[c{}]]\n", (slot + 1) % 6);
            harness.apply_note(path, body.into_bytes(), stamp);
        }
        harness.synchronize_and_check();
    }
    harness.assert_no_churn();
}

/// Unresolved flip-flop: a ghost target repeatedly appears and disappears
/// while a link-bearing note keeps pointing at it. The mirror must add the
/// unresolved node, replace it with the real note, and fall back — each
/// time byte-identically to the full read.
#[test]
fn unresolved_flip_flop() {
    let mut harness = Harness::new();
    harness.synchronize_and_check();
    harness.apply_note(
        "source.md".into(),
        b"see [[flip]] and ![[flip.png]]".to_vec(),
        1,
    );
    harness.synchronize_and_check();
    let snapshot = harness.mirror.view().expect("view");
    assert!(
        snapshot
            .unresolved_nodes
            .iter()
            .any(|node| node.title.contains("[[flip]]")),
        "link target must start unresolved"
    );
    for round in 0..6 {
        if round % 2 == 0 {
            harness.apply_note(
                "flip.md".into(),
                b"# arrived".to_vec(),
                i64::from(20 + round),
            );
            harness
                .actor
                .apply_saved_attachment(
                    b"flip.png",
                    "flip.png",
                    Some(&[round as u8; 9]),
                    9,
                    i64::from(20 + round),
                    i64::from(20 + round),
                    true,
                )
                .unwrap();
        } else {
            harness.actor.remove_note_path(b"flip.md").unwrap();
            harness.actor.remove_attachment_path(b"flip.png").unwrap();
        }
        harness.synchronize_and_check();
        let snapshot = harness.mirror.view().expect("view");
        let note_resolved =
            snapshot.saved.nodes.iter().any(|node| {
                node.id.kind == flavite_graph::NodeKind::Note && node.path == "flip.md"
            });
        assert_eq!(round % 2 == 0, note_resolved);
    }
    harness.actor.remove_note_path(b"source.md").unwrap();
    harness.synchronize_and_check();
    harness.assert_no_churn();
}

/// R10 burst: fifty generations applied while the mirror sleeps; ONE
/// synchronize must consume the whole window and match the full read.
#[test]
fn fifty_generation_burst_in_one_synchronize() {
    let mut harness = Harness::new();
    harness.synchronize_and_check();
    harness.apply_note("anchor.md".into(), b"# anchor #pin".to_vec(), 1);
    harness.synchronize_and_check();
    let before = harness.mirror.stats().deltas_total;
    let start_generation = harness.actor.current_version().unwrap().1;
    for index in 0..50 {
        let path = format!("burst{index}.md");
        let stamp = i64::from(100 + index);
        let body = format!("link [[anchor]] body {index}").into_bytes();
        harness.apply_note(path, body, stamp);
    }
    let end_generation = harness.actor.current_version().unwrap().1;
    assert!(
        end_generation - start_generation >= 50,
        "burst applied {start_generation}..{end_generation}"
    );
    // Exactly one synchronize consumes the whole window.
    let outcome = harness.mirror.synchronize(&harness.conn).unwrap();
    let Synchronize::Delta { version, .. } = outcome else {
        panic!("a 50-generation window must arrive as one delta");
    };
    assert!(version.generation >= end_generation);
    assert_eq!(harness.mirror.stats().deltas_total, before + 1);
    let full = full_saved_graph(&harness.conn, None).unwrap();
    assert_eq!(
        parity_findings(&full, harness.mirror.view().expect("view")),
        Vec::<String>::new()
    );
    assert_eq!(
        harness.mirror.stats().last_delta_entities,
        50,
        "the burst touched 50 fresh notes (anchor edit was consumed earlier)"
    );
    // And UpToDate afterwards.
    assert!(matches!(
        harness.mirror.synchronize(&harness.conn).unwrap(),
        Synchronize::UpToDate
    ));
    harness.assert_no_churn();
}

/// Tags/properties-only edits (identity-irrelevant: the spine skips the
/// affected-link pass and writes no `outgoing` rows) must still refresh
/// mirrored metadata through the note's own `note` row.
#[test]
fn metadata_only_edit_refreshes_mirror() {
    let mut harness = Harness::new();
    harness.synchronize_and_check();
    harness.apply_note(
        "quiet.md".into(),
        b"---\ntags: [first]\nrating: 1\n---\nplain body".to_vec(),
        1,
    );
    harness.synchronize_and_check();
    let node = flavite_graph::NodeId::note(
        harness
            .conn
            .query_row(
                "SELECT note_id FROM notes WHERE path_native=?1",
                [b"quiet.md".as_slice()],
                |r| r.get::<_, i64>(0),
            )
            .unwrap() as u64,
    );
    let tags = harness.mirror.view().expect("view").metadata[&node]
        .tags
        .clone();
    assert_eq!(
        tags.iter().cloned().collect::<Vec<_>>(),
        vec!["first".to_string()]
    );
    harness.apply_note(
        "quiet.md".into(),
        b"---\ntags: [second, third]\nrating: 2\n---\nplain body".to_vec(),
        2,
    );
    harness.synchronize_and_check();
    let tags = harness.mirror.view().expect("view").metadata[&node]
        .tags
        .clone();
    assert_eq!(
        tags.iter().cloned().collect::<Vec<_>>(),
        vec!["second".to_string(), "third".to_string()],
        "the mirrored metadata must track the edit"
    );
    let rating = &harness.mirror.view().expect("view").metadata[&node].properties["rating"];
    assert_eq!(rating.len(), 1);
    harness.assert_no_churn();
}

/// The reset path rebuilds byte-identically to the delta path at the same
/// version (AC-4 "full-read fallback produces identical output").
#[test]
fn force_reset_matches_delta_state() {
    let mut harness = Harness::new();
    harness.synchronize_and_check();
    for index in 0..10 {
        let slot = u8::try_from(index % 4).unwrap();
        harness.drive(&Op {
            action: 0,
            slot,
            target: u8::try_from(index).unwrap(),
            variant: 2,
            defer: false,
        });
    }
    let delta_view = harness.mirror.view().expect("view").to_owned();
    harness.mirror.force_reset(&harness.conn).unwrap();
    let reset_view = harness.mirror.view().expect("view").to_owned();
    assert_eq!(
        parity_findings(&reset_view, &delta_view),
        Vec::<String>::new()
    );
    assert_eq!(delta_view, reset_view);
}

/// DB-recreate signal: a new epoch forces `Reset` and the rebuild matches
/// the fresh full read.
#[test]
fn epoch_change_forces_reset() {
    let mut harness = Harness::new();
    harness.synchronize_and_check();
    harness.apply_note("x.md".into(), b"[[y]]".to_vec(), 1);
    harness.synchronize_and_check();
    let before = harness.mirror.stats().resets_total;
    harness
        .actor
        .writer()
        .conn()
        .execute("UPDATE meta SET value='7' WHERE key='epoch'", [])
        .unwrap();
    match harness.mirror.synchronize(&harness.conn).unwrap() {
        Synchronize::Reset { reason } => {
            assert_eq!(reason, flavite_index::changelog::ResetReason::EpochChanged);
        }
        other => panic!("an epoch change is a reset, got {other:?}"),
    }
    let version = harness.mirror.force_reset(&harness.conn).unwrap();
    assert_eq!(version.epoch, 7);
    assert_eq!(harness.mirror.stats().resets_total, before + 1);
    harness.check_view("epoch reset rebuild");
}
