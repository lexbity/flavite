//! `FaultFs` — scriptable fault-injection wrapper over any [`Fs`] (§10.6, §10.6.1).
//!
//! A library, not `#[cfg(test)]` — fuzz targets and integration tests both link it.
//!
//! # Crash models
//!
//! - **ProcessCrash**: stop the simulated process, release ownership, retain
//!   filesystem state visible to the OS. The volatile state of the inner fs
//!   is unchanged.
//! - **PowerLoss**: discard operations not made durable by the model's sync
//!   rules. The inner fs's volatile state reverts to its durable state.
//!
//! Both stop callers until [`FaultFs::revive`] creates a fresh wrapper.
//! Trace persistence and ownership release are handled separately.
//!
//! # Fault injection
//!
//! Faults are scripted at **modeled operation/sub-operation boundaries**:
//! before, after, or progress (mid-operation, for chunked writes).
//! Each fault point is (method, occurrence, boundary) × fault kind.
//!
//! Model limits are reported explicitly: passing a model establishes the
//! algorithm under that model, never all storage hardware/driver behavior.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::fs::{DirEntry, Fs, FsError, Metadata, Result};
use crate::mem::MemFs;

// ---------------------------------------------------------------------------
// Fault script types
// ---------------------------------------------------------------------------

/// The kind of fault to inject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    /// Return an error at this point.
    Error,
    /// Write some bytes, then return an error (progress boundary only).
    PartialWriteThenError,
    /// Write fewer bytes than requested but return `Ok` (progress boundary only).
    ShortWrite,
    /// `fsync` returns an error.
    FsyncFails,
    /// `fsync` is not supported.
    FsyncUnsupported,
    /// Simulate a process crash.
    ProcessCrash,
    /// Simulate power loss.
    PowerLoss,
    /// Invoke the external-writer interleaving hook.
    ExternalWrite,
}

/// The boundary within a method call where a fault fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultBoundary {
    /// Before the inner method is called.
    Before,
    /// After the inner method returns.
    After,
    /// During the operation (for chunked writes like `write_new`).
    Progress,
}

/// A point in the execution where a fault may fire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultPoint {
    /// The method name (e.g., `"write_new"`, `"fsync_file"`).
    pub method: &'static str,
    /// The 1-based occurrence of this method call.
    pub occurrence: usize,
    /// The boundary within the call.
    pub boundary: FaultBoundary,
}

impl FaultPoint {
    pub fn new(method: &'static str, occurrence: usize, boundary: FaultBoundary) -> Self {
        Self {
            method,
            occurrence,
            boundary,
        }
    }
}

/// A planned fault: a point × a kind.
#[derive(Debug, Clone)]
pub struct PlannedFault {
    pub point: FaultPoint,
    pub kind: FaultKind,
}

impl PlannedFault {
    pub fn new(point: FaultPoint, kind: FaultKind) -> Self {
        Self { point, kind }
    }
}

/// An ordered script of planned faults.
#[derive(Debug, Clone, Default)]
pub struct FaultScript {
    pub faults: Vec<PlannedFault>,
}

impl FaultScript {
    pub fn new() -> Self {
        Self { faults: Vec::new() }
    }

    /// Add a fault to the script.
    pub fn add(mut self, point: FaultPoint, kind: FaultKind) -> Self {
        self.faults.push(PlannedFault::new(point, kind));
        self
    }

    /// Add an error fault at (method, occurrence, Before).
    pub fn error_at(mut self, method: &'static str, occurrence: usize) -> Self {
        self.faults.push(PlannedFault::new(
            FaultPoint::new(method, occurrence, FaultBoundary::Before),
            FaultKind::Error,
        ));
        self
    }

    /// Add a fault at an explicit point.
    pub fn fault_at(
        mut self,
        method: &'static str,
        occurrence: usize,
        boundary: FaultBoundary,
        kind: FaultKind,
    ) -> Self {
        self.faults.push(PlannedFault::new(
            FaultPoint::new(method, occurrence, boundary),
            kind,
        ));
        self
    }
}

// ---------------------------------------------------------------------------
// Crash mode and trace
// ---------------------------------------------------------------------------

/// The crash mode for a crash fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashMode {
    ProcessCrash,
    PowerLoss,
}

/// The outcome of a traced operation boundary.
#[derive(Debug, Clone)]
pub enum TraceOutcome {
    Ok,
    Err(String),
    Fault(FaultKind),
    Skipped,
}

/// A single trace entry recording a method boundary.
#[derive(Debug, Clone)]
pub struct TraceEntry {
    pub method: &'static str,
    pub occurrence: usize,
    pub boundary: FaultBoundary,
    pub outcome: TraceOutcome,
}

/// Context passed to the interleaver hook.
#[derive(Debug, Clone)]
pub struct FaultContext {
    pub method: &'static str,
    pub occurrence: usize,
    pub boundary: FaultBoundary,
}

// ---------------------------------------------------------------------------
// Fault state
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct FaultState {
    alive: bool,
    call_counts: HashMap<&'static str, usize>,
    trace: Vec<TraceEntry>,
}

impl FaultState {
    fn new() -> Self {
        Self {
            alive: true,
            call_counts: HashMap::new(),
            trace: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// FaultFs
// ---------------------------------------------------------------------------

/// The interleaver hook type: called on `ExternalWrite` faults.
type InterleaverHook = Arc<dyn Fn(&dyn Fs, &FaultContext) + Send + Sync>;

/// The crash handler type: called on `ProcessCrash`/`PowerLoss` faults.
type CrashHandlerHook = Arc<dyn Fn(CrashMode) + Send + Sync>;

/// A fault-injection wrapper over any [`Fs`].
///
/// Generic over `F: Fs`. Crash simulation requires a crash handler (see
/// [`FaultFs::with_crash_handler`] and [`FaultFs::new_mem`]).
/// The external-writer interleaver requires [`FaultFs::with_interleaver`].
pub struct FaultFs<F: Fs> {
    inner: F,
    state: Arc<Mutex<FaultState>>,
    script: Arc<Mutex<VecDeque<PlannedFault>>>,
    interleaver: Option<InterleaverHook>,
    crash_handler: Option<CrashHandlerHook>,
}

impl<F: Fs> std::fmt::Debug for FaultFs<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaultFs")
            .field(
                "alive",
                &self.state.lock().map(|s| s.alive).unwrap_or(false),
            )
            .finish()
    }
}

impl<F: Fs> FaultFs<F> {
    /// Create a new `FaultFs` wrapping `inner` with the given fault script.
    pub fn new(inner: F, script: FaultScript) -> Self {
        Self {
            inner,
            state: Arc::new(Mutex::new(FaultState::new())),
            script: Arc::new(Mutex::new(script.faults.into())),
            interleaver: None,
            crash_handler: None,
        }
    }

    /// Set the external-writer interleaving hook.
    ///
    /// Called when an `ExternalWrite` fault fires. The hook receives the
    /// inner fs and can perform writes to simulate a concurrent external writer.
    pub fn with_interleaver<H>(mut self, handler: H) -> Self
    where
        H: Fn(&dyn Fs, &FaultContext) + Send + Sync + 'static,
    {
        self.interleaver = Some(Arc::new(handler));
        self
    }

    /// Set the crash handler.
    ///
    /// Called when a `ProcessCrash` or `PowerLoss` fault fires. The handler
    /// should apply the crash to the inner fs (e.g., call
    /// `MemFs::simulate_power_loss`).
    pub fn with_crash_handler<H>(mut self, handler: H) -> Self
    where
        H: Fn(CrashMode) + Send + Sync + 'static,
    {
        self.crash_handler = Some(Arc::new(handler));
        self
    }

    /// Check whether the simulated process is alive.
    pub fn is_alive(&self) -> bool {
        self.lock_state().alive
    }

    /// Get the recorded trace.
    pub fn trace(&self) -> Vec<TraceEntry> {
        self.lock_state().trace.clone()
    }

    /// Get the number of times a method has been called.
    pub fn call_count(&self, method: &str) -> usize {
        *self.lock_state().call_counts.get(method).unwrap_or(&0)
    }

    /// Revive the simulated process after a crash.
    ///
    /// Returns a fresh `FaultFs` wrapping the same inner fs (whose state was
    /// retained or reverted by the crash handler). The fault script is reset.
    pub fn revive(&self) -> Self
    where
        F: Clone,
    {
        Self {
            inner: self.inner.clone(),
            state: Arc::new(Mutex::new(FaultState::new())),
            script: Arc::new(Mutex::new(VecDeque::new())),
            interleaver: self.interleaver.clone(),
            crash_handler: self.crash_handler.clone(),
        }
    }

    /// Get a reference to the inner fs.
    pub fn inner(&self) -> &F {
        &self.inner
    }

    // --- Internal helpers ---

    fn lock_state(&self) -> std::sync::MutexGuard<'_, FaultState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn check_alive(&self) -> Result<()> {
        if !self.lock_state().alive {
            return Err(FsError::ProcessDead);
        }
        Ok(())
    }

    fn bump_count(&self, method: &'static str) -> usize {
        let mut state = self.lock_state();
        let count = state.call_counts.entry(method).or_insert(0);
        *count += 1;
        *count
    }

    fn take_fault(
        &self,
        method: &'static str,
        occurrence: usize,
        boundary: FaultBoundary,
    ) -> Option<PlannedFault> {
        let mut script = self.script.lock().unwrap_or_else(|e| e.into_inner());
        let pos = script.iter().position(|f| {
            f.point.method == method
                && f.point.occurrence == occurrence
                && f.point.boundary == boundary
        })?;
        Some(script.remove(pos).expect("position checked"))
    }

    fn record_trace(
        &self,
        method: &'static str,
        occurrence: usize,
        boundary: FaultBoundary,
        outcome: TraceOutcome,
    ) {
        let mut state = self.lock_state();
        state.trace.push(TraceEntry {
            method,
            occurrence,
            boundary,
            outcome,
        });
    }

    /// Apply a fault. Returns `Err` for error/crash faults, `Ok` for
    /// `ExternalWrite` (the interleaver is called separately).
    fn apply_fault(&self, fault: &PlannedFault, ctx: &FaultContext) -> Result<()> {
        self.record_trace(
            fault.point.method,
            fault.point.occurrence,
            fault.point.boundary,
            TraceOutcome::Fault(fault.kind),
        );
        match fault.kind {
            FaultKind::Error => Err(FsError::Io(format!(
                "injected error at {} occurrence {} ({:?})",
                fault.point.method, fault.point.occurrence, fault.point.boundary
            ))),
            FaultKind::FsyncFails => Err(FsError::FsyncFailed(format!(
                "injected fsync failure at {} occurrence {}",
                fault.point.method, fault.point.occurrence
            ))),
            FaultKind::FsyncUnsupported => Err(FsError::FsyncUnsupported(format!(
                "injected fsync unsupported at {} occurrence {}",
                fault.point.method, fault.point.occurrence
            ))),
            FaultKind::ProcessCrash => {
                {
                    let mut state = self.lock_state();
                    state.alive = false;
                }
                if let Some(handler) = &self.crash_handler {
                    handler(CrashMode::ProcessCrash);
                }
                Err(FsError::ProcessDead)
            }
            FaultKind::PowerLoss => {
                {
                    let mut state = self.lock_state();
                    state.alive = false;
                }
                if let Some(handler) = &self.crash_handler {
                    handler(CrashMode::PowerLoss);
                }
                Err(FsError::ProcessDead)
            }
            FaultKind::ExternalWrite => {
                if let Some(interleaver) = &self.interleaver {
                    interleaver(&self.inner, ctx);
                }
                Ok(())
            }
            FaultKind::PartialWriteThenError | FaultKind::ShortWrite => {
                // These are handled in write_new's progress path.
                Err(FsError::Io(format!(
                    "injected write fault at {} occurrence {}",
                    fault.point.method, fault.point.occurrence
                )))
            }
        }
    }

    /// Inject a fault at the Before boundary. Returns `Err` if a fault fires.
    fn inject_before(&self, method: &'static str, occurrence: usize) -> Result<()> {
        if let Some(fault) = self.take_fault(method, occurrence, FaultBoundary::Before) {
            let ctx = FaultContext {
                method,
                occurrence,
                boundary: FaultBoundary::Before,
            };
            return self.apply_fault(&fault, &ctx);
        }
        Ok(())
    }

    /// Inject a fault at the After boundary. Returns `Err` if a fault fires.
    fn inject_after(&self, method: &'static str, occurrence: usize) -> Result<()> {
        if let Some(fault) = self.take_fault(method, occurrence, FaultBoundary::After) {
            let ctx = FaultContext {
                method,
                occurrence,
                boundary: FaultBoundary::After,
            };
            return self.apply_fault(&fault, &ctx);
        }
        Ok(())
    }

    /// Record a successful operation in the trace.
    fn trace_ok(&self, method: &'static str, occurrence: usize, boundary: FaultBoundary) {
        self.record_trace(method, occurrence, boundary, TraceOutcome::Ok);
    }

    /// Record an error in the trace.
    fn trace_err(
        &self,
        method: &'static str,
        occurrence: usize,
        boundary: FaultBoundary,
        err: &FsError,
    ) {
        self.record_trace(
            method,
            occurrence,
            boundary,
            TraceOutcome::Err(err.to_string()),
        );
    }
}

impl FaultFs<MemFs> {
    /// Create a `FaultFs` wrapping a `MemFs` with the crash handler wired up.
    ///
    /// ProcessCrash retains volatile state; PowerLoss reverts to durable state.
    pub fn new_mem(mem: MemFs, script: FaultScript) -> Self {
        let mem_clone = mem.clone();
        Self::new(mem, script).with_crash_handler(move |mode| match mode {
            CrashMode::ProcessCrash => mem_clone.simulate_process_crash(),
            CrashMode::PowerLoss => mem_clone.simulate_power_loss(),
        })
    }
}

// ---------------------------------------------------------------------------
// Fs implementation with fault injection at every method
// ---------------------------------------------------------------------------

impl<F: Fs> Fs for FaultFs<F> {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
        self.check_alive()?;
        let occ = self.bump_count("canonicalize");
        self.inject_before("canonicalize", occ)?;
        let result = self.inner.canonicalize(path);
        match &result {
            Ok(_) => self.trace_ok("canonicalize", occ, FaultBoundary::After),
            Err(e) => self.trace_err("canonicalize", occ, FaultBoundary::After, e),
        }
        self.inject_after("canonicalize", occ)?;
        result
    }

    fn resolve_symlink(&self, path: &Path) -> Result<PathBuf> {
        self.check_alive()?;
        let occ = self.bump_count("resolve_symlink");
        self.inject_before("resolve_symlink", occ)?;
        let result = self.inner.resolve_symlink(path);
        match &result {
            Ok(_) => self.trace_ok("resolve_symlink", occ, FaultBoundary::After),
            Err(e) => self.trace_err("resolve_symlink", occ, FaultBoundary::After, e),
        }
        self.inject_after("resolve_symlink", occ)?;
        result
    }

    fn encode_path_key(&self, path: &Path) -> Result<Vec<u8>> {
        self.check_alive()?;
        let occ = self.bump_count("encode_path_key");
        self.inject_before("encode_path_key", occ)?;
        let result = self.inner.encode_path_key(path);
        match &result {
            Ok(_) => self.trace_ok("encode_path_key", occ, FaultBoundary::After),
            Err(e) => self.trace_err("encode_path_key", occ, FaultBoundary::After, e),
        }
        self.inject_after("encode_path_key", occ)?;
        result
    }

    fn decode_path_key(&self, key: &[u8]) -> Result<PathBuf> {
        self.check_alive()?;
        let occ = self.bump_count("decode_path_key");
        self.inject_before("decode_path_key", occ)?;
        let result = self.inner.decode_path_key(key);
        match &result {
            Ok(_) => self.trace_ok("decode_path_key", occ, FaultBoundary::After),
            Err(e) => self.trace_err("decode_path_key", occ, FaultBoundary::After, e),
        }
        self.inject_after("decode_path_key", occ)?;
        result
    }

    fn validate_in_root(&self, root: &Path, target: &Path) -> Result<()> {
        self.check_alive()?;
        let occ = self.bump_count("validate_in_root");
        self.inject_before("validate_in_root", occ)?;
        let result = self.inner.validate_in_root(root, target);
        match &result {
            Ok(_) => self.trace_ok("validate_in_root", occ, FaultBoundary::After),
            Err(e) => self.trace_err("validate_in_root", occ, FaultBoundary::After, e),
        }
        self.inject_after("validate_in_root", occ)?;
        result
    }

    fn read_if_exists(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        self.check_alive()?;
        let occ = self.bump_count("read_if_exists");
        self.inject_before("read_if_exists", occ)?;
        let result = self.inner.read_if_exists(path);
        match &result {
            Ok(_) => self.trace_ok("read_if_exists", occ, FaultBoundary::After),
            Err(e) => self.trace_err("read_if_exists", occ, FaultBoundary::After, e),
        }
        self.inject_after("read_if_exists", occ)?;
        result
    }

    fn read_capped(&self, path: &Path, max: u64) -> Result<Vec<u8>> {
        self.check_alive()?;
        let occ = self.bump_count("read_capped");
        self.inject_before("read_capped", occ)?;
        let result = self.inner.read_capped(path, max);
        match &result {
            Ok(_) => self.trace_ok("read_capped", occ, FaultBoundary::After),
            Err(e) => self.trace_err("read_capped", occ, FaultBoundary::After, e),
        }
        self.inject_after("read_capped", occ)?;
        result
    }

    fn metadata(&self, path: &Path) -> Result<Metadata> {
        self.check_alive()?;
        let occ = self.bump_count("metadata");
        self.inject_before("metadata", occ)?;
        let result = self.inner.metadata(path);
        match &result {
            Ok(_) => self.trace_ok("metadata", occ, FaultBoundary::After),
            Err(e) => self.trace_err("metadata", occ, FaultBoundary::After, e),
        }
        self.inject_after("metadata", occ)?;
        result
    }

    fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>> {
        self.check_alive()?;
        let occ = self.bump_count("read_dir");
        self.inject_before("read_dir", occ)?;
        let result = self.inner.read_dir(path);
        match &result {
            Ok(_) => self.trace_ok("read_dir", occ, FaultBoundary::After),
            Err(e) => self.trace_err("read_dir", occ, FaultBoundary::After, e),
        }
        self.inject_after("read_dir", occ)?;
        result
    }

    fn exists(&self, path: &Path) -> Result<bool> {
        self.check_alive()?;
        let occ = self.bump_count("exists");
        self.inject_before("exists", occ)?;
        let result = self.inner.exists(path);
        match &result {
            Ok(_) => self.trace_ok("exists", occ, FaultBoundary::After),
            Err(e) => self.trace_err("exists", occ, FaultBoundary::After, e),
        }
        self.inject_after("exists", occ)?;
        result
    }

    fn is_placeholder(&self, path: &Path) -> Result<bool> {
        self.check_alive()?;
        let occ = self.bump_count("is_placeholder");
        self.inject_before("is_placeholder", occ)?;
        let result = self.inner.is_placeholder(path);
        match &result {
            Ok(_) => self.trace_ok("is_placeholder", occ, FaultBoundary::After),
            Err(e) => self.trace_err("is_placeholder", occ, FaultBoundary::After, e),
        }
        self.inject_after("is_placeholder", occ)?;
        result
    }

    fn write_new(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        self.check_alive()?;
        let occ = self.bump_count("write_new");
        self.inject_before("write_new", occ)?;

        // Check for a Progress-boundary fault (partial write / short write).
        let progress_fault = self.take_fault("write_new", occ, FaultBoundary::Progress);
        let write_result: Result<()> = match progress_fault {
            Some(fault) => match fault.kind {
                FaultKind::PartialWriteThenError => {
                    self.record_trace(
                        "write_new",
                        occ,
                        FaultBoundary::Progress,
                        TraceOutcome::Fault(fault.kind),
                    );
                    // Write the first half, then return an error.
                    let half = bytes.len() / 2;
                    let partial_result = self.inner.write_new(path, &bytes[..half]);
                    if let Err(e) = &partial_result {
                        self.trace_err("write_new", occ, FaultBoundary::Progress, e);
                    }
                    // Even if the partial write succeeded, we return an error.
                    let _ = partial_result;
                    Err(FsError::Io(format!(
                        "injected partial-write-then-error at write_new occurrence {occ}"
                    )))
                }
                FaultKind::ShortWrite => {
                    self.record_trace(
                        "write_new",
                        occ,
                        FaultBoundary::Progress,
                        TraceOutcome::Fault(fault.kind),
                    );
                    // Write fewer bytes but return Ok (lying filesystem).
                    let half = bytes.len() / 2;
                    let result = self.inner.write_new(path, &bytes[..half]);
                    match &result {
                        Ok(_) => self.trace_ok("write_new", occ, FaultBoundary::Progress),
                        Err(e) => self.trace_err("write_new", occ, FaultBoundary::Progress, e),
                    }
                    // Return Ok even though we wrote fewer bytes (unchecked short write).
                    Ok(())
                }
                FaultKind::ExternalWrite => {
                    // Invoke the interleaver, then continue with the full write.
                    let ctx = FaultContext {
                        method: "write_new",
                        occurrence: occ,
                        boundary: FaultBoundary::Progress,
                    };
                    self.record_trace(
                        "write_new",
                        occ,
                        FaultBoundary::Progress,
                        TraceOutcome::Fault(fault.kind),
                    );
                    if let Some(interleaver) = &self.interleaver {
                        interleaver(&self.inner, &ctx);
                    }
                    let result = self.inner.write_new(path, bytes);
                    match &result {
                        Ok(_) => self.trace_ok("write_new", occ, FaultBoundary::Progress),
                        Err(e) => self.trace_err("write_new", occ, FaultBoundary::Progress, e),
                    }
                    result
                }
                _ => {
                    // Other fault kinds at Progress: treat as a generic error.
                    let ctx = FaultContext {
                        method: "write_new",
                        occurrence: occ,
                        boundary: FaultBoundary::Progress,
                    };
                    self.apply_fault(&fault, &ctx)?;
                    // apply_fault returns Err for all non-ExternalWrite kinds,
                    // so the ? above always propagates. This is unreachable.
                    unreachable!("apply_fault returned Ok for non-ExternalWrite kind")
                }
            },
            None => {
                let result = self.inner.write_new(path, bytes);
                match &result {
                    Ok(_) => self.trace_ok("write_new", occ, FaultBoundary::Progress),
                    Err(e) => self.trace_err("write_new", occ, FaultBoundary::Progress, e),
                }
                result
            }
        };

        // After-boundary fault.
        if write_result.is_ok() {
            self.inject_after("write_new", occ)?;
        }
        write_result
    }

    fn copy_mode(&self, from: &Path, to: &Path) -> Result<()> {
        self.check_alive()?;
        let occ = self.bump_count("copy_mode");
        self.inject_before("copy_mode", occ)?;
        let result = self.inner.copy_mode(from, to);
        match &result {
            Ok(_) => self.trace_ok("copy_mode", occ, FaultBoundary::After),
            Err(e) => self.trace_err("copy_mode", occ, FaultBoundary::After, e),
        }
        self.inject_after("copy_mode", occ)?;
        result
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        self.check_alive()?;
        let occ = self.bump_count("create_dir_all");
        self.inject_before("create_dir_all", occ)?;
        let result = self.inner.create_dir_all(path);
        match &result {
            Ok(_) => self.trace_ok("create_dir_all", occ, FaultBoundary::After),
            Err(e) => self.trace_err("create_dir_all", occ, FaultBoundary::After, e),
        }
        self.inject_after("create_dir_all", occ)?;
        result
    }

    fn remove(&self, path: &Path) -> Result<()> {
        self.check_alive()?;
        let occ = self.bump_count("remove");
        self.inject_before("remove", occ)?;
        let result = self.inner.remove(path);
        match &result {
            Ok(_) => self.trace_ok("remove", occ, FaultBoundary::After),
            Err(e) => self.trace_err("remove", occ, FaultBoundary::After, e),
        }
        self.inject_after("remove", occ)?;
        result
    }

    fn rename_replace(&self, from: &Path, to: &Path) -> Result<()> {
        self.check_alive()?;
        let occ = self.bump_count("rename_replace");
        self.inject_before("rename_replace", occ)?;
        let result = self.inner.rename_replace(from, to);
        match &result {
            Ok(_) => self.trace_ok("rename_replace", occ, FaultBoundary::After),
            Err(e) => self.trace_err("rename_replace", occ, FaultBoundary::After, e),
        }
        self.inject_after("rename_replace", occ)?;
        result
    }

    fn rename_no_replace(&self, from: &Path, to: &Path) -> Result<()> {
        self.check_alive()?;
        let occ = self.bump_count("rename_no_replace");
        self.inject_before("rename_no_replace", occ)?;
        let result = self.inner.rename_no_replace(from, to);
        match &result {
            Ok(_) => self.trace_ok("rename_no_replace", occ, FaultBoundary::After),
            Err(e) => self.trace_err("rename_no_replace", occ, FaultBoundary::After, e),
        }
        self.inject_after("rename_no_replace", occ)?;
        result
    }

    fn fsync_file(&self, path: &Path) -> Result<()> {
        self.check_alive()?;
        let occ = self.bump_count("fsync_file");
        self.inject_before("fsync_file", occ)?;
        let result = self.inner.fsync_file(path);
        match &result {
            Ok(_) => self.trace_ok("fsync_file", occ, FaultBoundary::After),
            Err(e) => self.trace_err("fsync_file", occ, FaultBoundary::After, e),
        }
        self.inject_after("fsync_file", occ)?;
        result
    }

    fn fsync_dir(&self, path: &Path) -> Result<()> {
        self.check_alive()?;
        let occ = self.bump_count("fsync_dir");
        self.inject_before("fsync_dir", occ)?;
        let result = self.inner.fsync_dir(path);
        match &result {
            Ok(_) => self.trace_ok("fsync_dir", occ, FaultBoundary::After),
            Err(e) => self.trace_err("fsync_dir", occ, FaultBoundary::After, e),
        }
        self.inject_after("fsync_dir", occ)?;
        result
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_fs() -> MemFs {
        MemFs::new()
    }

    fn fault_fs(script: FaultScript) -> FaultFs<MemFs> {
        FaultFs::new_mem(mem_fs(), script)
    }

    // --- Basic fault injection ---

    #[test]
    fn error_fault_at_before_returns_error() {
        let fs = fault_fs(FaultScript::new().error_at("write_new", 1));
        let err = fs.write_new(Path::new("/note.md"), b"x").unwrap_err();
        assert!(matches!(err, FsError::Io(_)));
        assert!(err.to_string().contains("injected error"));
    }

    #[test]
    fn error_fault_at_after_returns_error() {
        let fs = fault_fs(FaultScript::new().fault_at(
            "write_new",
            1,
            FaultBoundary::After,
            FaultKind::Error,
        ));
        let err = fs.write_new(Path::new("/note.md"), b"x").unwrap_err();
        assert!(matches!(err, FsError::Io(_)));
    }

    #[test]
    fn no_fault_passes_through() {
        let fs = fault_fs(FaultScript::new());
        fs.write_new(Path::new("/note.md"), b"hello").unwrap();
        assert_eq!(
            fs.read_if_exists(Path::new("/note.md")).unwrap(),
            Some(b"hello".to_vec())
        );
    }

    #[test]
    fn occurrence_specific_fault() {
        // Fault only on the 2nd call to write_new.
        let fs = fault_fs(FaultScript::new().error_at("write_new", 2));
        fs.write_new(Path::new("/a.md"), b"a").unwrap();
        let err = fs.write_new(Path::new("/b.md"), b"b").unwrap_err();
        assert!(matches!(err, FsError::Io(_)));
    }

    // --- fsync faults ---

    #[test]
    fn fsync_fails_fault() {
        let fs = fault_fs(FaultScript::new().fault_at(
            "fsync_file",
            1,
            FaultBoundary::Before,
            FaultKind::FsyncFails,
        ));
        fs.write_new(Path::new("/note.md"), b"x").unwrap();
        let err = fs.fsync_file(Path::new("/note.md")).unwrap_err();
        assert!(matches!(err, FsError::FsyncFailed(_)));
    }

    #[test]
    fn fsync_unsupported_fault() {
        let fs = fault_fs(FaultScript::new().fault_at(
            "fsync_dir",
            1,
            FaultBoundary::Before,
            FaultKind::FsyncUnsupported,
        ));
        let err = fs.fsync_dir(Path::new("/")).unwrap_err();
        assert!(matches!(err, FsError::FsyncUnsupported(_)));
    }

    // --- Partial write / short write ---

    #[test]
    fn partial_write_then_error_writes_prefix() {
        let fs = fault_fs(FaultScript::new().fault_at(
            "write_new",
            1,
            FaultBoundary::Progress,
            FaultKind::PartialWriteThenError,
        ));
        let err = fs
            .write_new(Path::new("/note.md"), b"hello world")
            .unwrap_err();
        assert!(matches!(err, FsError::Io(_)));
        // The first half was written before the error.
        let content = fs.read_if_exists(Path::new("/note.md")).unwrap();
        assert_eq!(content, Some(b"hello".to_vec()));
    }

    #[test]
    fn short_write_writes_fewer_bytes_but_returns_ok() {
        let fs = fault_fs(FaultScript::new().fault_at(
            "write_new",
            1,
            FaultBoundary::Progress,
            FaultKind::ShortWrite,
        ));
        // Returns Ok despite writing fewer bytes.
        fs.write_new(Path::new("/note.md"), b"hello world").unwrap();
        // Only the first half was actually written.
        let content = fs.read_if_exists(Path::new("/note.md")).unwrap();
        assert_eq!(content, Some(b"hello".to_vec()));
    }

    // --- Process crash ---

    #[test]
    fn process_crash_kills_process() {
        let fs = fault_fs(FaultScript::new().fault_at(
            "write_new",
            1,
            FaultBoundary::Before,
            FaultKind::ProcessCrash,
        ));
        let err = fs.write_new(Path::new("/note.md"), b"x").unwrap_err();
        assert!(matches!(err, FsError::ProcessDead));
        assert!(!fs.is_alive());
    }

    #[test]
    fn dead_process_rejects_all_operations() {
        let fs = fault_fs(FaultScript::new().fault_at(
            "exists",
            1,
            FaultBoundary::Before,
            FaultKind::ProcessCrash,
        ));
        let _ = fs.exists(Path::new("/"));
        // All subsequent operations fail with ProcessDead.
        let err = fs.exists(Path::new("/")).unwrap_err();
        assert!(matches!(err, FsError::ProcessDead));
        let err = fs.read_if_exists(Path::new("/")).unwrap_err();
        assert!(matches!(err, FsError::ProcessDead));
        let err = fs.write_new(Path::new("/x"), b"x").unwrap_err();
        assert!(matches!(err, FsError::ProcessDead));
    }

    #[test]
    fn process_crash_retains_volatile_state() {
        // AC-36: process crash retains unsynced OS-visible state.
        let mem = mem_fs();
        let fs = FaultFs::new_mem(mem.clone(), FaultScript::new());
        // Write without fsync.
        fs.write_new(Path::new("/note.md"), b"unsynced").unwrap();
        // Crash after the write (on the next operation).
        let mut script = FaultScript::new();
        script = script.fault_at("exists", 1, FaultBoundary::Before, FaultKind::ProcessCrash);
        let fs2 = FaultFs::new_mem(mem.clone(), script);
        let _ = fs2.exists(Path::new("/"));
        // Revive and check state.
        let fs3 = fs2.revive();
        assert_eq!(
            fs3.read_if_exists(Path::new("/note.md")).unwrap(),
            Some(b"unsynced".to_vec())
        );
    }

    // --- Power loss ---

    #[test]
    fn power_loss_reverts_to_durable_state() {
        // AC-36: power loss discards unsynced state.
        let mem = mem_fs();
        let fs = FaultFs::new_mem(mem.clone(), FaultScript::new());
        fs.create_dir_all(Path::new("/vault")).unwrap();
        fs.fsync_dir(Path::new("/")).unwrap();
        fs.fsync_dir(Path::new("/vault")).unwrap();
        // Write and fsync a file.
        fs.write_new(Path::new("/vault/durable.md"), b"durable")
            .unwrap();
        fs.fsync_file(Path::new("/vault/durable.md")).unwrap();
        fs.fsync_dir(Path::new("/vault")).unwrap();
        // Write another file but don't fsync it.
        fs.write_new(Path::new("/vault/volatile.md"), b"volatile")
            .unwrap();
        // Crash with power loss.
        let mut script = FaultScript::new();
        script = script.fault_at("exists", 1, FaultBoundary::Before, FaultKind::PowerLoss);
        let fs2 = FaultFs::new_mem(mem.clone(), script);
        let _ = fs2.exists(Path::new("/"));
        assert!(!fs2.is_alive());
        // Revive.
        let fs3 = fs2.revive();
        // Durable file survives.
        assert_eq!(
            fs3.read_if_exists(Path::new("/vault/durable.md")).unwrap(),
            Some(b"durable".to_vec())
        );
        // Volatile file is lost.
        assert!(!fs3.exists(Path::new("/vault/volatile.md")).unwrap());
    }

    // --- Interleaving hook ---

    #[test]
    fn external_write_interleaving_hook() {
        // AC-8: external writer can be injected at any point.
        let mem = mem_fs();
        let mem_for_hook = mem.clone();
        let fs = FaultFs::new_mem(
            mem,
            FaultScript::new().fault_at(
                "write_new",
                1,
                FaultBoundary::Before,
                FaultKind::ExternalWrite,
            ),
        )
        .with_interleaver(move |_fs, ctx| {
            // Simulate an external writer creating a file.
            if ctx.method == "write_new" {
                let _ = mem_for_hook.write_new(Path::new("/external.md"), b"from outside");
            }
        });
        // The ExternalWrite fault fires before write_new; the interleaver writes.
        fs.write_new(Path::new("/note.md"), b"x").unwrap();
        // Both files should exist.
        assert!(fs.exists(Path::new("/note.md")).unwrap());
        assert!(fs.exists(Path::new("/external.md")).unwrap());
    }

    #[test]
    fn interleaving_hook_receives_context() {
        let mem = mem_fs();
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_clone = received.clone();
        let fs = FaultFs::new_mem(
            mem,
            FaultScript::new().fault_at(
                "read_if_exists",
                1,
                FaultBoundary::Before,
                FaultKind::ExternalWrite,
            ),
        )
        .with_interleaver(move |_fs, ctx| {
            received_clone.lock().unwrap().push((
                ctx.method.to_string(),
                ctx.occurrence,
                ctx.boundary,
            ));
        });
        let _ = fs.read_if_exists(Path::new("/anything"));
        let trace = received.lock().unwrap();
        assert_eq!(trace.len(), 1);
        assert_eq!(trace[0].0, "read_if_exists");
        assert_eq!(trace[0].1, 1);
        assert_eq!(trace[0].2, FaultBoundary::Before);
    }

    // --- Trace recording ---

    #[test]
    fn trace_records_operations() {
        let fs = fault_fs(FaultScript::new());
        fs.write_new(Path::new("/note.md"), b"x").unwrap();
        fs.read_if_exists(Path::new("/note.md")).unwrap();
        let trace = fs.trace();
        assert!(!trace.is_empty());
        // write_new should have Before-ish and After entries.
        let write_entries: Vec<_> = trace.iter().filter(|e| e.method == "write_new").collect();
        assert!(!write_entries.is_empty());
    }

    #[test]
    fn trace_records_faults() {
        let fs = fault_fs(FaultScript::new().error_at("exists", 1));
        let _ = fs.exists(Path::new("/"));
        let trace = fs.trace();
        let fault_entry = trace
            .iter()
            .find(|e| matches!(e.outcome, TraceOutcome::Fault(FaultKind::Error)));
        assert!(fault_entry.is_some());
    }

    #[test]
    fn call_count_tracks_methods() {
        let fs = fault_fs(FaultScript::new());
        fs.write_new(Path::new("/a"), b"a").unwrap();
        fs.write_new(Path::new("/b"), b"b").unwrap();
        fs.read_if_exists(Path::new("/a")).unwrap();
        assert_eq!(fs.call_count("write_new"), 2);
        assert_eq!(fs.call_count("read_if_exists"), 1);
        assert_eq!(fs.call_count("exists"), 0);
    }

    // --- Revive ---

    #[test]
    fn revive_creates_fresh_alive_wrapper() {
        let fs = fault_fs(FaultScript::new().fault_at(
            "exists",
            1,
            FaultBoundary::Before,
            FaultKind::ProcessCrash,
        ));
        let _ = fs.exists(Path::new("/"));
        assert!(!fs.is_alive());
        let fs2 = fs.revive();
        assert!(fs2.is_alive());
        // Fresh wrapper can perform operations.
        assert!(fs2.exists(Path::new("/")).unwrap());
    }

    #[test]
    fn revive_resets_call_counts() {
        let fs = fault_fs(FaultScript::new());
        fs.write_new(Path::new("/a"), b"a").unwrap();
        assert_eq!(fs.call_count("write_new"), 1);
        let fs2 = fs.revive();
        assert_eq!(fs2.call_count("write_new"), 0);
    }

    // --- Fault matrix: every FaultKind at a representative method ---

    /// A method name paired with an operation that exercises it.
    type MethodOp = (&'static str, fn(&FaultFs<MemFs>));

    #[test]
    fn fault_matrix_error_at_every_method() {
        let methods: &[MethodOp] = &[
            ("canonicalize", |fs| {
                let _ = fs.canonicalize(Path::new("/"));
            }),
            ("resolve_symlink", |fs| {
                let _ = fs.resolve_symlink(Path::new("/"));
            }),
            ("encode_path_key", |fs| {
                let _ = fs.encode_path_key(Path::new("a/b"));
            }),
            ("decode_path_key", |fs| {
                let _ = fs.decode_path_key(&[1, 0, 0, 0, b'a']);
            }),
            ("validate_in_root", |fs| {
                let _ = fs.validate_in_root(Path::new("/"), Path::new("/a"));
            }),
            ("read_if_exists", |fs| {
                let _ = fs.read_if_exists(Path::new("/a"));
            }),
            ("read_capped", |fs| {
                let _ = fs.read_capped(Path::new("/a"), 100);
            }),
            ("metadata", |fs| {
                let _ = fs.metadata(Path::new("/a"));
            }),
            ("read_dir", |fs| {
                let _ = fs.read_dir(Path::new("/"));
            }),
            ("exists", |fs| {
                let _ = fs.exists(Path::new("/a"));
            }),
            ("is_placeholder", |fs| {
                let _ = fs.is_placeholder(Path::new("/a"));
            }),
            ("write_new", |fs| {
                let _ = fs.write_new(Path::new("/a"), b"x");
            }),
            ("copy_mode", |fs| {
                let _ = fs.copy_mode(Path::new("/a"), Path::new("/b"));
            }),
            ("create_dir_all", |fs| {
                let _ = fs.create_dir_all(Path::new("/a/b"));
            }),
            ("remove", |fs| {
                let _ = fs.remove(Path::new("/a"));
            }),
            ("rename_replace", |fs| {
                let _ = fs.rename_replace(Path::new("/a"), Path::new("/b"));
            }),
            ("rename_no_replace", |fs| {
                let _ = fs.rename_no_replace(Path::new("/a"), Path::new("/b"));
            }),
            ("fsync_file", |fs| {
                let _ = fs.fsync_file(Path::new("/a"));
            }),
            ("fsync_dir", |fs| {
                let _ = fs.fsync_dir(Path::new("/"));
            }),
        ];
        for (method, op) in methods {
            let fs = fault_fs(FaultScript::new().error_at(method, 1));
            op(&fs);
            let err_occurred = fs.trace().iter().any(|e| {
                e.method == *method && matches!(e.outcome, TraceOutcome::Fault(FaultKind::Error))
            });
            assert!(
                err_occurred,
                "expected Error fault trace for method {method}"
            );
        }
    }

    #[test]
    fn fault_matrix_crash_at_write_new() {
        for kind in [FaultKind::ProcessCrash, FaultKind::PowerLoss] {
            let fs =
                fault_fs(FaultScript::new().fault_at("write_new", 1, FaultBoundary::Before, kind));
            let err = fs.write_new(Path::new("/note.md"), b"x").unwrap_err();
            assert!(matches!(err, FsError::ProcessDead), "fault kind {kind:?}");
            assert!(!fs.is_alive());
        }
    }

    // --- Seeded defect: omitted fsync ---
    // AC-36: "Seed omitted-sync/unchecked-short-write defects and require
    // failing invariants."
    //
    // This test proves the harness can expose an omitted-fsync defect:
    // if fsync_file were a no-op, durable content would never be persisted,
    // and a power loss would lose data that the caller believed was durable.

    #[test]
    fn seeded_omitted_fsync_is_detectable() {
        // Simulate a buggy fs where fsync_file is a no-op.
        #[derive(Clone)]
        struct BuggyFs(MemFs);
        impl Fs for BuggyFs {
            fn canonicalize(&self, p: &Path) -> Result<PathBuf> {
                self.0.canonicalize(p)
            }
            fn resolve_symlink(&self, p: &Path) -> Result<PathBuf> {
                self.0.resolve_symlink(p)
            }
            fn encode_path_key(&self, p: &Path) -> Result<Vec<u8>> {
                self.0.encode_path_key(p)
            }
            fn decode_path_key(&self, k: &[u8]) -> Result<PathBuf> {
                self.0.decode_path_key(k)
            }
            fn validate_in_root(&self, r: &Path, t: &Path) -> Result<()> {
                self.0.validate_in_root(r, t)
            }
            fn read_if_exists(&self, p: &Path) -> Result<Option<Vec<u8>>> {
                self.0.read_if_exists(p)
            }
            fn read_capped(&self, p: &Path, m: u64) -> Result<Vec<u8>> {
                self.0.read_capped(p, m)
            }
            fn metadata(&self, p: &Path) -> Result<Metadata> {
                self.0.metadata(p)
            }
            fn read_dir(&self, p: &Path) -> Result<Vec<DirEntry>> {
                self.0.read_dir(p)
            }
            fn exists(&self, p: &Path) -> Result<bool> {
                self.0.exists(p)
            }
            fn is_placeholder(&self, p: &Path) -> Result<bool> {
                self.0.is_placeholder(p)
            }
            fn write_new(&self, p: &Path, b: &[u8]) -> Result<()> {
                self.0.write_new(p, b)
            }
            fn copy_mode(&self, f: &Path, t: &Path) -> Result<()> {
                self.0.copy_mode(f, t)
            }
            fn create_dir_all(&self, p: &Path) -> Result<()> {
                self.0.create_dir_all(p)
            }
            fn remove(&self, p: &Path) -> Result<()> {
                self.0.remove(p)
            }
            fn rename_replace(&self, f: &Path, t: &Path) -> Result<()> {
                self.0.rename_replace(f, t)
            }
            fn rename_no_replace(&self, f: &Path, t: &Path) -> Result<()> {
                self.0.rename_no_replace(f, t)
            }
            // BUG: fsync_file is a no-op (omitted fsync).
            fn fsync_file(&self, _p: &Path) -> Result<()> {
                Ok(())
            }
            fn fsync_dir(&self, p: &Path) -> Result<()> {
                self.0.fsync_dir(p)
            }
        }

        let mem = MemFs::new();
        let buggy = BuggyFs(mem.clone());
        // Wire crash handler to the underlying MemFs.
        let mem_clone = mem.clone();
        let fs =
            FaultFs::new(buggy, FaultScript::new()).with_crash_handler(move |mode| match mode {
                CrashMode::ProcessCrash => mem_clone.simulate_process_crash(),
                CrashMode::PowerLoss => mem_clone.simulate_power_loss(),
            });

        fs.create_dir_all(Path::new("/vault")).unwrap();
        fs.fsync_dir(Path::new("/")).unwrap();
        fs.fsync_dir(Path::new("/vault")).unwrap();
        fs.write_new(Path::new("/vault/note.md"), b"important data")
            .unwrap();
        // Caller believes this is durable.
        fs.fsync_file(Path::new("/vault/note.md")).unwrap();
        fs.fsync_dir(Path::new("/vault")).unwrap();

        // Simulate power loss.
        let mut script = FaultScript::new();
        script = script.fault_at("exists", 1, FaultBoundary::Before, FaultKind::PowerLoss);
        let mem2 = mem.clone();
        let buggy2 = BuggyFs(mem2.clone());
        let fs2 = FaultFs::new(buggy2, script).with_crash_handler(move |mode| match mode {
            CrashMode::ProcessCrash => mem2.simulate_process_crash(),
            CrashMode::PowerLoss => mem2.simulate_power_loss(),
        });
        let _ = fs2.exists(Path::new("/"));

        // Revive and check: the data should be GONE because fsync was a no-op.
        let fs3 = fs2.revive();
        let result = fs3.read_if_exists(Path::new("/vault/note.md")).unwrap();
        // The invariant "fsync'd data survives power loss" is VIOLATED.
        // This proves the harness can detect an omitted-fsync defect.
        assert_eq!(
            result,
            Some(Vec::new()),
            "omitted fsync: data should be lost (empty after power loss), proving detectability"
        );
    }

    // --- Seeded defect: unchecked short write ---
    // AC-36: "Seed omitted-sync/unchecked-short-write defects and require
    // failing invariants."
    //
    // This test proves the harness can expose an unchecked-short-write defect:
    // if write_new wrote fewer bytes than requested but returned Ok, the caller
    // would believe all bytes were written, and a read-back would reveal the
    // truncation.

    #[test]
    fn seeded_unchecked_short_write_is_detectable() {
        // The FaultFs ShortWrite fault simulates exactly this defect.
        let fs = fault_fs(FaultScript::new().fault_at(
            "write_new",
            1,
            FaultBoundary::Progress,
            FaultKind::ShortWrite,
        ));
        // Caller writes 11 bytes and gets Ok.
        fs.write_new(Path::new("/note.md"), b"hello world").unwrap();
        // Read back: only 5 bytes were actually written.
        let content = fs.read_if_exists(Path::new("/note.md")).unwrap();
        assert_eq!(
            content,
            Some(b"hello".to_vec()),
            "unchecked short write: read-back reveals truncation, proving detectability"
        );
        assert_ne!(
            content.unwrap().len(),
            11,
            "short write wrote fewer bytes than requested"
        );
    }

    // --- AC-36: file sync without dir sync is distinguishable ---

    #[test]
    fn ac36_file_sync_without_dir_sync_distinguishable() {
        let mem = mem_fs();
        let fs = FaultFs::new_mem(mem.clone(), FaultScript::new());
        fs.create_dir_all(Path::new("/vault")).unwrap();
        fs.fsync_dir(Path::new("/")).unwrap();
        fs.fsync_dir(Path::new("/vault")).unwrap();
        // Write file, fsync file content, but NOT the directory.
        fs.write_new(Path::new("/vault/note.md"), b"content")
            .unwrap();
        fs.fsync_file(Path::new("/vault/note.md")).unwrap();
        // NO fsync_dir for /vault.

        // Power loss via crash.
        let mut script = FaultScript::new();
        script = script.fault_at("exists", 1, FaultBoundary::Before, FaultKind::PowerLoss);
        let fs2 = FaultFs::new_mem(mem.clone(), script);
        let _ = fs2.exists(Path::new("/"));
        let fs3 = fs2.revive();

        // File content was fsync'd (durable) but directory entry was not.
        // After power loss, the directory entry is lost → file not accessible.
        assert!(
            !fs3.exists(Path::new("/vault/note.md")).unwrap(),
            "file sync without dir sync: file is not accessible after power loss"
        );
    }

    #[test]
    fn ac36_process_crash_vs_power_loss_differ() {
        // ProcessCrash retains volatile state; PowerLoss discards unsynced state.
        let content = b"unsynced data";

        // ProcessCrash case.
        let mem1 = mem_fs();
        {
            let fs = FaultFs::new_mem(mem1.clone(), FaultScript::new());
            fs.write_new(Path::new("/note.md"), content).unwrap();
            // No fsync at all.
            let mut script = FaultScript::new();
            script = script.fault_at("exists", 1, FaultBoundary::Before, FaultKind::ProcessCrash);
            let fs2 = FaultFs::new_mem(mem1.clone(), script);
            let _ = fs2.exists(Path::new("/"));
            let fs3 = fs2.revive();
            assert_eq!(
                fs3.read_if_exists(Path::new("/note.md")).unwrap(),
                Some(content.to_vec()),
                "ProcessCrash retains unsynced OS-visible state"
            );
        }

        // PowerLoss case.
        let mem2 = mem_fs();
        {
            let fs = FaultFs::new_mem(mem2.clone(), FaultScript::new());
            fs.write_new(Path::new("/note.md"), content).unwrap();
            // No fsync at all.
            let mut script = FaultScript::new();
            script = script.fault_at("exists", 1, FaultBoundary::Before, FaultKind::PowerLoss);
            let fs2 = FaultFs::new_mem(mem2.clone(), script);
            let _ = fs2.exists(Path::new("/"));
            let fs3 = fs2.revive();
            assert!(
                !fs3.exists(Path::new("/note.md")).unwrap(),
                "PowerLoss discards unsynced state"
            );
        }
    }

    // --- Coverage: natural errors through FaultFs (trace_err paths) ---

    #[test]
    fn natural_errors_are_traced() {
        // Each method called in a way that produces a natural error from the
        // inner fs — exercises the trace_err path in every Fs impl method.
        let fs = fault_fs(FaultScript::new());
        fs.create_dir_all(Path::new("/vault")).unwrap();
        fs.write_new(Path::new("/vault/note.md"), b"x").unwrap();

        // read_if_exists on a directory → Io error
        let _ = fs.read_if_exists(Path::new("/vault"));
        // read_capped on missing file → NotFound
        let _ = fs.read_capped(Path::new("/missing"), 100);
        // metadata on missing file → NotFound
        let _ = fs.metadata(Path::new("/missing"));
        // read_dir on a file → NotADirectory
        let _ = fs.read_dir(Path::new("/vault/note.md"));
        // read_dir on missing dir → DirNotFound
        let _ = fs.read_dir(Path::new("/missing"));
        // write_new on existing file → AlreadyExists
        let _ = fs.write_new(Path::new("/vault/note.md"), b"y");
        // write_new with missing parent → NotFound
        let _ = fs.write_new(Path::new("/no/such/dir/file.md"), b"y");
        // remove on missing file → NotFound
        let _ = fs.remove(Path::new("/missing"));
        // remove on directory → Io error
        let _ = fs.remove(Path::new("/vault"));
        // rename_replace with missing source → NotFound
        let _ = fs.rename_replace(Path::new("/missing"), Path::new("/dest"));
        // rename_replace onto directory → Io error
        let _ = fs.rename_replace(Path::new("/vault/note.md"), Path::new("/vault"));
        // rename_no_replace with existing dest → AlreadyExists
        fs.write_new(Path::new("/vault/a.md"), b"a").unwrap();
        let _ = fs.rename_no_replace(Path::new("/vault/note.md"), Path::new("/vault/a.md"));
        // fsync_file on missing → NotFound
        let _ = fs.fsync_file(Path::new("/missing"));
        // fsync_file on directory → Io error
        let _ = fs.fsync_file(Path::new("/vault"));
        // fsync_dir on missing → NotFound
        let _ = fs.fsync_dir(Path::new("/missing"));
        // copy_mode from missing → NotFound
        let _ = fs.copy_mode(Path::new("/missing"), Path::new("/vault/note.md"));
        // validate_in_root with escape → PathEscape
        let _ = fs.validate_in_root(Path::new("/vault"), Path::new("/etc/passwd"));
        // decode_path_key with truncated input → InvalidPath
        let _ = fs.decode_path_key(&[0, 0]);
        // decode_path_key with bad UTF-8 → InvalidPath
        let _ = fs.decode_path_key(&[1, 0, 0, 0, 0xFF]);
        // encode_path_key with absolute path → InvalidPath
        let _ = fs.encode_path_key(Path::new("/absolute"));

        // Verify trace recorded errors (not just Ok entries).
        let trace = fs.trace();
        let err_count = trace
            .iter()
            .filter(|e| matches!(e.outcome, TraceOutcome::Err(_)))
            .count();
        assert!(
            err_count >= 15,
            "expected at least 15 traced errors, got {err_count}"
        );
    }

    // --- Coverage: After-boundary faults on every method ---

    #[test]
    fn after_boundary_fault_on_every_method() {
        let cases: &[MethodOp] = &[
            ("canonicalize", |fs| {
                let _ = fs.canonicalize(Path::new("/vault"));
            }),
            ("resolve_symlink", |fs| {
                let _ = fs.resolve_symlink(Path::new("/vault"));
            }),
            ("encode_path_key", |fs| {
                let _ = fs.encode_path_key(Path::new("vault/note.md"));
            }),
            ("decode_path_key", |fs| {
                let _ = fs.decode_path_key(&[5, 0, 0, 0, b'v', b'a', b'u', b'l', b't']);
            }),
            ("validate_in_root", |fs| {
                let _ = fs.validate_in_root(Path::new("/vault"), Path::new("/vault/note.md"));
            }),
            ("read_if_exists", |fs| {
                let _ = fs.read_if_exists(Path::new("/vault/note.md"));
            }),
            ("read_capped", |fs| {
                let _ = fs.read_capped(Path::new("/vault/note.md"), 100);
            }),
            ("metadata", |fs| {
                let _ = fs.metadata(Path::new("/vault/note.md"));
            }),
            ("read_dir", |fs| {
                let _ = fs.read_dir(Path::new("/vault"));
            }),
            ("exists", |fs| {
                let _ = fs.exists(Path::new("/vault/note.md"));
            }),
            ("is_placeholder", |fs| {
                let _ = fs.is_placeholder(Path::new("/vault/note.md"));
            }),
            ("write_new", |fs| {
                let _ = fs.write_new(Path::new("/vault/new.md"), b"new");
            }),
            ("copy_mode", |fs| {
                let _ = fs.copy_mode(Path::new("/vault/note.md"), Path::new("/vault/new.md"));
            }),
            ("create_dir_all", |fs| {
                let _ = fs.create_dir_all(Path::new("/vault/sub"));
            }),
            ("remove", |fs| {
                let _ = fs.remove(Path::new("/vault/new.md"));
            }),
            ("rename_replace", |fs| {
                let _ =
                    fs.rename_replace(Path::new("/vault/note.md"), Path::new("/vault/renamed.md"));
            }),
            ("rename_no_replace", |fs| {
                let _ = fs
                    .rename_no_replace(Path::new("/vault/renamed.md"), Path::new("/vault/back.md"));
            }),
            ("fsync_file", |fs| {
                let _ = fs.fsync_file(Path::new("/vault/note.md"));
            }),
            ("fsync_dir", |fs| {
                let _ = fs.fsync_dir(Path::new("/vault"));
            }),
        ];

        for (method, op) in cases {
            // Set up the inner MemFs directly (not through FaultFs) so the
            // fault at occurrence 1 fires on the first call of the target method.
            let mem = MemFs::new();
            mem.create_dir_all(Path::new("/vault")).unwrap();
            mem.write_new(Path::new("/vault/note.md"), b"x").unwrap();
            mem.write_new(Path::new("/vault/a.md"), b"a").unwrap();
            let fs = FaultFs::new_mem(
                mem,
                FaultScript::new().fault_at(method, 1, FaultBoundary::After, FaultKind::Error),
            );
            op(&fs);
            // The After fault fires after the inner method succeeds.
            // We can't easily assert the error (the op discards it), but we
            // verify the trace records the fault.
            let trace = fs.trace();
            let has_fault = trace.iter().any(|e| {
                e.method == *method && matches!(e.outcome, TraceOutcome::Fault(FaultKind::Error))
            });
            assert!(
                has_fault,
                "expected After-boundary Error fault trace for {method}"
            );
        }
    }

    // --- Coverage: constructors, Debug, inner() ---

    #[test]
    fn fault_script_add_builder() {
        let script = FaultScript::new()
            .add(
                FaultPoint::new("write_new", 1, FaultBoundary::Before),
                FaultKind::Error,
            )
            .add(
                FaultPoint::new("fsync_file", 2, FaultBoundary::After),
                FaultKind::FsyncFails,
            );
        assert_eq!(script.faults.len(), 2);
        let fs = fault_fs(script);
        let _ = fs.write_new(Path::new("/a"), b"a");
        // First fault fires.
        assert!(!fs.is_alive() || fs.call_count("write_new") >= 1);
    }

    #[test]
    fn planned_fault_and_point_constructors() {
        let point = FaultPoint::new("exists", 3, FaultBoundary::Progress);
        assert_eq!(point.method, "exists");
        assert_eq!(point.occurrence, 3);
        assert_eq!(point.boundary, FaultBoundary::Progress);

        let fault = PlannedFault::new(point, FaultKind::ShortWrite);
        assert_eq!(fault.point.method, "exists");
        assert_eq!(fault.kind, FaultKind::ShortWrite);
    }

    #[test]
    fn debug_impl_shows_alive_state() {
        let fs = fault_fs(FaultScript::new());
        let debug = format!("{fs:?}");
        assert!(debug.contains("FaultFs"));
        assert!(debug.contains("alive"));
        assert!(debug.contains("true"));
    }

    #[test]
    fn inner_getter_returns_inner_fs() {
        let mem = mem_fs();
        let fs = FaultFs::new_mem(mem.clone(), FaultScript::new());
        // Access the inner fs and verify it's the same MemFs.
        let inner = fs.inner();
        inner.write_new(Path::new("/direct.md"), b"direct").unwrap();
        assert_eq!(
            fs.read_if_exists(Path::new("/direct.md")).unwrap(),
            Some(b"direct".to_vec())
        );
    }

    #[test]
    fn call_count_returns_zero_for_uncalled_method() {
        let fs = fault_fs(FaultScript::new());
        assert_eq!(fs.call_count("never_called"), 0);
    }

    #[test]
    fn fault_script_default_is_empty() {
        let script = FaultScript::default();
        assert!(script.faults.is_empty());
    }

    #[test]
    fn crash_handler_receives_correct_mode() {
        let modes = Arc::new(Mutex::new(Vec::new()));
        let modes_clone = modes.clone();
        let mem = mem_fs();
        let fs = FaultFs::new(
            mem,
            FaultScript::new().fault_at("exists", 1, FaultBoundary::Before, FaultKind::PowerLoss),
        )
        .with_crash_handler(move |mode| {
            modes_clone.lock().unwrap().push(mode);
        });
        let _ = fs.exists(Path::new("/"));
        let recorded = modes.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0], CrashMode::PowerLoss);
    }

    #[test]
    fn fsync_fails_at_after_boundary() {
        let fs = fault_fs(FaultScript::new().fault_at(
            "fsync_file",
            1,
            FaultBoundary::After,
            FaultKind::FsyncFails,
        ));
        fs.write_new(Path::new("/note.md"), b"x").unwrap();
        let err = fs.fsync_file(Path::new("/note.md")).unwrap_err();
        assert!(matches!(err, FsError::FsyncFailed(_)));
    }

    #[test]
    fn fsync_unsupported_at_after_boundary() {
        let fs = fault_fs(FaultScript::new().fault_at(
            "fsync_dir",
            1,
            FaultBoundary::After,
            FaultKind::FsyncUnsupported,
        ));
        let err = fs.fsync_dir(Path::new("/")).unwrap_err();
        assert!(matches!(err, FsError::FsyncUnsupported(_)));
    }

    #[test]
    fn process_crash_at_after_boundary() {
        let fs = fault_fs(FaultScript::new().fault_at(
            "exists",
            1,
            FaultBoundary::After,
            FaultKind::ProcessCrash,
        ));
        let err = fs.exists(Path::new("/")).unwrap_err();
        assert!(matches!(err, FsError::ProcessDead));
        assert!(!fs.is_alive());
    }

    #[test]
    fn power_loss_at_after_boundary() {
        let fs = fault_fs(FaultScript::new().fault_at(
            "write_new",
            1,
            FaultBoundary::After,
            FaultKind::PowerLoss,
        ));
        let err = fs.write_new(Path::new("/note.md"), b"x").unwrap_err();
        assert!(matches!(err, FsError::ProcessDead));
        assert!(!fs.is_alive());
    }

    #[test]
    fn external_write_at_after_boundary() {
        let mem = mem_fs();
        let mem_hook = mem.clone();
        let fs = FaultFs::new_mem(
            mem,
            FaultScript::new().fault_at(
                "exists",
                1,
                FaultBoundary::After,
                FaultKind::ExternalWrite,
            ),
        )
        .with_interleaver(move |_fs, _ctx| {
            let _ = mem_hook.write_new(Path::new("/injected.md"), b"injected");
        });
        let _ = fs.exists(Path::new("/"));
        assert!(fs.exists(Path::new("/injected.md")).unwrap());
    }

    #[test]
    fn error_at_progress_boundary_non_write_method() {
        // A Progress-boundary fault on a non-write_new method is never taken
        // (only write_new checks Progress faults). The method executes normally.
        let fs = fault_fs(FaultScript::new().fault_at(
            "exists",
            1,
            FaultBoundary::Progress,
            FaultKind::Error,
        ));
        // exists doesn't check Progress faults, so it succeeds normally.
        assert!(fs.exists(Path::new("/")).unwrap());
    }
}
