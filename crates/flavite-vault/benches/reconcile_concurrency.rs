//! Nightly D14 latency probe: Save replies while a 20k-note VerifyContent
//! reconcile is reading and parsing on the worker thread.
//!
//! Run with `cargo bench -p flavite-vault --bench reconcile_concurrency`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use flavite_fs::{Fs, MemFs};
use flavite_index::IndexActor;
use flavite_vault::actor::{VaultActorBuilder, VaultActorResp};
use flavite_vault::save::{DiskPrecondition, SaveRequest};
use flavite_vault::types::{ReconcileMode, ReconcileScope, VaultIdentity};

const NOTE_COUNT: usize = 20_000;
const SAMPLES: usize = 200;

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
        fs.write_new(
            &path,
            format!("# Note {id}\n\nBody {id} with [[target]].\n").as_bytes(),
        )
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

    let initial = actor
        .submit_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent)
        .expect("initial reconcile")
        .recv_timeout(Duration::from_secs(120))
        .expect("initial reconcile completion");
    assert!(matches!(initial, VaultActorResp::ReconcileComplete { .. }));
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
    let _ = actor.send(flavite_vault::actor::VaultActorMsg::Shutdown);
}
