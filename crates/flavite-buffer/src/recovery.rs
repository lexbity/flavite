//! Recovery file writer (§4.5.3, D6, R16).
//!
//! Dirty buffers are written to a recovery file every 2 s (default;
//! **configurable** `recovery_interval`), atomic, with their buffer identity
//! and revision.
//!
//! After a crash, restore is offered via a Channel-B notice (D6).
//! A successful save retires only recovery state covered by its saved revision;
//! recovery for newer edits MUST remain reachable (§4.5.3).
//!
//! Recovery files are written to:
//! `<os-data>/flavite/vaults/<vault-uuid>/recovery/<note-path-hash>.bin`
//!
//! The path is supplied by the vault using native PathKey.

use flavite_syntax::text::TextByte;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::eol::EolTable;
use crate::model::EditorModel;
use crate::revision::{BaseHash, BufferId, Revision};
use crate::state::Buffer;

/// Magic bytes at the start of a recovery file.
const RECOVERY_MAGIC: &[u8; 4] = b"FLVT";

/// Current version of the recovery file format.
const RECOVERY_VERSION: u8 = 1;

/// Header at the start of every recovery file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecoveryHeader {
    /// Magic bytes for identification.
    pub magic: [u8; 4],
    /// Format version.
    pub version: u8,
    /// Buffer identity.
    pub buffer_id: BufferId,
    /// Buffer revision at the time of writing.
    pub revision: Revision,
    /// Timestamp of this recovery write.
    pub timestamp: u64,
    /// Checkpoint sequence number (for ordering recovery files).
    pub checkpoint_seq: u64,
    /// Whether this is a durable checkpoint (vs. periodic recovery).
    pub is_checkpoint: bool,
}

impl RecoveryHeader {
    /// Create a new recovery header.
    #[must_use]
    pub fn new(
        buffer_id: BufferId,
        revision: Revision,
        checkpoint_seq: u64,
        is_checkpoint: bool,
    ) -> Self {
        Self {
            magic: *RECOVERY_MAGIC,
            version: RECOVERY_VERSION,
            buffer_id,
            revision,
            timestamp: Self::current_timestamp(),
            checkpoint_seq,
            is_checkpoint,
        }
    }

    /// Get the current timestamp as u64 (seconds since epoch).
    #[must_use]
    pub fn current_timestamp() -> u64 {
        use std::time::SystemTime;
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Serialize the header to bytes.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        // Manual serialization for simplicity (no bincode dependency)
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.magic);
        bytes.push(self.version);
        bytes.extend_from_slice(&self.buffer_id.as_u64().to_le_bytes());
        bytes.extend_from_slice(&self.revision.as_u64().to_le_bytes());
        bytes.extend_from_slice(&self.timestamp.to_le_bytes());
        bytes.extend_from_slice(&self.checkpoint_seq.to_le_bytes());
        bytes.push(self.is_checkpoint as u8);
        bytes
    }

    /// Deserialize a header from bytes.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        // Manual deserialization
        if bytes.len() < 38 {
            return None;
        }
        let magic: [u8; 4] = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if &magic != RECOVERY_MAGIC {
            return None;
        }

        let version = bytes[4];
        if version != RECOVERY_VERSION {
            return None;
        }
        let buffer_id = u64::from_le_bytes([
            bytes[5], bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11], bytes[12],
        ]);
        let revision = u64::from_le_bytes([
            bytes[13], bytes[14], bytes[15], bytes[16], bytes[17], bytes[18], bytes[19], bytes[20],
        ]);
        let timestamp = u64::from_le_bytes([
            bytes[21], bytes[22], bytes[23], bytes[24], bytes[25], bytes[26], bytes[27], bytes[28],
        ]);
        let checkpoint_seq = u64::from_le_bytes([
            bytes[29], bytes[30], bytes[31], bytes[32], bytes[33], bytes[34], bytes[35], bytes[36],
        ]);
        let is_checkpoint = match bytes.get(37).copied()? {
            0 => false,
            1 => true,
            _ => return None,
        };

        Some(Self {
            magic,
            version,
            buffer_id: BufferId::from(buffer_id),
            revision: Revision::new(revision),
            timestamp,
            checkpoint_seq,
            is_checkpoint,
        })
    }
}

/// Recovery file metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecoveryFile {
    /// The header.
    pub header: RecoveryHeader,
    /// The EOL table.
    pub eol_table: EolTable,
    /// The normalized text (CRLF folded).
    pub text: String,
    /// The original source bytes length (for validation).
    pub source_len: usize,
    /// The content hash of the source bytes.
    pub content_hash: BaseHash,
    /// The base hash at the time of the last save.
    pub base_hash: Option<BaseHash>,
}

impl RecoveryFile {
    /// Create a new recovery file from a buffer.
    #[must_use]
    pub fn from_buffer(buffer: &Buffer, checkpoint_seq: u64, is_checkpoint: bool) -> Self {
        let text = buffer.text().to_string();
        let eol_table = buffer.model().eol_table().clone();
        let content_hash = buffer.content_hash();
        let base_hash = buffer.base_hash();

        Self {
            header: RecoveryHeader::new(
                buffer.buffer_id(),
                buffer.revision(),
                checkpoint_seq,
                is_checkpoint,
            ),
            eol_table,
            text,
            source_len: buffer.len(),
            content_hash,
            base_hash,
        }
    }

    /// Create a new recovery file from an EditorModel.
    #[must_use]
    pub fn from_model(
        model: &EditorModel,
        buffer_id: BufferId,
        revision: Revision,
        checkpoint_seq: u64,
        is_checkpoint: bool,
        base_hash: Option<BaseHash>,
    ) -> Self {
        let text = model.text().to_string();
        let eol_table = model.eol_table().clone();
        let content_hash = model.content_hash();

        Self {
            header: RecoveryHeader::new(buffer_id, revision, checkpoint_seq, is_checkpoint),
            eol_table,
            text,
            source_len: model.len(),
            content_hash,
            base_hash,
        }
    }

    /// Serialize the recovery file to bytes.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();

        // Write header
        bytes.extend_from_slice(&self.header.to_bytes());

        // Write EOL table as JSON
        let eol_json = serde_json::to_vec(&self.eol_table).unwrap_or_default();
        bytes.extend_from_slice(&(eol_json.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&eol_json);

        // Write text
        bytes.extend_from_slice(&(self.text.len() as u32).to_le_bytes());
        bytes.extend_from_slice(self.text.as_bytes());

        // Write source_len
        bytes.extend_from_slice(&(self.source_len as u64).to_le_bytes());

        // Write content_hash
        bytes.extend_from_slice(self.content_hash.as_slice());

        // Write base_hash (optional)
        bytes.push(self.base_hash.is_some() as u8);
        if let Some(hash) = &self.base_hash {
            bytes.extend_from_slice(hash.as_slice());
        }

        bytes
    }

    /// Deserialize a recovery file from bytes.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        // Read header (38 bytes with magic + version + fields)
        if bytes.len() < 38 {
            return None;
        }
        let header = RecoveryHeader::from_bytes(&bytes[..38])?;

        let mut offset = 38;

        // Read EOL table length
        if offset + 4 > bytes.len() {
            return None;
        }
        let eol_len = u32::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]) as usize;
        offset += 4;

        // Read EOL table JSON
        if offset + eol_len > bytes.len() {
            return None;
        }
        let eol_json = &bytes[offset..offset + eol_len];
        let eol_table: EolTable = serde_json::from_slice(eol_json).ok()?;
        offset += eol_len;

        // Read text length
        if offset + 4 > bytes.len() {
            return None;
        }
        let text_len = u32::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]) as usize;
        offset += 4;

        // Read text
        if offset + text_len > bytes.len() {
            return None;
        }
        let text = std::str::from_utf8(&bytes[offset..offset + text_len])
            .ok()?
            .to_owned();
        offset += text_len;

        // Read source_len
        if offset + 8 > bytes.len() {
            return None;
        }
        let source_len = u64::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ]) as usize;
        if source_len > 64 * 1024 * 1024 {
            return None;
        }
        offset += 8;

        // Read content_hash
        if offset + 32 > bytes.len() {
            return None;
        }
        let content_hash = BaseHash::from_slice(&bytes[offset..offset + 32]);
        offset += 32;

        // Read base_hash
        let has_base_hash = match bytes.get(offset).copied()? {
            0 => false,
            1 => true,
            _ => return None,
        };
        offset += 1;
        let base_hash = if has_base_hash {
            if offset + 32 <= bytes.len() {
                let hash = BaseHash::from_slice(&bytes[offset..offset + 32]);
                offset += 32;
                Some(hash)
            } else {
                None
            }
        } else {
            None
        };

        if offset != bytes.len() {
            return None;
        }
        let recovery = Self {
            header,
            eol_table,
            text,
            source_len,
            content_hash,
            base_hash,
        };
        // `str::split('\n')` counts an extra empty field after a trailing LF,
        // while TextSnapshot stores that LF on the preceding line. Empty text
        // still has one logical line in the EOL table.
        let expected_lines = recovery.text.lines().count().max(1);
        if recovery.eol_table.line_count() != expected_lines {
            return None;
        }
        let source = recovery.to_source_bytes();
        if source.len() != recovery.source_len
            || BaseHash::from_bytes(*blake3::hash(&source).as_bytes()) != recovery.content_hash
        {
            return None;
        }
        Some(recovery)
    }

    /// Reconstruct the source bytes from the recovery file.
    #[must_use]
    pub fn to_source_bytes(&self) -> Vec<u8> {
        // Use the EOL table to serialize with correct line endings
        // line_breaks should be the end positions of each line's text (before the newline)
        let line_breaks: Vec<TextByte> = self
            .text
            .char_indices()
            .filter(|(_, c)| *c == '\n')
            .map(|(idx, _)| TextByte(idx))
            .collect();

        self.eol_table.serialize(&self.text, &line_breaks)
    }

    /// Check if the recovery file's content hash matches the expected hash.
    #[must_use]
    pub fn matches_hash(&self, hash: BaseHash) -> bool {
        self.content_hash == hash
    }
}

/// Recovery writer configuration.
#[derive(Debug, Clone)]
pub struct RecoveryConfig {
    /// Interval between recovery writes (in seconds).
    pub interval_secs: u64,
    /// Directory to write recovery files to.
    pub recovery_dir: PathBuf,
    /// Vault UUID.
    pub vault_uuid: String,
}

impl RecoveryConfig {
    /// Create a new recovery configuration.
    #[must_use]
    pub fn new(vault_uuid: String, recovery_dir: PathBuf, interval_secs: u64) -> Self {
        Self {
            vault_uuid,
            recovery_dir,
            interval_secs,
        }
    }
}

/// The recovery file writer.
///
/// Writes recovery files for dirty buffers at a configurable interval.
/// Recovery files are atomic writes to prevent corruption.
#[derive(Debug)]
pub struct RecoveryWriter {
    /// The configuration.
    config: RecoveryConfig,
    /// The last write time for each buffer.
    last_write: HashMap<BufferId, Instant>,
    /// The checkpoint sequence for each buffer.
    checkpoint_seq: HashMap<BufferId, u64>,
    /// Highest revision durably covered by a successful save. A stale recovery
    /// worker snapshot arriving after save retirement must not recreate it.
    covered_revision: HashMap<BufferId, Revision>,
}

impl RecoveryWriter {
    /// Create a new recovery writer.
    #[must_use]
    pub fn new(config: RecoveryConfig) -> Self {
        Self {
            config,
            last_write: HashMap::new(),
            checkpoint_seq: HashMap::new(),
            covered_revision: HashMap::new(),
        }
    }

    /// Get the recovery directory.
    #[must_use]
    pub fn recovery_dir(&self) -> &Path {
        &self.config.recovery_dir
    }

    /// Get the recovery interval.
    #[must_use]
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.config.interval_secs)
    }

    /// Set the recovery interval.
    pub fn set_interval(&mut self, secs: u64) {
        self.config.interval_secs = secs;
    }

    /// Generate the recovery file path from the note's reversible native key.
    #[must_use]
    pub fn recovery_path_for_note_key(&self, note_key: &[u8]) -> PathBuf {
        let hash = blake3::hash(note_key).to_hex();
        self.config.recovery_dir.join(format!("{hash}.bin"))
    }

    /// Check if a buffer should be written to recovery.
    ///
    /// Returns true if the buffer is dirty and the interval has elapsed.
    #[must_use]
    pub fn should_write(&self, buffer: &Buffer) -> bool {
        if !buffer.is_dirty() {
            return false;
        }

        let buffer_id = buffer.buffer_id();
        if let Some(last) = self.last_write.get(&buffer_id) {
            last.elapsed() >= self.interval()
        } else {
            // Never written before
            true
        }
    }

    /// Whether an immutable snapshot is due for a recovery write.
    #[must_use]
    pub fn should_write_snapshot(&self, recovery: &RecoveryFile) -> bool {
        self.last_write
            .get(&recovery.header.buffer_id)
            .is_none_or(|last| last.elapsed() >= self.interval())
    }

    /// Write recovery files for all dirty buffers.
    ///
    /// Returns the paths that were written.
    pub fn write_all(&mut self, buffers: &[(Vec<u8>, &Buffer)]) -> Vec<PathBuf> {
        let mut written = Vec::new();

        for (note_key, buffer) in buffers {
            if self.should_write(buffer)
                && let Some(path) = self.write_buffer(note_key, buffer)
            {
                written.push(path);
            }
        }

        written
    }

    /// Write a recovery file for a specific buffer.
    ///
    /// The write is atomic: write to a temp file, fsync, rename.
    /// Returns the path if successful.
    pub fn write_buffer(&mut self, note_key: &[u8], buffer: &Buffer) -> Option<PathBuf> {
        let path = self.recovery_path_for_note_key(note_key);
        let buffer_id = buffer.buffer_id();

        // Get or create checkpoint sequence
        let seq = *self.checkpoint_seq.entry(buffer_id).or_insert(0);
        let next_seq = seq + 1;
        self.checkpoint_seq.insert(buffer_id, next_seq);

        // Create recovery file
        let recovery_file = RecoveryFile::from_buffer(buffer, seq, false);
        let bytes = recovery_file.to_bytes();

        // Ensure directory exists
        if let Some(parent) = path.parent()
            && let Err(_) = fs::create_dir_all(parent)
        {
            return None;
        }

        // Write atomically
        let temp_path = path.with_extension(format!(
            "{}.{}.tmp",
            buffer_id.as_u64(),
            buffer.revision().as_u64()
        ));

        let mut file = match File::create(&temp_path) {
            Ok(f) => f,
            Err(_) => {
                return None;
            }
        };

        if file.write_all(&bytes).is_err() {
            fs::remove_file(&temp_path).ok();
            return None;
        }

        if file.sync_all().is_err() {
            fs::remove_file(&temp_path).ok();
            return None;
        }

        drop(file);

        // Rename atomically
        if fs::rename(&temp_path, &path).is_err() {
            fs::remove_file(&temp_path).ok();
            return None;
        }

        #[cfg(unix)]
        if let Some(parent) = path.parent()
            && let Err(_) = File::open(parent).and_then(|directory| directory.sync_all())
        {
            return None;
        }

        // Update last write time
        self.last_write.insert(buffer_id, Instant::now());

        Some(path)
    }

    /// Write an immutable recovery snapshot after the caller has released the
    /// BufferManager lock and durably pinned its merge base.
    pub fn write_snapshot(
        &mut self,
        note_key: &[u8],
        recovery_file: RecoveryFile,
    ) -> Option<PathBuf> {
        if !self.should_write_snapshot(&recovery_file) {
            return None;
        }
        self.persist_snapshot(note_key, recovery_file)
    }

    /// Durably write a recovery snapshot regardless of the periodic interval.
    /// Session shutdown uses this for dirty buffers that have not reached their
    /// next timer deadline.
    pub fn write_snapshot_now(
        &mut self,
        note_key: &[u8],
        recovery_file: RecoveryFile,
    ) -> Option<PathBuf> {
        self.persist_snapshot(note_key, recovery_file)
    }

    fn persist_snapshot(
        &mut self,
        note_key: &[u8],
        mut recovery_file: RecoveryFile,
    ) -> Option<PathBuf> {
        let buffer_id = recovery_file.header.buffer_id;
        if self
            .covered_revision
            .get(&buffer_id)
            .is_some_and(|covered| recovery_file.header.revision <= *covered)
        {
            return None;
        }
        let seq = *self.checkpoint_seq.entry(buffer_id).or_insert(0);
        self.checkpoint_seq.insert(buffer_id, seq.saturating_add(1));
        recovery_file.header.checkpoint_seq = seq;
        recovery_file.header.timestamp = RecoveryHeader::current_timestamp();
        recovery_file.header.is_checkpoint = false;
        let path = self.recovery_path_for_note_key(note_key);
        if self.read_recovery_file(&path).is_some_and(|existing| {
            existing.header.buffer_id == buffer_id
                && existing.header.revision > recovery_file.header.revision
        }) {
            return None;
        }
        if write_recovery_atomically(&path, &recovery_file.to_bytes()).is_err() {
            return None;
        }
        self.last_write.insert(buffer_id, Instant::now());
        Some(path)
    }

    /// Write a checkpoint for a buffer.
    ///
    /// Checkpoints are durable recovery points that are written before
    /// operations like save or undo.
    pub fn write_checkpoint(&mut self, note_key: &[u8], buffer: &Buffer) -> Option<PathBuf> {
        let buffer_id = buffer.buffer_id();

        // Get or create checkpoint sequence
        let seq = *self.checkpoint_seq.entry(buffer_id).or_insert(0);
        let next_seq = seq + 1;
        self.checkpoint_seq.insert(buffer_id, next_seq);

        // Create recovery file with checkpoint flag
        let recovery_file = RecoveryFile::from_buffer(buffer, seq, true);
        let path = self.recovery_path_for_note_key(note_key);
        let bytes = recovery_file.to_bytes();

        // Ensure directory exists
        if let Some(parent) = path.parent()
            && let Err(_) = fs::create_dir_all(parent)
        {
            return None;
        }

        // Write atomically (same as write_buffer)
        let temp_path = path.with_extension("tmp");

        let mut file = match File::create(&temp_path) {
            Ok(f) => f,
            Err(_) => {
                return None;
            }
        };

        if file.write_all(&bytes).is_err() {
            fs::remove_file(&temp_path).ok();
            return None;
        }

        if file.sync_all().is_err() {
            fs::remove_file(&temp_path).ok();
            return None;
        }

        drop(file);

        if fs::rename(&temp_path, &path).is_err() {
            fs::remove_file(&temp_path).ok();
            return None;
        }

        // Update last write time
        self.last_write.insert(buffer_id, Instant::now());

        Some(path)
    }

    /// Read a recovery file.
    ///
    /// Returns the recovery file if successful and valid.
    pub fn read_recovery_file(&self, path: &Path) -> Option<RecoveryFile> {
        const MAX_RECOVERY_FILE_BYTES: u64 = 128 * 1024 * 1024;
        if fs::metadata(path).ok()?.len() > MAX_RECOVERY_FILE_BYTES {
            return None;
        }
        let bytes = fs::read(path).ok()?;
        RecoveryFile::from_bytes(&bytes)
    }

    /// Read the latest recovery checkpoint associated with a native note key.
    #[must_use]
    pub fn read_for_note_key(&self, note_key: &[u8]) -> Option<RecoveryFile> {
        self.read_recovery_file(&self.recovery_path_for_note_key(note_key))
    }

    /// List all recovery files for this vault.
    #[must_use]
    pub fn list_recovery_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();

        if let Ok(entries) = fs::read_dir(&self.config.recovery_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "bin") {
                    files.push(path);
                }
            }
        }

        files
    }

    /// Delete a recovery file.
    pub fn delete_recovery_file(&self, path: &Path) -> bool {
        fs::remove_file(path).is_ok()
    }

    /// Delete all recovery files for a specific buffer.
    pub fn delete_for_buffer(&self, buffer_id: BufferId) -> usize {
        self.remove_matching(|file| file.header.buffer_id == buffer_id)
    }

    /// Retire only recovery records for this buffer whose revision is covered
    /// by an acknowledged save. Newer revisions remain reachable.
    pub fn retire_covered(&mut self, buffer_id: BufferId, saved_revision: Revision) -> usize {
        self.covered_revision
            .entry(buffer_id)
            .and_modify(|covered| *covered = (*covered).max(saved_revision))
            .or_insert(saved_revision);
        self.remove_matching(|file| {
            file.header.buffer_id == buffer_id && file.header.revision <= saved_revision
        })
    }

    /// Delete recovery for a note only after the caller has completed its
    /// explicit discard decision.
    pub fn delete_for_note_key(&self, note_key: &[u8]) -> bool {
        self.delete_recovery_file(&self.recovery_path_for_note_key(note_key))
    }

    fn remove_matching(&self, predicate: impl Fn(&RecoveryFile) -> bool) -> usize {
        let mut removed = 0;
        for path in self.list_recovery_files() {
            if self
                .read_recovery_file(&path)
                .is_some_and(|file| predicate(&file))
                && self.delete_recovery_file(&path)
            {
                removed += 1;
            }
        }
        removed
    }

    /// Delete all recovery files.
    pub fn delete_all(&self) -> usize {
        let files = self.list_recovery_files();
        let mut deleted = 0usize;

        for file in &files {
            if self.delete_recovery_file(file) {
                deleted += 1;
            }
        }

        deleted
    }
}

impl Default for RecoveryWriter {
    fn default() -> Self {
        Self::new(RecoveryConfig::new(
            "default".to_string(),
            PathBuf::new(),
            2,
        ))
    }
}

fn write_recovery_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("recovery path has no parent"))?;
    fs::create_dir_all(parent)?;
    static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = parent.join(format!(
        ".recovery-{}-{timestamp}-{nonce}.tmp",
        std::process::id()
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

/// Builder for creating RecoveryWriter with custom settings.
#[derive(Debug)]
pub struct RecoveryWriterBuilder {
    vault_uuid: String,
    recovery_dir: PathBuf,
    interval_secs: u64,
}

impl RecoveryWriterBuilder {
    /// Create a new builder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            vault_uuid: "default".to_string(),
            recovery_dir: PathBuf::new(),
            interval_secs: 2,
        }
    }

    /// Set the vault UUID.
    #[must_use]
    pub fn vault_uuid(mut self, uuid: String) -> Self {
        self.vault_uuid = uuid;
        self
    }

    /// Set the recovery directory.
    #[must_use]
    pub fn recovery_dir(mut self, dir: PathBuf) -> Self {
        self.recovery_dir = dir;
        self
    }

    /// Set the recovery interval in seconds.
    #[must_use]
    pub fn interval_secs(mut self, secs: u64) -> Self {
        self.interval_secs = secs;
        self
    }

    /// Build the RecoveryWriter.
    #[must_use]
    pub fn build(self) -> RecoveryWriter {
        let config = RecoveryConfig::new(self.vault_uuid, self.recovery_dir, self.interval_secs);
        RecoveryWriter::new(config)
    }
}

impl Default for RecoveryWriterBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn recovery_header_round_trip() {
        let header = RecoveryHeader::new(BufferId::new(), Revision::new(5), 10, false);

        let bytes = header.to_bytes();
        let decoded = RecoveryHeader::from_bytes(&bytes);

        assert!(decoded.is_some());
        let decoded = decoded.unwrap();
        assert_eq!(decoded.version, header.version);
        assert_eq!(decoded.revision, header.revision);
        assert_eq!(decoded.checkpoint_seq, header.checkpoint_seq);
    }

    #[test]
    fn recovery_file_round_trip() {
        let buffer = Buffer::from_text("hello\nworld");
        let recovery_file = RecoveryFile::from_buffer(&buffer, 1, false);

        let bytes = recovery_file.to_bytes();
        let decoded = RecoveryFile::from_bytes(&bytes);

        assert!(decoded.is_some());
        let decoded = decoded.unwrap();
        assert_eq!(decoded.text, "hello\nworld");
    }

    #[test]
    fn receipt_for_revision_n_cannot_retire_recovery_for_revision_n_plus_one() {
        let dir = tempdir().expect("recovery dir");
        let mut writer = RecoveryWriterBuilder::new()
            .vault_uuid("receipt-test".into())
            .recovery_dir(dir.path().to_path_buf())
            .interval_secs(0)
            .build();
        let buffer_id = BufferId::new();
        let note_key = b"notes/receipt.md";

        let mut revision = |text: &str, rev: u64| {
            let mut file = RecoveryFile::from_buffer(&Buffer::from_text(text), 0, false);
            file.header.buffer_id = buffer_id;
            file.header.revision = Revision::new(rev);
            writer
                .write_snapshot_now(note_key, file)
                .expect("snapshot persisted");
        };
        revision("stale content", 4);
        revision("current content", 5);

        // One durable file per note holds the newest snapshot: a save receipt
        // for revision 4 retires nothing, and the revision-5 recovery file
        // survives for the next decision.
        let retired = writer.retire_covered(buffer_id, Revision::new(4));
        assert_eq!(retired, 0);
        let remaining = writer.read_for_note_key(note_key).expect("revision 5 kept");
        assert_eq!(remaining.text, "current content");

        // Retiring revision 5 does remove the file...
        let retired = writer.retire_covered(buffer_id, Revision::new(5));
        assert_eq!(retired, 1);
        assert!(writer.read_for_note_key(note_key).is_none());

        // ...and the retired revisions cannot be re-persisted under the
        // receipt: a delayed writer for revision n is refused outright.
        let mut replay = RecoveryFile::from_buffer(&Buffer::from_text("stale content"), 0, false);
        replay.header.buffer_id = buffer_id;
        replay.header.revision = Revision::new(4);
        assert!(writer.write_snapshot_now(note_key, replay).is_none());
        let mut replay = RecoveryFile::from_buffer(&Buffer::from_text("stale content"), 0, false);
        replay.header.buffer_id = buffer_id;
        replay.header.revision = Revision::new(4);
        assert!(writer.write_snapshot_now(note_key, replay).is_none());
    }

    #[test]
    fn recovery_file_to_source_bytes() {
        let buffer = Buffer::from_text("hello\nworld");
        let recovery_file = RecoveryFile::from_buffer(&buffer, 1, false);

        let source_bytes = recovery_file.to_source_bytes();
        assert_eq!(source_bytes, b"hello\nworld");
    }

    #[test]
    fn recovery_writer_write_read() {
        let dir = tempdir().unwrap();
        let config = RecoveryConfig::new("test-vault".to_string(), dir.path().to_path_buf(), 2);
        let mut writer = RecoveryWriter::new(config);

        let buffer = Buffer::from_text("hello world");
        let path = writer.write_buffer(b"notes/hello.md", &buffer);

        assert!(path.is_some());
        let path = path.unwrap();

        let recovery_file = writer.read_recovery_file(&path);
        assert!(recovery_file.is_some());
        let recovery_file = recovery_file.unwrap();
        assert_eq!(recovery_file.text, "hello world");
    }

    #[test]
    fn recovery_lookup_survives_buffer_identity_change_after_restart() {
        let dir = tempdir().unwrap();
        let config = RecoveryConfig::new("test-vault".to_string(), dir.path().to_path_buf(), 2);
        let note_key = b"notes/identity.md";
        let first_process = Buffer::from_text("recovered");
        let mut writer = RecoveryWriter::new(config.clone());
        writer.write_buffer(note_key, &first_process).unwrap();
        drop(writer);

        let second_process = Buffer::from_text("disk");
        assert_ne!(first_process.buffer_id(), second_process.buffer_id());
        let restarted_writer = RecoveryWriter::new(config);
        let recovered = restarted_writer.read_for_note_key(note_key).unwrap();
        assert_eq!(recovered.to_source_bytes(), b"recovered");
    }

    #[test]
    fn recovery_parser_rejects_tampered_or_invalid_utf8_payloads() {
        let buffer = Buffer::from_text("payload");
        let recovery = RecoveryFile::from_buffer(&buffer, 0, false);
        let mut tampered = recovery.to_bytes();
        let text_start = 38 + 4 + serde_json::to_vec(&recovery.eol_table).unwrap().len() + 4;
        tampered[text_start] ^= 1;
        assert!(RecoveryFile::from_bytes(&tampered).is_none());

        let invalid = RecoveryFile {
            text: "\u{fffd}".into(),
            ..recovery
        };
        // Serializer writes valid UTF-8, but a recovery file with arbitrary
        // bytes in its text section must be rejected rather than lossy-decoded.
        let mut bytes = invalid.to_bytes();
        bytes[text_start] = 0xff;
        assert!(RecoveryFile::from_bytes(&bytes).is_none());
    }

    #[test]
    fn recovery_writer_atomic_write() {
        let dir = tempdir().unwrap();
        let config = RecoveryConfig::new("test-vault".to_string(), dir.path().to_path_buf(), 2);
        let mut writer = RecoveryWriter::new(config);

        let buffer = Buffer::from_text("hello world");
        let path = writer.write_buffer(b"notes/hello.md", &buffer);

        assert!(path.is_some());

        // Check that the file exists
        assert!(path.as_ref().unwrap().exists());

        // Check that there's no temp file left behind
        let temp_path = path.unwrap().with_extension("tmp");
        assert!(!temp_path.exists());
    }

    #[test]
    fn recovery_writer_should_write() {
        use flavite_syntax::text::{Span, TextByte};

        let dir = tempdir().unwrap();
        let config = RecoveryConfig::new("test-vault".to_string(), dir.path().to_path_buf(), 2);
        let mut writer = RecoveryWriter::new(config);

        let mut buffer = Buffer::from_text("hello world");
        // Make the buffer dirty so it should be written
        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " test".to_string());
        buffer.apply_local_edit(edit);

        // First write should happen (buffer is dirty and never written before)
        assert!(writer.should_write(&buffer));

        // Write it
        writer.write_buffer(b"notes/hello.md", &buffer);

        // Second write within interval should not happen
        assert!(!writer.should_write(&buffer));
    }

    #[test]
    fn recovery_writer_checkpoint() {
        let dir = tempdir().unwrap();
        let config = RecoveryConfig::new("test-vault".to_string(), dir.path().to_path_buf(), 2);
        let mut writer = RecoveryWriter::new(config);

        let buffer = Buffer::from_text("hello world");
        let path = writer.write_checkpoint(b"notes/hello.md", &buffer);

        assert!(path.is_some());

        let recovery_file = writer.read_recovery_file(&path.unwrap());
        assert!(recovery_file.is_some());
        assert!(recovery_file.unwrap().header.is_checkpoint);
    }

    #[test]
    fn recovery_writer_list_delete() {
        let dir = tempdir().unwrap();
        let config = RecoveryConfig::new("test-vault".to_string(), dir.path().to_path_buf(), 2);
        let mut writer = RecoveryWriter::new(config);

        // Write some buffers
        let buffer1 = Buffer::from_text("hello");
        let buffer2 = Buffer::from_text("world");
        writer.write_buffer(b"notes/one.md", &buffer1);
        writer.write_buffer(b"notes/two.md", &buffer2);

        // List files
        let files = writer.list_recovery_files();
        assert_eq!(files.len(), 2);

        // Delete one
        if let Some(file) = files.first() {
            assert!(writer.delete_recovery_file(file));
        }

        // Check count
        let files = writer.list_recovery_files();
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn save_retirement_preserves_recovery_for_newer_revision() {
        use flavite_syntax::text::{Span, TextByte};
        let dir = tempdir().unwrap();
        let config = RecoveryConfig::new("vault".into(), dir.path().to_path_buf(), 2);
        let mut writer = RecoveryWriter::new(config);
        let mut buffer = Buffer::from_text("before");
        let old = RecoveryFile::from_buffer(&buffer, 1, false);
        buffer.apply_local_edit(crate::undo::EditOp::new(
            Span::new(TextByte(6), TextByte(6)),
            " after".into(),
        ));
        let newer = RecoveryFile::from_buffer(&buffer, 2, false);
        let old_path = dir.path().join("old.bin");
        let new_path = dir.path().join("new.bin");
        fs::write(&old_path, old.to_bytes()).unwrap();
        fs::write(&new_path, newer.to_bytes()).unwrap();

        assert_eq!(
            writer.retire_covered(buffer.buffer_id(), Revision::new(0)),
            1
        );
        assert!(!old_path.exists());
        assert!(new_path.exists());
    }

    #[test]
    fn stale_checkpoint_cannot_resurrect_after_save_retirement() {
        use crate::state::Buffer;
        use flavite_syntax::text::{Span, TextByte};

        let dir = tempfile::tempdir().unwrap();
        let mut writer = RecoveryWriter::new(RecoveryConfig::new(
            "test-vault".into(),
            dir.path().to_path_buf(),
            0,
        ));
        let mut buffer = Buffer::from_text("base");
        buffer.apply_local_edit(crate::undo::EditOp::new(
            Span::new(TextByte(4), TextByte(4)),
            " edit".into(),
        ));
        let mut stale = RecoveryFile::from_buffer(&buffer, 1, false);
        stale.header.revision = Revision::new(1);
        let mut newer = RecoveryFile::from_buffer(&buffer, 2, false);
        newer.header.revision = Revision::new(2);

        writer.retire_covered(buffer.buffer_id(), Revision::new(1));
        assert!(writer.write_snapshot(b"note.md", stale).is_none());
        assert!(writer.write_snapshot(b"note.md", newer).is_some());
        let recovered = writer.read_for_note_key(b"note.md").unwrap();
        assert_eq!(recovered.header.revision, Revision::new(2));
    }

    #[test]
    fn forced_shutdown_checkpoint_is_durable_and_revision_tagged() {
        use crate::state::Buffer;
        use flavite_syntax::text::{Span, TextByte};

        let dir = tempfile::tempdir().unwrap();
        let mut writer = RecoveryWriter::new(RecoveryConfig::new(
            "test-vault".into(),
            dir.path().to_path_buf(),
            3600,
        ));
        let mut buffer = Buffer::from_text("base");
        buffer.apply_local_edit(crate::undo::EditOp::new(
            Span::new(TextByte(4), TextByte(4)),
            " edit".into(),
        ));
        let recovery = RecoveryFile::from_buffer(&buffer, 7, false);

        // This must bypass the normal interval: shutdown cannot rely on when
        // the periodic recovery timer last happened to run.
        let path = writer
            .write_snapshot_now(b"notes/base.md", recovery)
            .expect("forced checkpoint must be written");
        let restored = writer
            .read_for_note_key(b"notes/base.md")
            .expect("forced checkpoint must be readable");
        assert_eq!(restored.header.revision, buffer.revision());
        assert_eq!(restored.header.checkpoint_seq, 0);
        assert_eq!(restored.to_source_bytes(), b"base edit");
        assert_eq!(path, writer.recovery_path_for_note_key(b"notes/base.md"));
    }

    #[test]
    fn builder() {
        let writer = RecoveryWriterBuilder::new()
            .vault_uuid("test-uuid".to_string())
            .recovery_dir(PathBuf::from("/tmp/flavite/recovery"))
            .interval_secs(5)
            .build();

        assert_eq!(writer.config.vault_uuid, "test-uuid");
        assert_eq!(writer.config.interval_secs, 5);
    }

    #[test]
    fn recovery_batch_snapshot_interval_and_retirement_paths() {
        use flavite_syntax::text::{Span, TextByte};

        let dir = tempdir().unwrap();
        let config = RecoveryConfig::new("vault".into(), dir.path().to_path_buf(), 60);
        let mut writer = RecoveryWriter::new(config);
        let mut first = Buffer::from_text("first");
        let mut second = Buffer::from_text("second");
        first.apply_local_edit(crate::undo::EditOp::new(
            Span::new(TextByte(5), TextByte(5)),
            "!".into(),
        ));
        second.apply_local_edit(crate::undo::EditOp::new(
            Span::new(TextByte(6), TextByte(6)),
            "!".into(),
        ));
        let clean = Buffer::from_text("clean");
        assert!(!writer.should_write(&clean));

        let written = writer.write_all(&[
            (b"first.md".to_vec(), &first),
            (b"second.md".to_vec(), &second),
            (b"clean.md".to_vec(), &clean),
        ]);
        assert_eq!(written.len(), 2);
        assert!(!writer.should_write(&first));
        assert_eq!(writer.list_recovery_files().len(), 2);

        let mut immutable = RecoveryFile::from_buffer(&clean, 99, true);
        assert!(writer.should_write_snapshot(&immutable));
        let snapshot_path = writer
            .write_snapshot(b"first.md", immutable.clone())
            .unwrap();
        immutable.header.buffer_id = BufferId::new();
        assert!(writer.should_write_snapshot(&immutable));
        assert!(writer.delete_recovery_file(&snapshot_path));
        writer.write_buffer(b"first.md", &first).unwrap();

        assert_eq!(writer.delete_for_buffer(second.buffer_id()), 1);
        assert!(writer.read_for_note_key(b"second.md").is_none());
        assert!(writer.delete_for_note_key(b"first.md"));
        let extra = writer.write_buffer(b"extra.md", &first).unwrap();
        assert!(writer.delete_all() >= 1);
        assert!(!extra.exists());
        assert_eq!(writer.list_recovery_files().len(), 0);
        writer.set_interval(0);
        assert_eq!(writer.interval(), Duration::ZERO);
    }
}
