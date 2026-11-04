//! RFC-0004 S2 — apply-transaction wall bench (AC-2 precursor).
//!
//! 10k-note synthetic vault. Measures `apply_saved_note` transaction wall
//! for the S2 targeted spine and, in the same run, the serial full-walk
//! baseline via the backstop seam (`with_resolver_backstop(.., 1)` makes
//! every apply run the whole-vault `re_resolve_all`, i.e. the pre-S2 cost
//! shape). Tantivy post-commit is excluded from the measurement: document
//! ops are applied to the in-memory writer and `commit()` is deferred by
//! the actor's timer — as reported separately per NFR-1.
//!
//! Run with `cargo bench -p flavite-index --bench apply_and_search`
//! (threshold assertions land in RFC-0004 S8; S2 records the numbers).

use flavite_index::{IndexActor, open_or_init};
use std::path::Path;
use std::time::{Duration, Instant};

const NOTE_COUNT: usize = 10_000;
const SAMPLES: usize = 300;

fn body(index: usize, nonce: usize) -> Vec<u8> {
    format!(
        "# Note {index}\n\ncontent-{index}-{nonce} [[n{}]]\n",
        index % 50
    )
    .into_bytes()
}

fn percentiles(samples: &[Duration]) -> (Duration, Duration) {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let at = |percent: usize| sorted[(sorted.len() - 1) * percent / 100];
    (at(50), at(99))
}

fn build_vault(db: &Path, backstop: u64) -> IndexActor {
    drop(open_or_init(db).expect("schema"));
    let mut actor = IndexActor::with_resolver_backstop(db, 1, backstop)
        .expect("index actor with backstop seam");
    for index in 0..NOTE_COUNT {
        actor
            .apply_saved_note(
                format!("n{index}.md").as_bytes(),
                &format!("n{index}.md"),
                &body(index, 0),
                1,
                1,
            )
            .expect("fixture apply");
    }
    actor
}

fn measure_update_saves(actor: &mut IndexActor, nonce: usize) -> Vec<Duration> {
    let mut samples = Vec::with_capacity(SAMPLES);
    for index in 0..SAMPLES {
        let note = index % NOTE_COUNT;
        let path = format!("n{note}.md");
        let started = Instant::now();
        actor
            .apply_saved_note(path.as_bytes(), &path, &body(note, nonce), 2, 2)
            .expect("measured apply");
        samples.push(started.elapsed());
    }
    samples
}

fn main() {
    let temp = tempfile::tempdir().expect("tempdir");

    let db_targeted = temp.path().join("index-targeted.sqlite3");
    let build_started = Instant::now();
    let mut targeted = build_vault(&db_targeted, flavite_index::RESOLVER_BACKSTOP_GENERATIONS);
    let build_wall = build_started.elapsed();
    let targeted_samples = measure_update_saves(&mut targeted, 1);
    let (targeted_p50, targeted_p99) = percentiles(&targeted_samples);
    let targeted_diagnostics = targeted.resolver_diagnostics();

    let db_full = temp.path().join("index-full.sqlite3");
    let mut full = build_vault(&db_full, 1);
    let full_samples = measure_update_saves(&mut full, 1);
    let (full_p50, full_p99) = percentiles(&full_samples);

    println!(
        "apply_and_search (10k notes): build {build_wall:?}; update-save p50 {targeted_p50:?} p99 {targeted_p99:?} (targeted spine; full_total={}, targeted_total={})",
        targeted_diagnostics.resolve_full_total, targeted_diagnostics.resolve_targeted_total,
    );
    println!(
        "apply_and_search serial baseline (whole-vault walk every apply, same corpus): p50 {full_p50:?} p99 {full_p99:?}"
    );
    assert!(
        targeted_p50 > Duration::ZERO && full_p99 > Duration::ZERO,
        "both series must produce real timings"
    );
    assert!(
        targeted_p99 <= full_p99,
        "targeted spine must not be slower than the full-walk baseline (targeted p99 {targeted_p99:?} > full p99 {full_p99:?})"
    );
    // The search half of this bench lands with RFC-0004 S4 (200-row excerpt
    // assembly wall); S2 delivers the apply half.
}
