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

use flavite_index::{IndexActor, Query, open_or_init};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const NOTE_COUNT: usize = 10_000;
const SAMPLES: usize = 300;

fn body(index: usize, nonce: usize) -> Vec<u8> {
    if std::env::var("HEAVY").is_ok() {
        let mut note = format!(
            "---\nid: I{index}\ntags: [topic-{m}, shared, project/{p}]\naliases: [N{index}, note {index}]\nrating: {m}\n---\n# Note {index}\n\nIntro.\n",
            m = index % 97,
            p = index % 13,
        );
        for section in 0..6 {
            let _ = write!(
                note,
                "## S{section}\n- a\n- b\n\ncode {index}-{section}\n\n"
            );
            for link in 0..3 {
                let _ = writeln!(
                    note,
                    "See [[n{:05}]] ![[pic{index}-{link}.png]] [[tt{index}-{link}]]",
                    (index * 31 + link * 7) % NOTE_COUNT
                );
            }
        }
        note.push_str("trailing #tag [[target]] and [[deep/in/nest]]\n");
        return note.into_bytes();
    }
    format!(
        "# Note {index}\n\nbenchtoken content-{index}-{nonce} [[n{}]]\n",
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

fn build_vault(dir: &Path, name: &str, backstop: u64) -> (PathBuf, IndexActor) {
    // Each vault owns its cache directory: the Tantivy index lives beside
    // the SQLite file, and two actors must never share one directory.
    std::fs::create_dir_all(dir.join(name)).expect("vault dir");
    let db = dir.join(name).join("index.sqlite3");
    drop(open_or_init(&db).expect("schema"));
    let mut actor = IndexActor::with_resolver_backstop(&db, 1, backstop)
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
    (db, actor)
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

    let build_started = Instant::now();
    let (_db_targeted, mut targeted) = build_vault(
        temp.path(),
        "targeted",
        flavite_index::RESOLVER_BACKSTOP_GENERATIONS,
    );
    let build_wall = build_started.elapsed();
    let targeted_samples = measure_update_saves(&mut targeted, 1);
    let (targeted_p50, targeted_p99) = percentiles(&targeted_samples);
    let targeted_diagnostics = targeted.resolver_diagnostics();

    let (_db_full, mut full) = build_vault(temp.path(), "full", 1);
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
    // RFC-0004 S4 half: 200-row excerpt assembly wall on the same 10k
    // corpus (AC-5 precursor). The first call absorbs the deferred tantivy
    // commit; subsequent samples run against a clean writer, isolating the
    // query + batched-assembly path from post-commit cost.
    let mut search_samples = Vec::with_capacity(SAMPLES);
    let probe = Query::Text(flavite_index::query::TextLeaf::all_fields("benchtoken"));
    let warm = Instant::now();
    let page = targeted
        .evaluate_saved_search(&probe, 200)
        .expect("warm search commits pending writer ops");
    let warm_wall = warm.elapsed();
    assert_eq!(page.items.len(), 200);
    assert!(page.has_more, "10k corpus must exceed a 200-row page");
    assert!(
        page.items
            .iter()
            .all(|row| row.score.is_some() && !row.excerpt.is_empty()),
        "text pages carry relative scores and excerpts (D11)"
    );
    for _ in 0..SAMPLES {
        let started = Instant::now();
        let page = targeted
            .evaluate_saved_search(&probe, 200)
            .expect("measured search");
        assert_eq!(page.items.len(), 200);
        search_samples.push(started.elapsed());
    }
    let (search_p50, search_p99) = percentiles(&search_samples);
    println!(
        "apply_and_search search half: 200-row assembly p50 {search_p50:?} p99 {search_p99:?} (first-call commit absorbed separately: {warm_wall:?})"
    );
    assert!(
        search_p99 > Duration::ZERO && search_p50 <= search_p99,
        "search assembly must produce real, ordered timings"
    );
}
