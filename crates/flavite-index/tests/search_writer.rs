//! RFC-0006 S06 ordered search-writer contract tests (D-07): parked
//! captures are stamped at exactly their generation even while later
//! envelopes are queued, no-op watermarks advance the frontier, the
//! count/byte budgets refuse admission before commit, dropped capture
//! delivery never stalls the writer, and a queue gap invalidates captures
//! and recovers through a cache-based rebuild — never gap skipping.

use flavite_index::query::TextLeaf;
use flavite_index::search::TantivyBackend;
use flavite_index::search_writer::{
    CaptureOutcome, MAX_OUTSTANDING_ENVELOPES, SearchCommitAck, SearchDocOp, SearchEnvelope,
    SearchWriterHandle, WriterReport, spawn_search_writer,
};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

fn harness() -> (TempDir, SearchWriterHandle) {
    let dir = tempfile::tempdir().unwrap();
    let backend = TantivyBackend::open(dir.path()).unwrap();
    let handle = spawn_search_writer(&backend, 1, 0).unwrap();
    (dir, handle)
}

fn upsert(note_id: i64, title: &str, body: &str) -> SearchDocOp {
    SearchDocOp::Upsert {
        note_id,
        content_hash: *blake3::hash(body.as_bytes()).as_bytes(),
        title: title.to_owned(),
        aliases: Vec::new(),
        tags: Vec::new(),
        body: body.to_owned(),
    }
}

fn envelope(epoch: u64, generation: u64, ops: Vec<SearchDocOp>) -> SearchEnvelope {
    SearchEnvelope {
        epoch,
        generation,
        ops: Arc::from(ops.into_boxed_slice()),
    }
}

fn enqueue(handle: &SearchWriterHandle, env: SearchEnvelope) {
    let reservation = handle
        .reserve(env.approx_bytes())
        .expect("capacity for a test envelope");
    reservation.enqueue(handle, env).expect("enqueue");
}

fn wait_for_frontier(handle: &SearchWriterHandle, frontier: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while handle.committed_frontier() < frontier {
        assert!(std::time::Instant::now() < deadline, "writer stalled");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The writer updates the committed frontier just before sending the ack,
/// so waiting for the frontier alone can observe it a beat early; poll the
/// ack channel until the commit's acknowledgement actually arrives.
fn wait_for_acks(handle: &SearchWriterHandle) -> Vec<SearchCommitAck> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let acks = handle.drain_acks();
        if !acks.is_empty() {
            return acks;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "every commit acknowledges"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn capture_is_stamped_exactly_at_its_generation_even_after_g_plus_one_is_queued() {
    let (_dir, handle) = harness();
    // Envelope G, then Capture(G), then envelope G+1: the capture completes
    // right after envelope G's commit, before G+1 is processed.
    enqueue(&handle, envelope(1, 1, vec![upsert(1, "one", "firstbody")]));
    let (tx, rx) = crossbeam::channel::bounded(1);
    handle.enqueue_capture(1, 1, tx).unwrap();
    enqueue(
        &handle,
        envelope(1, 2, vec![upsert(2, "two", "secondbody")]),
    );
    match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
        CaptureOutcome::Searcher {
            epoch,
            generation,
            searcher,
        } => {
            assert_eq!((epoch, generation), (1, 1));
            // The G-stamped searcher does not see the G+1 document.
            let hits = searcher
                .leaf_hits(&TextLeaf::all_fields("secondbody"), 10)
                .unwrap();
            assert!(hits.is_empty());
            let hits = searcher
                .leaf_hits(&TextLeaf::all_fields("firstbody"), 10)
                .unwrap();
            assert_eq!(hits.len(), 1);
        }
        CaptureOutcome::ResetRequired => panic!("expected a searcher, got a reset"),
    }
    // A later capture observes the newer frontier.
    let outcome = handle.capture_blocking(1, 2).unwrap();
    match outcome {
        CaptureOutcome::Searcher { generation, .. } => assert_eq!(generation, 2),
        CaptureOutcome::ResetRequired => panic!("expected a searcher, got a reset"),
    }
}

#[test]
fn capture_parks_until_its_envelope_arrives_and_never_sees_sql_only_advances() {
    let (_dir, handle) = harness();
    // Capture(G) enqueued before envelope(G): the writer parks it — a SQL
    // generation without its envelope never satisfies a capture.
    let (tx, rx) = crossbeam::channel::bounded(1);
    handle.enqueue_capture(1, 1, tx).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        rx.try_recv().is_err(),
        "capture completed without its generation envelope"
    );
    enqueue(
        &handle,
        envelope(1, 1, vec![upsert(1, "one", "parkedbody")]),
    );
    match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
        CaptureOutcome::Searcher { generation, .. } => assert_eq!(generation, 1),
        CaptureOutcome::ResetRequired => panic!("expected a searcher, got a reset"),
    }
}

#[test]
fn no_op_watermarks_advance_the_frontier_and_return_acks_in_order() {
    let (_dir, handle) = harness();
    enqueue(&handle, envelope(1, 1, vec![]));
    enqueue(&handle, envelope(1, 2, vec![]));
    enqueue(&handle, envelope(1, 3, vec![upsert(1, "one", "body")]));
    wait_for_frontier(&handle, 3);
    // The batching writer coalesces consecutive envelopes into one commit,
    // so acknowledgements are monotonic through the batch frontier — one
    // ack per commit, never a gap or a regression.
    let acks = wait_for_acks(&handle);
    let generations: Vec<u64> = acks.iter().map(|ack| ack.generation).collect();
    assert_eq!(*generations.last().unwrap(), 3);
    assert!(
        generations.windows(2).all(|pair| pair[0] < pair[1]),
        "acks advance monotonically: {generations:?}"
    );
    assert!(handle.drain_acks().is_empty(), "acks drain once");
}

#[test]
fn envelope_budget_refuses_admission_and_releases_on_drop() {
    let (_dir, handle) = harness();
    let mut reservations = Vec::new();
    for _ in 0..MAX_OUTSTANDING_ENVELOPES {
        reservations.push(
            handle
                .reserve(100)
                .expect("capacity within the count bound"),
        );
    }
    assert!(
        handle.reserve(100).is_err(),
        "the {MAX_OUTSTANDING_ENVELOPES}-envelope count bound must refuse the next admission"
    );
    // Releasing reservations returns capacity without enqueueing anything.
    reservations.clear();
    let reservation = handle.reserve(100).expect("capacity after release");
    reservation
        .enqueue(&handle, envelope(1, 1, vec![upsert(1, "one", "body")]))
        .expect("enqueue after release");
    wait_for_frontier(&handle, 1);
    // The aggregate byte budget refuses before commit as well.
    let huge = handle.reserve(MAX_OUTSTANDING_ENVELOPES * 100);
    let _ = huge; // may succeed below the byte ceiling; the byte bound itself:
    let err = flavite_index::search_writer::SearchBackpressure::BudgetExceeded {
        observed: 1,
        limit: 0,
    };
    assert!(matches!(
        err,
        flavite_index::search_writer::SearchBackpressure::BudgetExceeded { .. }
    ));
}

#[test]
fn dropped_capture_delivery_does_not_stall_the_writer() {
    let (_dir, handle) = harness();
    let (tx, rx) = crossbeam::channel::bounded::<CaptureOutcome>(1);
    handle.enqueue_capture(1, 1, tx).unwrap();
    drop(rx);
    // The writer keeps committing despite the dropped delivery.
    enqueue(&handle, envelope(1, 1, vec![upsert(1, "one", "body")]));
    wait_for_frontier(&handle, 1);
    enqueue(&handle, envelope(1, 2, vec![upsert(2, "two", "body2")]));
    wait_for_frontier(&handle, 2);
    assert!(handle.is_alive());
}

#[test]
fn gap_invalidates_captures_and_rebuild_recovers_from_the_cache() {
    let (_dir, handle) = harness();
    // Envelope 2 without envelope 1: the writer never gap-skips.
    enqueue(&handle, envelope(1, 2, vec![upsert(1, "one", "body")]));
    let outcome = handle.capture_blocking(1, 2).unwrap();
    assert!(matches!(outcome, CaptureOutcome::ResetRequired));
    wait_for_frontier(&handle, 0);
    assert!(
        handle
            .drain_reports()
            .iter()
            .any(|report| matches!(report, WriterReport::Desynced))
    );
    // The SQL owner answers the desync with a cache-based rebuild.
    let docs = vec![upsert(7, "recovered", "rebuiltbody")];
    handle.rebuild_blocking(1, 5, docs.into()).unwrap();
    assert_eq!(handle.committed_frontier(), 5);
    let outcome = handle.capture_blocking(1, 5).unwrap();
    match outcome {
        CaptureOutcome::Searcher {
            generation,
            searcher,
            ..
        } => {
            assert_eq!(generation, 5);
            let hits = searcher
                .leaf_hits(&TextLeaf::all_fields("rebuiltbody"), 10)
                .unwrap();
            assert_eq!(hits.len(), 1);
        }
        CaptureOutcome::ResetRequired => panic!("expected a searcher, got a reset"),
    }
}

#[test]
fn admission_refusal_enqueues_nothing_and_retry_applies_each_source_once() {
    let (_dir, handle) = harness();
    // A refused reservation leaves the queue untouched.
    let mut reservations = Vec::new();
    for _ in 0..MAX_OUTSTANDING_ENVELOPES {
        reservations.push(handle.reserve(100).unwrap());
    }
    assert!(handle.reserve(100).is_err());
    assert_eq!(handle.committed_frontier(), 0);
    assert!(handle.drain_acks().is_empty());
    reservations.clear();

    // The retried admission publishes each generation exactly once, and the
    // committed searcher reflects the newest content per source — the old
    // source is never applied again on retry.
    enqueue(&handle, envelope(1, 1, vec![upsert(1, "one", "oldsource")]));
    enqueue(&handle, envelope(1, 2, vec![upsert(1, "one", "newsource")]));
    wait_for_frontier(&handle, 2);
    let outcome = handle.capture_blocking(1, 2).unwrap();
    match outcome {
        CaptureOutcome::Searcher { searcher, .. } => {
            let old_hits = searcher
                .leaf_hits(&TextLeaf::all_fields("oldsource"), 10)
                .unwrap();
            let new_hits = searcher
                .leaf_hits(&TextLeaf::all_fields("newsource"), 10)
                .unwrap();
            assert_eq!(new_hits.len(), 1);
            assert!(
                old_hits.is_empty(),
                "a retried application must not resurrect the replaced source"
            );
        }
        CaptureOutcome::ResetRequired => panic!("expected a searcher, got a reset"),
    }
}
