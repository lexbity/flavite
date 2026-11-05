//! Filesystem-only reconcile worker (D14; RFC-0004 S7 parallel scan).
//!
//! This module deliberately has no index or buffer dependencies. It returns
//! immutable filesystem observations; the VaultActor applies them after the
//! per-path save-watermark check.
//!
//! #[derive(Debug, Default, Clone, PartialEq)]Parallel scan structure (§6.6)
//!
//! Three phases, in exactly the pre-S7 order:
//!
//! * (a) **enumeration** — serial, unchanged (`walk_paths` / nominated set);
//! * (b) **observation** — pure [`observe_path`] per file (read + metadata +
//!   note parse); independent per path, so segments of
//!   [`SEGMENT_PATHS`] run on a dedicated bounded [`rayon`] pool
//!   (≤ [`MAX_SCAN_THREADS`] threads) created **once per worker-thread
//!   lifetime** by [`scan_pool`];
//! * (c) **emission** — one [`Emitter`] consumes observations in input order
//!   and owns every externally visible effect: the 128-outcome batch
//!   cadence, the 128-index progress cadence, `incomplete` semantics, and
//!   the channel-error early-exit behavior, all **identical** to the serial
//!   loop they came from (FR-9: parallel output is byte-identical to
//!   serial).
//!
//! `par_iter().map(..).collect()` preserves input order, so `ScanOutcome`
//! batches, `present_notes`/`present_attachments` contents, and progress
//! counters are the same sequence either way. Early exit truncates *emission*
//! only: at most one already-computed segment (≤ `SEGMENT_PATHS` paths) is
//! discarded, which produces no messages, so the stream is unchanged.
//!
//! `FLAVITE_SERIAL_SCAN=1` forces the serial path (equivalence testing,
//! benches, and a bug-blind escape hatch); a machine with
//! `available_parallelism() == 1` degenerates to the same serial path with
//! zero behavioral difference. Worst-case transient in-flight bytes are
//! `SEGMENT_PATHS × MAX_NOTE_BYTES` (256 MiB pathological, ≪ 1 MiB typical —
//! D10's accepted bound; a byte-budget scheduler is the named escalation if
//! large-note vaults appear). The scan worker stays the sole coordinator:
//! the VaultActor application path, watermarks, and job-queue caps are
//! untouched.

use crate::types::{ReconcileMode, ReconcileScope};
use flavite_fs::Fs;
use rayon::prelude::*;
use rayon::{ThreadPool, ThreadPoolBuilder};
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

/// Paths observed per in-flight segment (D10).
pub const SEGMENT_PATHS: usize = 8;
/// Hard ceiling on scan-pool threads (D10): the reconcile worker must never
/// starve the layout worker or the actor threads on a big box.
pub const MAX_SCAN_THREADS: usize = 4;

#[derive(Debug, Clone)]
pub(crate) struct ReconcileJob {
    pub id: u64,
    pub scope: ReconcileScope,
    pub mode: ReconcileMode,
    pub watermark: u64,
    pub nominated: HashSet<PathBuf>,
    pub reply: Option<crossbeam::channel::Sender<crate::actor::VaultActorResp>>,
}

/// The complete public input of one scan (the parts of a [`ReconcileJob`]
/// the filesystem phase consumes; `mode`/`watermark`/`reply` belong to the
/// actor's application side, never to observation or emission).
#[derive(Debug, Clone)]
pub struct ScanJob<'a> {
    pub id: u64,
    pub scope: &'a ReconcileScope,
    pub nominated: &'a HashSet<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)] // the parsed note rides with its bytes by design (§6.6)
pub enum ScanOutcome {
    Note {
        path: PathBuf,
        path_key: Vec<u8>,
        display_path: String,
        size: u64,
        mtime_ns: i64,
        bytes: Option<Vec<u8>>,
        parsed: Option<flavite_syntax::pulldown_glue::ParsedNote>,
    },
    Attachment {
        path: PathBuf,
        path_key: Vec<u8>,
        display_path: String,
        size: u64,
        mtime_ns: i64,
        placeholder: bool,
        hash: Option<[u8; 32]>,
        bytes: Option<Vec<u8>>,
    },
    Missing(PathBuf),
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ScanResult {
    pub outcomes: Vec<ScanOutcome>,
    pub present_notes: HashSet<Vec<u8>>,
    pub present_attachments: HashSet<Vec<u8>>,
    /// If traversal/read metadata was incomplete, absence cannot safely mean deletion.
    pub incomplete: bool,
    pub total: u32,
    pub full_scan: bool,
    pub complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanProgress {
    pub job_id: u64,
    pub done: u32,
    pub total: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkerMessage {
    Progress(ScanProgress),
    Batch {
        job_id: u64,
        outcomes: Vec<ScanOutcome>,
    },
    Complete {
        job_id: u64,
        result: ScanResult,
    },
}

/// The dedicated scan pool for one worker-thread lifetime (§6.6): built once
/// by the `vault-reconcile` thread before its job loop. `None` means the
/// serial path — either forced by `FLAVITE_SERIAL_SCAN=1`, a
/// single-core machine, or a pool-build failure (warned, never fatal).
#[must_use]
pub fn scan_pool() -> Option<ThreadPool> {
    if serial_scan_forced() {
        return None;
    }
    let available = std::thread::available_parallelism()
        .unwrap_or(NonZeroUsize::MIN)
        .get();
    let threads = available.min(MAX_SCAN_THREADS);
    if threads <= 1 {
        return None;
    }
    match ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|index| format!("vault-reconcile-scan-{index}"))
        .build()
    {
        Ok(pool) => Some(pool),
        Err(error) => {
            tracing::warn!(%error, "reconcile scan pool unavailable; the scan degrades to serial");
            None
        }
    }
}

/// `FLAVITE_SERIAL_SCAN=1` forces the serial scan path (equivalence tests,
/// benches, and the bug-blind escape hatch; any other value keeps parallel).
#[must_use]
pub fn serial_scan_forced() -> bool {
    std::env::var("FLAVITE_SERIAL_SCAN").is_ok_and(|value| value == "1")
}

/// Run one reconcile scan. `pool = Some` observes segments of
/// [`SEGMENT_PATHS`] paths in parallel; `pool = None` is the serial path.
/// Both feed the identical [`Emitter`], so the `WorkerMessage` stream and
/// the returned [`ScanResult`] are identical for the same filesystem state
/// (FR-9; `tests/scan_equivalence.rs` is the gate).
#[must_use]
pub fn scan(
    fs: &dyn Fs,
    root: &Path,
    job: &ScanJob<'_>,
    progress: &crossbeam::channel::Sender<WorkerMessage>,
    pool: Option<&ThreadPool>,
) -> ScanResult {
    let requested_paths = if !job.nominated.is_empty() {
        Some(job.nominated.clone())
    } else if let ReconcileScope::Single(path) = job.scope {
        Some(HashSet::from([path.clone()]))
    } else {
        None
    };
    let fallback_full = requested_paths.as_ref().is_some_and(|paths| {
        paths.iter().any(|path| {
            let path = if path.is_absolute() {
                path.clone()
            } else {
                root.join(path)
            };
            match fs.exists(&path) {
                Ok(false) => false,
                Ok(true) => fs.metadata(&path).map_or(true, |metadata| metadata.is_dir),
                Err(_) => true,
            }
        })
    });
    let nominated = if fallback_full { None } else { requested_paths };
    let full_scan = nominated.is_none();
    let (paths, traversal_incomplete) = if let Some(nominated) = nominated {
        (
            nominated
                .into_iter()
                .map(|path| {
                    if path.is_absolute() {
                        path
                    } else {
                        root.join(path)
                    }
                })
                .collect::<Vec<_>>(),
            false,
        )
    } else {
        walk_paths(fs, root)
    };
    let mut emitter = Emitter::new(
        job.id,
        ScanResult {
            total: paths.len().min(u32::MAX as usize) as u32,
            incomplete: traversal_incomplete,
            full_scan,
            ..ScanResult::default()
        },
        progress,
    );
    let __scan_started = std::time::Instant::now();
    match pool {
        None => {
            for (index, path) in paths.iter().enumerate() {
                let observation = observe_path(fs, root, path);
                if !emitter.step(index, observation) {
                    return emitter.into_result();
                }
            }
        }
        Some(pool) => {
            let mut start = 0;
            while start < paths.len() {
                let end = (start + SEGMENT_PATHS).min(paths.len());
                // Order-preserving collect: the segment's observations land
                // in input order, so the emission stream is the serial one.
                let observations: Vec<PathObservation> = pool.install(|| {
                    paths[start..end]
                        .par_iter()
                        .map(|path| observe_path(fs, root, path))
                        .collect()
                });
                for (offset, observation) in observations.into_iter().enumerate() {
                    if !emitter.step(start + offset, observation) {
                        return emitter.into_result();
                    }
                }
                start = end;
            }
        }
    }
    let out = emitter.finish();
    if std::env::var("FLAVITE_SCAN_TIMING").is_ok() {
        eprintln!(
            "worker scan paths={} parallel={} wall={:?}",
            out.total,
            pool.is_some(),
            __scan_started.elapsed()
        );
    }
    out
}

// ---------------------------------------------------------------------------
// (b) Observation — pure per-path filesystem work
// ---------------------------------------------------------------------------

/// One path's contribution to the scan, produced without touching any
/// shared state: the serial loop and the parallel segments feed identical
/// observations into the identical emitter.
#[derive(Debug, Clone, PartialEq)]
enum PathObservation {
    /// A directory, symlink, or an extension neither note nor attachment —
    /// nothing is emitted and nothing is incomplete.
    Skip,
    /// Validation/metadata failed: absence cannot safely mean deletion.
    Incomplete,
    /// The nominated path vanished: a `Missing` outcome.
    Missing(PathBuf),
    /// A present note (bytes may be absent for oversize listings, which is
    /// NOT a read failure; `incomplete` marks only real read errors).
    Note {
        outcome: ScanOutcome,
        incomplete: bool,
    },
    /// A present attachment (`incomplete` marks only real read errors).
    Attachment {
        outcome: ScanOutcome,
        incomplete: bool,
    },
}

fn observe_path(fs: &dyn Fs, root: &Path, path: &Path) -> PathObservation {
    if !path.starts_with(root) || fs.validate_in_root(root, path).is_err() {
        return PathObservation::Incomplete;
    }
    let exists = match fs.exists(path) {
        Ok(exists) => exists,
        Err(_) => return PathObservation::Incomplete,
    };
    if !exists {
        return PathObservation::Missing(path.to_path_buf());
    }
    let metadata = match fs.metadata(path) {
        Ok(metadata) if !metadata.is_dir && !metadata.is_symlink => metadata,
        Ok(_) => return PathObservation::Skip,
        Err(_) => return PathObservation::Incomplete,
    };
    let relative = match path.strip_prefix(root) {
        Ok(relative) => relative,
        Err(_) => return PathObservation::Incomplete,
    };
    let path_key = match fs.encode_path_key(relative) {
        Ok(key) => key,
        Err(_) => return PathObservation::Incomplete,
    };
    let display_path = relative.to_string_lossy().replace('\\', "/");
    if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
    {
        let mut incomplete = false;
        let bytes = if metadata.size <= flavite_index::MAX_NOTE_BYTES as u64 {
            match fs.read_capped(path, flavite_index::MAX_NOTE_BYTES as u64) {
                Ok(bytes) => Some(bytes),
                Err(_) => {
                    incomplete = true;
                    None
                }
            }
        } else {
            None
        };
        let parsed = bytes
            .as_deref()
            .and_then(|bytes| flavite_syntax::pulldown_glue::parse_note(bytes).ok());
        return PathObservation::Note {
            outcome: ScanOutcome::Note {
                path: path.to_path_buf(),
                path_key,
                display_path,
                size: metadata.size,
                mtime_ns: metadata.mtime_ns,
                bytes,
                parsed,
            },
            incomplete,
        };
    }
    let Some(_kind) = attachment_kind(path) else {
        return PathObservation::Skip;
    };
    let mut incomplete = false;
    let placeholder = fs.is_placeholder(path).unwrap_or(false);
    let hash_cap = flavite_index::types::ATTACHMENT_HASH_MAX as u64;
    let bytes_for_hash = if !placeholder && metadata.size <= hash_cap {
        match fs.read_capped(path, hash_cap) {
            Ok(bytes) => Some(bytes),
            Err(_) => {
                incomplete = true;
                None
            }
        }
    } else {
        None
    };
    let hash = bytes_for_hash
        .as_deref()
        .map(|bytes| *blake3::hash(bytes).as_bytes());
    let hydrated = !placeholder && (metadata.size > hash_cap || hash.is_some());
    let bytes = (hydrated && metadata.size <= hash_cap)
        .then(|| bytes_for_hash.clone())
        .flatten();
    PathObservation::Attachment {
        outcome: ScanOutcome::Attachment {
            path: path.to_path_buf(),
            path_key,
            display_path,
            size: metadata.size,
            mtime_ns: metadata.mtime_ns,
            placeholder,
            hash,
            bytes,
        },
        incomplete,
    }
}

// ---------------------------------------------------------------------------
// (c) Emission — the single externally visible cadence
// ---------------------------------------------------------------------------

/// The emission half of the scan, verbatim in cadence and effect from the
/// pre-S7 serial loop: 128-index batch sends (channel error ⇒
/// `incomplete = true` and an early return of the accumulated result),
/// 128-index progress `try_send`s, presence-set growth, outcome collection,
/// the tail batch, and the final progress + `complete` flag. Serial and
/// parallel modes share this one implementation, which is why their message
/// streams are byte-identical, not merely equivalent.
struct Emitter<'p> {
    job_id: u64,
    result: ScanResult,
    progress: &'p crossbeam::channel::Sender<WorkerMessage>,
}

impl<'p> Emitter<'p> {
    fn new(
        job_id: u64,
        result: ScanResult,
        progress: &'p crossbeam::channel::Sender<WorkerMessage>,
    ) -> Self {
        Self {
            job_id,
            result,
            progress,
        }
    }

    /// Process one index. Returns `false` when a batch send failed and the
    /// scan must stop with the accumulated (marked-incomplete) result.
    fn step(&mut self, index: usize, observation: PathObservation) -> bool {
        if index > 0 && index.is_multiple_of(128) && !self.result.outcomes.is_empty() {
            let outcomes = std::mem::take(&mut self.result.outcomes);
            if self
                .progress
                .send(WorkerMessage::Batch {
                    job_id: self.job_id,
                    outcomes,
                })
                .is_err()
            {
                self.result.incomplete = true;
                return false;
            }
        }
        if index.is_multiple_of(128) || index + 1 == self.result.total as usize {
            let _ = self
                .progress
                .try_send(WorkerMessage::Progress(ScanProgress {
                    job_id: self.job_id,
                    done: index.min(u32::MAX as usize) as u32,
                    total: self.result.total,
                }));
        }
        match observation {
            PathObservation::Skip => {}
            PathObservation::Incomplete => self.result.incomplete = true,
            PathObservation::Missing(path) => self.result.outcomes.push(ScanOutcome::Missing(path)),
            PathObservation::Note {
                outcome,
                incomplete,
            } => {
                if let ScanOutcome::Note { path_key, .. } = &outcome {
                    self.result.present_notes.insert(path_key.clone());
                }
                if incomplete {
                    self.result.incomplete = true;
                }
                self.result.outcomes.push(outcome);
            }
            PathObservation::Attachment {
                outcome,
                incomplete,
            } => {
                if let ScanOutcome::Attachment { path_key, .. } = &outcome {
                    self.result.present_attachments.insert(path_key.clone());
                }
                if incomplete {
                    self.result.incomplete = true;
                }
                self.result.outcomes.push(outcome);
            }
        }
        true
    }

    /// The tail: final batch, final progress, and the completion stamp.
    fn finish(mut self) -> ScanResult {
        if !self.result.outcomes.is_empty() {
            let outcomes = std::mem::take(&mut self.result.outcomes);
            if self
                .progress
                .send(WorkerMessage::Batch {
                    job_id: self.job_id,
                    outcomes,
                })
                .is_err()
            {
                self.result.incomplete = true;
                return self.result;
            }
        }
        let total = self.result.total;
        let _ = self
            .progress
            .try_send(WorkerMessage::Progress(ScanProgress {
                job_id: self.job_id,
                done: total,
                total,
            }));
        self.result.complete = true;
        self.result
    }

    /// The early-exit result: `incomplete` is already set by the failing
    /// `step`; the drained batch is gone exactly as in the pre-S7 loop.
    fn into_result(self) -> ScanResult {
        self.result
    }
}

fn attachment_kind(path: &Path) -> Option<()> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "svg"
            | "avif"
            | "bmp"
            | "ico"
            | "pdf"
            | "mp3"
            | "wav"
            | "ogg"
            | "flac"
            | "m4a"
            | "mp4"
            | "webm"
            | "mov"
            | "mkv"
            | "zip"
            | "tar"
            | "gz"
            | "tgz"
    )
    .then_some(())
}

fn walk_paths(fs: &dyn Fs, root: &Path) -> (Vec<PathBuf>, bool) {
    let mut stack = vec![root.to_path_buf()];
    let mut paths = Vec::new();
    let mut incomplete = false;
    while let Some(directory) = stack.pop() {
        let entries = match fs.read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => {
                incomplete = true;
                continue;
            }
        };
        for entry in entries {
            if entry.is_dir {
                if !entry.is_symlink && entry.name != ".flavite" {
                    stack.push(entry.path);
                }
            } else if !entry.is_symlink {
                paths.push(entry.path);
            }
        }
    }
    (paths, incomplete)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_and_thread_constants_match_the_locked_policy() {
        // D10: segment 8 paths, pool ≤ 4 threads.
        assert_eq!(SEGMENT_PATHS, 8);
        assert_eq!(MAX_SCAN_THREADS, 4);
    }

    #[test]
    fn scan_pool_respects_the_thread_cap_and_serial_switch() {
        match scan_pool() {
            None => {
                // Serial is legitimate only via the env switch or a
                // single-core machine — never silently.
                assert!(
                    serial_scan_forced()
                        || std::thread::available_parallelism().is_ok_and(|n| n.get() <= 1),
                    "pool build failed on a parallel machine"
                );
            }
            Some(pool) => {
                assert!(pool.current_num_threads() <= MAX_SCAN_THREADS);
                assert!(pool.current_num_threads() > 1);
                assert!(!serial_scan_forced());
            }
        }
    }
}
