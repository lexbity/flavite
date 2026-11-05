//! Writer-side generation broadcast (RFC-0005 §5.1).
//!
//! The index writer announces every committed structural generation on a
//! lossy in-process channel so derived views (the graph service first;
//! tantivy/resolver adoption is a later slice) can chase commits at event
//! latency instead of polling for them.
//!
//! # The change log stays the only correctness authority
//!
//! Events are a latency optimization, never a data dependency. The channel
//! is lossy by design ([`GenerationSignal::publish`] drops when the queue is
//! full), so consumers MUST verify every wake against their own cursor over
//! the SQLite change feed before trusting freshness. A dropped event is
//! recovered by the next event or by the consumer's periodic self-verifying
//! sweep (RFC-0005 §4.1/§4.2).
//!
//! # FR-1: the publish contract
//!
//! Every code path that commits a `MetaKey::Generation` bump MUST call
//! [`GenerationSignal::publish`] immediately after its `tx.commit()`
//! succeeds, carrying the bumped `IndexVersion`. Commits that do not bump
//! the generation (`FastPath` commits) MUST NOT publish: they are
//! graph-invisible by contract (`pool.rs`, attachment/note stat refresh).
//! The contract is enforced structurally: the only in-spine generation writes
//! live in `IndexActor::apply_entity_core`, each of its two
//! bump-and-commit sites publishes on commit success, and
//! `graph_event_contract.rs` (flavite-api) property-tests every public apply
//! path against a real signal. The enforcement grep gate is:
//! every `MetaKey::Generation` write in production code is adjacent to a
//! `publish_generation` call on its commit-success path.
//!
//! # Watermark discipline (§4.9)
//!
//! `publish` stores `epoch` first and `generation` second; `watermark()`
//! reads `generation` first and `epoch` second (reverse order). With one
//! writer thread this makes the dangerous torn pair — a *stale* epoch
//! observed with a *fresh* generation — impossible to act on: observing the
//! fresh generation happens-after its release store, which happens-after the
//! epoch store, so the subsequent epoch load cannot return a value older
//! than that epoch in the epoch atomic's modification order. The remaining
//! tear — a fresh epoch with a stale generation — is safe by consumer policy:
//! a consumer skips work only on exact `watermark == cursor` equality, and
//! any epoch mismatch never skips (RFC-0005 §4.9). The watermark is an
//! advisory lower bound on database truth, never a substitute for it.
//!
//! # Single writer (§4.3)
//!
//! Exactly one thread owns the index writer connection, so the watermark and
//! epoch atomics are written in program order from one writer; the
//! monotonicity guarantees above hold per atomic. Multiple `Clone`d senders
//! must therefore only publish from that writer thread (or from a caller
//! ordered after the commit they advertise).
//!
//! # Coalescing (§4.2)
//!
//! The channel is `bounded(1)` with drop-on-full, exactly the existing
//! graph-service hint discipline but maximal: waiters never want a queue of
//! stale pulses, only "something committed, here is a version at least as
//! new". A burst delivers one wake; the watermark carries the newest
//! advertised version.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crossbeam::channel::{Receiver, Sender, bounded};

use crate::types::IndexVersion;

/// Queue capacity of one: latest-wins coalescing (RFC-0005 §4.2). We never
/// queue stale pulses; the watermark atomic carries the newest version and
/// the queue slot only guarantees "a wake happened".
const SIGNAL_QUEUE_CAPACITY: usize = 1;

/// The sender half of a generation broadcast. Cheap to `Clone` (shared
/// atomics and channel); clones MUST publish from the writer thread
/// (see the module header's single-writer rule).
#[derive(Clone)]
pub struct GenerationSignal {
    tx: Sender<IndexVersion>,
    epoch: Arc<AtomicU64>,
    watermark: Arc<AtomicU64>,
}

/// The receiver half of a generation broadcast. Each consumer holds one
/// `Arc`-shared watch; the wake value is compared against the consumer's
/// own cursor, never trusted blindly.
pub struct GenerationWatch {
    rx: Receiver<IndexVersion>,
    epoch: Arc<AtomicU64>,
    watermark: Arc<AtomicU64>,
}

impl GenerationSignal {
    /// Create a connected sender/watch pair.
    ///
    /// No `Default`: the constructor is inherently a pair, and a `Default`
    /// sender would silently drop its watch half, disconnecting the channel
    /// for every later `publish` (drop-on-disconnected is harmless but
    /// pointless — such a signal can never wake anyone).
    #[must_use]
    pub fn new() -> (Self, GenerationWatch) {
        let (tx, rx) = bounded(SIGNAL_QUEUE_CAPACITY);
        let epoch = Arc::new(AtomicU64::new(0));
        let watermark = Arc::new(AtomicU64::new(0));
        (
            Self {
                tx,
                epoch: Arc::clone(&epoch),
                watermark: Arc::clone(&watermark),
            },
            GenerationWatch {
                rx,
                epoch,
                watermark,
            },
        )
    }

    /// Advertise a committed structural generation.
    ///
    /// Contract (FR-1): call immediately after the `tx.commit()` that bumped
    /// `MetaKey::Generation` succeeds; never call for a commit that did not
    /// bump the generation (`FastPath`), and never before the commit is
    /// durable (§4.9: a reader that observes a watermark must be able to
    /// find that generation in the database).
    ///
    /// Cost: two atomic stores and one `try_send` (~50 ns on the writer
    /// thread, RFC-0005 NFR-9). The send drops silently when the queue slot
    /// is occupied (latest-wins coalescing) or when no watch remains —
    /// neither is an error: consumers recover via the next wake or their
    /// sweep.
    pub fn publish(&self, version: IndexVersion) {
        // Store order is the §4.9 discipline: epoch first, generation
        // second. Reversing it would re-open the stale-epoch/fresh-generation
        // tear that the read order otherwise makes impossible to observe.
        self.epoch.store(version.epoch, Ordering::Release);
        self.watermark.store(version.generation, Ordering::Release);
        let _ = self.tx.try_send(version);
    }
}

impl GenerationWatch {
    /// The latest version the writer advertised.
    ///
    /// Advisory lower bound on database truth (never equality): a skipped
    /// wake can make this lag a committed generation, and a concurrent
    /// publish may make it stale the instant it is read. Consumers compare
    /// it to their own cursor and treat `watermark != cursor` as "work to
    /// do", never `watermark == cursor` as proof against a newer database
    /// state that a sweep has not yet bounded (RFC-0005 §4.1).
    #[must_use]
    pub fn watermark(&self) -> IndexVersion {
        // Read order mirrors the store order in reverse (§4.9): generation
        // first, epoch second. Observing the fresh generation happens-after
        // its release store, hence after the epoch store, so the epoch load
        // cannot regress to an epoch older than the generation's own.
        let generation = self.watermark.load(Ordering::Acquire);
        let epoch = self.epoch.load(Ordering::Acquire);
        IndexVersion::new(epoch, generation)
    }

    /// Wait up to `timeout` for the next wake. Returns the version the
    /// writer advertised with that wake — a lower bound; re-read
    /// [`Self::watermark`] when the latest value matters. `None` on timeout
    /// and on disconnection (no senders remain).
    #[must_use]
    pub fn recv_timeout(&self, timeout: Duration) -> Option<IndexVersion> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// The underlying receiver handle. Consumers that multiplex this watch
    /// with other channels in one wait — the graph service's wake loop
    /// (`crossbeam::select!` over the index watch, the control channel, and
    /// the next deadline) — use this accessor. The watch's watermark
    /// atomics remain the authority for the *latest* advertised version.
    /// The watch is `Arc`-shared across service restarts; sending into a
    /// dead attempt's receiver is a `try_send` drop.
    #[must_use]
    pub fn receiver(&self) -> &Receiver<IndexVersion> {
        &self.rx
    }
}

impl fmt::Debug for GenerationSignal {
    /// Hand-rolled: the crossbeam sender is not `Debug`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GenerationSignal")
            .field("epoch", &self.epoch.load(Ordering::Relaxed))
            .field("watermark", &self.watermark.load(Ordering::Relaxed))
            .field("queued", &self.tx.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for GenerationWatch {
    /// Hand-rolled: the crossbeam receiver is not `Debug`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GenerationWatch")
            .field("epoch", &self.epoch.load(Ordering::Relaxed))
            .field("watermark", &self.watermark.load(Ordering::Relaxed))
            .field("queued", &self.rx.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUIET: Duration = Duration::from_millis(5);

    #[test]
    fn burst_publishes_coalesce_into_one_queued_wake_and_the_latest_watermark() {
        let (signal, watch) = GenerationSignal::new();
        for generation in 1..=1_000 {
            signal.publish(IndexVersion::new(1, generation));
        }
        // bounded(1) + drop-on-full: exactly one wake is queued — the value
        // is whatever filled the slot first; the *latest* version is always
        // on the watermark, which is what consumers read when they need it.
        let first_wake = watch.recv_timeout(QUIET).expect("one wake must be queued");
        assert_eq!(first_wake.epoch, 1);
        assert!((1..=1_000).contains(&first_wake.generation));
        assert!(
            watch.recv_timeout(QUIET).is_none(),
            "the queue must never grow past the single coalesced slot"
        );
        assert_eq!(watch.watermark(), IndexVersion::new(1, 1_000));
    }

    #[test]
    fn recv_timeout_on_quiet_watch_returns_none() {
        let (_signal, watch) = GenerationSignal::new();
        assert_eq!(watch.recv_timeout(QUIET), None);
        assert_eq!(watch.watermark(), IndexVersion::new(0, 0));
    }

    #[test]
    fn watermark_reflects_every_publish_in_order_including_epoch_changes() {
        let (signal, watch) = GenerationSignal::new();
        let sequence = [
            IndexVersion::new(1, 1),
            IndexVersion::new(1, 2),
            // DB recreate: the epoch jumps and generations restart.
            IndexVersion::new(9, 1),
            IndexVersion::new(9, 2),
        ];
        for version in sequence {
            signal.publish(version);
            assert_eq!(watch.watermark(), version);
            // One wake per publish while the slot is drained between them.
            assert_eq!(watch.recv_timeout(QUIET), Some(version));
            assert_eq!(watch.recv_timeout(QUIET), None);
        }
    }

    #[test]
    fn cloned_senders_share_the_watermark_and_the_single_queue_slot() {
        let (signal, watch) = GenerationSignal::new();
        let spare = signal.clone();
        signal.publish(IndexVersion::new(1, 1));
        // Same queue and same atomics behind the clone.
        assert_eq!(watch.recv_timeout(QUIET), Some(IndexVersion::new(1, 1)));
        spare.publish(IndexVersion::new(1, 2));
        assert_eq!(watch.watermark(), IndexVersion::new(1, 2));
        assert_eq!(watch.recv_timeout(QUIET), Some(IndexVersion::new(1, 2)));
        // Dropping the spare sender does not disconnect the watch: the
        // original signal still holds the queue open.
        drop(spare);
        signal.publish(IndexVersion::new(1, 3));
        assert_eq!(watch.recv_timeout(QUIET), Some(IndexVersion::new(1, 3)));
    }

    #[test]
    fn publishes_into_a_dropped_watch_are_harmless() {
        let (signal, watch) = GenerationSignal::new();
        signal.publish(IndexVersion::new(1, 1));
        drop(watch);
        // §4.10: a send into a dead worker's channel is a try_send drop.
        signal.publish(IndexVersion::new(1, 2));
        assert_eq!(signal.epoch.load(Ordering::Relaxed), 1);
        assert_eq!(signal.watermark.load(Ordering::Relaxed), 2);
    }

    /// The torn-pair safety property of the §4.9 store/read order, exercised
    /// against a real concurrent publisher.
    ///
    /// Schedule: epochs increase and generations never decrease —
    /// epoch 1 publishes generations `1..=K`, epoch 2 publishes `K+1..=2K`,
    /// epoch 3 publishes `2K+1..=3K`. Two invariants are asserted over every
    /// sampled `watermark()`:
    ///
    /// 1. **No stale-epoch/fresh-generation tear is ever observable**: a
    ///    sample carrying epoch `e` never carries a generation beyond the
    ///    highest ever published for epoch `<= e` (that would be an epoch
    ///    from the past paired with a generation from the future — the
    ///    dangerous direction, made impossible by the store order).
    /// 2. **Single-writer monotonicity**: samples never decrease in epoch,
    ///    and never decrease in generation while the epoch stays equal; the
    ///    fresh-epoch/stale-generation tear (sample `(e, g)` where `g`
    ///    belongs to an earlier epoch's range) may occur, is exactly the
    ///    harmless direction §4.9 documents (a consumer's epoch equality
    ///    check against its cursor never skips on it), and is only ever
    ///    counted, never propagated into the monotonicity check.
    #[test]
    fn concurrent_samples_never_observe_a_stale_epoch_with_a_fresh_generation() {
        const EPOCHS: u64 = 3;
        const PER_EPOCH: u64 = 4_000;
        let (signal, watch) = GenerationSignal::new();
        let publisher = signal.clone();
        let writer = std::thread::spawn(move || {
            for epoch in 1..=EPOCHS {
                for offset in 1..=PER_EPOCH {
                    publisher.publish(IndexVersion::new(epoch, (epoch - 1) * PER_EPOCH + offset));
                }
            }
        });

        let mut last_epoch = 0;
        let mut last_generation = 0;
        let mut fresh_epoch_stale_generation_tears = 0;
        // Drain the queue alongside sampling so `publish` never blocks on
        // semantics we rely on (it cannot block — try_send — but keeps the
        // burst realistic).
        loop {
            let sample = watch.watermark();
            assert!(
                sample.epoch <= EPOCHS,
                "sampled epoch {} beyond the published range",
                sample.epoch
            );
            assert!(
                sample.generation <= EPOCHS * PER_EPOCH,
                "sampled generation {} beyond the published range",
                sample.generation
            );
            // Invariant 1 (the dangerous direction must never occur): for
            // any sampled epoch e, the generation must not exceed the
            // highest generation published by any epoch <= e in the
            // schedule — i.e. e * PER_EPOCH.
            assert!(
                sample.generation <= sample.epoch * PER_EPOCH,
                "torn read: epoch {} sampled with a future generation {}",
                sample.epoch,
                sample.generation
            );
            // Invariant 2: monotone across samples (single writer).
            assert!(
                sample.epoch > last_epoch
                    || (sample.epoch == last_epoch && sample.generation >= last_generation),
                "non-monotone samples: ({last_epoch}, {last_generation}) then {sample:?}"
            );
            if sample.epoch > last_epoch && sample.generation <= last_epoch * PER_EPOCH {
                // The documented harmless tear: a fresh epoch paired with a
                // generation from a past epoch. Count it; §4.9 makes it
                // non-actionable for skipping consumers.
                fresh_epoch_stale_generation_tears += 1;
            }
            last_epoch = sample.epoch;
            last_generation = sample.generation;
            let _ = watch.recv_timeout(Duration::ZERO);
            if sample.epoch == EPOCHS && sample.generation == EPOCHS * PER_EPOCH {
                break;
            }
        }
        writer.join().expect("publisher thread must not panic");
        assert_eq!(
            watch.watermark(),
            IndexVersion::new(EPOCHS, EPOCHS * PER_EPOCH),
            "the final watermark must carry the last committed publish"
        );
        // The tear is permitted, not required; recording keeps the argument
        // honest — if it ever shows up it is counted, never propagated.
        let _ = fresh_epoch_stale_generation_tears;
    }

    #[test]
    fn debug_impressions_carry_the_watermark_without_panicking() {
        let (signal, watch) = GenerationSignal::new();
        signal.publish(IndexVersion::new(2, 3));
        let signal_text = format!("{signal:?}");
        assert!(signal_text.contains("epoch: 2"), "{signal_text}");
        assert!(signal_text.contains("watermark: 3"), "{signal_text}");
        let watch_text = format!("{watch:?}");
        assert!(watch_text.contains("epoch: 2"), "{watch_text}");
        assert!(watch_text.contains("watermark: 3"), "{watch_text}");
    }
}
