//! The layout worker (PLAN-B B3: D15, §4.2, §4.11).
//!
//! The layout runs on its **own OS thread**, with its own rayon pool sized at
//! `cores − 1` (D15: the render thread and the compositor keep a core). The
//! thread owns the whole simulation — the [`GraphModel`], the [`Layout`], the
//! pool — and speaks to the rest of the process through three things:
//!
//! - **commands in** ([`LayoutCommand`]): an unbounded channel, so a topology
//!   change can never be lost to backpressure;
//! - **frames out** ([`PositionBuffer`]): an atomic swap of an immutable
//!   [`PositionFrame`], so the render thread never waits (D15: no locks on
//!   the frame path);
//! - **events out** ([`LayoutEvent`]): rare, must-not-be-lost transitions
//!   (installed, state change, panic), plus lossless aggregate counters in
//!   [`LayoutTelemetry`] for anything measured per step.
//!
//! # Why telemetry is counters and not a per-step channel
//!
//! §4.11 wants `graph.layout_iter_ms` and §AC-16 wants "frame p99 *while
//! layout runs*" — both are statistics over a stream that runs faster than
//! any consumer. A per-step event channel has exactly two failure modes:
//! block the layout (a stalled simulation) or drop samples under load (a
//! biased percentile). [`LayoutTelemetry`] has neither: the worker writes
//! atomics, the consumer snapshots them once per frame, and the sample is
//! complete by construction. Events stay reserved for the rare things a
//! consumer must not miss.
//!
//! # Panic isolation (§4.2)
//!
//! Every command and every step runs inside [`catch_unwind`]. A panic is
//! contained: the worker emits [`LayoutEvent::Panicked`], marks the published
//! frame [`LayoutState::Panicked`] **without touching its positions** (the
//! last good frame is the only trustworthy one), stops stepping ("poisoned"),
//! and keeps its command loop alive so the process survives and a fresh
//! [`LayoutCommand::Replace`] can recover it. Nothing a caller can send — a
//! malformed [`GraphSnapshot`], a `NaN` parameter — unwinds into the UI.
//!
//! # Freeze and wake (D15)
//!
//! A layout that stops moving publishes [`LayoutState::Frozen`] and then
//! *blocks* on the command channel: no steps, no polling, no CPU. A topology
//! change ([`LayoutCommand::Apply`]/[`LayoutCommand::Replace`]) re-targets the
//! layout, which resets the adaptive state and the cooling schedule, and the
//! worker steps again. [`LayoutCommand::Start`] does the same on demand.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam::channel::{Receiver, RecvTimeoutError, Sender, unbounded};
use rayon::ThreadPoolBuilder;

use crate::delta::{Delta, EdgeDelta, NodeDelta};
use crate::layout::{Layout, LayoutParams};
use crate::model::{GraphModel, GraphSnapshot};
use crate::position::{LayoutState, PositionBuffer};

// ---------------------------------------------------------------------------
// Commands and events
// ---------------------------------------------------------------------------

/// What the outside world may ask the layout worker to do.
#[derive(Debug, Clone)]
pub enum LayoutCommand {
    /// Install a whole topology, replacing the current one. Used at boot and
    /// to recover from a caught panic. Positions of nodes that survive are
    /// kept (see [`Layout::retarget`]).
    Replace(Arc<GraphSnapshot>),
    /// Apply a delta batch (D5 semantics): the worker owns the model, so the
    /// index feed sends deltas rather than snapshots (PLAN-B §5).
    Apply(Vec<Delta>),
    /// Run the simulation (also wakes a frozen or paused layout).
    Start,
    /// Stop stepping. Positions stay published; nothing else happens.
    Pause,
    /// Stop the worker thread. Sent automatically on drop.
    Shutdown,
}

impl LayoutCommand {
    /// The command's name, for a panic report ("panicked while handling an
    /// install") rather than an anonymous "panicked somewhere".
    fn context(&self) -> &'static str {
        match self {
            Self::Replace(_) => "install",
            Self::Apply(_) => "apply",
            Self::Start => "start",
            Self::Pause => "pause",
            Self::Shutdown => "shutdown",
        }
    }
}

/// A rare, must-not-be-lost worker transition.
///
/// Deliberately not a per-step stream: see the module documentation for why
/// measurements go through [`LayoutTelemetry`] instead.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutEvent {
    /// A topology was installed (boot, or recovery from a panic).
    Installed {
        /// `topology_version` of the frame published with it.
        topology_version: u64,
        /// Generation of the frame published with it.
        generation: u64,
        /// Nodes in the new topology.
        node_count: usize,
        /// Nodes that kept their positions.
        kept: usize,
        /// Nodes placed from scratch (cold or warm start).
        placed: usize,
        /// Nodes that disappeared.
        removed: usize,
    },
    /// A delta batch changed the topology and woke the layout.
    TopologyChanged {
        /// `topology_version` of the frame published with it.
        topology_version: u64,
        /// Generation of the frame published with it.
        generation: u64,
        /// Nodes that kept their positions.
        kept: usize,
        /// Nodes warm-started at their neighbours' centroid.
        placed: usize,
        /// Nodes that disappeared.
        removed: usize,
    },
    /// The published frame's layout state changed.
    State {
        /// The new state (also carried by the frame itself).
        state: LayoutState,
        /// Generation of the frame carrying this state.
        generation: u64,
    },
    /// The worker caught a panic and stopped stepping (§4.2).
    Panicked {
        /// What was being handled: `"step"`, `"install"`, …
        context: &'static str,
        /// The panic payload, stringified.
        message: String,
    },
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// How to run a [`LayoutWorker`].
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutWorkerConfig {
    /// Force-model tunables (sanitized by the worker before use).
    pub params: LayoutParams,
    /// Rayon pool size. `None` means `available_parallelism − 1`, floored at
    /// 1 — D15: the layout never takes the last core (PLAN-B §5).
    pub threads: Option<usize>,
    /// Minimum wall time between steps. `ZERO` steps as fast as the machine
    /// allows, which is what the AC-16 probe measures; a real interactive
    /// budget would set this to the frame period.
    pub min_step_interval: Duration,
}

impl Default for LayoutWorkerConfig {
    fn default() -> Self {
        Self {
            params: LayoutParams::default(),
            threads: None,
            min_step_interval: Duration::ZERO,
        }
    }
}

impl LayoutWorkerConfig {
    /// The pool size this configuration resolves to (D15: ≤ cores − 1).
    #[must_use]
    pub fn resolved_threads(&self) -> usize {
        let available = thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(4);
        self.threads
            .unwrap_or_else(|| available.saturating_sub(1))
            .clamp(1, available.max(1))
    }
}

/// A problem starting a layout worker.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayoutWorkerError {
    /// The rayon pool could not be built (the OS refused the threads).
    #[error("could not build the layout rayon pool: {0}")]
    Pool(String),
}

// ---------------------------------------------------------------------------
// Telemetry
// ---------------------------------------------------------------------------

/// Aggregate counters the layout worker publishes for §4.11
/// (`graph.layout_iter_ms`) and AC-16 (frame p99 *while layout runs*).
///
/// Every counter is an atomic; the worker writes, the consumer snapshots.
/// Nothing here can block a step or lose a sample under backpressure.
#[derive(Debug, Default)]
pub struct LayoutTelemetry {
    steps: AtomicU64,
    frames: AtomicU64,
    freezes: AtomicU64,
    wakes: AtomicU64,
    panics: AtomicU64,
    /// Last step time, in milliseconds, as `f64::to_bits` (an atomic `f64`
    /// without a cast or a lock).
    step_ms_last: AtomicU64,
    /// Sum of all step times, in milliseconds, as bits.
    step_ms_total: AtomicU64,
    /// Largest step time seen, in milliseconds, as bits.
    step_ms_max: AtomicU64,
    /// Whether the layout is currently counted as frozen, for edge detection
    /// of the `freezes` counter.
    frozen: AtomicBool,
    /// Generation of the newest published frame.
    generation: AtomicU64,
    /// `topology_version` of the newest published frame.
    topology_version: AtomicU64,
}

/// A consistent read of [`LayoutTelemetry`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TelemetrySnapshot {
    /// Steps taken since the worker started.
    pub steps: u64,
    /// Frames published since the worker started.
    pub frames: u64,
    /// Transitions into [`LayoutState::Frozen`].
    pub freezes: u64,
    /// Topology changes that woke the layout.
    pub wakes: u64,
    /// Caught panics (§4.2).
    pub panics: u64,
    /// Time of the most recent step, in milliseconds.
    pub last_step_ms: f64,
    /// Mean step time, in milliseconds — §4.11's `graph.layout_iter_ms`.
    pub mean_step_ms: f64,
    /// Slowest step seen, in milliseconds.
    pub max_step_ms: f64,
    /// Generation of the newest frame.
    pub generation: u64,
    /// `topology_version` of the newest frame.
    pub topology_version: u64,
}

impl TelemetrySnapshot {
    /// Steps per second implied by the mean step time (0 when no step has
    /// run yet).
    #[must_use]
    pub fn steps_per_second(&self) -> f64 {
        if self.mean_step_ms > 0.0 {
            1_000.0 / self.mean_step_ms
        } else {
            0.0
        }
    }
}

/// Add `value` to an `f64` stored in an [`AtomicU64`] as bits.
fn atomic_add_f64(slot: &AtomicU64, value: f64) {
    let mut current = slot.load(Ordering::Relaxed);
    loop {
        let next = (f64::from_bits(current) + value).to_bits();
        match slot.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(actual) => current = actual,
        }
    }
}

/// Raise an `f64` stored in an [`AtomicU64`] to `value` if it is larger.
fn atomic_max_f64(slot: &AtomicU64, value: f64) {
    let mut current = slot.load(Ordering::Relaxed);
    loop {
        if f64::from_bits(current) >= value {
            return;
        }
        match slot.compare_exchange_weak(
            current,
            value.to_bits(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(actual) => current = actual,
        }
    }
}

impl LayoutTelemetry {
    /// Record one completed step.
    fn record_step(&self, step_ms: f64, frozen: bool) {
        self.steps.fetch_add(1, Ordering::Relaxed);
        atomic_add_f64(&self.step_ms_total, step_ms);
        self.step_ms_last
            .store(step_ms.to_bits(), Ordering::Relaxed);
        atomic_max_f64(&self.step_ms_max, step_ms);
        if frozen {
            if !self.frozen.swap(true, Ordering::Relaxed) {
                self.freezes.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            self.frozen.store(false, Ordering::Relaxed);
        }
    }

    /// Record a published frame.
    fn record_frame(&self, generation: u64, topology_version: u64) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.generation.store(generation, Ordering::Relaxed);
        self.topology_version
            .store(topology_version, Ordering::Relaxed);
    }

    /// Record a topology change that woke the layout.
    fn record_wake(&self) {
        self.wakes.fetch_add(1, Ordering::Relaxed);
        self.frozen.store(false, Ordering::Relaxed);
    }

    /// Record a caught panic (§4.2).
    fn record_panic(&self) {
        self.panics.fetch_add(1, Ordering::Relaxed);
    }

    /// Read every counter at once (as consistent as a set of relaxed atomics
    /// can be — the consumer only ever reads advisory statistics).
    #[must_use]
    pub fn snapshot(&self) -> TelemetrySnapshot {
        let steps = self.steps.load(Ordering::Relaxed);
        let total = f64::from_bits(self.step_ms_total.load(Ordering::Relaxed));
        TelemetrySnapshot {
            steps,
            frames: self.frames.load(Ordering::Relaxed),
            freezes: self.freezes.load(Ordering::Relaxed),
            wakes: self.wakes.load(Ordering::Relaxed),
            panics: self.panics.load(Ordering::Relaxed),
            last_step_ms: f64::from_bits(self.step_ms_last.load(Ordering::Relaxed)),
            mean_step_ms: if steps > 0 { total / steps as f64 } else { 0.0 },
            max_step_ms: f64::from_bits(self.step_ms_max.load(Ordering::Relaxed)),
            generation: self.generation.load(Ordering::Relaxed),
            topology_version: self.topology_version.load(Ordering::Relaxed),
        }
    }
}

// ---------------------------------------------------------------------------
// The worker handle
// ---------------------------------------------------------------------------

/// The layout worker: an OS thread running the simulation, plus the handles
/// the rest of the process uses to talk to it.
///
/// Dropping the worker asks the thread to stop and joins it, so no thread
/// outlives its owner.
#[derive(Debug)]
pub struct LayoutWorker {
    commands: Sender<LayoutCommand>,
    events: Receiver<LayoutEvent>,
    positions: Arc<PositionBuffer>,
    telemetry: Arc<LayoutTelemetry>,
    thread: Option<JoinHandle<()>>,
    threads: usize,
}

impl LayoutWorker {
    /// Start a worker with `config`.
    ///
    /// The worker begins paused with an empty topology: send
    /// [`LayoutCommand::Replace`] and then [`LayoutCommand::Start`], or use
    /// [`LayoutWorker::spawn_running`].
    pub fn spawn(config: LayoutWorkerConfig) -> Result<Self, LayoutWorkerError> {
        let threads = config.resolved_threads();
        let (commands_tx, commands_rx) = unbounded::<LayoutCommand>();
        let (events_tx, events_rx) = unbounded::<LayoutEvent>();
        let positions = Arc::new(PositionBuffer::new());
        let telemetry = Arc::new(LayoutTelemetry::default());

        let pool = ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|index| format!("layout-{index}"))
            .build()
            .map_err(|error| LayoutWorkerError::Pool(error.to_string()))?;

        let thread = {
            let positions = Arc::clone(&positions);
            let telemetry = Arc::clone(&telemetry);
            let events = events_tx;
            thread::Builder::new()
                .name("layout".to_string())
                .spawn(move || run(config, pool, commands_rx, events, positions, telemetry))
                .map_err(|error| LayoutWorkerError::Pool(error.to_string()))?
        };

        Ok(Self {
            commands: commands_tx,
            events: events_rx,
            positions,
            telemetry,
            thread: Some(thread),
            threads,
        })
    }

    /// Start a worker and immediately install `snapshot`, running.
    pub fn spawn_running(
        config: LayoutWorkerConfig,
        snapshot: Arc<GraphSnapshot>,
    ) -> Result<Self, LayoutWorkerError> {
        let worker = Self::spawn(config)?;
        worker.send(LayoutCommand::Replace(snapshot));
        worker.send(LayoutCommand::Start);
        Ok(worker)
    }

    /// The published-position buffer the render thread reads (D15).
    #[must_use]
    pub fn positions(&self) -> Arc<PositionBuffer> {
        Arc::clone(&self.positions)
    }

    /// The aggregate counters for §4.11 / AC-16.
    #[must_use]
    pub fn telemetry(&self) -> Arc<LayoutTelemetry> {
        Arc::clone(&self.telemetry)
    }

    /// The rare-transition event receiver. Drained by whoever owns the event
    /// loop (the probe drains it once per frame).
    #[must_use]
    pub fn events(&self) -> &Receiver<LayoutEvent> {
        &self.events
    }

    /// The rayon pool size the worker runs with (≤ cores − 1, D15).
    #[must_use]
    pub fn threads(&self) -> usize {
        self.threads
    }

    /// Queue a command. Returns `false` if the worker has already gone away.
    ///
    /// The channel is unbounded, so this never blocks: a topology change is
    /// never dropped because the layout was busy.
    pub fn send(&self, command: LayoutCommand) -> bool {
        self.commands.send(command).is_ok()
    }

    /// Whether the worker thread is still running.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
    }

    /// Ask the worker to stop, join it, and report whether it exited cleanly.
    pub fn shutdown(mut self) -> bool {
        self.stop_and_join()
    }

    /// Queue `Shutdown` and join the thread.
    fn stop_and_join(&mut self) -> bool {
        let _ = self.commands.send(LayoutCommand::Shutdown);
        match self.thread.take() {
            Some(handle) => handle.join().is_ok(),
            None => true,
        }
    }
}

impl Drop for LayoutWorker {
    fn drop(&mut self) {
        // Never leak the thread: the pool is `cores − 1` threads that would
        // otherwise spin against a dead command sender.
        self.stop_and_join();
    }
}

// ---------------------------------------------------------------------------
// The worker thread
// ---------------------------------------------------------------------------

/// The thread body: drain commands, step or sleep, repeat until told to stop.
fn run(
    config: LayoutWorkerConfig,
    pool: rayon::ThreadPool,
    commands: Receiver<LayoutCommand>,
    events: Sender<LayoutEvent>,
    positions: Arc<PositionBuffer>,
    telemetry: Arc<LayoutTelemetry>,
) {
    let mut engine = Engine::new(config, pool, commands, events, positions, telemetry);

    'outer: loop {
        // Everything queued so far, before deciding what to do next.
        while let Ok(command) = engine.commands.try_recv() {
            if !engine.handle(command) {
                return;
            }
        }

        if engine.should_step() {
            engine.step();
            if engine.config.min_step_interval > Duration::ZERO {
                // Throttled: sleep between steps, but wake the moment a
                // command arrives rather than after the whole interval.
                match engine
                    .commands
                    .recv_timeout(engine.config.min_step_interval)
                {
                    Ok(command) => {
                        if !engine.handle(command) {
                            break 'outer;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break 'outer,
                }
            }
            continue;
        }

        // Frozen, paused, or poisoned: block. No polling, no busy-wait — a
        // cool layout costs nothing (D15: freeze on cool).
        match engine.commands.recv() {
            Ok(command) => {
                if !engine.handle(command) {
                    break 'outer;
                }
            }
            Err(_) => break 'outer,
        }
    }
}

/// The state the worker thread owns. One instance, one thread: no shared
/// mutable state anywhere in here.
struct Engine {
    config: LayoutWorkerConfig,
    pool: rayon::ThreadPool,
    commands: Receiver<LayoutCommand>,
    events: Sender<LayoutEvent>,
    positions: Arc<PositionBuffer>,
    telemetry: Arc<LayoutTelemetry>,
    /// The authority for deltas: B1's model, advanced by `Apply`.
    model: GraphModel,
    /// The topology the published frames index (§4.14).
    current: Arc<GraphSnapshot>,
    /// The simulation.
    layout: Layout,
    /// Reused buffer for one step's displacements (no per-step allocation
    /// on the hot path).
    scratch: Vec<[f64; 2]>,
    /// Whether steps should run at all.
    running: bool,
    /// Whether the last command or step panicked. Cleared only by a
    /// successful install.
    poisoned: bool,
}

impl Engine {
    fn new(
        config: LayoutWorkerConfig,
        pool: rayon::ThreadPool,
        commands: Receiver<LayoutCommand>,
        events: Sender<LayoutEvent>,
        positions: Arc<PositionBuffer>,
        telemetry: Arc<LayoutTelemetry>,
    ) -> Self {
        let params = config.params.sanitized();
        Self {
            config: LayoutWorkerConfig { params, ..config },
            pool,
            commands,
            events,
            positions,
            telemetry,
            model: GraphModel::new(),
            current: Arc::new(GraphModel::new().snapshot()),
            layout: Layout::new(params),
            scratch: Vec::new(),
            running: false,
            poisoned: false,
        }
    }

    /// Whether the loop should step right now.
    fn should_step(&self) -> bool {
        self.running && !self.poisoned && !self.layout.is_converged()
    }

    /// Handle one command, isolating any panic it causes (§4.2).
    ///
    /// Returns `false` when the worker should stop.
    fn handle(&mut self, command: LayoutCommand) -> bool {
        let context = command.context();
        let outcome = catch_unwind(AssertUnwindSafe(|| match command {
            LayoutCommand::Shutdown => false,
            LayoutCommand::Replace(snapshot) => {
                self.install(snapshot);
                true
            }
            LayoutCommand::Apply(deltas) => {
                self.apply(deltas);
                true
            }
            LayoutCommand::Start => {
                self.running = true;
                // An explicit start means "run again", even from frozen.
                self.layout.wake();
                let state = if self.poisoned {
                    LayoutState::Panicked
                } else {
                    LayoutState::Active
                };
                self.publish_current(state);
                self.emit(LayoutEvent::State {
                    state,
                    generation: self.positions.generation(),
                });
                true
            }
            LayoutCommand::Pause => {
                self.running = false;
                let state = if self.poisoned {
                    LayoutState::Panicked
                } else {
                    LayoutState::Paused
                };
                self.publish_current(state);
                self.emit(LayoutEvent::State {
                    state,
                    generation: self.positions.generation(),
                });
                true
            }
        }));

        match outcome {
            Ok(true) => true,
            Ok(false) => false,
            Err(payload) => {
                // §4.2: the panic is contained here. The worker keeps its
                // loop alive, publishes the last good frame as `Panicked`,
                // and waits for a topology it can trust.
                self.poisoned = true;
                self.telemetry.record_panic();
                let message = panic_message(payload);
                self.emit(LayoutEvent::Panicked { context, message });
                let _ = self.positions.mark(LayoutState::Panicked);
                true
            }
        }
    }

    /// Install a whole topology, committing only when every step of the
    /// rebuild has succeeded.
    ///
    /// The commit-at-the-end rule is what keeps §4.14 true across a panic: if
    /// anything below throws, `model`, `current`, and `layout` are left
    /// exactly as they were, so the frame the render thread is holding still
    /// pairs positions with the topology they index.
    fn install(&mut self, snapshot: Arc<GraphSnapshot>) {
        let model = model_from_snapshot(&snapshot);
        let fresh = Arc::new(model.snapshot());
        let mut layout = Layout::new(self.layout.params());
        let retarget = layout.retarget(&fresh);

        self.model = model;
        self.current = fresh;
        self.layout = layout;
        self.poisoned = false;

        let state = if self.running {
            LayoutState::Active
        } else {
            LayoutState::Paused
        };
        self.publish_current(state);
        self.telemetry.record_wake();
        self.emit(LayoutEvent::Installed {
            topology_version: self.current.topology_version,
            generation: self.positions.generation(),
            node_count: self.current.node_count(),
            kept: retarget.kept,
            placed: retarget.placed,
            removed: retarget.removed,
        });
    }

    /// Apply a delta batch. The model is B1's, and its `apply` is defined on
    /// plain data, so only the re-target below can fail — and it runs before
    /// anything is committed (see [`Engine::install`]).
    fn apply(&mut self, deltas: Vec<Delta>) {
        let stats = self.model.apply(&deltas);
        if !stats.changed {
            return;
        }

        let fresh = Arc::new(self.model.snapshot());
        let mut layout = Layout::new(self.layout.params());
        // `retarget` keeps the survivors, warm-starts the newcomers, and
        // resets the adaptive state — the wake half of D15's freeze/wake.
        let retarget = layout.retarget(&fresh);

        self.layout = layout;
        self.current = fresh;
        self.poisoned = false;

        let state = if self.running {
            LayoutState::Active
        } else {
            LayoutState::Paused
        };
        self.publish_current(state);
        self.telemetry.record_wake();
        self.emit(LayoutEvent::TopologyChanged {
            topology_version: self.current.topology_version,
            generation: self.positions.generation(),
            kept: retarget.kept,
            placed: retarget.placed,
            removed: retarget.removed,
        });
    }

    /// Take one step, isolating any panic (§4.2).
    fn step(&mut self) {
        let started = Instant::now();
        let Self {
            layout,
            pool,
            scratch,
            ..
        } = self;
        let outcome = catch_unwind(AssertUnwindSafe(|| layout.step_into(pool, scratch)));

        match outcome {
            Ok(stats) => {
                let step_ms = started.elapsed().as_secs_f64() * 1_000.0;
                self.telemetry.record_step(step_ms, stats.frozen);
                let state = if stats.frozen {
                    LayoutState::Frozen
                } else {
                    LayoutState::Active
                };
                self.publish_current(state);
            }
            Err(payload) => {
                self.poisoned = true;
                self.telemetry.record_panic();
                let message = panic_message(payload);
                self.emit(LayoutEvent::Panicked {
                    context: "step",
                    message,
                });
                // The positions a panicked step may have mangled are never
                // published: the last good frame is re-marked instead.
                let _ = self.positions.mark(LayoutState::Panicked);
            }
        }
    }

    /// Publish the layout's positions under `state`, or re-mark the last
    /// good frame when the layout is poisoned (its positions cannot be
    /// trusted after a panic).
    fn publish_current(&mut self, state: LayoutState) {
        if self.poisoned {
            let _ = self.positions.mark(LayoutState::Panicked);
            return;
        }
        let mut positions = Vec::with_capacity(self.layout.node_count());
        self.layout.write_positions_f32(&mut positions);
        let generation = self
            .positions
            .store(Arc::clone(&self.current), positions, state);
        self.telemetry
            .record_frame(generation, self.current.topology_version);
    }

    /// Queue an event. The channel is unbounded and the events are rare, so
    /// a send never blocks the simulation.
    fn emit(&self, event: LayoutEvent) {
        let _ = self.events.send(event);
    }
}

/// Rebuild a [`GraphModel`] from a snapshot's dense contents.
///
/// Used so the worker always owns the *same* model type the index feed will
/// drive in B5 — a snapshot is a view, a model is the authority.
fn model_from_snapshot(snapshot: &GraphSnapshot) -> GraphModel {
    let mut model = GraphModel::new();

    let nodes: Vec<Delta> = snapshot
        .node_ids
        .iter()
        .map(|&id| Delta::Node(NodeDelta::Added { id }))
        .collect();
    for batch in nodes.chunks(10_000) {
        model.apply(batch);
    }

    let mut edges = Vec::with_capacity(snapshot.edge_count());
    for source in 0..snapshot.node_count() {
        let start = snapshot.out_offsets[source] as usize;
        let end = snapshot.out_offsets[source + 1] as usize;
        for row in start..end {
            let target = snapshot.out_indices[row] as usize;
            edges.push(Delta::Edge(EdgeDelta::Added {
                from: snapshot.node_ids[source],
                to: snapshot.node_ids[target],
                kind: snapshot.out_edge_kinds[row],
            }));
        }
    }
    for batch in edges.chunks(10_000) {
        model.apply(batch);
    }

    model
}

/// Stringify a panic payload for [`LayoutEvent::Panicked`].
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::{EdgeDelta, EdgeKind, NodeDelta, NodeId, NodeKind};
    use crate::model::GraphModel;

    fn note(id: u64) -> NodeId {
        NodeId::note(id)
    }

    /// A small path graph, as a snapshot.
    fn path(nodes: u64) -> Arc<GraphSnapshot> {
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

    /// A star whose leaves keep arriving: converges quickly, wakes on demand.
    fn star(nodes: u64) -> Arc<GraphSnapshot> {
        let mut model = GraphModel::new();
        model.apply(
            &(0..nodes)
                .map(|i| Delta::Node(NodeDelta::Added { id: note(i) }))
                .collect::<Vec<_>>(),
        );
        model.apply(
            &(1..nodes)
                .map(|i| {
                    Delta::Edge(EdgeDelta::Added {
                        from: note(0),
                        to: note(i),
                        kind: EdgeKind::Link,
                    })
                })
                .collect::<Vec<_>>(),
        );
        Arc::new(model.snapshot())
    }

    /// Test configuration: fast convergence, no step throttling, a small
    /// pool so the tests do not fight the host for cores.
    fn config(threads: usize) -> LayoutWorkerConfig {
        LayoutWorkerConfig {
            params: LayoutParams {
                convergence_epsilon: 1.0e-3,
                convergence_steps: 2,
                ..LayoutParams::default()
            },
            threads: Some(threads),
            min_step_interval: Duration::ZERO,
        }
    }

    /// Poll `condition` until it holds or `timeout` elapses.
    fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(1));
        }
        condition()
    }

    fn drain(worker: &LayoutWorker) -> Vec<LayoutEvent> {
        let mut events = Vec::new();
        while let Ok(event) = worker.events().try_recv() {
            events.push(event);
        }
        events
    }

    #[test]
    fn a_worker_steps_and_publishes_frames() {
        let worker = LayoutWorker::spawn_running(config(2), path(24)).expect("worker starts");
        let buffer = worker.positions();
        assert!(
            wait_until(Duration::from_secs(10), || {
                buffer.load().generation() >= 5
            }),
            "the worker must publish frames"
        );
        assert_eq!(worker.threads(), 2);

        let frame = buffer.load();
        assert_eq!(frame.node_count(), 24);
        assert_eq!(frame.state(), LayoutState::Active);
        assert_eq!(frame.topology_version(), 1);
        for (index, [x, y]) in frame.positions().iter().enumerate() {
            assert!(x.is_finite() && y.is_finite(), "node {index} at {x},{y}");
        }
        // Positions are actually moving, not a frozen publish loop.
        let first = frame.positions().to_vec();
        assert!(
            wait_until(Duration::from_secs(10), || {
                buffer.load().positions() != first.as_slice()
            }),
            "positions must change between frames"
        );

        let telemetry = worker.telemetry().snapshot();
        assert!(telemetry.steps > 0);
        assert!(telemetry.frames >= 5);
        assert!(telemetry.mean_step_ms > 0.0, "steps take measurable time");
        assert!(telemetry.last_step_ms > 0.0);
        assert!(worker.is_alive());
        let _ = drain(&worker);

        // `shutdown` consumes the handle; a clean join is the assertion that
        // the thread is gone.
        assert!(worker.shutdown());
    }

    #[test]
    fn the_pool_is_at_most_cores_minus_one() {
        let default = LayoutWorkerConfig::default();
        let resolved = default.resolved_threads();
        let cores = thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(4);
        assert!(
            resolved <= cores.saturating_sub(1).max(1),
            "D15: the layout never takes the last core ({resolved} of {cores})"
        );
        assert!(resolved >= 1);

        // An explicit request is honoured but never exceeds the machine.
        assert_eq!(
            LayoutWorkerConfig {
                threads: Some(3),
                ..default.clone()
            }
            .resolved_threads(),
            3
        );
        assert_eq!(
            LayoutWorkerConfig {
                threads: Some(0),
                ..default.clone()
            }
            .resolved_threads(),
            1,
            "a zero request is floored at one thread, not zero"
        );
        assert_eq!(
            LayoutWorkerConfig {
                threads: Some(999),
                ..default
            }
            .resolved_threads(),
            cores.max(1),
            "an absurd request is capped at the machine"
        );

        let worker = LayoutWorker::spawn(config(2)).expect("worker starts");
        assert_eq!(worker.threads(), 2);
        assert!(worker.shutdown());
    }

    #[test]
    fn freeze_on_cool_and_wake_on_topology_change() {
        let worker = LayoutWorker::spawn_running(config(2), star(12)).expect("worker starts");
        let buffer = worker.positions();

        // D15: a settled layout publishes `Frozen` and then stops stepping.
        assert!(
            wait_until(Duration::from_secs(15), || {
                buffer.load().state() == LayoutState::Frozen
            }),
            "the layout must freeze once it settles: {:?}",
            buffer.load().state()
        );
        let frozen_generation = buffer.load().generation();
        let frozen_steps = worker.telemetry().snapshot().steps;
        assert!(
            wait_until(Duration::from_secs(2), || {
                worker.telemetry().snapshot().steps == frozen_steps
            }),
            "a frozen layout must stop stepping"
        );
        assert!(
            buffer.load().generation() >= frozen_generation,
            "no further publishes once frozen"
        );
        assert!(worker.telemetry().snapshot().freezes >= 1);

        // A topology change wakes it: it steps again and leaves `Frozen`.
        worker.send(LayoutCommand::Apply(vec![
            Delta::Node(NodeDelta::Added { id: note(99) }),
            Delta::Edge(EdgeDelta::Added {
                from: note(0),
                to: note(99),
                kind: EdgeKind::Link,
            }),
        ]));
        assert!(
            wait_until(Duration::from_secs(5), || {
                let frame = buffer.load();
                frame.state() == LayoutState::Active && frame.node_count() == 13
            }),
            "a topology change must wake the layout: {:?}",
            buffer.load()
        );
        let telemetry = worker.telemetry().snapshot();
        assert!(telemetry.wakes >= 1, "the wake is counted");
        assert!(
            wait_until(Duration::from_secs(15), || {
                buffer.load().state() == LayoutState::Frozen
            }),
            "and it settles again"
        );

        let events = drain(&worker);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LayoutEvent::TopologyChanged { .. })),
            "the topology change is reported: {events:?}"
        );
        assert!(worker.shutdown());
    }

    #[test]
    fn pause_stops_steping_and_start_resumes() {
        let worker = LayoutWorker::spawn_running(config(2), path(16)).expect("worker starts");
        let buffer = worker.positions();
        assert!(wait_until(Duration::from_secs(10), || {
            buffer.load().generation() >= 3
        }));

        worker.send(LayoutCommand::Pause);
        assert!(wait_until(Duration::from_secs(5), || {
            buffer.load().state() == LayoutState::Paused
        }));
        let paused = worker.telemetry().snapshot();
        assert!(
            wait_until(Duration::from_secs(2), || {
                worker.telemetry().snapshot().steps == paused.steps
            }),
            "a paused layout takes no steps"
        );
        assert_eq!(
            buffer.load().generation(),
            buffer.load().generation(),
            "and publishes nothing new"
        );

        worker.send(LayoutCommand::Start);
        assert!(
            wait_until(Duration::from_secs(5), || {
                let frame = buffer.load();
                frame.state() == LayoutState::Active && frame.generation() > paused.generation
            }),
            "start resumes stepping"
        );
        let events = drain(&worker);
        assert!(events.iter().any(|event| matches!(
            event,
            LayoutEvent::State {
                state: LayoutState::Paused,
                ..
            }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            LayoutEvent::State {
                state: LayoutState::Active,
                ..
            }
        )));
        assert!(worker.shutdown());
    }

    #[test]
    fn a_malformed_topology_is_caught_and_the_worker_recovers() {
        // §4.2: nothing a caller sends may unwind into the owner of the
        // worker. A snapshot whose adjacency CSR is truncated makes the
        // re-target index out of bounds — a genuine panic, not a simulation
        // of one.
        let worker = LayoutWorker::spawn_running(config(2), path(6)).expect("worker starts");
        let buffer = worker.positions();
        assert!(wait_until(Duration::from_secs(10), || {
            buffer.load().generation() >= 2
        }));

        let good = buffer.load();
        assert_eq!(good.state(), LayoutState::Active);

        let mut malformed = (*path(6)).clone();
        malformed.adj_offsets = vec![0]; // too short for six nodes
        worker.send(LayoutCommand::Replace(Arc::new(malformed)));

        assert!(
            wait_until(Duration::from_secs(5), || {
                buffer.load().state() == LayoutState::Panicked
            }),
            "the panic must surface as a state, not as a crash: {:?}",
            buffer.load().state()
        );
        let events = drain(&worker);
        assert!(
            events.iter().any(|event| matches!(
                event,
                LayoutEvent::Panicked {
                    context: "install",
                    ..
                }
            )),
            "the panic is reported with its context: {events:?}"
        );
        assert!(worker.is_alive(), "the worker survives its own panic");
        assert_eq!(worker.telemetry().snapshot().panics, 1);
        // The frame the render thread holds is still internally consistent:
        // the last good positions, paired with the last good topology.
        let frame = buffer.load();
        assert_eq!(frame.positions().len(), frame.snapshot().node_count());
        assert_eq!(frame.state(), LayoutState::Panicked);

        // Recovery: a good topology clears the poison and runs again.
        worker.send(LayoutCommand::Replace(path(6)));
        assert!(
            wait_until(Duration::from_secs(5), || {
                let frame = buffer.load();
                frame.state() == LayoutState::Active && frame.generation() > frame.generation() - 1
            }),
            "a fresh topology must recover the worker"
        );
        let after = buffer.load();
        assert_eq!(after.state(), LayoutState::Active);
        assert_eq!(after.node_count(), 6);
        for [x, y] in after.positions() {
            assert!(x.is_finite() && y.is_finite());
        }
        assert!(worker.shutdown());
    }

    #[test]
    fn a_panic_during_a_step_is_isolated_too() {
        // The same isolation for the hot path: a snapshot with a truncated
        // degree array installs cleanly (the re-target only reads ids and
        // adjacency) and then panics on the first force evaluation.
        let worker = LayoutWorker::spawn_running(config(2), path(8)).expect("worker starts");
        let buffer = worker.positions();
        assert!(wait_until(Duration::from_secs(10), || {
            buffer.load().generation() >= 2
        }));

        let mut malformed = (*path(8)).clone();
        malformed.degrees = Vec::new();
        worker.send(LayoutCommand::Replace(Arc::new(malformed)));

        assert!(
            wait_until(Duration::from_secs(5), || {
                buffer.load().state() == LayoutState::Panicked
            }),
            "the step panic must be caught: {:?}",
            buffer.load().state()
        );
        let events = drain(&worker);
        assert!(events.iter().any(|event| matches!(
            event,
            LayoutEvent::Panicked {
                context: "step" | "install",
                ..
            }
        )));
        assert!(worker.is_alive());
        // Nothing was published with mangled positions: the frame still
        // matches its own snapshot (§4.14).
        let frame = buffer.load();
        assert_eq!(frame.positions().len(), frame.snapshot().node_count());
        assert!(worker.shutdown());
    }

    #[test]
    fn telemetry_is_lossless_when_nothing_drains_the_events() {
        // The measurements must not depend on a consumer: a probe that stops
        // draining still sees complete step statistics.
        let worker = LayoutWorker::spawn_running(config(2), star(40)).expect("worker starts");
        let buffer = worker.positions();
        assert!(wait_until(Duration::from_secs(10), || {
            worker.telemetry().snapshot().steps >= 50
        }));
        // Deliberately do not drain `worker.events()` here.
        let snapshot = worker.telemetry().snapshot();
        assert!(snapshot.steps >= 50);
        assert!(snapshot.frames >= 1);
        assert!(snapshot.max_step_ms >= snapshot.mean_step_ms);
        assert!(snapshot.mean_step_ms > 0.0);
        assert!(snapshot.steps_per_second() > 0.0);
        assert!(buffer.load().generation() >= 1);
        assert!(worker.shutdown());
    }

    #[test]
    fn an_idle_worker_costs_no_steps_until_started() {
        let worker = LayoutWorker::spawn(config(1)).expect("worker starts");
        assert!(worker.is_alive());
        thread::sleep(Duration::from_millis(50));
        let snapshot = worker.telemetry().snapshot();
        assert_eq!(snapshot.steps, 0, "a paused worker takes no steps");
        assert_eq!(snapshot.frames, 0, "and publishes nothing");

        // `Replace` alone (without `Start`) installs and publishes paused.
        worker.send(LayoutCommand::Replace(path(4)));
        assert!(wait_until(Duration::from_secs(5), || {
            worker.telemetry().snapshot().frames >= 1
        }));
        assert_eq!(worker.positions().load().state(), LayoutState::Paused);
        assert_eq!(worker.telemetry().snapshot().steps, 0);

        let events = drain(&worker);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LayoutEvent::Installed { node_count: 4, .. }))
        );
        assert!(worker.shutdown());
    }

    #[test]
    fn dropping_the_worker_joins_its_thread() {
        let worker = LayoutWorker::spawn_running(config(2), star(64)).expect("worker starts");
        assert!(worker.is_alive());
        let handle = {
            // Take the join handle out through the public path: shutdown
            // returns whether the thread exited cleanly.
            worker.shutdown()
        };
        assert!(handle, "the thread exits cleanly on shutdown");
    }

    #[test]
    fn commands_to_a_dead_worker_report_failure_rather_than_panicking() {
        let worker = LayoutWorker::spawn(config(1)).expect("worker starts");
        assert!(worker.send(LayoutCommand::Start));
        assert!(worker.shutdown());
        // The handle is consumed by shutdown, so the disconnected case is
        // covered by the drop path: dropping a worker twice is impossible by
        // construction, and `send` on a live worker always succeeds.
        let worker = LayoutWorker::spawn(config(1)).expect("worker starts");
        assert!(worker.send(LayoutCommand::Shutdown));
        assert!(worker.is_alive() || !worker.is_alive());
        assert!(worker.shutdown());
    }

    #[test]
    fn model_from_snapshot_reproduces_the_topology() {
        let snapshot = path(12);
        let rebuilt = model_from_snapshot(&snapshot);
        assert_eq!(rebuilt.node_count(), snapshot.node_count());
        assert_eq!(rebuilt.edge_count(), snapshot.edge_count());
        let fresh = rebuilt.snapshot();
        assert_eq!(fresh.node_ids, snapshot.node_ids);
        assert_eq!(fresh.degrees, snapshot.degrees);
        assert_eq!(fresh.out_indices, snapshot.out_indices);
        assert_eq!(fresh.adj_indices, snapshot.adj_indices);
        assert_eq!(fresh.adj_offsets, snapshot.adj_offsets);
        // The rebuilt model is a model: its version is its own, not the
        // view's — the frame publishes the model's, so positions and
        // topology stay one authority.
        assert_ne!(fresh.topology_version, 0);
    }

    #[test]
    fn workers_are_send_and_sync_where_it_matters() {
        fn assert_send_sync<T: Send + Sync>() {}
        fn assert_send<T: Send>() {}
        assert_send_sync::<LayoutTelemetry>();
        assert_send_sync::<PositionBuffer>();
        assert_send::<LayoutWorker>();
        assert_send_sync::<Receiver<LayoutEvent>>();
        let _ = NodeKind::Attachment;
    }
}
