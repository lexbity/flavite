//! RFC-0005 S4 NFR-6 allocation gate: projection materialization on the
//! keystroke-only publish path performs **exactly one** `O(vault)`
//! construction — the `effective` batch clone — plus `O(dirty)` work. The
//! saved base, metadata, and occurrence runs are shared `Arc`s and must
//! NOT be copied per publish (FR-9).
//!
//! How it is proven (no guesses):
//!
//! * A counting global allocator wraps `System`; only the measured region (a
//!   `build` call from the cached handoff snapshot) is counted. The binary
//!   holds exactly these tests, and a measurement mutex serializes the
//!   measurement windows.
//!
//! * The bound is analytic: [one deep clone of the effective `GraphBatch` +
//!   one resolver-catalog load] (computed from measured record sizes and
//!   counts) × 1.5 + fixed slack. The measured per-publish allocation is the
//!   sum of exactly those two terms plus the small bound target set (O(U),
//!   loop-built per the production code) — verified empirically to coincide.
//!   The pre-F3 path — saved + occurrences + metadata deep clones on top —
//!   exceeds the bound many times over (at 5k notes with 60 links/note:
//!   ~90 MB vs the ~25 MB bound).
//!
//! * The 5k-note gate is `#[ignore]`d and runs under `just bench-scale`
//!   (like `graph_mirror_scale`); a 400-note smoke runs in the default suite
//!   so the gate is exercised on every `cargo test`.

use flavite_api::graph_mirror::{GraphMirror, SavedGraph, Synchronize};
use flavite_graph::NodeKind;
use rusqlite::{Connection, params};
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Serializes measurement windows across this binary's tests (cargo runs
/// them on separate threads; the counting allocator must never see two
/// measured regions interleaved).
static MEASURE_LOCK: Mutex<()> = Mutex::new(());
static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);

struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATED.fetch_add(new_size.saturating_sub(layout.size()), Ordering::Relaxed);
        }
        new_ptr
    }
}

#[global_allocator]
static ALLOCATOR: CountingAlloc = CountingAlloc;

/// Deterministic seed: `notes` notes, each with `links_per_note` resolved
/// outgoing links (the dense edge-heavy shape that dominates clone cost).
fn seed_vault(conn: &Connection, notes: i64, links_per_note: i64) {
    let tx = conn.unchecked_transaction().unwrap();
    {
        let mut statement = tx
            .prepare(
                "INSERT INTO notes(path, path_native, path_key, stem_key, title, id_conflict,
                                   content_hash, size, mtime_ns, scan_ns, parse_ok, generation)
                 VALUES(?1, ?2, ?3, ?4, ?5, 0, ?6, 64, 1, 1, 1, 1)",
            )
            .unwrap();
        for id in 1..=notes {
            let path = format!("n{id}.md");
            let key = flavite_syntax::text::fold_key(&path);
            let hash = blake3::hash(&id.to_le_bytes()).as_bytes().to_vec();
            statement
                .execute(params![
                    path,
                    path.as_bytes(),
                    key,
                    key,
                    format!("Note {id}"),
                    hash
                ])
                .unwrap();
        }
    }
    {
        let mut statement = tx
            .prepare(
                "INSERT INTO links(src, ord, kind, dst_raw, target_key, target_stem,
                                   span_start, span_end, dst_note, res_state)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'resolved')",
            )
            .unwrap();
        for src in 1..=notes {
            for ord in 0..links_per_note {
                let target = (src * 7 + ord) % notes + 1;
                let raw = format!("n{target}");
                let key = flavite_syntax::text::fold_key(&raw);
                statement
                    .execute(params![
                        src,
                        ord,
                        "link",
                        raw,
                        key,
                        key,
                        10_i64 + ord * 100,
                        20_i64 + ord * 100,
                        target
                    ])
                    .unwrap();
            }
        }
    }
    tx.execute("UPDATE meta SET value='1' WHERE key='generation'", [])
        .unwrap();
    tx.commit().unwrap();
}

/// One deep clone of the effective batch (nodes + edges + their heap
/// strings) — the single allowed per-publish `O(V)` construction.
fn effective_clone_estimate(view: &SavedGraph) -> usize {
    let nodes: usize = view
        .saved
        .nodes
        .iter()
        .map(|node| {
            std::mem::size_of::<flavite_graph::GraphNodeRecord>()
                + node.path.len()
                + node.path_native.len()
                + node.native_path.as_os_str().len()
                + node.title.len()
                + node.stable_id.as_ref().map_or(0, String::len)
        })
        .sum();
    let edges = view.saved.edges.len() * std::mem::size_of::<flavite_graph::GraphEdgeRecord>();
    let unresolved_nodes: usize = view
        .unresolved_nodes
        .iter()
        .map(|node| std::mem::size_of::<flavite_graph::GraphNodeRecord>() + node.title.len())
        .sum();
    let unresolved_edges =
        view.unresolved_edges.len() * std::mem::size_of::<flavite_graph::GraphEdgeRecord>();
    nodes + edges + unresolved_nodes + unresolved_edges
}

/// A generous analytic upper bound for one `ResolverCatalog::load` (entity
/// records, folded-key maps, path-suffix map entries, B-tree node
/// overhead): per note ≈ 700 B, per attachment ≈ 400 B, plus a base.
fn resolver_catalog_estimate(view: &SavedGraph) -> usize {
    let notes = view
        .saved
        .nodes
        .iter()
        .filter(|node| node.id.kind == NodeKind::Note)
        .count();
    let attachments = view
        .saved
        .nodes
        .iter()
        .filter(|node| node.id.kind == NodeKind::Attachment)
        .count();
    notes * 700 + attachments * 400 + 256 * 1024
}

/// Measure average allocated bytes for one `build` from the cached snapshot
/// (keystroke-only shape: no scope requests, no session buffers).
fn measure_build_allocations(
    conn: &mut Connection,
    snapshot: &Arc<SavedGraph>,
    runs: usize,
) -> usize {
    let _guard = MEASURE_LOCK.lock().unwrap();
    // Warmup (uncounted): one build so per-process lazy setup is paid
    // before the measurement window.
    let _ = flavite_api::graph_projection::build(conn, snapshot, None, &BTreeMap::new(), None)
        .expect("warmup build");
    ALLOCATED.store(0, Ordering::SeqCst);
    COUNTING.store(true, Ordering::SeqCst);
    for _ in 0..runs {
        let built =
            flavite_api::graph_projection::build(conn, snapshot, None, &BTreeMap::new(), None)
                .expect("measured build")
                .expect("a fresh projection must be produced (previous = None)");
        drop(built);
    }
    COUNTING.store(false, Ordering::SeqCst);
    let total = ALLOCATED.load(Ordering::SeqCst);
    ALLOCATED.store(0, Ordering::SeqCst);
    total / runs
}

fn run_at_scale(notes: i64, links_per_note: i64, runs: usize) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("alloc.sqlite");
    let conn = flavite_index::open_or_init(&db).unwrap();
    conn.pragma_update(None, "synchronous", "OFF").unwrap();
    seed_vault(&conn, notes, links_per_note);
    drop(conn);
    let mut reader = Connection::open(&db).unwrap();
    let mut mirror = GraphMirror::new(None);
    assert!(matches!(
        mirror.synchronize(&reader).unwrap(),
        Synchronize::Reset { .. }
    ));
    mirror.force_reset(&reader).unwrap();
    let snapshot = mirror.view_arc().expect("handoff snapshot");
    let view: &SavedGraph = &snapshot;

    let measured = measure_build_allocations(&mut reader, &snapshot, runs);
    let clone_estimate = effective_clone_estimate(view);
    let resolver_estimate = resolver_catalog_estimate(view);
    let allowed = (clone_estimate + resolver_estimate) * 3 / 2 + 512 * 1024;

    println!(
        "NFR-6 allocation profile at {notes} notes / {} links: measured {measured} B per build \
         (runs={runs}); effective-clone estimate {clone_estimate} B + resolver estimate \
         {resolver_estimate} B ⇒ allowed {allowed} B",
        notes * links_per_note
    );
    assert!(
        measured >= clone_estimate,
        "measurement sanity: a real build must at least clone the effective batch \
         (measured {measured} < estimate {clone_estimate})"
    );
    assert!(
        measured <= allowed,
        "NFR-6 violated: per-publish allocation {measured} B > bound {allowed} B \
         (effective clone + resolver only; the pre-F3 path would add saved + occurrences \
         + metadata deep clones on top)"
    );
}

#[test]
fn projection_materialization_allocates_only_the_effective_batch_smoke() {
    run_at_scale(400, 30, 5);
}

#[test]
#[ignore = "NFR-6 allocation gate at the 5k-note bound: run via `just bench-scale`"]
fn projection_materialization_allocates_only_the_effective_batch_at_scale() {
    run_at_scale(5_000, 60, 3);
}
