//! RFC-0008 S9 — F6 verdict evidence.
//!
//! The F6 hypothesis (§2): `driving_turn` contention between a keystroke
//! and query/graph snapshot copies. S1 shipped the measurement mechanism —
//! the `edit.lock_wait` span (`wait_us` field) around the keystroke turn's
//! `driving_turn` acquire in `Session::execute`.
//!
//! This probe drives real scripted keystrokes through `Session::execute`
//! (`ApplyEdits`, typing policy — the same session turn the UI path uses)
//! under three conditions:
//!
//! 1. **Quiet** — no other turn-taker: the keystroke's uncontended wait.
//! 2. **Graph snapshot copies** — a background thread hammers
//!    `session.graph_buffer_snapshots()` in a loop: the exact worker copy
//!    the F6 hypothesis names (each copy takes `driving_turn`).
//! 3. **Host frame pump** — the background thread also calls
//!    `session.pump()` (event/completion drain, the per-frame host work
//!    that shares the turn with the graph copy).
//!
//! Printed stats are the evidence for the verdict in RFC-0008 Appendix B.
//! The only assertion is a generous sanity ceiling (p99 < 10 ms): the
//! verdict is decided by the measured distribution, not by a gate.
//!
//! Run: `cargo test -p flavite-vault --test f6_lock_wait_probe -- --nocapture`
//! (not ignored — it is fast; the harness is a real session, no release
//! build needed).

use flavite_buffer::revision::{BufferId, Revision};
use flavite_buffer::undo::{EditOp, UndoPolicy};
use flavite_vault::session::{Session, SessionCommand, SessionResult};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::field::{Field, Visit};
use tracing::{Event, Id, Metadata, Subscriber, span};

/// Records `edit.lock_wait` span `wait_us` field values.
#[derive(Clone)]
struct WaitRecorder {
    inner: Arc<Mutex<RecorderInner>>,
}

/// Captures the `wait_us` field (recorded as `u128` by `Session::execute`).
#[derive(Default)]
struct WaitVisitor {
    wait_us: Option<u128>,
}

impl Visit for WaitVisitor {
    fn record_u128(&mut self, field: &Field, value: u128) {
        if field.name() == "wait_us" {
            self.wait_us = Some(value);
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "wait_us" {
            self.wait_us = Some(value as u128);
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

struct RecorderInner {
    span_names: HashMap<u64, &'static str>,
    next_id: u64,
    waits: Vec<u128>,
}

impl Default for RecorderInner {
    fn default() -> Self {
        Self {
            span_names: HashMap::new(),
            next_id: 1,
            waits: Vec::new(),
        }
    }
}

impl Subscriber for WaitRecorder {
    fn register_callsite(
        &self,
        _metadata: &'static Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::always()
    }

    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, attrs: &span::Attributes<'_>) -> Id {
        let mut inner = self.inner.lock().expect("recorder lock");
        let id = inner.next_id;
        inner.next_id += 1;
        inner.span_names.insert(id, attrs.metadata().name());
        Id::from_u64(id)
    }

    fn record(&self, span_id: &Id, values: &span::Record<'_>) {
        let mut inner = self.inner.lock().expect("recorder lock");
        if inner.span_names.get(&span_id.into_u64()) == Some(&"edit.lock_wait") {
            let mut visitor = WaitVisitor::default();
            values.record(&mut visitor);
            if let Some(us) = visitor.wait_us {
                inner.waits.push(us);
            }
        }
    }

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

    fn event(&self, _event: &Event<'_>) {}

    fn enter(&self, _span: &span::Id) {}

    fn exit(&self, _span: &span::Id) {}

    fn clone_span(&self, span_id: &span::Id) -> span::Id {
        span_id.clone()
    }
}

fn fixture() -> (Session, u64) {
    use flavite_buffer::manager::BufferManager;
    use flavite_fs::{Fs, MemFs};
    use flavite_index::IndexActor;
    use flavite_vault::{ChannelSet, VaultActorBuilder, VaultConfig};
    let fs = MemFs::new();
    let root = std::path::PathBuf::from("/test/f6");
    fs.create_dir_all(&root).unwrap();
    fs.write_new(&root.join("note.md"), b"abc\ndef\n").unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let db = db_dir.path().join("f6.sqlite3");
    drop(flavite_index::open_or_init(&db).unwrap());
    let data_dir = tempfile::tempdir().unwrap();
    let identity =
        flavite_vault::types::VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![73]);
    let actor = VaultActorBuilder::new()
        .with_fs(Box::new(fs))
        .with_identity(identity.clone())
        .with_index(IndexActor::new(&db, 1).unwrap())
        .build()
        .unwrap()
        .start()
        .unwrap();
    let channels = ChannelSet::new();
    let session = Session::new_with_data_dir(
        identity,
        actor,
        BufferManager::new(),
        channels.pulse,
        channels.notice,
        VaultConfig::default(),
        Some(data_dir.path().to_path_buf()),
    );
    let opened = session.execute(SessionCommand::OpenNote {
        path: root.join("note.md"),
        policy: flavite_vault::session::OpenPolicy::ExistingOnly,
    });
    let opened = match opened {
        SessionResult::Pending { request_id } => {
            let deadline = Instant::now() + std::time::Duration::from_secs(10);
            loop {
                session.pump();
                let done = session.wait_for_operation(request_id);
                if !matches!(done, SessionResult::Pending { .. }) {
                    break done;
                }
                assert!(Instant::now() < deadline, "open never finished");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        other => other,
    };
    match opened {
        SessionResult::Success(flavite_vault::session::CommandResult::NoteOpened {
            buffer_id,
            ..
        }) => (session, buffer_id.as_u64()),
        other => panic!("open failed: {other:?}"),
    }
}

/// Type `count` characters at EOF through the real session turn. Returns the
/// recorded `edit.lock_wait` wait_us samples and the final revision (so the
/// next phase starts from the live current revision).
fn type_keystrokes(session: &Session, buffer: u64, count: usize) -> (Vec<u128>, u64) {
    let recorder = WaitRecorder {
        inner: Arc::new(Mutex::new(RecorderInner::default())),
    };
    let mut revision = session
        .shared_buffer_snapshot(BufferId::from(buffer), None)
        .expect("buffer snapshot")
        .revision
        .as_u64();
    tracing::subscriber::with_default(recorder.clone(), || {
        for i in 0..count {
            let at = if i == 0 { 4 } else { 4 + i };
            let edit = EditOp::new(
                flavite_syntax::text::Span::new(
                    flavite_syntax::text::TextByte(at),
                    flavite_syntax::text::TextByte(at),
                ),
                "x".into(),
            );
            let current = session
                .shared_buffer_snapshot(BufferId::from(buffer), None)
                .map(|s| s.revision.as_u64())
                .unwrap_or(u64::MAX);
            let operation = session
                .issue_editor_operation(BufferId::from(buffer), Revision::new(revision), None)
                .unwrap_or_else(|| {
                    panic!("operation ticket (tracked={revision}, current={current})")
                });
            let result = session.execute(SessionCommand::ApplyEdits {
                buffer_id: BufferId::from(buffer),
                revision: Revision::new(revision),
                ops: vec![edit],
                undo_policy: UndoPolicy::Typing,
                editor: None,
                operation,
                unit: None,
            });
            match result {
                SessionResult::Success(flavite_vault::session::CommandResult::EditsApplied {
                    new_revision,
                    ..
                }) => revision = new_revision.as_u64(),
                other => panic!("keystroke failed: {other:?}"),
            }
        }
    });
    (
        recorder.inner.lock().expect("recorder lock").waits.clone(),
        revision,
    )
}

fn percentile(mut samples: Vec<u128>, percent: usize) -> u128 {
    samples.sort_unstable();
    let idx = (samples.len().saturating_sub(1)) * percent / 100;
    samples.get(idx).copied().unwrap_or(0)
}

fn report(name: &str, waits: &[u128]) {
    if waits.is_empty() {
        println!("f6 {name}: no lock_wait samples observed");
        return;
    }
    let sum: u128 = waits.iter().sum();
    let mean = sum as f64 / waits.len() as f64;
    let max = *waits.iter().max().unwrap();
    println!(
        "f6 {name}: n={} mean={mean:.2}us p50={}us p99={}us max={max}us",
        waits.len(),
        percentile(waits.to_vec(), 50),
        percentile(waits.to_vec(), 99),
    );
}

/// The F6 evidence: quiet vs graph-snapshot-copy vs host-pump contention.
#[test]
fn lock_wait_under_graph_snapshot_and_pump_contention() {
    let (session, buffer) = fixture();
    let session = Arc::new(session);

    // Phase 1: quiet (no other turn-taker).
    let (quiet, _phase1_rev) = type_keystrokes(&session, buffer, 60);
    report("quiet", &quiet);

    // Phase 2: the F6 hypothesis — a graph worker thread copying snapshots
    // (each copy takes driving_turn).
    let hammer_session = session.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hammer_stop = stop.clone();
    let hammer = std::thread::spawn(move || {
        let mut copies = 0u64;
        while !hammer_stop.load(std::sync::atomic::Ordering::Relaxed) {
            let snapshots = hammer_session.graph_buffer_snapshots();
            copies += snapshots.len() as u64;
        }
        copies
    });
    let (contended_graph, _phase2_rev) = type_keystrokes(&session, buffer, 60);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let copies = hammer.join().expect("hammer thread");
    report("graph_snapshot_copies", &contended_graph);
    println!("f6 graph_snapshot hammer completed {copies} snapshot copies");

    // Phase 3: host-frame pumping (drain turns) + graph copies — the full
    // worker mix a keystroke competes with in a running app.
    let pump_session = session.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pump_stop = stop.clone();
    let pump = std::thread::spawn(move || {
        let mut turns = 0u64;
        while !pump_stop.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = pump_session.graph_buffer_snapshots();
            pump_session.pump();
            turns += 1;
        }
        turns
    });
    let (contended_pump, _phase3_rev) = type_keystrokes(&session, buffer, 60);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let turns = pump.join().expect("pump thread");
    report("graph_copy_plus_pump", &contended_pump);
    println!("f6 pump+graph hammer completed {turns} pump turns");

    // Sanity ceiling only (the verdict is the distribution, not a gate).
    for (name, waits) in [
        ("quiet", &quiet),
        ("graph_snapshot_copies", &contended_graph),
        ("graph_copy_plus_pump", &contended_pump),
    ] {
        let p99 = percentile(waits.to_vec(), 99);
        assert!(
            p99 < 10_000,
            "f6 {name} p99 {p99}us ≥ 10 ms — contention is pathological; see Appendix B verdict"
        );
    }
}
