//! The published position hand-off (PLAN-B B3, D15).
//!
//! Layout runs on its own thread; the render thread must never wait for it.
//! The contract is therefore a *single immutable value* published atomically:
//!
//! ```text
//! worker thread ──store──▶ ArcSwap<PositionFrame> ◀──load── render thread
//! ```
//!
//! A reader takes an `Arc` to the newest frame in one atomic operation, keeps
//! it for as long as it likes (building geometry, drawing), and never blocks
//! the writer — and the writer never blocks on a reader. There is no lock on
//! either side of the hand-off, which is what D15's *"no locks on the frame
//! path"* means in practice.
//!
//! # One frame, one topology
//!
//! [`PositionFrame`] carries the positions **and the [`GraphSnapshot`] they
//! index** together with the [`LayoutState`] they were produced under. That
//! is §4.14 made structural: it is impossible to hand the renderer positions
//! from one topology version and a snapshot from another, because they cannot
//! be obtained separately. The pairing rule is not a convention a caller has
//! to remember — the type system enforces it.
//!
//! # Generations
//!
//! Every publish gets a monotonically increasing `generation`, so a consumer
//! can tell "nothing changed" from "something changed" in O(1)
//! ([`crate::graph`] reuses this to skip geometry rebuilds), and can order two
//! frames it happens to hold. Generation `0` is reserved for the initial
//! empty frame a [`PositionBuffer`] starts with: it is never a real publish.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;

use crate::model::{GraphModel, GraphSnapshot};

/// Where the layout is in its life cycle.
///
/// The value travels *inside* the frame so that a reader always learns the
/// state that the positions it is holding were produced under — a frame that
/// says [`LayoutState::Frozen`] describes a layout that has stopped moving,
/// not one that is between steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum LayoutState {
    /// The layout is stepping: positions change every step.
    Active,
    /// The layout converged and stopped stepping (D15: freeze on cool). A
    /// topology change wakes it.
    #[default]
    Frozen,
    /// Explicitly paused by a command: nothing steps until [`LayoutCommand::Start`].
    ///
    /// [`LayoutCommand::Start`]: crate::worker::LayoutCommand::Start
    Paused,
    /// The worker caught a panic and stopped stepping; the positions in this
    /// frame are the last good ones (§4.2).
    Panicked,
}

impl LayoutState {
    /// The wire code of the state, for atomics and logs.
    #[must_use]
    pub fn code(self) -> u8 {
        self as u8
    }

    /// Decode a wire code; an unknown code maps to [`LayoutState::Frozen`],
    /// the conservative reading (nothing is moving).
    #[must_use]
    pub fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Active,
            2 => Self::Paused,
            3 => Self::Panicked,
            _ => Self::Frozen,
        }
    }
}

/// One published set of positions, paired with the topology they index.
///
/// Immutable once constructed; shared by `Arc` between the layout thread and
/// every reader.
#[derive(Debug, Clone, PartialEq)]
pub struct PositionFrame {
    positions: Vec<[f32; 2]>,
    snapshot: Arc<GraphSnapshot>,
    topology_version: u64,
    generation: u64,
    state: LayoutState,
}

impl PositionFrame {
    /// Build a frame. Called by [`PositionBuffer::store`], which assigns the
    /// generation; the fields are private so a frame can never be constructed
    /// with a generation that does not match its publication order.
    fn new(
        positions: Vec<[f32; 2]>,
        snapshot: Arc<GraphSnapshot>,
        topology_version: u64,
        generation: u64,
        state: LayoutState,
    ) -> Self {
        debug_assert_eq!(
            positions.len(),
            snapshot.node_count(),
            "a frame must hold exactly one position per node of its own \
             snapshot (§4.14)"
        );
        Self {
            positions,
            snapshot,
            topology_version,
            generation,
            state,
        }
    }

    /// World positions in the snapshot's dense index order.
    #[must_use]
    pub fn positions(&self) -> &[[f32; 2]] {
        &self.positions
    }

    /// The topology these positions index — always the same version as
    /// [`PositionFrame::topology_version`].
    #[must_use]
    pub fn snapshot(&self) -> &GraphSnapshot {
        &self.snapshot
    }

    /// Clone the snapshot handle (cheap: the frame already owns an `Arc`).
    #[must_use]
    pub fn snapshot_arc(&self) -> Arc<GraphSnapshot> {
        Arc::clone(&self.snapshot)
    }

    /// `topology_version` of [`PositionFrame::snapshot`].
    #[must_use]
    pub fn topology_version(&self) -> u64 {
        self.topology_version
    }

    /// Publication order of this frame. `0` only on the initial empty frame.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The layout state these positions were produced under.
    #[must_use]
    pub fn state(&self) -> LayoutState {
        self.state
    }

    /// Number of nodes (and therefore positions) in the frame.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.positions.len()
    }
}

/// The lock-free position double buffer: write on the layout thread, read on
/// the render thread, never a lock in between.
///
/// "Double buffer" in the classic sense — the writer builds the next frame
/// while readers hold the current one — but implemented as an atomic pointer
/// swap rather than two slots plus a flip flag: [`arc_swap::ArcSwap`] lets
/// any number of readers hold a frame for any length of time while the writer
/// publishes the next one, with no reader ever blocking a writer and no
/// generation of reader ever observing a half-written frame.
#[derive(Debug)]
pub struct PositionBuffer {
    frame: ArcSwap<PositionFrame>,
    published: AtomicU64,
}

impl Default for PositionBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl PositionBuffer {
    /// A buffer holding the initial frame: no nodes, generation `0`,
    /// [`LayoutState::Frozen`] (nothing has been laid out yet).
    #[must_use]
    pub fn new() -> Self {
        let empty = PositionFrame::new(
            Vec::new(),
            Arc::new(GraphModel::new().snapshot()),
            0,
            0,
            LayoutState::Frozen,
        );
        Self {
            frame: ArcSwap::from_pointee(empty),
            published: AtomicU64::new(0),
        }
    }

    /// Publish a frame and return its generation.
    ///
    /// The positions must be dense-indexed by `snapshot` — the debug assert
    /// in [`PositionFrame::new`] checks it, so a mismatched pair cannot be
    /// published even by mistake.
    #[must_use]
    pub fn store(
        &self,
        snapshot: Arc<GraphSnapshot>,
        positions: Vec<[f32; 2]>,
        state: LayoutState,
    ) -> u64 {
        let generation = self.published.fetch_add(1, Ordering::AcqRel) + 1;
        let topology_version = snapshot.topology_version;
        let frame = PositionFrame::new(positions, snapshot, topology_version, generation, state);
        self.frame.store(Arc::new(frame));
        generation
    }

    /// Publish the current frame again with a new [`LayoutState`], keeping
    /// its positions and topology.
    ///
    /// Used on a pause or a caught panic: the state must change but the
    /// positions must not — and re-publishing the *same* `Arc` rather than a
    /// copy keeps the reader's frame identity stable.
    #[must_use]
    pub fn mark(&self, state: LayoutState) -> u64 {
        let generation = self.published.fetch_add(1, Ordering::AcqRel) + 1;
        let current = self.frame.load_full();
        let frame = PositionFrame::new(
            current.positions.clone(),
            Arc::clone(&current.snapshot),
            current.topology_version,
            generation,
            state,
        );
        self.frame.store(Arc::new(frame));
        generation
    }

    /// Load the newest frame.
    ///
    /// One atomic load and one `Arc` clone: the caller may hold the result
    /// for as long as it wants without ever blocking the writer.
    #[must_use]
    pub fn load(&self) -> Arc<PositionFrame> {
        self.frame.load_full()
    }

    /// The generation of the newest frame, without cloning it. `0` until the
    /// first real publish.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.published.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::{Delta, EdgeDelta, EdgeKind, NodeDelta, NodeId};

    fn note(id: u64) -> NodeId {
        NodeId::note(id)
    }

    fn snapshot_of(nodes: u64) -> Arc<GraphSnapshot> {
        let mut model = GraphModel::new();
        model.apply(
            &(0..nodes)
                .map(|i| Delta::Node(NodeDelta::Added { id: note(i) }))
                .collect::<Vec<_>>(),
        );
        model.apply(
            &(0..nodes.saturating_sub(1))
                .map(|i| {
                    Delta::Edge(EdgeDelta::Added {
                        from: note(i),
                        to: note(i + 1),
                        kind: EdgeKind::Link,
                    })
                })
                .collect::<Vec<_>>(),
        );
        Arc::new(model.snapshot())
    }

    fn positions(n: usize, offset: f32) -> Vec<[f32; 2]> {
        (0..n).map(|i| [i as f32 + offset, offset]).collect()
    }

    #[test]
    fn a_buffer_starts_empty_at_generation_zero() {
        let buffer = PositionBuffer::new();
        let frame = buffer.load();
        assert_eq!(frame.generation(), 0, "generation 0 is the initial frame");
        assert_eq!(frame.node_count(), 0);
        assert_eq!(frame.state(), LayoutState::Frozen);
        assert_eq!(frame.topology_version(), 0);
        assert_eq!(buffer.generation(), 0);
        assert!(frame.snapshot().is_empty());
    }

    #[test]
    fn store_publishes_positions_paired_with_their_snapshot() {
        let buffer = PositionBuffer::new();
        let snapshot = snapshot_of(4);
        let generation = buffer.store(
            Arc::clone(&snapshot),
            positions(4, 1.0),
            LayoutState::Active,
        );

        assert_eq!(generation, 1);
        let frame = buffer.load();
        assert_eq!(frame.generation(), 1);
        assert_eq!(frame.node_count(), 4);
        assert_eq!(frame.state(), LayoutState::Active);
        assert_eq!(frame.topology_version, snapshot.topology_version);
        // The frame owns the topology it indexes: §4.14 is structural.
        assert_eq!(frame.snapshot().node_ids, snapshot.node_ids);
        assert_eq!(frame.positions()[0], [1.0, 1.0]);
        assert_eq!(buffer.generation(), 1);
    }

    #[test]
    fn generations_are_monotone_and_unique() {
        let buffer = PositionBuffer::new();
        let snapshot = snapshot_of(1);
        let mut seen = Vec::new();
        for i in 0..16 {
            let generation = buffer.store(
                Arc::clone(&snapshot),
                positions(1, i as f32),
                LayoutState::Active,
            );
            seen.push(generation);
        }
        assert_eq!(seen, (1..=16).collect::<Vec<_>>());
        assert_eq!(buffer.generation(), 16);
        assert_eq!(buffer.load().generation(), 16);
        // Earlier frames stay valid for a reader that still holds them.
        let old = buffer.load();
        let _ = buffer.store(
            Arc::clone(&snapshot),
            positions(1, 99.0),
            LayoutState::Active,
        );
        assert_eq!(old.generation(), 16, "a held frame is never invalidated");
        assert_eq!(old.positions()[0][1], 15.0);
    }

    #[test]
    fn mark_changes_only_the_state() {
        let buffer = PositionBuffer::new();
        let snapshot = snapshot_of(3);
        let _ = buffer.store(
            Arc::clone(&snapshot),
            positions(3, 2.0),
            LayoutState::Active,
        );

        let generation = buffer.mark(LayoutState::Paused);
        assert_eq!(generation, 2, "marking publishes a new generation");
        let frame = buffer.load();
        assert_eq!(frame.state(), LayoutState::Paused);
        assert_eq!(frame.generation(), 2);
        assert_eq!(frame.positions()[0], [2.0, 2.0], "positions untouched");
        assert_eq!(frame.topology_version, snapshot.topology_version);
        assert_eq!(frame.snapshot().node_ids, snapshot.node_ids);
    }

    #[test]
    fn readers_never_observe_a_partially_published_frame() {
        // A writer publishes in a loop while readers check the invariant
        // every frame must satisfy: positions and snapshot agree on the node
        // count, and the generation only ever moves forward.
        let buffer = Arc::new(PositionBuffer::new());
        let writer = {
            let buffer = Arc::clone(&buffer);
            std::thread::spawn(move || {
                let snapshot = snapshot_of(8);
                for i in 0..400 {
                    let _ = buffer.store(
                        Arc::clone(&snapshot),
                        positions(8, i as f32),
                        if i % 3 == 0 {
                            LayoutState::Active
                        } else {
                            LayoutState::Frozen
                        },
                    );
                }
            })
        };
        let reader = {
            let buffer = Arc::clone(&buffer);
            std::thread::spawn(move || {
                let mut last = 0_u64;
                for _ in 0..400 {
                    let frame = buffer.load();
                    assert_eq!(frame.positions().len(), frame.snapshot().node_count());
                    assert!(
                        frame.generation() >= last,
                        "generations must not go backwards"
                    );
                    last = frame.generation();
                }
                last
            })
        };
        writer.join().expect("the writer must not panic");
        let observed = reader.join().expect("the reader must not panic");
        assert!(observed >= 1, "the reader saw at least one real frame");
        assert_eq!(buffer.load().generation(), 400);
    }

    #[test]
    fn state_codes_round_trip() {
        for state in [
            LayoutState::Active,
            LayoutState::Frozen,
            LayoutState::Paused,
            LayoutState::Panicked,
        ] {
            assert_eq!(LayoutState::from_code(state.code()), state);
        }
        // An unknown code reads as "nothing is moving".
        assert_eq!(LayoutState::from_code(9), LayoutState::Frozen);
        assert_eq!(LayoutState::from_code(255), LayoutState::Frozen);
        // The default is the safe reading.
        assert_eq!(LayoutState::default(), LayoutState::Frozen);
    }

    #[test]
    fn frames_are_send_and_sync() {
        // The render thread holds a frame the layout thread produced.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PositionFrame>();
        assert_send_sync::<PositionBuffer>();
        assert_send_sync::<Arc<PositionFrame>>();
    }
}
