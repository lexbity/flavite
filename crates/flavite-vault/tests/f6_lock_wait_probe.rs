//! RFC-0011 S7 — F6 verdict evidence (rebuild).
//!
//! The F6 hypothesis (RFC-0008 §2): `driving_turn` / `buffer_manager`
//! contention between a keystroke and query/graph snapshot copies. S1 shipped
//! the measurement mechanism — the `edit.lock_wait` span around the
//! keystroke turn's `driving_turn` acquire; S7 extended it with the
//! `buffer_manager_wait` field (accumulated buffer-manager lock waits of the
//! turn body) and rebuilt this probe against the real contention matrix:
//!
//! - **8 buffers** (4×1 MiB, 2×16 MiB, 2×small) — the F6 probe's original
//!   single-buffer setup could not see multi-buffer lock hold patterns.
//! - **Graph snapshot + materialization hammer** (the reader copy pattern).
//! - **Undo/redo interleaves** — the O(1) pointer-swap transitions that
//!   publish roots and contend for the manager lock.
//! - **External transitions** — watcher `Modified` events applied by the
//!   pump (external reload publishing) while keystrokes run.
//! - **Link-verdict workers** — `LinkPrepare` jobs through the link worker
//!   channel (resolver/verdict work reading published snapshot roots).
//! - **1 MiB vs 16 MiB** notes: if any lock hold were O(n), the 16 MiB
//!   phases would show it. The verdict asserts the holds are flat across
//!   document size (O(1) publication), not just small.
//!
//! Printed stats are the evidence for the verdict in RFC-0008 Appendix B.
//! The only assertions are generous sanity ceilings: lock-wait p99 < 10 ms
//! (driving turn) and buffer-manager wait p99 < 5 ms, plus the size-flatness
//! check (16 MiB buffer-manager wait p99 ≤ 10× 1 MiB). The verdict is decided
//! by the measured distribution, not by a gate.
//!
//! Run: `cargo test -p flavite-vault --test f6_lock_wait_probe -- --nocapture`

use flavite_buffer::revision::{BufferId, Revision};
use flavite_buffer::undo::{EditOp, UndoPolicy};
use flavite_vault::link::{ClientToken, LinkIntent, LinkPrepareRequest, StableLinkPolicy};
use flavite_vault::session::{Session, SessionCommand, SessionResult};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::field::{Field, Visit};
use tracing::{Event, Id, Metadata, Subscriber, span};

/// Records `edit.lock_wait` span fields: `wait_us` (driving-turn acquire)
/// and `buffer_manager_wait` (accumulated buffer-manager lock waits).
#[derive(Clone)]
struct WaitRecorder {
    inner: Arc<Mutex<RecorderInner>>,
}

#[derive(Default)]
struct WaitVisitor {
    wait_us: Option<u128>,
    buffer_manager_wait: Option<u128>,
}

impl Visit for WaitVisitor {
    fn record_u128(&mut self, field: &Field, value: u128) {
        match field.name() {
            "wait_us" => self.wait_us = Some(value),
            "buffer_manager_wait" => self.buffer_manager_wait = Some(value),
            _ => {}
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "wait_us" => self.wait_us = Some(value as u128),
            "buffer_manager_wait" => self.buffer_manager_wait = Some(value as u128),
            _ => {}
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

struct RecorderInner {
    span_names: HashMap<u64, &'static str>,
    next_id: u64,
    waits: Vec<u128>,
    buffer_manager_waits: Vec<u128>,
}

impl Default for RecorderInner {
    fn default() -> Self {
        Self {
            span_names: HashMap::new(),
            next_id: 1,
            waits: Vec::new(),
            buffer_manager_waits: Vec::new(),
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
            if let Some(us) = visitor.buffer_manager_wait {
                inner.buffer_manager_waits.push(us);
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

struct Fixture {
    session: Arc<Session>,
    /// buffer_id per slot (0..8).
    buffers: Vec<u64>,
    /// The 16 MiB buffer (slot index).
    big: Vec<u64>,
    /// Watcher event source sender (external transitions).
    watcher: crossbeam::channel::Sender<flavite_vault::coalesce::WatcherEvent>,
    /// MemFs clone for external rewrites.
    fs: flavite_fs::MemFs,
    root: std::path::PathBuf,
}

fn fixture() -> Fixture {
    use flavite_buffer::manager::BufferManager;
    use flavite_fs::{Fs, MemFs};
    use flavite_index::IndexActor;
    use flavite_vault::{ChannelSet, VaultActorBuilder, VaultConfig};
    let fs = MemFs::new();
    let root = std::path::PathBuf::from("/test/f6");
    fs.create_dir_all(&root).unwrap();
    let names = [
        "n1.md", "n2.md", "n3.md", "n4.md", // 4 × 1 MiB
        "b1.md", "b2.md", // 2 × 16 MiB
        "s1.md", "s2.md", // 2 × small
    ];
    for (i, name) in names.iter().enumerate() {
        let bytes = if i < 4 {
            vec![b'x'; 1024 * 1024]
        } else if i < 6 {
            vec![b'x'; 16 * 1024 * 1024]
        } else {
            b"small note\n".to_vec()
        };
        fs.write_new(&root.join(name), &bytes).unwrap();
    }
    let (event_tx, event_rx) = flavite_vault::actor::event_source_channel();
    let db_dir = tempfile::tempdir().unwrap();
    let db = db_dir.path().join("f6.sqlite3");
    drop(flavite_index::open_or_init(&db).unwrap());
    let data_dir = tempfile::tempdir().unwrap();
    let identity =
        flavite_vault::types::VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![73, 74]);
    let actor = VaultActorBuilder::new()
        .with_fs(Box::new(fs.clone()))
        .with_identity(identity.clone())
        .with_index(IndexActor::new(&db, 1).unwrap())
        .with_event_source(event_rx)
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
    let mut buffers = Vec::with_capacity(names.len());
    for name in names {
        let path = root.join(name);
        let opened = session.execute(SessionCommand::OpenNote {
            path: path.clone(),
            policy: flavite_vault::session::OpenPolicy::ExistingOnly,
        });
        let opened = match opened {
            SessionResult::Pending { request_id } => {
                let deadline = Instant::now() + std::time::Duration::from_secs(20);
                loop {
                    session.pump();
                    let done = session.wait_for_operation(request_id);
                    if !matches!(done, SessionResult::Pending { .. }) {
                        break done;
                    }
                    assert!(Instant::now() < deadline, "open never finished: {name}");
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
            other => other,
        };
        match opened {
            SessionResult::Success(flavite_vault::session::CommandResult::NoteOpened {
                buffer_id,
                ..
            }) => buffers.push(buffer_id.as_u64()),
            other => panic!("open {name} failed: {other:?}"),
        }
    }
    let big = vec![buffers[4], buffers[5]];
    Fixture {
        session: Arc::new(session),
        buffers,
        big,
        watcher: event_tx,
        fs,
        root,
    }
}

fn type_keystrokes(
    session: &Session,
    buffer: u64,
    count: usize,
    recorder: &WaitRecorder,
) -> (Vec<u128>, Vec<u128>) {
    let mut revision = session
        .shared_buffer_snapshot(BufferId::from(buffer), None)
        .expect("buffer snapshot")
        .revision
        .as_u64();
    tracing::subscriber::with_default(recorder.clone(), || {
        for i in 0..count {
            // Fixed near-start offsets (the original probe's proven pattern):
            // always in bounds, independent of document size.
            let at = if i == 0 { 4 } else { 4 + i };
            let edit = EditOp::new(
                flavite_syntax::text::Span::new(
                    flavite_syntax::text::TextByte(at),
                    flavite_syntax::text::TextByte(at),
                ),
                "x".into(),
            );
            let operation = session
                .issue_editor_operation(BufferId::from(buffer), Revision::new(revision), None)
                .unwrap_or_else(|| panic!("operation ticket (tracked={revision})"));
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
    let inner = recorder.inner.lock().expect("recorder lock");
    (inner.waits.clone(), inner.buffer_manager_waits.clone())
}

fn run_undo_redo_cycles(
    session: &Session,
    buffer: u64,
    count: usize,
    recorder: &WaitRecorder,
) -> (Vec<u128>, Vec<u128>) {
    let mut revision = session
        .shared_buffer_snapshot(BufferId::from(buffer), None)
        .expect("buffer snapshot")
        .revision
        .as_u64();
    tracing::subscriber::with_default(recorder.clone(), || {
        for i in 0..count {
            match i % 3 {
                0 => {
                    // A keystroke (then undo then redo: pointer swaps + publish).
                    let at = 4 + i;
                    let edit = EditOp::new(
                        flavite_syntax::text::Span::new(
                            flavite_syntax::text::TextByte(at),
                            flavite_syntax::text::TextByte(at),
                        ),
                        "y".into(),
                    );
                    let operation = session
                        .issue_editor_operation(
                            BufferId::from(buffer),
                            Revision::new(revision),
                            None,
                        )
                        .unwrap_or_else(|| panic!("operation ticket (tracked={revision})"));
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
                        SessionResult::Success(
                            flavite_vault::session::CommandResult::EditsApplied {
                                new_revision,
                                ..
                            },
                        ) => revision = new_revision.as_u64(),
                        other => panic!("keystroke failed: {other:?}"),
                    }
                }
                1 => {
                    let operation = session
                        .issue_editor_operation(
                            BufferId::from(buffer),
                            Revision::new(revision),
                            None,
                        )
                        .unwrap_or_else(|| panic!("undo ticket"));
                    let result = session.execute(SessionCommand::Undo {
                        buffer_id: BufferId::from(buffer),
                        revision: Revision::new(revision),
                        operation,
                    });
                    match result {
                        SessionResult::Success(
                            flavite_vault::session::CommandResult::UndoPerformed {
                                new_revision,
                                ..
                            },
                        ) => revision = new_revision.as_u64(),
                        other => panic!("undo failed: {other:?}"),
                    }
                }
                _ => {
                    let operation = session
                        .issue_editor_operation(
                            BufferId::from(buffer),
                            Revision::new(revision),
                            None,
                        )
                        .unwrap_or_else(|| panic!("redo ticket"));
                    let result = session.execute(SessionCommand::Redo {
                        buffer_id: BufferId::from(buffer),
                        revision: Revision::new(revision),
                        operation,
                    });
                    match result {
                        SessionResult::Success(
                            flavite_vault::session::CommandResult::RedoPerformed {
                                new_revision,
                                ..
                            },
                        ) => revision = new_revision.as_u64(),
                        other => panic!("redo failed: {other:?}"),
                    }
                }
            }
        }
    });
    let inner = recorder.inner.lock().expect("recorder lock");
    (inner.waits.clone(), inner.buffer_manager_waits.clone())
}

fn percentile(mut samples: Vec<u128>, percent: usize) -> u128 {
    samples.sort_unstable();
    let idx = (samples.len().saturating_sub(1)) * percent / 100;
    samples.get(idx).copied().unwrap_or(0)
}

fn report(name: &str, waits: &[u128], buffer_manager_waits: &[u128]) {
    println!(
        "f6 {name}: lock_wait n={} p50={}us p99={}us max={}us",
        waits.len(),
        percentile(waits.to_vec(), 50),
        percentile(waits.to_vec(), 99),
        waits.iter().max().copied().unwrap_or(0),
    );
    println!(
        "f6 {name}: buffer_manager_wait n={} p50={}us p99={}us max={}us",
        buffer_manager_waits.len(),
        percentile(buffer_manager_waits.to_vec(), 50),
        percentile(buffer_manager_waits.to_vec(), 99),
        buffer_manager_waits.iter().max().copied().unwrap_or(0),
    );
}

/// A reader hammer: graph snapshot copies + materialization (the F6 reader
/// pattern). Returns the materialization count.
fn spawn_reader_hammer(session: &Arc<Session>) -> (Arc<AtomicBool>, std::thread::JoinHandle<u64>) {
    let hammer_session = session.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let hammer_stop = stop.clone();
    let hammer = std::thread::spawn(move || {
        let mut materializations = 0u64;
        while !hammer_stop.load(Ordering::Relaxed) {
            for snapshot in hammer_session.graph_buffer_snapshots() {
                let _ = snapshot.text.materialize();
                materializations += 1;
            }
        }
        materializations
    });
    (stop, hammer)
}

/// Spawn a background pump+graph hammer (host-frame pattern).
fn spawn_pump_hammer(session: &Arc<Session>) -> (Arc<AtomicBool>, std::thread::JoinHandle<u64>) {
    let pump_session = session.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let pump_stop = stop.clone();
    let pump = std::thread::spawn(move || {
        let mut turns = 0u64;
        while !pump_stop.load(Ordering::Relaxed) {
            let _ = pump_session.graph_buffer_snapshots();
            pump_session.pump();
            turns += 1;
        }
        turns
    });
    (stop, pump)
}

/// Spawn a background external-transition producer: rewrites a dedicated
/// note on the MemFs, fires a watcher `Modified`, and pumps the session
/// (external reload publishing a new root).
fn spawn_external_transition_producer(
    session: &Arc<Session>,
    fs: flavite_fs::MemFs,
    path: std::path::PathBuf,
    watcher: crossbeam::channel::Sender<flavite_vault::coalesce::WatcherEvent>,
) -> (Arc<AtomicBool>, std::thread::JoinHandle<u64>) {
    use flavite_fs::Fs;
    let producer_session = session.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let producer_stop = stop.clone();
    let producer = std::thread::spawn(move || {
        let mut seq = 0u64;
        while !producer_stop.load(Ordering::Relaxed) {
            let content = format!("external revision {seq}\n");
            // MemFs has no overwrite primitive; rewrite via temp + atomic
            // rename_replace (the same mechanical the saves use).
            let tmp = path.with_extension(format!("tmp{seq}"));
            if fs.write_new(&tmp, content.as_bytes()).is_ok()
                && fs.rename_replace(&tmp, &path).is_ok()
            {
                let _ = watcher.send(flavite_vault::coalesce::WatcherEvent::Modified(
                    path.clone(),
                ));
                producer_session.pump();
            }
            seq += 1;
        }
        seq
    });
    (stop, producer)
}

/// Spawn a background link-prepare worker: submits LinkPrepare jobs through
/// the same link-worker channel the app uses (resolver/verdict workers
/// reading published snapshot roots).
fn spawn_link_verdict_hammer(
    session: &Arc<Session>,
    buffer: u64,
) -> (Arc<AtomicBool>, std::thread::JoinHandle<u64>) {
    let link_session = session.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let link_stop = stop.clone();
    let link = std::thread::spawn(move || {
        let mut submits = 0u64;
        let mut sequence = 1u64;
        while !link_stop.load(Ordering::Relaxed) {
            let revision = link_session
                .shared_buffer_snapshot(BufferId::from(buffer), None)
                .map(|s| s.revision.as_u64())
                .unwrap_or(1);
            let request = LinkPrepareRequest {
                intent: LinkIntent {
                    client: ClientToken(0x00F6_0000_0000_0001),
                    sequence,
                },
                buffer_id: buffer,
                revision,
                caret: 0,
                target_path: "n2.md".to_string(),
                expected_target: None,
                stable_link_policy: StableLinkPolicy::Disabled,
                removal: None,
                completion: None,
            };
            let _ = link_session.execute(SessionCommand::LinkPrepare {
                request,
                request_id: sequence,
            });
            submits += 1;
            sequence += 1;
        }
        submits
    });
    (stop, link)
}

#[test]
fn lock_wait_under_full_contention_matrix() {
    let fixture_obj = fixture();
    let session = fixture_obj.session;
    let buffer_1mib = fixture_obj.buffers[0];
    let buffer_16mib = fixture_obj.big[0];
    let ext_path = fixture_obj.root.join("n4.md");

    // Phase 1: quiet baseline (1 MiB).
    let recorder = WaitRecorder {
        inner: Arc::new(Mutex::new(RecorderInner::default())),
    };
    let (quiet, quiet_bm) = type_keystrokes(&session, buffer_1mib, 60, &recorder);
    report("quiet_1mib", &quiet, &quiet_bm);

    // Phase 2: graph snapshot + materialization copies.
    let (stop, hammer) = spawn_reader_hammer(&session);
    let recorder = WaitRecorder {
        inner: Arc::new(Mutex::new(RecorderInner::default())),
    };
    let (graph, graph_bm) = type_keystrokes(&session, buffer_1mib, 60, &recorder);
    stop.store(true, Ordering::Relaxed);
    let materializations = hammer.join().expect("hammer thread");
    report("graph_copies_1mib", &graph, &graph_bm);
    println!("f6 graph_copies_1mib: reader completed {materializations} materializations");

    // Phase 3: undo/redo interleaves under a host-frame pump+graph hammer.
    let (stop, pump_hammer) = spawn_pump_hammer(&session);
    let recorder = WaitRecorder {
        inner: Arc::new(Mutex::new(RecorderInner::default())),
    };
    let (undo_redo, undo_redo_bm) = run_undo_redo_cycles(&session, buffer_1mib, 60, &recorder);
    stop.store(true, Ordering::Relaxed);
    let pump_turns = pump_hammer.join().expect("pump hammer thread");
    report("undo_redo_interleave_1mib", &undo_redo, &undo_redo_bm);
    println!("f6 undo_redo_interleave_1mib: pump hammer completed {pump_turns} turns");

    // Phase 4: external transitions (watcher Modified + pump) while typing.
    let (stop, producer) = spawn_external_transition_producer(
        &session,
        fixture_obj.fs.clone(),
        ext_path.clone(),
        fixture_obj.watcher.clone(),
    );
    let recorder = WaitRecorder {
        inner: Arc::new(Mutex::new(RecorderInner::default())),
    };
    let (ext, ext_bm) = type_keystrokes(&session, buffer_1mib, 60, &recorder);
    stop.store(true, Ordering::Relaxed);
    let transitions = producer.join().expect("external producer thread");
    report("external_transitions_1mib", &ext, &ext_bm);
    println!("f6 external_transitions_1mib: producer fired {transitions} transitions");

    // Phase 5: link-verdict workers + graph hammer while typing.
    let (stop_link, link_hammer) = spawn_link_verdict_hammer(&session, buffer_1mib);
    let (stop_graph, graph_hammer) = spawn_reader_hammer(&session);
    let recorder = WaitRecorder {
        inner: Arc::new(Mutex::new(RecorderInner::default())),
    };
    let (links, links_bm) = type_keystrokes(&session, buffer_1mib, 60, &recorder);
    stop_link.store(true, Ordering::Relaxed);
    stop_graph.store(true, Ordering::Relaxed);
    let link_submits = link_hammer.join().expect("link hammer thread");
    let _ = graph_hammer.join().expect("graph hammer thread");
    report("link_verdicts_plus_graph_1mib", &links, &links_bm);
    println!("f6 link_verdicts_plus_graph_1mib: link worker accepted {link_submits} prepares");

    // Phase 6: the size-flatness check — the same contention on a 16 MiB
    // note. If any lock hold were O(n), buffer_manager_wait would scale.
    let (stop, hammer) = spawn_reader_hammer(&session);
    let recorder = WaitRecorder {
        inner: Arc::new(Mutex::new(RecorderInner::default())),
    };
    let (big, big_bm) = type_keystrokes(&session, buffer_16mib, 60, &recorder);
    stop.store(true, Ordering::Relaxed);
    let _ = hammer.join().expect("hammer thread");
    report("graph_copies_16mib", &big, &big_bm);

    // Sanity ceilings (the verdict is the distribution, not a gate).
    for (name, waits, bm) in [
        ("quiet_1mib", &quiet, &quiet_bm),
        ("graph_copies_1mib", &graph, &graph_bm),
        ("undo_redo_interleave_1mib", &undo_redo, &undo_redo_bm),
        ("external_transitions_1mib", &ext, &ext_bm),
        ("link_verdicts_plus_graph_1mib", &links, &links_bm),
        ("graph_copies_16mib", &big, &big_bm),
    ] {
        let p99 = percentile(waits.to_vec(), 99);
        assert!(
            p99 < 10_000,
            "f6 {name} lock_wait p99 {p99}us ≥ 10 ms — contention is pathological; see Appendix B verdict"
        );
        let bm_p99 = percentile(bm.to_vec(), 99);
        assert!(
            bm_p99 < 5_000,
            "f6 {name} buffer_manager_wait p99 {bm_p99}us ≥ 5 ms — an O(n) hold remains under the manager lock"
        );
    }

    // Size-flatness: the 16 MiB phase must not show O(n) manager holds
    // relative to the 1 MiB phase (10× is generous for scheduler noise;
    // O(n) would be ~16× before the threshold even applies).
    let bm_1mib = percentile(graph_bm.clone(), 99);
    let bm_16mib = percentile(big_bm.clone(), 99);
    println!("f6 size-flatness: buffer_manager_wait p99 1MiB={bm_1mib}us 16MiB={bm_16mib}us");
    assert!(
        bm_16mib <= bm_1mib.max(100) * 10,
        "f6 buffer_manager_wait p99 at 16 MiB ({bm_16mib}us) ≫ 1 MiB ({bm_1mib}us) — lock holds scale with document size"
    );
}
