//! Nightly D14 latency probe: Save replies while a 20k-note VerifyContent
//! reconcile is reading and parsing on the worker thread, plus the RFC-0004
//! S7 self-relative scan speedup (AC-8): identical serial vs 4-thread-pool
//! scan streams and a ≥ 2.0× wall assertion when ≥ 4 threads are usable.
//!
//! The fixture corpus is the parse-heavy editor-note shape (frontmatter,
//! headings, code, a dozen links, ~2 KB each): W-E exists to parallelize
//! the CPU-bound parse stage, and a corpus whose scan time is dominated by
//! MemFs bookkeeping would assert scheduling noise, not the property.
//!
//! Run with `cargo bench -p flavite-vault --bench reconcile_concurrency`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use flavite_fs::{Fs, MemFs};
use flavite_index::IndexActor;
use flavite_vault::actor::{VaultActorBuilder, VaultActorMsg, VaultActorResp};
use flavite_vault::reconcile_worker::{ScanJob, WorkerMessage, scan};
use flavite_vault::save::{DiskPrecondition, SaveRequest};
use flavite_vault::types::{ReconcileMode, ReconcileScope, VaultIdentity};
use rayon::ThreadPoolBuilder;

const NOTE_COUNT: usize = 20_000;
const SAMPLES: usize = 200;

/// A realistic editor-scale note (~2 KB): frontmatter with arrays, headings,
/// lists, code, and a dozen links/tags — the parse-heavy shape whose CPU
/// stage the parallel scan (W-E) serves.
fn bench_note(id: usize) -> String {
    let mut note = String::new();
    note.push_str(&format!(
        "---\nid: I{id}\ntags: [topic-{m1}, shared, project/{m2}]\naliases: [N{id}, note {id}]\nrating: {m1}\n---\n",
        m1 = id % 97,
        m2 = id % 13,
    ));
    note.push_str(&format!("# Note {id}\n\nIntro paragraph for note {id}.\n\n"));
    for section in 0..6 {
        note.push_str(&format!(
            "## Section {section}\n\n- item alpha\n- item beta\n\n```\ncode {id}-{section}\n```\n\n"
        ));
        for link in 0..3 {
            note.push_str(&format!(
                "See [[note-{:05}]] and ![[pic{id}-{link}.png]] plus [[target-{id}-{link}]].\n\n",
                (id * 31 + link * 7) % NOTE_COUNT,
            ));
        }
    }
    note.push_str("trailing #tag inline [[target]]\n");
    note
}

fn main() {
    let temp = tempfile::tempdir().expect("tempdir");
    // History writes must stay inside this benchmark's disposable directory.
    // SAFETY: set once, before any actor or benchmark worker thread is started.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", temp.path().join("xdg-data"));
    }

    let fs = MemFs::new();
    let root = PathBuf::from("/reconcile-bench");
    fs.create_dir_all(&root).expect("vault root");
    for id in 0..NOTE_COUNT {
        let path = root.join(format!("note-{id:05}.md"));
        fs.write_new(&path, bench_note(id).as_bytes())
            .expect("fixture note");
    }
    let target = root.join("target.md");
    fs.write_new(&target, b"# Target\n").expect("target note");
    let db = temp.path().join("index.sqlite3");
    drop(flavite_index::open_or_init(&db).expect("index schema"));
    let note_key = fs
        .encode_path_key(Path::new("target.md"))
        .expect("native key");
    let actor = VaultActorBuilder::new()
        .with_fs(Box::new(fs.clone()) as Box<dyn Fs + Send + Sync>)
        .with_identity(VaultIdentity::new(
            uuid::Uuid::now_v7(),
            root.clone(),
            vec![0x44],
        ))
        .with_index(IndexActor::new(&db, 2).expect("index actor"))
        .build()
        .expect("vault actor")
        .start()
        .expect("start actor");

    let reconcile_started = Instant::now();
    let initial = actor
        .submit_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent)
        .expect("initial reconcile")
        .recv_timeout(Duration::from_secs(600))
        .expect("initial reconcile completion");
    println!(
        "initial_reconcile notes={NOTE_COUNT} wall={:?}",
        reconcile_started.elapsed()
    );
    assert!(matches!(initial, VaultActorResp::ReconcileComplete { .. }));
    let reconcile_started = Instant::now();
    let reconcile = actor
        .submit_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent)
        .expect("measured reconcile");

    let mut bytes = b"# Target\n".to_vec();
    let mut latencies = Vec::with_capacity(SAMPLES);
    let mut overlapping = 0usize;
    let mut reconcile_done = false;
    for request_id in 0..SAMPLES {
        let previous_hash = *blake3::hash(&bytes).as_bytes();
        bytes = format!("# Target\n\nrevision {request_id}\n").into_bytes();
        let request = SaveRequest {
            request_id: request_id as u64 + 1,
            path: target.clone(),
            note_key: note_key.clone(),
            bytes: bytes.clone(),
            merge_base: if request_id == 0 {
                b"# Target\n".to_vec()
            } else {
                format!("# Target\n\nrevision {}\n", request_id - 1).into_bytes()
            },
            precondition: DiskPrecondition::Existing(previous_hash),
            buffer_id: 1,
            revision: request_id as u64 + 1,
        };
        let started = Instant::now();
        let response = actor
            .submit_save(request)
            .expect("admit save")
            .recv_timeout(Duration::from_secs(10))
            .expect("save reply");
        assert!(
            matches!(response, VaultActorResp::Saved(Ok(_))),
            "save failed: {response:?}"
        );
        if !reconcile_done {
            match reconcile.try_recv() {
                Ok(response) => {
                    assert!(matches!(response, VaultActorResp::ReconcileComplete { .. }));
                    reconcile_done = true;
                    println!(
                        "second_reconcile wall={:?} overlapped_saves={}",
                        reconcile_started.elapsed(),
                        request_id + 1
                    );
                }
                Err(crossbeam::channel::TryRecvError::Empty) => overlapping += 1,
                Err(crossbeam::channel::TryRecvError::Disconnected) => reconcile_done = true,
            }
        }
        latencies.push(started.elapsed());
        if reconcile_done {
            break;
        }
    }
    latencies.sort_unstable();
    let percentile =
        |p: usize| latencies[((latencies.len() - 1) * p / 100).min(latencies.len() - 1)];
    println!(
        "reconcile_concurrency notes={NOTE_COUNT} save_samples={} overlap_samples={overlapping} save_reply_ms p50={:.2} p95={:.2} p99={:.2}",
        latencies.len(),
        percentile(50).as_secs_f64() * 1000.0,
        percentile(95).as_secs_f64() * 1000.0,
        percentile(99).as_secs_f64() * 1000.0,
    );
    let _ = actor.send(VaultActorMsg::Shutdown);

    // --- RFC-0004 S7: self-relative scan speedup (AC-8) ---------------------
    // Same filesystem, both execution modes of the shared emitter: the
    // streams and results must match exactly (FR-9), and the wall-clock
    // ratio must show the parallel CPU stage pay for itself.
    let usable = std::thread::available_parallelism()
        .map_or(1, |n| n.get().min(flavite_vault::reconcile_worker::MAX_SCAN_THREADS));
    let (scan_tx, scan_rx) = crossbeam::channel::bounded::<WorkerMessage>(8192);
    let drain = std::thread::spawn(move || {
        let mut collected = 0usize;
        while let Ok(message) = scan_rx.recv() {
            collected += usize::from(!matches!(message, WorkerMessage::Progress(_)));
        }
        collected
    });
    let nominated = HashSet::new();
    let scope = ReconcileScope::Full;
    let job = ScanJob {
        id: 1,
        scope: &scope,
        nominated: &nominated,
    };
    if usable < 4 {
        println!(
            "reconcile_scan speedup assertion skipped: {usable} usable threads (< 4); \
             serial is behaviorally identical via the shared emitter"
        );
    } else {
        let pool = ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .expect("4-thread scan pool");
        let _ = scan(&fs, &root, &job, &scan_tx, None); // warm caches
        let _ = scan(&fs, &root, &job, &scan_tx, Some(&pool)); // warm pool
        let started = Instant::now();
        let serial = scan(&fs, &root, &job, &scan_tx, None);
        let serial_wall = started.elapsed();
        let started = Instant::now();
        let parallel = scan(&fs, &root, &job, &scan_tx, Some(&pool));
        let parallel_wall = started.elapsed();
        assert_eq!(serial, parallel, "FR-9: scan modes diverged");
        assert!(serial.complete && !serial.incomplete, "fixture drift");
        let speedup = serial_wall.as_secs_f64() / parallel_wall.as_secs_f64();
        println!(
            "reconcile_scan notes={NOTE_COUNT} serial_ms={:.1} parallel_ms={:.1} speedup={speedup:.2}x (4 threads)",
            serial_wall.as_secs_f64() * 1000.0,
            parallel_wall.as_secs_f64() * 1000.0,
        );
        assert!(
            speedup >= 2.0,
            "AC-8: parallel scan speedup {speedup:.2}x below 2.0x with a 4-thread pool"
        );
    }
    drop(scan_tx);
    let batches = drain.join().expect("scan drain");
    println!("reconcile_scan non-progress messages={batches}");
}
