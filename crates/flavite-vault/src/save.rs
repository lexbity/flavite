//! Journaled-by-history atomic saves (D6, §4.4, §4.5.3).

use crate::history::{HistoryError, HistoryReason, HistoryStore};
use flavite_fs::Fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskPrecondition {
    Existing([u8; 32]),
    Absent,
}

#[derive(Debug, Clone)]
pub struct SaveRequest {
    pub request_id: u64,
    pub path: PathBuf,
    pub note_key: Vec<u8>,
    pub bytes: Vec<u8>,
    /// Exact base bytes from the loaded/saved snapshot. The actor durably pins
    /// this merge base before it begins a destructive save.
    pub merge_base: Vec<u8>,
    pub precondition: DiskPrecondition,
    pub buffer_id: u64,
    pub revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurabilityOutcome {
    NotReplaced,
    ReplacedDurabilityUncertain,
    ReplacedIndexPending,
    OutcomeUnknown,
    /// The replacement installed our bytes, but an external writer raced at
    /// the exchange: the retained displaced bytes differ from the expected
    /// base. Never represented as a clean Saved (RFC-0006 D-10/§6.5).
    ReplacedDisplacedConflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveFailureStage {
    Resolve,
    StageWrite,
    StageSync,
    PriorRead,
    History,
    Compare,
    Replace,
    DirectorySync,
    IndexApply,
    /// Archiving the retained displaced object failed; the object stays on
    /// disk (never deleted) until durable archival succeeds.
    Archive,
}

#[derive(Debug, Clone, Error)]
#[error("save failed during {stage:?} ({outcome:?}): {message}")]
pub struct SaveFailure {
    pub stage: SaveFailureStage,
    pub outcome: DurabilityOutcome,
    pub current_hash: Option<[u8; 32]>,
    pub request_id: Option<u64>,
    pub buffer_id: Option<u64>,
    pub revision: Option<u64>,
    pub message: String,
}

impl SaveFailure {
    pub(crate) fn from_message(message: impl ToString) -> Self {
        Self::at(
            SaveFailureStage::History,
            DurabilityOutcome::NotReplaced,
            message,
        )
    }
    pub(crate) fn for_request(mut self, request: &SaveRequest) -> Self {
        self.request_id = Some(request.request_id);
        self.buffer_id = Some(request.buffer_id);
        self.revision = Some(request.revision);
        self
    }
    fn at(stage: SaveFailureStage, outcome: DurabilityOutcome, err: impl ToString) -> Self {
        Self {
            stage,
            outcome,
            current_hash: None,
            request_id: None,
            buffer_id: None,
            revision: None,
            message: err.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveReceipt {
    pub request_id: u64,
    pub buffer_id: u64,
    pub saved_revision: u64,
    pub hash: [u8; 32],
    pub epoch: u64,
    pub generation: u64,
}

/// Save exact serialized source bytes. The index callback is invoked only after
/// replacement and directory sync; an error there never repeats the overwrite.
pub fn save<F>(
    fs: &dyn Fs,
    history: &mut HistoryStore,
    request: SaveRequest,
    apply_index: F,
) -> Result<SaveReceipt, SaveFailure>
where
    F: FnOnce(&Path, &[u8], i64) -> Result<(u64, u64), String>,
{
    let result = save_inner(fs, history, request.clone(), apply_index);
    result.map_err(|failure| failure.for_request(&request))
}

fn save_inner<F>(
    fs: &dyn Fs,
    history: &mut HistoryStore,
    request: SaveRequest,
    apply_index: F,
) -> Result<SaveReceipt, SaveFailure>
where
    F: FnOnce(&Path, &[u8], i64) -> Result<(u64, u64), String>,
{
    let real = fs.resolve_symlink(&request.path).map_err(|e| {
        SaveFailure::at(SaveFailureStage::Resolve, DurabilityOutcome::NotReplaced, e)
    })?;

    // Prior bytes are snapshotted into history BEFORE any source effect; if
    // an external writer raced earlier, those exact bytes are preserved.
    let prior = match fs.read_if_exists(&real) {
        Ok(v) => v,
        Err(e) => {
            return Err(SaveFailure::at(
                SaveFailureStage::PriorRead,
                DurabilityOutcome::NotReplaced,
                e,
            ));
        }
    };
    if let Some(ref bytes) = prior {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if let Err(e) = history.snapshot(&request.note_key, bytes, HistoryReason::Save, false, now)
        {
            return Err(SaveFailure::at(
                SaveFailureStage::History,
                DurabilityOutcome::NotReplaced,
                e,
            ));
        }
    }
    let current = prior.as_deref().map(|b| *blake3::hash(b).as_bytes());
    let matches = match request.precondition {
        DiskPrecondition::Absent => current.is_none(),
        DiskPrecondition::Existing(hash) => current == Some(hash),
    };
    if !matches {
        let mut failure = SaveFailure::at(
            SaveFailureStage::Compare,
            DurabilityOutcome::NotReplaced,
            "disk content changed since the buffer base was captured",
        );
        failure.current_hash = current;
        return Err(failure);
    }

    // The shared retained-replacement primitive (RFC-0006 D-10/§6.5):
    // stage+fsync the exact output, re-check the base, exchange atomically
    // while retaining the ACTUAL displaced object, sync the directory, and
    // verify the displaced bytes against the expected base. Platforms
    // without the guarantee refuse with Unsupported — there is no hidden
    // legacy overwrite fallback.
    let expected_base_hash = match request.precondition {
        DiskPrecondition::Absent => None,
        DiskPrecondition::Existing(hash) => Some(hash),
    };
    let retained = flavite_fs::RetainedReplaceRequest {
        target: real.clone(),
        expected_base_hash,
        bytes: request.bytes.clone(),
    };
    let outcome = match fs.replace_preserving_displaced(&retained) {
        Ok(outcome) => outcome,
        Err(error) => {
            let mut failure = match error {
                flavite_fs::FsError::BaseMismatch { actual, .. } => {
                    let mut failure = SaveFailure::at(
                        SaveFailureStage::Compare,
                        DurabilityOutcome::NotReplaced,
                        "disk content changed since the buffer base was captured",
                    );
                    failure.current_hash = Some(actual);
                    failure
                }
                flavite_fs::FsError::Unsupported(_) => SaveFailure::at(
                    SaveFailureStage::Replace,
                    DurabilityOutcome::NotReplaced,
                    "retained replacement is not available on this filesystem; the save capability is refused rather than silently weakened",
                ),
                flavite_fs::FsError::AlreadyExists(_) => SaveFailure::at(
                    SaveFailureStage::Compare,
                    DurabilityOutcome::NotReplaced,
                    "a file appeared at the destination during save",
                ),
                flavite_fs::FsError::ShortWrite { .. } => SaveFailure::at(
                    SaveFailureStage::StageWrite,
                    DurabilityOutcome::NotReplaced,
                    "the staged output write failed before any source effect",
                ),
                flavite_fs::FsError::RetainedReplace(message) => SaveFailure::at(
                    SaveFailureStage::Replace,
                    DurabilityOutcome::OutcomeUnknown,
                    message,
                ),
                other => SaveFailure::at(
                    SaveFailureStage::Replace,
                    DurabilityOutcome::OutcomeUnknown,
                    other,
                ),
            };
            failure.current_hash = fs
                .read_if_exists(&real)
                .ok()
                .flatten()
                .map(|b| *blake3::hash(&b).as_bytes());
            return Err(failure);
        }
    };
    let displaced_conflict = match &outcome {
        flavite_fs::RetainedReplaceOutcome::Created => false,
        flavite_fs::RetainedReplaceOutcome::Replaced {
            displaced_matches_base,
            ..
        } => !displaced_matches_base,
    };
    if displaced_conflict {
        // An external writer modified the retained object between the base
        // check and the exchange. Our bytes ARE installed, but this is NOT
        // a clean Saved: the displaced bytes are archived durably and the
        // conflict is reported. Never auto-rolled-back against a possibly
        // newer external writer.
        if let flavite_fs::RetainedReplaceOutcome::Replaced { displaced, .. } = &outcome
            && let Ok(Some(bytes)) = fs.read_if_exists(&displaced.path)
        {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let archived = history.snapshot(
                &request.note_key,
                &bytes,
                HistoryReason::Displaced,
                false,
                now,
            );
            if archived.is_ok() {
                let _ = fs.remove(&displaced.path);
                let _ = fs.remove(displaced.path.parent().expect("staging parent"));
            }
        }
        let mut failure = SaveFailure::at(
            SaveFailureStage::Archive,
            DurabilityOutcome::ReplacedDisplacedConflict,
            "an external writer raced during save; your bytes are installed and the external writer's bytes are preserved",
        );
        failure.current_hash = Some(*blake3::hash(&request.bytes).as_bytes());
        return Err(failure);
    }
    // Durable archival of the displaced object (normal saves retire it only
    // after the archival receipt), then best-effort removal of the sibling
    // staging directory.
    if let flavite_fs::RetainedReplaceOutcome::Replaced { displaced, .. } = &outcome {
        let displaced_bytes = fs.read_if_exists(&displaced.path);
        match displaced_bytes {
            Ok(Some(bytes)) if *blake3::hash(&bytes).as_bytes() == displaced.hash => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                match history.snapshot(
                    &request.note_key,
                    &bytes,
                    HistoryReason::Displaced,
                    false,
                    now,
                ) {
                    Ok(_) => {
                        let stage_dir = displaced.path.parent();
                        let _ = fs.remove(&displaced.path);
                        if let Some(dir) = stage_dir {
                            let _ = fs.remove(dir);
                        }
                    }
                    Err(e) => {
                        let mut failure = SaveFailure::at(
                            SaveFailureStage::Archive,
                            DurabilityOutcome::ReplacedDurabilityUncertain,
                            e,
                        );
                        failure.current_hash = Some(*blake3::hash(&request.bytes).as_bytes());
                        return Err(failure);
                    }
                }
            }
            Ok(_) => {
                let mut failure = SaveFailure::at(
                    SaveFailureStage::Archive,
                    DurabilityOutcome::ReplacedDurabilityUncertain,
                    "displaced object bytes did not match the retained object receipt",
                );
                failure.current_hash = Some(*blake3::hash(&request.bytes).as_bytes());
                return Err(failure);
            }
            Err(e) => {
                let mut failure = SaveFailure::at(
                    SaveFailureStage::Archive,
                    DurabilityOutcome::ReplacedDurabilityUncertain,
                    e,
                );
                failure.current_hash = Some(*blake3::hash(&request.bytes).as_bytes());
                return Err(failure);
            }
        }
    }

    let hash = *blake3::hash(&request.bytes).as_bytes();
    let mtime_ns = fs
        .metadata(&real)
        .map(|metadata| metadata.mtime_ns)
        .map_err(|error| {
            let mut failure = SaveFailure::at(
                SaveFailureStage::IndexApply,
                DurabilityOutcome::ReplacedIndexPending,
                format!("cannot read metadata for replaced file: {error}"),
            );
            failure.current_hash = Some(hash);
            failure
        })?;
    let (epoch, generation) = match apply_index(&real, &request.bytes, mtime_ns) {
        Ok(version) => version,
        Err(e) => {
            let mut failure = SaveFailure::at(
                SaveFailureStage::IndexApply,
                DurabilityOutcome::ReplacedIndexPending,
                e,
            );
            failure.current_hash = Some(hash);
            return Err(failure);
        }
    };
    Ok(SaveReceipt {
        request_id: request.request_id,
        buffer_id: request.buffer_id,
        saved_revision: request.revision,
        hash,
        epoch,
        generation,
    })
}

impl From<HistoryError> for SaveFailure {
    fn from(value: HistoryError) -> Self {
        Self::at(
            SaveFailureStage::History,
            DurabilityOutcome::NotReplaced,
            value,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryStore;
    use flavite_fs::{FaultBoundary, FaultContext, FaultFs, FaultKind, FaultScript, Fs, MemFs};
    use std::sync::Arc;

    fn setup() -> (Arc<MemFs>, HistoryStore, PathBuf) {
        let fs = Arc::new(MemFs::new());
        fs.create_dir_all(Path::new("/vault")).unwrap();
        fs.write_new(Path::new("/vault/a.md"), b"prior").unwrap();
        let history = HistoryStore::open(fs.clone(), PathBuf::from("/data/history")).unwrap();
        (fs, history, PathBuf::from("/vault/a.md"))
    }

    fn request(path: PathBuf, expected: &[u8]) -> SaveRequest {
        SaveRequest {
            request_id: 7,
            path,
            note_key: b"a.md".to_vec(),
            bytes: b"replacement".to_vec(),
            merge_base: expected.to_vec(),
            precondition: DiskPrecondition::Existing(*blake3::hash(expected).as_bytes()),
            buffer_id: 11,
            revision: 3,
        }
    }

    #[test]
    fn successful_save_snapshots_prior_bytes_before_replace_and_indexes_after() {
        let (fs, mut history, path) = setup();
        let mut indexed = false;
        let receipt = save(
            &*fs,
            &mut history,
            request(path.clone(), b"prior"),
            |p, b, _mtime| {
                assert_eq!(p, path);
                assert_eq!(b, b"replacement");
                indexed = true;
                Ok((2, 9))
            },
        )
        .unwrap();
        assert!(indexed);
        assert_eq!(fs.read_if_exists(&path).unwrap().unwrap(), b"replacement");
        assert_eq!(history.get(&history.records()[0].hash).unwrap(), b"prior");
        assert_eq!(
            (receipt.epoch, receipt.generation, receipt.saved_revision),
            (2, 9, 3)
        );
    }

    #[test]
    fn conflict_branch_snapshots_observed_external_bytes_and_does_not_replace() {
        let (fs, mut history, path) = setup();
        fs.remove(&path).unwrap();
        fs.write_new(&path, b"external").unwrap();
        let failure = save(
            &*fs,
            &mut history,
            request(path.clone(), b"prior"),
            |_, _, _| Ok((1, 1)),
        )
        .unwrap_err();
        assert_eq!(failure.outcome, DurabilityOutcome::NotReplaced);
        assert_eq!(fs.read_if_exists(&path).unwrap().unwrap(), b"external");
        assert_eq!(
            history.get(&history.records()[0].hash).unwrap(),
            b"external"
        );
    }

    #[test]
    fn index_failure_is_reported_after_replacement_without_retrying_it() {
        let (fs, mut history, path) = setup();
        let failure = save(
            &*fs,
            &mut history,
            request(path.clone(), b"prior"),
            |_, _, _| Err("index offline".into()),
        )
        .unwrap_err();
        assert_eq!(failure.outcome, DurabilityOutcome::ReplacedIndexPending);
        assert_eq!(fs.read_if_exists(&path).unwrap().unwrap(), b"replacement");
    }

    #[test]
    fn history_capacity_failure_aborts_before_overwrite() {
        let (fs, _, path) = setup();
        let mut history =
            HistoryStore::open_with_cap(fs.clone(), PathBuf::from("/data/tiny-history"), 1)
                .unwrap();
        let failure = save(
            &*fs,
            &mut history,
            request(path.clone(), b"prior"),
            |_, _, _| Ok((1, 1)),
        )
        .unwrap_err();
        assert_eq!(failure.outcome, DurabilityOutcome::NotReplaced);
        assert_eq!(fs.read_if_exists(&path).unwrap().unwrap(), b"prior");
    }

    #[test]
    fn unchecked_short_writes_cannot_install_truncated_note_or_history_bytes() {
        // Order under the retained-replacement primitive: three history
        // snapshot writes (1–3), then the staged-output write (4).
        for (write_occurrence, expected_stage) in [
            (1, SaveFailureStage::History),
            (4, SaveFailureStage::StageWrite),
        ] {
            let memory = MemFs::new();
            for directory in ["/vault", "/data", "/data/history"] {
                memory.create_dir_all(Path::new(directory)).unwrap();
            }
            memory
                .write_new(Path::new("/vault/a.md"), b"prior")
                .unwrap();
            memory.fsync_file(Path::new("/vault/a.md")).unwrap();
            memory.fsync_dir(Path::new("/vault")).unwrap();
            memory.fsync_dir(Path::new("/data")).unwrap();
            memory.fsync_dir(Path::new("/")).unwrap();

            let script = FaultScript::new().fault_at(
                "write_new",
                write_occurrence,
                FaultBoundary::Progress,
                FaultKind::ShortWrite,
            );
            let fs = Arc::new(FaultFs::new_mem(memory, script));
            let history_fs: Arc<dyn Fs + Send + Sync> = fs.clone();
            let mut history =
                HistoryStore::open(history_fs, PathBuf::from("/data/history")).unwrap();
            let path = PathBuf::from("/vault/a.md");
            let failure = save(
                &*fs,
                &mut history,
                request(path.clone(), b"prior"),
                |_, _, _| panic!("indexing cannot run after an unchecked short write"),
            )
            .unwrap_err();

            assert_eq!(failure.stage, expected_stage);
            assert_eq!(failure.outcome, DurabilityOutcome::NotReplaced);
            assert_eq!(fs.read_if_exists(&path).unwrap().unwrap(), b"prior");
        }
    }

    #[test]
    fn external_write_before_the_final_read_is_snapshotted_before_conflict() {
        let memory = MemFs::new();
        for directory in ["/vault", "/data", "/data/history"] {
            memory.create_dir_all(Path::new(directory)).unwrap();
        }
        let path = PathBuf::from("/vault/a.md");
        memory.write_new(&path, b"base").unwrap();
        memory.fsync_file(&path).unwrap();
        memory.fsync_dir(Path::new("/vault")).unwrap();
        memory.fsync_dir(Path::new("/data")).unwrap();
        memory.fsync_dir(Path::new("/")).unwrap();

        let script = FaultScript::new().fault_at(
            "read_if_exists",
            1,
            FaultBoundary::Before,
            FaultKind::ExternalWrite,
        );
        let external_path = path.clone();
        let fs = Arc::new(FaultFs::new_mem(memory, script).with_interleaver(
            move |inner, _context: &FaultContext| {
                inner.remove(&external_path).unwrap();
                inner
                    .write_new(&external_path, b"external bytes observed")
                    .unwrap();
            },
        ));
        let history_fs: Arc<dyn Fs + Send + Sync> = fs.clone();
        let mut history = HistoryStore::open(history_fs, PathBuf::from("/data/history")).unwrap();

        let failure = save(
            &*fs,
            &mut history,
            request(path.clone(), b"base"),
            |_, _, _| panic!("conflicted content must not reach indexing"),
        )
        .unwrap_err();

        assert_eq!(failure.stage, SaveFailureStage::Compare);
        assert_eq!(
            fs.read_if_exists(&path).unwrap().unwrap(),
            b"external bytes observed"
        );
        assert!(
            history
                .records()
                .iter()
                .any(|record| { history.get(&record.hash).unwrap() == b"external bytes observed" })
        );
    }

    #[test]
    fn ten_thousand_fault_schedules_per_crash_model_leave_old_or_new_complete_bytes() {
        // Deterministic xorshift seed; the seed is fixed so a failure is
        // reproducible. These schedules cover write, file-sync, rename, and
        // directory-sync boundaries, before and after each modeled operation.
        const SEED: u64 = 0x4c41_5454_4943_4531;
        let points = [
            ("write_new", 1),
            ("write_new", 2),
            ("write_new", 3),
            ("write_new", 4),
            ("fsync_file", 1),
            ("fsync_file", 2),
            ("fsync_file", 3),
            ("fsync_file", 4),
            ("rename_replace", 1),
            ("rename_replace", 2),
            ("rename_replace", 3),
            ("fsync_dir", 2),
            ("fsync_dir", 3),
            ("fsync_dir", 4),
            ("fsync_dir", 5),
        ];
        let mut random = SEED;

        for crash_mode in [
            flavite_fs::CrashMode::ProcessCrash,
            flavite_fs::CrashMode::PowerLoss,
        ] {
            let crash_kind = match crash_mode {
                flavite_fs::CrashMode::ProcessCrash => FaultKind::ProcessCrash,
                flavite_fs::CrashMode::PowerLoss => FaultKind::PowerLoss,
            };
            for case in 0..10_000 {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                let (method, occurrence) = points[(random as usize) % points.len()];
                let boundary = if random & 0x100 != 0 {
                    FaultBoundary::After
                } else {
                    FaultBoundary::Before
                };

                let memory = MemFs::new();
                for directory in ["/vault", "/data", "/data/history"] {
                    memory.create_dir_all(Path::new(directory)).unwrap();
                }
                memory
                    .write_new(Path::new("/vault/a.md"), b"old-complete")
                    .unwrap();
                memory.fsync_file(Path::new("/vault/a.md")).unwrap();
                memory.fsync_dir(Path::new("/vault")).unwrap();
                memory.fsync_dir(Path::new("/data")).unwrap();
                memory.fsync_dir(Path::new("/")).unwrap();

                let script = FaultScript::new().fault_at(method, occurrence, boundary, crash_kind);
                let fault_fs = Arc::new(FaultFs::new_mem(memory, script));
                let history_fs: Arc<dyn Fs + Send + Sync> = fault_fs.clone();
                let mut history =
                    HistoryStore::open(history_fs, PathBuf::from("/data/history")).unwrap();
                let path = PathBuf::from("/vault/a.md");
                let result = save(
                    &*fault_fs,
                    &mut history,
                    request(path.clone(), b"old-complete"),
                    |_, _, _| Ok((1, 1)),
                );
                assert!(
                    !fault_fs.is_alive(),
                    "seed={SEED:#x}, case={case}, point={method}[{occurrence}]/{boundary:?} was not reached"
                );

                let revived = fault_fs.revive();
                let installed = revived.read_if_exists(&path).unwrap().unwrap();
                assert!(
                    installed == b"old-complete" || installed == b"replacement",
                    "seed={SEED:#x}, case={case}, mode={crash_mode:?}, point={method}[{occurrence}]/{boundary:?}: partial bytes installed: {installed:?}"
                );
                if result.is_ok() {
                    assert_eq!(installed, b"replacement");
                }
            }
        }
    }
}
