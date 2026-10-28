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
        let is_checkpoint = bytes.get(37).copied() == Some(1);

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
        let _offset = 0usize;

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
        let text = String::from_utf8_lossy(&bytes[offset..offset + text_len]).to_string();
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
        offset += 8;

        // Read content_hash
        if offset + 32 > bytes.len() {
            return None;
        }
        let content_hash = BaseHash::from_slice(&bytes[offset..offset + 32]);
        offset += 32;

        // Read base_hash
        let has_base_hash = bytes.get(offset).copied() == Some(1);
        offset += 1;
        let base_hash = if has_base_hash {
            if offset + 32 <= bytes.len() {
                Some(BaseHash::from_slice(&bytes[offset..offset + 32]))
            } else {
                None
            }
        } else {
            None
        };

        Some(Self {
            header,
            eol_table,
            text,
            source_len,
            content_hash,
            base_hash,
        })
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
}

impl RecoveryWriter {
    /// Create a new recovery writer.
    #[must_use]
    pub fn new(config: RecoveryConfig) -> Self {
        Self {
            config,
            last_write: HashMap::new(),
            checkpoint_seq: HashMap::new(),
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

    /// Generate the recovery file path for a buffer.
    #[must_use]
    pub fn recovery_path(&self, buffer: &Buffer) -> PathBuf {
        self.recovery_path_for(buffer.buffer_id(), buffer.model().eol_table())
    }

    /// Generate the recovery file path for a buffer with a specific EOL table.
    #[must_use]
    pub fn recovery_path_for(&self, buffer_id: BufferId, eol_table: &EolTable) -> PathBuf {
        // Hash the buffer ID and EOL table to create a unique filename
        let mut hasher = blake3::Hasher::new();
        hasher.update(&buffer_id.as_u64().to_le_bytes());
        hasher.update(&eol_table.has_bom().to_string().as_bytes());
        hasher.update(&eol_table.has_trailing_newline().to_string().as_bytes());
        for eol in &eol_table.line_endings {
            hasher.update(&[*eol as u8]);
        }
        let hash = hasher.finalize();
        let hash_hex = hash.to_hex();

        self.config.recovery_dir.join(format!("{}.bin", hash_hex))
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

    /// Write recovery files for all dirty buffers.
    ///
    /// Returns the paths that were written.
    pub fn write_all(&mut self, buffers: &[&Buffer]) -> Vec<PathBuf> {
        let mut written = Vec::new();

        for buffer in buffers {
            if self.should_write(buffer) {
                if let Some(path) = self.write_buffer(buffer) {
                    written.push(path);
                }
            }
        }

        written
    }

    /// Write a recovery file for a specific buffer.
    ///
    /// The write is atomic: write to a temp file, fsync, rename.
    /// Returns the path if successful.
    pub fn write_buffer(&mut self, buffer: &Buffer) -> Option<PathBuf> {
        let path = self.recovery_path(buffer);
        let buffer_id = buffer.buffer_id();

        // Get or create checkpoint sequence
        let seq = *self.checkpoint_seq.entry(buffer_id).or_insert(0);
        let next_seq = seq + 1;
        self.checkpoint_seq.insert(buffer_id, next_seq);

        // Create recovery file
        let recovery_file = RecoveryFile::from_buffer(buffer, seq, false);
        let bytes = recovery_file.to_bytes();

        // Ensure directory exists
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                eprintln!("WARN: Failed to create recovery directory: {}", e);
                return None;
            }
        }

        // Write atomically
        let temp_path = path.with_extension("tmp");

        let mut file = match File::create(&temp_path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("WARN: Failed to create temp recovery file: {}", e);
                return None;
            }
        };

        if let Err(e) = file.write_all(&bytes) {
            eprintln!("WARN: Failed to write recovery file: {}", e);
            fs::remove_file(&temp_path).ok();
            return None;
        }

        if let Err(e) = file.sync_all() {
            eprintln!("WARN: Failed to sync recovery file: {}", e);
            fs::remove_file(&temp_path).ok();
            return None;
        }

        drop(file);

        // Rename atomically
        if let Err(e) = fs::rename(&temp_path, &path) {
            eprintln!("WARN: Failed to rename recovery file: {}", e);
            fs::remove_file(&temp_path).ok();
            return None;
        }

        // Update last write time
        self.last_write.insert(buffer_id, Instant::now());

        Some(path)
    }

    /// Write a checkpoint for a buffer.
    ///
    /// Checkpoints are durable recovery points that are written before
    /// operations like save or undo.
    pub fn write_checkpoint(&mut self, buffer: &Buffer) -> Option<PathBuf> {
        let buffer_id = buffer.buffer_id();

        // Get or create checkpoint sequence
        let seq = *self.checkpoint_seq.entry(buffer_id).or_insert(0);
        let next_seq = seq + 1;
        self.checkpoint_seq.insert(buffer_id, next_seq);

        // Create recovery file with checkpoint flag
        let recovery_file = RecoveryFile::from_buffer(buffer, seq, true);
        let path = self.recovery_path(buffer);
        let bytes = recovery_file.to_bytes();

        // Ensure directory exists
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                eprintln!("WARN: Failed to create recovery directory: {}", e);
                return None;
            }
        }

        // Write atomically (same as write_buffer)
        let temp_path = path.with_extension("tmp");

        let mut file = match File::create(&temp_path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("WARN: Failed to create temp checkpoint file: {}", e);
                return None;
            }
        };

        if let Err(e) = file.write_all(&bytes) {
            eprintln!("WARN: Failed to write checkpoint file: {}", e);
            fs::remove_file(&temp_path).ok();
            return None;
        }

        if let Err(e) = file.sync_all() {
            eprintln!("WARN: Failed to sync checkpoint file: {}", e);
            fs::remove_file(&temp_path).ok();
            return None;
        }

        drop(file);

        if let Err(e) = fs::rename(&temp_path, &path) {
            eprintln!("WARN: Failed to rename checkpoint file: {}", e);
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
        let bytes = fs::read(path).ok()?;
        RecoveryFile::from_bytes(&bytes)
    }

    /// List all recovery files for this vault.
    #[must_use]
    pub fn list_recovery_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();

        if let Ok(entries) = fs::read_dir(&self.config.recovery_dir) {
            for entry in entries {
                if let Ok(entry) = entry {
                    let path = entry.path();
                    if path.extension().map_or(false, |ext| ext == "bin") {
                        files.push(path);
                    }
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
        let path = self.recovery_path_for(buffer_id, &EolTable::default());
        let pattern = path.file_name().unwrap_or_default();

        let mut deleted = 0usize;
        if let Ok(entries) = fs::read_dir(&self.config.recovery_dir) {
            for entry in entries {
                if let Ok(entry) = entry {
                    let entry_path = entry.path();
                    if entry_path.file_name() == Some(pattern) {
                        if self.delete_recovery_file(&entry_path) {
                            deleted += 1;
                        }
                    }
                }
            }
        }

        deleted
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
    use std::io::Write;
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
        let buffer = Buffer::from_str("hello\nworld");
        let recovery_file = RecoveryFile::from_buffer(&buffer, 1, false);

        let bytes = recovery_file.to_bytes();
        let decoded = RecoveryFile::from_bytes(&bytes);

        assert!(decoded.is_some());
        let decoded = decoded.unwrap();
        assert_eq!(decoded.text, "hello\nworld");
    }

    #[test]
    fn recovery_file_to_source_bytes() {
        let buffer = Buffer::from_str("hello\nworld");
        let recovery_file = RecoveryFile::from_buffer(&buffer, 1, false);

        let source_bytes = recovery_file.to_source_bytes();
        assert_eq!(source_bytes, b"hello\nworld");
    }

    #[test]
    fn recovery_writer_write_read() {
        let dir = tempdir().unwrap();
        let config = RecoveryConfig::new("test-vault".to_string(), dir.path().to_path_buf(), 2);
        let mut writer = RecoveryWriter::new(config);

        let buffer = Buffer::from_str("hello world");
        let path = writer.write_buffer(&buffer);

        assert!(path.is_some());
        let path = path.unwrap();

        let recovery_file = writer.read_recovery_file(&path);
        assert!(recovery_file.is_some());
        let recovery_file = recovery_file.unwrap();
        assert_eq!(recovery_file.text, "hello world");
    }

    #[test]
    fn recovery_writer_atomic_write() {
        let dir = tempdir().unwrap();
        let config = RecoveryConfig::new("test-vault".to_string(), dir.path().to_path_buf(), 2);
        let mut writer = RecoveryWriter::new(config);

        let buffer = Buffer::from_str("hello world");
        let path = writer.write_buffer(&buffer);

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

        let mut buffer = Buffer::from_str("hello world");
        // Make the buffer dirty so it should be written
        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " test".to_string());
        buffer.apply_local_edit(edit);

        // First write should happen (buffer is dirty and never written before)
        assert!(writer.should_write(&buffer));

        // Write it
        writer.write_buffer(&buffer);

        // Second write within interval should not happen
        assert!(!writer.should_write(&buffer));
    }

    #[test]
    fn recovery_writer_checkpoint() {
        let dir = tempdir().unwrap();
        let config = RecoveryConfig::new("test-vault".to_string(), dir.path().to_path_buf(), 2);
        let mut writer = RecoveryWriter::new(config);

        let buffer = Buffer::from_str("hello world");
        let path = writer.write_checkpoint(&buffer);

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
        let buffer1 = Buffer::from_str("hello");
        let buffer2 = Buffer::from_str("world");
        writer.write_buffer(&buffer1);
        writer.write_buffer(&buffer2);

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
    fn builder() {
        let writer = RecoveryWriterBuilder::new()
            .vault_uuid("test-uuid".to_string())
            .recovery_dir(PathBuf::from("/tmp/flavite/recovery"))
            .interval_secs(5)
            .build();

        assert_eq!(writer.config.vault_uuid, "test-uuid");
        assert_eq!(writer.config.interval_secs, 5);
    }
}
