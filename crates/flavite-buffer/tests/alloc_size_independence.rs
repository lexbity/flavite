//! RFC-0011 S2-T3 (NFR-7): allocation size-independence.
//!
//! A 200-keystroke typing session must allocate O(1) w.r.t. document size:
//! per-keystroke allocation counts at 1 KiB vs 1 MiB must scale < 3×.
//! The keystroke path never materializes the full document.
//!
//! Uses a counting global allocator; single-test binary (parallel tests
//! would share the counters).

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

const KEYSTROKES: usize = 200;

fn type_char(model: &mut EditorModel) {
    let at = model.len(); // O(1); never materializes
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

fn allocations_for_session(size: usize) -> usize {
    let doc = "x".repeat(size);
    let mut model = EditorModel::from_text(&doc);
    // Prime lazy statics and callsite decisions.
    for _ in 0..25 {
        type_char(&mut model);
    }
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..KEYSTROKES {
        type_char(&mut model);
    }
    ALLOCATIONS.load(Ordering::Relaxed) - before
}

#[test]
fn keystroke_allocations_do_not_scale_with_document_size() {
    let small = allocations_for_session(1024);
    let huge = allocations_for_session(1024 * 1024);
    eprintln!("allocs per {KEYSTROKES} keystrokes: 1KiB={small} 1MiB={huge}");
    let ratio = huge as f64 / small.max(1) as f64;
    assert!(
        ratio < 3.0,
        "per-keystroke allocations scale with document size: 1KiB={small}, 1MiB={huge} (ratio {ratio:.2} ≥ 3)"
    );
    // Both sessions made the same number of edits.
    assert!(small > 0 && huge > 0);
}
