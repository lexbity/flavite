//! Authenticated per-item operation log (RFC-0006 §6.4/§6.9).
//!
//! The operation coordinator state is authoritative in a versioned,
//! checksummed append-only per-item log plus content-addressed payloads
//! under `<vault>/.flavite/operations/replace/<job-id>/`. Every mutating
//! action has `(job, selection_revision, item, action_epoch)` identity;
//! repeated Apply/Cancel/Resume/Revert requests return the same receipt.
//!
//! Records are length-prefixed frames: `u32 LE length | JSON payload |
//! 32-byte keyed authenticator`. The authenticator chains the previous
//! record's tag, so a rolled-back or reordered log is detected. The
//! private high-water head (`<data>/flavite/vaults/<uuid>/operations/
//! <job>.head`) is updated by the same append: both vault-log and private
//! head durability boundaries must be acknowledged.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::plan::ENGINE_VERSION;
use super::store::{OpStoreIo, ReplaceStore, StoreError};
use crate::operation_key::OperationKey;

/// One log record frame length bound (NFR: one record ≤64 KiB).
pub const MAX_LOG_FRAME_BYTES: usize = 64 * 1024;
/// The zero previous-authenticator (genesis).
pub const GENESIS_AUTH: [u8; 32] = [0u8; 32];

/// Typed per-item action records (schema 1; `deny_unknown_fields`).
/// Revert records (RFC-0006 S09/D-11) are a distinct action epoch on the
/// same authenticated chain: `RevertIntentDurable` precedes the inverse
/// effect and `RevertOutcome` records its terminal state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum ItemAction {
    /// Durable intent recorded BEFORE any side effect.
    IntentDurable,
    /// A unique effect identifier (save/checkpoint) started.
    EffectStarted { op_id: String },
    /// Terminal per-item outcome.
    Outcome { outcome: PersistedItemOutcome },
    /// A durable job-level marker (Completed/Partial/Cancelled/Paused).
    JobTerminal { phase: String },
    /// Durable intent of a guarded Revert action, before its inverse effect.
    RevertIntentDurable,
    /// Terminal per-item Revert outcome.
    RevertOutcome { outcome: PersistedRevertOutcome },
}

/// Persisted terminal item outcomes (typed enum discriminants).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedItemOutcome {
    Saved,
    IndexPending,
    AppliedConflict,
    /// A draft buffer edit was installed and durably checkpointed (S09);
    /// the draft is NOT a file save.
    AppliedDraft,
    Stale,
    Denied,
    Busy,
    NeedsRecovery,
    SatisfiedUnattributed,
}

/// Persisted terminal Revert outcomes (RFC-0006 S09/D-11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedRevertOutcome {
    /// A saved note's before-payload was restored durably.
    Reverted,
    /// A draft's before text was restored with a durable checkpoint.
    RevertedDraft,
    /// A guard (later edit/rename/external write) blocked this note; it is
    /// skipped and offered compare/manual recovery.
    Stale,
    /// The inverse effect started but durability is unconfirmed.
    NeedsRecovery,
}

/// The canonical record payload (authenticator excluded; the tag is
/// computed over these exact bytes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordPayload {
    seq: u64,
    item_id: u64,
    action: ItemAction,
    prev_auth: String,
}

/// One decoded log record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    pub seq: u64,
    pub item_id: u64,
    pub action: ItemAction,
    pub prev_auth: [u8; 32],
    pub auth: [u8; 32],
}

/// Log errors.
#[derive(Debug, Clone)]
pub enum LogError {
    Io(String),
    /// The log is corrupt/unknown: never guessed, never truncated.
    Corrupt(String),
    /// The private head is missing/behind while records exist.
    HeadMismatch(String),
    /// Store-level error.
    Store(StoreError),
}

impl std::fmt::Display for LogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message) => write!(f, "operation log I/O failed: {message}"),
            Self::Corrupt(message) => write!(f, "operation log is corrupt: {message}"),
            Self::HeadMismatch(message) => write!(f, "operation log head mismatch: {message}"),
            Self::Store(error) => write!(f, "operation store error: {error}"),
        }
    }
}

impl std::error::Error for LogError {}

impl From<StoreError> for LogError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    hex::encode(bytes)
}

fn unhex(value: &str) -> Result<[u8; 32], LogError> {
    let bytes =
        hex::decode(value).map_err(|_| LogError::Corrupt("bad authenticator hex".into()))?;
    bytes
        .try_into()
        .map_err(|_| LogError::Corrupt("bad authenticator length".into()))
}

/// The authenticated append-only per-item log for one job.
pub struct JobLog {
    io: Box<dyn OpStoreIo>,
    records_dir: PathBuf,
    head_path: PathBuf,
    key: OperationKey,
    pub job_id: String,
    seq: u64,
    prev_auth: [u8; 32],
    last_auth: [u8; 32],
}

impl std::fmt::Debug for JobLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobLog")
            .field("job_id", &self.job_id)
            .field("seq", &self.seq)
            .field("last_auth", &hex(&self.last_auth))
            .finish()
    }
}

fn record_file(records_dir: &Path, seq: u64) -> PathBuf {
    records_dir.join(format!("r-{seq:016}.rec"))
}

impl JobLog {
    /// Open (or create) the log for a job, verifying the chain when
    /// records exist. `data_dir` is the private OS data directory for the
    /// high-water head.
    pub fn open(
        io: Box<dyn OpStoreIo>,
        store: &ReplaceStore,
        job_id: &str,
        key: OperationKey,
        data_dir: &Path,
        vault_uuid: &uuid::Uuid,
    ) -> Result<Self, LogError> {
        let records_dir = store.base().join(job_id).join("records");
        let head_dir = data_dir
            .join("flavite")
            .join("vaults")
            .join(vault_uuid.to_string())
            .join("operations");
        let head_path = head_dir.join(format!("{job_id}.head"));
        io.mkdir_all(&records_dir).map_err(LogError::Io)?;
        // The head directory lives in the OS private data dir (outside the
        // vault) and is created lazily by `write_head` via std::fs.
        let mut log = Self {
            io,
            records_dir,
            head_path,
            key,
            job_id: job_id.to_owned(),
            seq: 0,
            prev_auth: GENESIS_AUTH,
            last_auth: GENESIS_AUTH,
        };
        let head = log.read_head()?;
        let records = log.list_records()?;
        if records.is_empty() {
            if head.is_some() {
                return Err(LogError::HeadMismatch(
                    "a private head exists but the log has no records".into(),
                ));
            }
            return Ok(log);
        }
        let (last_seq, last_auth) = log.verify_chain(&records, head)?;
        log.restore_tail(last_seq, last_auth);
        Ok(log)
    }

    fn read_head(&self) -> Result<Option<(u64, [u8; 32])>, LogError> {
        let bytes = match crate::private_state::read_if_exists(&self.head_path) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok(None),
            Err(error) => return Err(LogError::Io(error)),
        };
        if bytes.len() != 8 + 32 {
            return Err(LogError::Corrupt("head file length".into()));
        }
        let seq = u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes"));
        let auth: [u8; 32] = bytes[8..].try_into().expect("32 bytes");
        Ok(Some((seq, auth)))
    }

    /// Write the private high-water head atomically (temp + rename): the
    /// head lives in the OS private data directory, OUTSIDE the vault, so
    /// the vault-contained operation store must not be used (and vault
    /// rollback must not be able to rewrite it).
    fn write_head(&self, seq: u64, auth: [u8; 32]) -> Result<(), LogError> {
        let mut bytes = Vec::with_capacity(40);
        bytes.extend_from_slice(&seq.to_le_bytes());
        bytes.extend_from_slice(&auth);
        crate::private_state::write_atomic(&self.head_path, &bytes).map_err(LogError::Io)
    }

    fn list_records(&self) -> Result<Vec<PathBuf>, LogError> {
        let Some(names) = self.io.list_dir(&self.records_dir).map_err(LogError::Io)? else {
            return Ok(Vec::new());
        };
        let mut files: Vec<PathBuf> = names
            .iter()
            .filter(|name| name.starts_with("r-") && name.ends_with(".rec"))
            .map(|name| self.records_dir.join(name))
            .collect();
        files.sort();
        Ok(files)
    }

    fn verify_chain(
        &self,
        records: &[PathBuf],
        head: Option<(u64, [u8; 32])>,
    ) -> Result<(u64, [u8; 32]), LogError> {
        let mut prev_auth = GENESIS_AUTH;
        let mut last_seq = 0u64;
        let mut last_auth = GENESIS_AUTH;
        for path in records {
            let record = self.read_record(path, prev_auth)?;
            if record.seq != last_seq.saturating_add(1) {
                return Err(LogError::Corrupt(format!(
                    "sequence gap at {} (expected {})",
                    record.seq,
                    last_seq.saturating_add(1)
                )));
            }
            last_seq = record.seq;
            last_auth = record.auth;
            prev_auth = record.auth;
        }
        if let Some((head_seq, head_auth)) = head {
            if head_seq > last_seq {
                return Err(LogError::HeadMismatch(format!(
                    "head {head_seq} is ahead of the log ({last_seq})"
                )));
            }
            if head_seq == last_seq && head_auth != last_auth {
                return Err(LogError::HeadMismatch(
                    "head authenticator does not match the last record".into(),
                ));
            }
        } else if last_seq > 0 {
            return Err(LogError::HeadMismatch(
                "records exist but no private high-water head".into(),
            ));
        }
        Ok((last_seq, last_auth))
    }

    fn restore_tail(&mut self, seq: u64, auth: [u8; 32]) {
        self.seq = seq;
        self.prev_auth = auth;
        self.last_auth = auth;
    }

    fn read_record(
        &self,
        path: &Path,
        expected_prev_auth: [u8; 32],
    ) -> Result<LogRecord, LogError> {
        let Some(bytes) = self.io.read(path).map_err(LogError::Io)? else {
            return Err(LogError::Corrupt(format!(
                "record file {} is missing",
                path.display()
            )));
        };
        if bytes.len() < 4 + 32 {
            return Err(LogError::Corrupt("record frame too short".into()));
        }
        let len = u32::from_le_bytes(bytes[..4].try_into().expect("4 bytes")) as usize;
        if len > MAX_LOG_FRAME_BYTES {
            return Err(LogError::Corrupt("record frame exceeds the bound".into()));
        }
        if bytes.len() != 4 + len + 32 {
            return Err(LogError::Corrupt("record frame length mismatch".into()));
        }
        let payload = &bytes[4..4 + len];
        let auth: [u8; 32] = bytes[4 + len..].try_into().expect("32 bytes");
        let expected = self.key.record_tag(&expected_prev_auth, payload);
        if auth != expected {
            return Err(LogError::Corrupt(format!(
                "record authenticator mismatch at {}",
                path.display()
            )));
        }
        let decoded: RecordPayload = serde_json::from_slice(payload)
            .map_err(|e| LogError::Corrupt(format!("record payload: {e}")))?;
        let prev_auth = unhex(&decoded.prev_auth)?;
        if prev_auth != expected_prev_auth {
            return Err(LogError::Corrupt(
                "record previous-authenticator mismatch".into(),
            ));
        }
        Ok(LogRecord {
            seq: decoded.seq,
            item_id: decoded.item_id,
            action: decoded.action,
            prev_auth,
            auth,
        })
    }

    /// Append one authenticated record. Both the vault-log frame and the
    /// private head are durable before success.
    pub fn append(&mut self, item_id: u64, action: ItemAction) -> Result<LogRecord, LogError> {
        let seq = self.seq.saturating_add(1);
        let payload = RecordPayload {
            seq,
            item_id,
            action: action.clone(),
            prev_auth: hex(&self.prev_auth),
        };
        let payload_bytes = serde_json::to_vec(&payload)
            .map_err(|e| LogError::Corrupt(format!("payload serialize: {e}")))?;
        if payload_bytes.len() > MAX_LOG_FRAME_BYTES {
            return Err(LogError::Corrupt("record payload exceeds the bound".into()));
        }
        let auth = self.key.record_tag(&self.prev_auth, &payload_bytes);
        let mut frame = Vec::with_capacity(4 + payload_bytes.len() + 32);
        frame.extend_from_slice(&(payload_bytes.len() as u32).to_le_bytes());
        frame.extend_from_slice(&payload_bytes);
        frame.extend_from_slice(&auth);
        let path = record_file(&self.records_dir, seq);
        self.io.write_new(&path, &frame).map_err(LogError::Io)?;
        self.io.fsync_file(&path).map_err(LogError::Io)?;
        self.io.fsync_dir(&self.records_dir).map_err(LogError::Io)?;
        // Private high-water head: vault-log AND head durability are both
        // required before the append is acknowledged.
        self.write_head(seq, auth)?;
        self.seq = seq;
        self.prev_auth = auth;
        self.last_auth = auth;
        Ok(LogRecord {
            seq,
            item_id,
            action,
            prev_auth: self.prev_auth,
            auth,
        })
    }

    /// Re-open the log after a crash (process restart): verify the chain
    /// against the persisted head and restore the in-memory tail.
    pub fn reload(&mut self) -> Result<(), LogError> {
        let records = self.list_records()?;
        if records.is_empty() {
            self.seq = 0;
            self.prev_auth = GENESIS_AUTH;
            self.last_auth = GENESIS_AUTH;
            return Ok(());
        }
        let head = self.read_head()?;
        let mut prev_auth = GENESIS_AUTH;
        let mut last_seq = 0u64;
        for path in &records {
            let record = self.read_record(path, prev_auth)?;
            last_seq = record.seq;
            prev_auth = record.auth;
        }
        if let Some((head_seq, head_auth)) = head {
            if head_seq > last_seq {
                return Err(LogError::HeadMismatch(format!(
                    "head {head_seq} is ahead of the log ({last_seq})"
                )));
            }
            if head_seq == last_seq && head_auth != prev_auth {
                return Err(LogError::HeadMismatch(
                    "head authenticator does not match the last record".into(),
                ));
            }
        } else {
            return Err(LogError::HeadMismatch(
                "records exist but no private high-water head".into(),
            ));
        }
        self.restore_tail(last_seq, prev_auth);
        Ok(())
    }

    /// Read every verified record in chain order (for recovery/classification).
    pub fn records(&self) -> Result<Vec<LogRecord>, LogError> {
        let files = self.list_records()?;
        let mut records = Vec::with_capacity(files.len());
        let mut prev_auth = GENESIS_AUTH;
        let mut last_seq = 0u64;
        for path in files {
            let record = self.read_record(&path, prev_auth)?;
            if record.seq != last_seq.saturating_add(1) {
                return Err(LogError::Corrupt(format!(
                    "sequence gap at {} (expected {})",
                    record.seq,
                    last_seq.saturating_add(1)
                )));
            }
            last_seq = record.seq;
            prev_auth = record.auth;
            records.push(record);
        }
        Ok(records)
    }

    /// Current sequence (next append will be seq+1).
    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// Current last authenticator.
    #[must_use]
    pub const fn last_auth(&self) -> [u8; 32] {
        self.last_auth
    }

    /// Version marker for the log schema/engine.
    #[must_use]
    pub const fn engine_version(&self) -> u32 {
        ENGINE_VERSION
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replace::plan::{
        PREVIEW_SCHEMA_VERSION, PreviewManifest, PreviewTotals, ScopeVersion,
    };
    use crate::replace::store::ReplaceStore;
    use flavite_fs::{Fs, MemFs};
    use std::path::{Path, PathBuf};

    #[derive(Clone)]
    struct MemIo(MemFs);

    impl OpStoreIo for MemIo {
        fn mkdir_all(&self, path: &Path) -> Result<(), String> {
            self.0.create_dir_all(path).map_err(|e| e.to_string())
        }
        fn write_new(&self, path: &Path, bytes: &[u8]) -> Result<(), String> {
            self.0.write_new(path, bytes).map_err(|e| e.to_string())
        }
        fn write_replace(&self, path: &Path, bytes: &[u8]) -> Result<(), String> {
            let parent = path.parent().ok_or_else(|| "no parent".to_owned())?;
            self.0.create_dir_all(parent).map_err(|e| e.to_string())?;
            let temp = parent.join(format!(".ophead-{}.tmp", uuid::Uuid::now_v7()));
            self.0.write_new(&temp, bytes).map_err(|e| e.to_string())?;
            self.0.fsync_file(&temp).map_err(|e| e.to_string())?;
            self.0
                .rename_replace(&temp, path)
                .map_err(|e| e.to_string())?;
            self.0.fsync_dir(parent).map_err(|e| e.to_string())
        }
        fn read(&self, path: &Path) -> Result<Option<Vec<u8>>, String> {
            self.0.read_if_exists(path).map_err(|e| e.to_string())
        }
        fn list_dir(&self, path: &Path) -> Result<Option<Vec<String>>, String> {
            match self.0.read_dir(path) {
                Ok(entries) => {
                    let mut names = entries
                        .into_iter()
                        .filter(|entry| !entry.is_dir)
                        .map(|entry| entry.name.into_string().map_err(|_| "not UTF-8".to_owned()))
                        .collect::<Result<Vec<_>, String>>()?;
                    names.sort();
                    Ok(Some(names))
                }
                Err(flavite_fs::FsError::DirNotFound(_)) => Ok(None),
                Err(error) => Err(error.to_string()),
            }
        }
        fn fsync_file(&self, path: &Path) -> Result<(), String> {
            self.0.fsync_file(path).map_err(|e| e.to_string())
        }
        fn fsync_dir(&self, path: &Path) -> Result<(), String> {
            self.0.fsync_dir(path).map_err(|e| e.to_string())
        }
        fn remove(&self, path: &Path) -> Result<(), String> {
            self.0.remove(path).map_err(|e| e.to_string())
        }
    }

    fn tmp_data_dir() -> PathBuf {
        std::env::temp_dir().join(format!("flavite-logtest-{}", uuid::Uuid::now_v7()))
    }

    fn empty_manifest(job: &str) -> PreviewManifest {
        PreviewManifest {
            schema: PREVIEW_SCHEMA_VERSION,
            engine_version: ENGINE_VERSION,
            job_id: job.to_owned(),
            vault_uuid: "uu".to_owned(),
            plan_digest: "p".to_owned(),
            scope_version: ScopeVersion {
                epoch: 1,
                generation: 1,
            },
            source_policy: "disk_with_explicit_drafts".to_owned(),
            state: "ready".to_owned(),
            totals: PreviewTotals::default(),
            payload_bytes: 0,
            created_at_unix_ms: 1,
            expires_at_unix_ms: 2,
            selection_revision: None,
            selection_digest: None,
            include_drafts: false,
            notes: vec![],
            exclusions: vec![],
        }
    }

    #[test]
    fn append_chain_and_reload_verify() {
        let fs = MemFs::new();
        let io = MemIo(fs.clone());
        let store = ReplaceStore::new(Path::new("/vault"));
        let job = "a".repeat(32);
        let job = job.as_str();
        store.seal_preview(&io, &empty_manifest(job)).expect("seal");
        let data_dir = tmp_data_dir();
        let vault = uuid::Uuid::now_v7();
        let key = crate::operation_key::load_or_create_key(&data_dir, &vault).expect("key");
        let mut log =
            JobLog::open(Box::new(io.clone()), &store, job, key, &data_dir, &vault).expect("open");
        let first = log.append(7, ItemAction::IntentDurable).expect("intent");
        assert_eq!(first.seq, 1);
        let second = log
            .append(
                7,
                ItemAction::Outcome {
                    outcome: PersistedItemOutcome::Saved,
                },
            )
            .expect("outcome");
        assert_eq!(second.seq, 2);
        drop(log);

        let key2 = crate::operation_key::load_or_create_key(&data_dir, &vault).expect("key2");
        let mut reopened =
            JobLog::open(Box::new(io), &store, job, key2, &data_dir, &vault).expect("reopen");
        assert_eq!(reopened.seq(), 2);
        assert_eq!(reopened.last_auth(), second.auth);
        let third = reopened
            .append(8, ItemAction::IntentDurable)
            .expect("append after reopen");
        assert_eq!(third.seq, 3);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn tampering_is_detected_and_never_guessed() {
        let fs = MemFs::new();
        let io = MemIo(fs.clone());
        let store = ReplaceStore::new(Path::new("/vault"));
        let job = "b".repeat(32);
        let job = job.as_str();
        store.seal_preview(&io, &empty_manifest(job)).expect("seal");
        let data_dir = tmp_data_dir();
        let vault = uuid::Uuid::now_v7();
        let key = crate::operation_key::load_or_create_key(&data_dir, &vault).expect("key");
        let mut log =
            JobLog::open(Box::new(io.clone()), &store, job, key, &data_dir, &vault).expect("open");
        log.append(1, ItemAction::IntentDurable).expect("intent");
        drop(log);

        // Corrupt the first record frame (flip a payload byte).
        let dir = Path::new("/vault/.flavite/operations/replace")
            .join(job)
            .join("records");
        let file = dir.join("r-0000000000000001.rec");
        let mut bytes = fs.read_if_exists(&file).unwrap().unwrap();
        bytes[10] ^= 0xFF;
        fs.remove(&file).unwrap();
        fs.write_new(&file, &bytes).unwrap();
        fs.fsync_file(&file).unwrap();

        let key2 = crate::operation_key::load_or_create_key(&data_dir, &vault).expect("key2");
        let err =
            JobLog::open(Box::new(io), &store, job, key2, &data_dir, &vault).expect_err("corrupt");
        assert!(matches!(err, LogError::Corrupt(_)));
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn missing_head_with_records_is_a_typed_quarantine() {
        let fs = MemFs::new();
        let io = MemIo(fs.clone());
        let store = ReplaceStore::new(Path::new("/vault"));
        let job = "c".repeat(32);
        let job = job.as_str();
        store.seal_preview(&io, &empty_manifest(job)).expect("seal");
        let data_dir = tmp_data_dir();
        let vault = uuid::Uuid::now_v7();
        let key = crate::operation_key::load_or_create_key(&data_dir, &vault).expect("key");
        let mut log = JobLog::open(
            Box::new(io.clone()),
            &store,
            job,
            key.clone(),
            &data_dir,
            &vault,
        )
        .expect("open");
        log.append(1, ItemAction::IntentDurable).expect("intent");
        drop(log);
        // Delete the private head (std::fs: it lives in the real OS data
        // dir, outside the MemFs vault).
        let head = data_dir
            .join("flavite")
            .join("vaults")
            .join(vault.to_string())
            .join("operations")
            .join(format!("{job}.head"));
        std::fs::remove_file(&head).expect("remove head");
        let err = JobLog::open(Box::new(io), &store, job, key, &data_dir, &vault)
            .expect_err("head missing");
        assert!(matches!(err, LogError::HeadMismatch(_)));
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
