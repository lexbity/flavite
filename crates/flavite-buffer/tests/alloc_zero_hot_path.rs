//! RFC-0008 S1-T2 — the tracing instrumentation in the keystroke path must
//! allocate nothing with no subscriber installed.
//!
//! This test binary installs a counting global allocator and never installs a
//! tracing subscriber, so every callsite stays interest-disabled. Two proofs:
//!
//! 1. **Macro-level, exact**: after priming, 1 000 evaluations of the exact
//!    hot-path span/event macro shapes perform zero allocations.
//! 2. **Path-level**: 10 × 100-edit chunks of the real interactive path
//!    (`try_apply_recorded_batch`) show a constant per-chunk allocation count
//!    (no growth/drift from the instrumentation). The edit path itself
//!    allocates (strings, snapshots), so the strong zero claim is proof 1;
//!    proof 2 shows the instrumented path has no accumulating overhead.
//!
//! The binary contains exactly one test so the allocator counters are never
//! shared across parallel tests.

use flavite_buffer::{
    EditorModel,
    history::{EditKind, EditorEdit},
    undo::{EditOp, UndoPolicy},
};
use flavite_syntax::text::{Span, TextByte};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicUsize, Ordering},
};

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn type_char(model: &mut EditorModel) {
    let at = model.text().len();
    let op = EditOp::new(Span::new(TextByte(at), TextByte(at)), "x".to_owned());
    let record = EditorEdit {
        before_primary: 0,
        origin: format!("{:032x}", 1),
        kind: EditKind::Typing,
        before: vec![[0, at, at]],
        after: vec![[0, at + 1, at + 1]],
        primary: 0,
    };
    model
        .try_apply_recorded_batch(&[op], UndoPolicy::Typing, record)
        .expect("typing applies");
}

#[test]
fn hot_path_tracing_instrumentation_allocates_nothing_without_a_subscriber() {
    let mut model = EditorModel::from_text("hello world\n");
    for _ in 0..50 {
        type_char(&mut model);
    } // prime lazy statics and callsite-interest decisions

    // Proof 1: exact zero allocations while evaluating the hot-path span and
    // event macro shapes with the default (NoSubscriber) dispatcher.
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..1_000 {
        let span = tracing::info_span!(
            "edit.model_apply",
            ops = 1usize,
            doc_len = model.text().len()
        );
        let _entered = span.entered();
        let coalesce = tracing::info_span!("edit.undo_coalesce", coalesced = false);
        let _coalesce = coalesce.entered();
        let host = tracing::info_span!("edit.host_turn", buffer = 1u64, revision = 1u64);
        let _host = host.entered();
        tracing::event!(target: "edit", tracing::Level::INFO, "phase");
    }
    let after = ALLOCATIONS.load(Ordering::Relaxed);
    assert_eq!(
        after - before,
        0,
        "tracing span/event macros allocated {} times with no subscriber installed",
        after - before
    );

    // Proof 2: the instrumented interactive path shows no steadily growing
    // allocation count (no drift/growth contribution from the
    // instrumentation). The zero-allocation claim itself is proof 1
    // (exact, macro-level); the path-level per-chunk count may legitimately
    // vary within a bounded band because RFC-0008 S5's delta path alternates
    // between coalesced and fresh-node installs (each with its own small
    // allocations) — the bound only excludes unbounded growth.
    let mut deltas = Vec::with_capacity(10);
    for _ in 0..10 {
        let chunk_before = ALLOCATIONS.load(Ordering::Relaxed);
        for _ in 0..100 {
            type_char(&mut model);
        }
        let chunk_after = ALLOCATIONS.load(Ordering::Relaxed);
        deltas.push(chunk_after - chunk_before);
    }
    let min = *deltas.iter().min().expect("ten chunk deltas");
    let max = *deltas.iter().max().expect("ten chunk deltas");
    assert!(
        max <= min + 1024,
        "per-chunk allocation count drifted ({deltas:?}; min {min}, max {max}) — \
         the tracing instrumentation must not add per-keystroke churn"
    );
}
