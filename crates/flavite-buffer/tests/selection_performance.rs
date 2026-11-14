//! Reproducible release measurements for FEUX-0002 NFR-1/2/3.
//! Run: cargo test --release -p flavite-buffer --test selection_performance -- --ignored --nocapture
use flavite_buffer::{
    commands::{EditOperation, plan},
    selection::{RawSelection, SelectionSet, SpanEdit},
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::atomic::{AtomicBool, AtomicIsize, Ordering},
    time::{Duration, Instant},
};
struct MeasuredAllocator;
static ACTIVE: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicIsize = AtomicIsize::new(0);
unsafe impl GlobalAlloc for MeasuredAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let result = unsafe { System.alloc(layout) };
        if !result.is_null() && ACTIVE.load(Ordering::Relaxed) {
            LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        result
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if ACTIVE.load(Ordering::Relaxed) {
            LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        }
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let result = unsafe { System.realloc(pointer, layout, size) };
        if !result.is_null() && ACTIVE.load(Ordering::Relaxed) {
            LIVE.fetch_add(size as isize - layout.size() as isize, Ordering::Relaxed);
        }
        result
    }
}
#[global_allocator]
static ALLOCATOR: MeasuredAllocator = MeasuredAllocator;
fn percentile(samples: &mut [Duration], percent: usize) -> Duration {
    samples.sort_unstable();
    samples[(samples.len() - 1) * percent / 100]
}
fn raw(count: usize, stride: usize) -> Vec<RawSelection> {
    (0..count)
        .map(|index| RawSelection {
            id: Some(index as u32),
            anchor: index * stride,
            head: index * stride,
        })
        .collect()
}

#[test]
#[ignore = "release measurements; explicit runner load and environment are recorded"]
fn thousand_samples_report_p95_scaling_and_retained_metadata() {
    if cfg!(debug_assertions) {
        panic!("run with --release");
    }
    let line = "0123456789abcdefé漢\n";
    let text = line.repeat((1024 * 1024) / line.len() + 1);
    let mut seed = 0xfeed_0002_u64;
    for count in [100, 1_000, 10_000] {
        let input = raw(count, line.len());
        LIVE.store(0, Ordering::Relaxed);
        ACTIVE.store(true, Ordering::Relaxed);
        let (set, remap) = SelectionSet::normalize(&text, input.clone(), Some(0)).unwrap();
        drop(remap);
        let metadata = LIVE.load(Ordering::Relaxed);
        ACTIVE.store(false, Ordering::Relaxed);
        assert!(
            metadata >= 0 && metadata as usize <= 128 * count,
            "retained metadata {metadata} at {count}"
        );
        let mut samples = Vec::with_capacity(1_000);
        let cold = Instant::now();
        black_box(plan(&text, &set, &EditOperation::Typing("X".into())).unwrap());
        let cold = cold.elapsed();
        for _ in 0..1_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let operation = if seed & 1 == 0 {
                EditOperation::Typing("X".into())
            } else {
                EditOperation::Delete
            };
            let start = Instant::now();
            let result = black_box(plan(&text, &set, &operation).unwrap());
            samples.push(start.elapsed());
            assert!(!result.after_set.is_empty());
        }
        let p95 = percentile(&mut samples, 95);
        println!(
            "note_bytes={} selections={count} samples=1000 cold_us={} p50_us={} p95_us={} retained_metadata_bytes={metadata}",
            text.len(),
            cold.as_micros(),
            percentile(&mut samples, 50).as_micros(),
            p95.as_micros()
        );
        if std::env::var_os("FLAVITE_REFERENCE_PERF").is_some() {
            assert!(p95 <= Duration::from_micros(if count <= 100 { 16_700 } else { 100_000 }));
        }
    }
    for count in [100, 1_000, 5_000] {
        let measure = |count| {
            let text = "a".repeat(count * 4 + 1);
            let (set, _) = SelectionSet::normalize(&text, raw(count, 4), Some(0)).unwrap();
            let spans: Vec<_> = (0..count)
                .map(|index| SpanEdit::new(index * 4 + 1, index * 4 + 2, 2))
                .collect();
            let mut samples = Vec::with_capacity(1_000);
            for _ in 0..1_000 {
                let start = Instant::now();
                black_box(set.map_through(&spans));
                samples.push(start.elapsed());
            }
            percentile(&mut samples, 50)
        };
        let one = measure(count);
        let two = measure(count * 2);
        println!(
            "mapping n=m={count} median_ns={} doubled_median_ns={} ratio={:.3}",
            one.as_nanos(),
            two.as_nanos(),
            two.as_secs_f64() / one.as_secs_f64()
        );
        if std::env::var_os("FLAVITE_REFERENCE_PERF").is_some() {
            assert!(two <= one * 3);
        }
    }
}
