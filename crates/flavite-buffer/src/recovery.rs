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

/// Current version of the recovery file format. v2 accompanies the
/// canonical UTF-8 text with the authoritative `SourceFormat` descriptor,
/// its codec stamp, and separately-verifiable decoded/encoded layer
/// hashes (RFC-0006 §6.10).
const RECOVERY_VERSION: u8 = 2;

/// Legacy recovery version that remains readable; v1 files are treated as
/// the supported legacy UTF-8 format with their recorded BOM flag and are
/// migrated to v2 atomically on the next durable write (the last v1 copy
/// is retained until the v2 receipt).
const RECOVERY_VERSION_V1: u8 = 1;

/// Raw upper bound for one recovery file on disk (§4.5.3): larger files are
/// refused before any read or decode.
const MAX_RECOVERY_FILE_BYTES: u64 = 128 * 1024 * 1024;

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
        if version != RECOVERY_VERSION && version != RECOVERY_VERSION_V1 {
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

/// v1's persisted EOL JSON shape, kept private for legacy reads only: the
/// retired authoritative BOM flag lives here just long enough to map the
/// record onto a UTF-8 `SourceFormat` (its existing BOM semantics).
#[derive(Deserialize)]
struct LegacyEolV1 {
    #[serde(default)]
    line_endings: Vec<crate::eol::LineEnding>,
    #[serde(default)]
    has_bom: bool,
    #[serde(default)]
    has_trailing_newline: bool,
}

/// v2 persisted format block: the descriptor, codec stamp and hint
/// generation, validated on decode (`SourceFormat::new` rejects
/// impossible combinations rather than coercing them).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FormatRecord {
    encoding: flavite_encoding::FileEncoding,
    bom: bool,
    codec_version: u32,
    hint_generation: Option<u64>,
}

/// Recovery file metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryFile {
    /// The header.
    pub header: RecoveryHeader,
    /// The EOL table.
    pub eol_table: EolTable,
    /// The normalized text (CRLF folded). Canonical recovery text remains
    /// UTF-8 irrespective of the note's on-disk `SourceFormat` (§6.10).
    pub text: String,
    /// Length of the DECODED layer (canonical UTF-8 with physical EOLs, no
    /// BOM). v2's authoritative validation length.
    pub source_len: usize,
    /// Hash of the encoded source bytes when the buffer was encodable at
    /// checkpoint time; otherwise the decoded-layer hash (a pending
    /// strict-ASCII draft stays intact/recoverable, saving it fails typed).
    pub content_hash: BaseHash,
    /// The base hash at the time of the last save.
    pub base_hash: Option<BaseHash>,
    /// Authoritative save-format descriptor at checkpoint time. Restores
    /// replay this recorded format — never a current guess (§6.10).
    pub source_format: flavite_encoding::SourceFormat,
    /// Codec implementation version recorded with the descriptor.
    pub codec_version: u32,
    /// Hint generation if the descriptor was hint-bound.
    pub hint_generation: Option<u64>,
    /// Exact encoded-source length and hash, when the content was
    /// encodable at checkpoint time.
    pub encoded: Option<(u64, BaseHash)>,
}

/// Line count as the snapshot serializer defines it (LF-based; a file
/// ending in a newline has no trailing empty line).
fn expected_line_count(text: &str) -> usize {
    if text.is_empty() {
        1
    } else {
        let lf = text.bytes().filter(|b| *b == b'\n').count();
        lf + usize::from(!text.ends_with('\n'))
    }
}

impl RecoveryFile {
    /// Create a new recovery file from a buffer.
    #[must_use]
    pub fn from_buffer(buffer: &Buffer, checkpoint_seq: u64, is_checkpoint: bool) -> Self {
        Self::from_model(
            buffer.model(),
            buffer.buffer_id(),
            buffer.revision(),
            checkpoint_seq,
            is_checkpoint,
            buffer.base_hash(),
        )
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
        let header = RecoveryHeader::new(buffer_id, revision, checkpoint_seq, is_checkpoint);
        let text = model.text().to_string();
        let eol_table = model.eol_table().clone();
        let decoded = eol_serialize(&eol_table, &text);
        let source_len = decoded.len();
        let decoded_hash = hash_bytes(&decoded);
        let (content_hash, encoded) = match model.try_to_source_bytes() {
            Ok(bytes) => {
                let hash = hash_bytes(&bytes);
                (hash, Some((bytes.len() as u64, hash)))
            }
            Err(_) => (decoded_hash, None),
        };
        Self {
            header,
            eol_table,
            text,
            source_len,
            content_hash,
            base_hash,
            source_format: model.source_format(),
            codec_version: model.format_stamp().codec_version,
            hint_generation: model.format_stamp().hint_generation,
            encoded,
        }
    }

    /// Serialize the recovery file to bytes (v2 layout).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();

        // Write header
        bytes.extend_from_slice(&self.header.to_bytes());

        // v2 format block (descriptor + codec stamp + hint generation).
        let record = FormatRecord {
            encoding: self.source_format.encoding(),
            bom: self.source_format.bom(),
            codec_version: self.codec_version,
            hint_generation: self.hint_generation,
        };
        // An impossible descriptor cannot exist in `self`, so serialization
        // here is infallible by construction.
        let format_json = serde_json::to_vec(&record).expect("validated descriptor serializes");
        bytes.extend_from_slice(&(format_json.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&format_json);

        // Write EOL table as JSON. A serde failure here would silently
        // destroy the EOL state, so it is a hard (documented) stop rather
        // than the retired `unwrap_or_default()` empty-blob fallback.
        let eol_json = serde_json::to_vec(&self.eol_table).expect("EolTable is serde-safe");
        bytes.extend_from_slice(&(eol_json.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&eol_json);

        // Write text
        bytes.extend_from_slice(&(self.text.len() as u32).to_le_bytes());
        bytes.extend_from_slice(self.text.as_bytes());

        // Decoded-layer length and hash
        bytes.extend_from_slice(&(self.source_len as u64).to_le_bytes());
        bytes.extend_from_slice(self.decoded_layer_hash().as_slice());

        // Optional encoded source record
        bytes.push(self.encoded.is_some() as u8);
        if let Some((len, hash)) = &self.encoded {
            bytes.extend_from_slice(&len.to_le_bytes());
            bytes.extend_from_slice(hash.as_slice());
        }

        // Write base_hash (optional)
        bytes.push(self.base_hash.is_some() as u8);
        if let Some(hash) = &self.base_hash {
            bytes.extend_from_slice(hash.as_slice());
        }

        bytes
    }

    /// The decoded UTF-8 layer (canonical text with physical EOLs restored,
    /// no BOM): the input to codec serialization.
    #[must_use]
    fn decoded_layer(&self) -> Vec<u8> {
        eol_serialize(&self.eol_table, &self.text)
    }

    #[must_use]
    fn decoded_layer_hash(&self) -> BaseHash {
        hash_bytes(&self.decoded_layer())
    }

    /// Deserialize a recovery file from bytes, accepting v2 and the legacy
    /// v1 layout. Verification failures (corruption, impossible descriptors,
    /// inconsistent layers) return `None`; nothing partial is ever handed
    /// back for restore.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        // Read header (38 bytes with magic + version + fields)
        if bytes.len() < 38 {
            return None;
        }
        let header = RecoveryHeader::from_bytes(&bytes[..38])?;
        match header.version {
            RECOVERY_VERSION => Self::from_bytes_v2(&header, &bytes[38..]),
            RECOVERY_VERSION_V1 => Self::from_bytes_v1(header, bytes),
            _ => None,
        }
    }

    fn from_bytes_v2(header: &RecoveryHeader, rest: &[u8]) -> Option<Self> {
        let mut offset = 0usize;

        // Format block JSON
        let format_json = read_len_prefixed(rest, &mut offset, 64 * 1024)?;
        let record: FormatRecord = serde_json::from_slice(format_json).ok()?;
        let source_format =
            flavite_encoding::SourceFormat::new(record.encoding, record.bom).ok()?;

        // EOL table JSON
        let eol_json = read_len_prefixed(rest, &mut offset, 8 * 1024 * 1024)?;
        let eol_table: EolTable = serde_json::from_slice(eol_json).ok()?;

        // Text
        let text_bytes = read_len_prefixed(rest, &mut offset, 64 * 1024 * 1024)?;
        let text = std::str::from_utf8(text_bytes).ok()?.to_owned();

        // Decoded-layer length and hash
        let source_len = read_u64(rest, &mut offset)?;
        let source_len = usize::try_from(source_len).ok()?;
        if source_len > 64 * 1024 * 1024 {
            return None;
        }
        let decoded_hash = read_hash(rest, &mut offset)?;

        // Optional encoded source record
        let has_encoded = match rest.get(offset).copied()? {
            0 => false,
            1 => true,
            _ => return None,
        };
        offset += 1;
        let encoded = if has_encoded {
            let len = read_u64(rest, &mut offset)?;
            let hash = read_hash(rest, &mut offset)?;
            Some((len, hash))
        } else {
            None
        };

        // Base hash
        let has_base_hash = match rest.get(offset).copied()? {
            0 => false,
            1 => true,
            _ => return None,
        };
        offset += 1;
        let base_hash = if has_base_hash {
            Some(read_hash(rest, &mut offset)?)
        } else {
            None
        };

        if offset != rest.len() {
            return None;
        }

        let recovery = Self {
            header: header.clone(),
            eol_table,
            text,
            source_len,
            content_hash: decoded_hash,
            base_hash,
            source_format,
            codec_version: record.codec_version,
            hint_generation: record.hint_generation,
            encoded,
        };

        // Verification: line count and layer consistency.
        if recovery.eol_table.line_count() != expected_line_count(&recovery.text) {
            return None;
        }
        let decoded = recovery.decoded_layer();
        if decoded.len() != recovery.source_len {
            return None;
        }
        if hash_bytes(&decoded) != decoded_hash {
            return None;
        }
        let decoded_str = std::str::from_utf8(&decoded).ok()?;
        let encoded_attempt = flavite_encoding::encode(
            decoded_str,
            source_format,
            // The writer encoded under the shared RFC envelope (32 MiB), so
            // verification uses the same limits.
            &flavite_encoding::EncodeLimits::default(),
            &flavite_encoding::NOT_CANCELLED,
        );
        match (&recovery.encoded, encoded_attempt) {
            (Some((len, hash)), Ok(bytes)) => {
                if bytes.len() as u64 != *len || hash_bytes(&bytes) != *hash {
                    return None;
                }
            }
            (None, Err(_)) => {}
            _ => return None,
        }
        let content_hash = match &recovery.encoded {
            Some((_, hash)) => *hash,
            None => decoded_hash,
        };
        let mut recovery = recovery;
        recovery.content_hash = content_hash;
        Some(recovery)
    }

    /// Legacy v1 reader: canonical UTF-8 with the recorded BOM flag.
    ///
    /// v1 wrote `source_len` as the normalized TEXT length while validating
    /// against the serialized SOURCE length, so BOM/CRLF records could fail
    /// their own checksum (a latent draft-loss bug this slice retires). The
    /// v2 reader therefore verifies v1 by content hash only — against the
    /// encoded source (the old writer's intent) or the decoded layer — and
    /// keeps every otherwise-valid draft readable instead of discarding it.
    fn from_bytes_v1(header: RecoveryHeader, bytes: &[u8]) -> Option<Self> {
        let mut offset = 38usize;

        let eol_json = read_len_prefixed(bytes, &mut offset, 8 * 1024 * 1024)?;
        let legacy: LegacyEolV1 = serde_json::from_slice(eol_json).ok()?;
        let eol_table = EolTable {
            line_endings: legacy.line_endings,
            has_trailing_newline: legacy.has_trailing_newline,
        };

        let text_bytes = read_len_prefixed(bytes, &mut offset, 64 * 1024 * 1024)?;
        let text = std::str::from_utf8(text_bytes).ok()?.to_owned();

        let source_len = read_u64(bytes, &mut offset)?;
        usize::try_from(source_len).ok()?;
        let content_hash = read_hash(bytes, &mut offset)?;

        let has_base_hash = match bytes.get(offset).copied()? {
            0 => false,
            1 => true,
            _ => return None,
        };
        offset += 1;
        let base_hash = if has_base_hash {
            if offset + 32 <= bytes.len() {
                let hash = read_hash(bytes, &mut offset)?;
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
            source_len: 0, // recomputed below from the decoded layer
            content_hash,
            base_hash,
            source_format: if legacy.has_bom {
                flavite_encoding::SourceFormat::utf8_with_bom()
            } else {
                flavite_encoding::SourceFormat::utf8()
            },
            codec_version: flavite_encoding::CODEC_VERSION,
            hint_generation: None,
            encoded: None,
        };
        let decoded = recovery.decoded_layer();
        let decoded_hash = hash_bytes(&decoded);
        let source_format = recovery.source_format;
        let encoded = flavite_encoding::encode(
            std::str::from_utf8(&decoded).ok()?,
            source_format,
            &flavite_encoding::EncodeLimits::default(),
            &flavite_encoding::NOT_CANCELLED,
        )
        .ok();
        let mut encoded_record = None;
        let verified = if let Some(bytes) = &encoded {
            let hash = hash_bytes(bytes);
            encoded_record = Some((bytes.len() as u64, hash));
            hash == content_hash
        } else {
            false
        } || decoded_hash == content_hash;
        if !verified {
            return None;
        }
        // v1 canonical text was snapshot-serialized, so line counts agree
        // by construction for anything that passes the hash check.
        let mut recovery = recovery;
        recovery.source_len = decoded.len();
        recovery.encoded = encoded_record.filter(|_| encoded.is_some());
        Some(recovery)
    }

    /// Reconstruct the exact SOURCE bytes of the recorded state by codec
    /// serialization under the recorded descriptor.
    ///
    /// # Errors
    ///
    /// Typed codec failure for a pending strict-ASCII draft
    /// (`Unencodable`) or a bound violation; the caller keeps the recovery
    /// file as evidence rather than writing substituted bytes.
    pub fn to_source_bytes(&self) -> Result<Vec<u8>, flavite_encoding::CodecError> {
        let decoded = self.decoded_layer();
        let decoded = std::str::from_utf8(&decoded)
            .map_err(|_| flavite_encoding::CodecError::InvalidUtf8 { source_byte: 0 })?;
        flavite_encoding::encode(
            decoded,
            self.source_format,
            &flavite_encoding::EncodeLimits::default(),
            &flavite_encoding::NOT_CANCELLED,
        )
    }

    /// Check if the recovery file's content hash matches the expected hash.
    #[must_use]
    pub fn matches_hash(&self, hash: BaseHash) -> bool {
        self.content_hash == hash
    }
}

/// Serialize the decoded layer (canonical text with physical EOLs restored,
/// no BOM) from the table. LF offsets are the line breaks; a missing final
/// LF appends the trailing content.
fn eol_serialize(table: &EolTable, text: &str) -> Vec<u8> {
    let line_breaks: Vec<TextByte> = text
        .char_indices()
        .filter(|(_, c)| *c == '\n')
        .map(|(idx, _)| TextByte(idx))
        .collect();
    table.serialize(text, &line_breaks)
}

fn hash_bytes(bytes: &[u8]) -> BaseHash {
    BaseHash::from_bytes(*blake3::hash(bytes).as_bytes())
}

fn read_len_prefixed<'a>(bytes: &'a [u8], offset: &mut usize, max: usize) -> Option<&'a [u8]> {
    let len_raw: [u8; 4] = bytes.get(*offset..*offset + 4)?.try_into().ok()?;
    let len = u32::from_le_bytes(len_raw) as usize;
    if len > max {
        return None;
    }
    *offset += 4;
    let slice = bytes.get(*offset..*offset + len)?;
    *offset += len;
    Some(slice)
}

fn read_u64(bytes: &[u8], offset: &mut usize) -> Option<u64> {
    let raw: [u8; 8] = bytes.get(*offset..*offset + 8)?.try_into().ok()?;
    *offset += 8;
    Some(u64::from_le_bytes(raw))
}

fn read_hash(bytes: &[u8], offset: &mut usize) -> Option<BaseHash> {
    let slice = bytes.get(*offset..*offset + 32)?;
    *offset += 32;
    Some(BaseHash::from_slice(slice))
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
        buffer.is_dirty() && self.should_write_revision(buffer.buffer_id(), buffer.revision())
    }

    /// Check cheap buffer metadata before copying/hashing a recovery snapshot.
    #[must_use]
    pub fn should_write_revision(&self, buffer_id: BufferId, revision: Revision) -> bool {
        if self
            .covered_revision
            .get(&buffer_id)
            .is_some_and(|covered| *covered >= revision)
        {
            return false;
        }
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
        self.should_write_revision(recovery.header.buffer_id, recovery.header.revision)
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

    /// Write a durable checkpoint (RFC-0006 S09/D-11): the same atomic,
    /// directory-synced write as the periodic/normal path, but stamped as a
    /// checkpoint so crash recovery can classify a draft-replacement effect
    /// against a durable marker. Returns the written path, or `None` when
    /// the covered-revision gate refuses (a save receipt already retires
    /// this revision) or the write fails.
    pub fn write_checkpoint_durable(
        &mut self,
        note_key: &[u8],
        buffer: &Buffer,
    ) -> Option<PathBuf> {
        let buffer_id = buffer.buffer_id();
        if self
            .covered_revision
            .get(&buffer_id)
            .is_some_and(|covered| buffer.revision() <= *covered)
        {
            return None;
        }
        let seq = *self.checkpoint_seq.entry(buffer_id).or_insert(0);
        self.checkpoint_seq.insert(buffer_id, seq.saturating_add(1));
        let recovery_file = RecoveryFile::from_buffer(buffer, seq, true);
        let path = self.recovery_path_for_note_key(note_key);
        // Never regress a preserved newer recovery record.
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

    /// Read a recovery file.
    ///
    /// Returns the recovery file if successful and valid.
    pub fn read_recovery_file(&self, path: &Path) -> Option<RecoveryFile> {
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
        self.remove_matching(|header| header.buffer_id == buffer_id)
    }

    /// Retire only recovery records for this buffer whose revision is covered
    /// by an acknowledged save. Newer revisions remain reachable.
    pub fn retire_covered(&mut self, buffer_id: BufferId, saved_revision: Revision) -> usize {
        self.covered_revision
            .entry(buffer_id)
            .and_modify(|covered| *covered = (*covered).max(saved_revision))
            .or_insert(saved_revision);
        self.remove_matching(|header| {
            header.buffer_id == buffer_id && header.revision <= saved_revision
        })
    }

    /// Delete recovery for a note only after the caller has completed its
    /// explicit discard decision.
    pub fn delete_for_note_key(&self, note_key: &[u8]) -> bool {
        self.delete_recovery_file(&self.recovery_path_for_note_key(note_key))
    }

    /// Remove every recovery file whose 38-byte header matches. The header
    /// carries (buffer_id, revision), so candidacy is decided without
    /// decoding; the full decode + re-encode verification only runs for
    /// those candidates — never for every `*.bin` on every save.
    fn remove_matching(&self, header_matches: impl Fn(&RecoveryHeader) -> bool) -> usize {
        let mut removed = 0;
        for path in self.list_recovery_files() {
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            if metadata.len() > MAX_RECOVERY_FILE_BYTES {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let Some(header) = RecoveryHeader::from_bytes(&bytes) else {
                continue;
            };
            if !header_matches(&header) {
                continue;
            }
            if RecoveryFile::from_bytes(&bytes).is_some_and(|file| header_matches(&file.header))
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

/// Adjacent retained copy for the v1→v2 migration receipt.
fn legacy_copy_path(path: &Path) -> PathBuf {
    path.with_extension("v1-last")
}

fn is_durable_v1(bytes: &[u8]) -> bool {
    bytes.len() >= 5 && bytes[..4] == *RECOVERY_MAGIC && bytes[4] == RECOVERY_VERSION_V1
}

/// v1→v2 migration receipt: when `path` holds a durable v1 file, copy it to
/// the adjacent retained name *before* the v2 overwrite replaces it, and
/// only retire the copy after the v2 write is durable (§6.10).
fn retain_v1_before_write(path: &Path) -> Option<PathBuf> {
    let existing = fs::read(path).ok()?;
    if !is_durable_v1(&existing) {
        return None;
    }
    let keep = legacy_copy_path(path);
    let temp = keep.with_extension("v1tmp");
    (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)?;
        file.write_all(&existing)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, &keep)?;
        if let Some(parent) = keep.parent() {
            #[cfg(unix)]
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    })()
    .ok()?;
    Some(keep)
}

/// Retire the retained v1 copy once the v2 receipt is durable.
fn retire_v1_copy(keep: Option<&PathBuf>) {
    if let Some(path) = keep {
        let _ = fs::remove_file(path);
    }
}

fn write_recovery_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("recovery path has no parent"))?;
    fs::create_dir_all(parent)?;
    let retained_v1 = retain_v1_before_write(path);
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
    } else {
        retire_v1_copy(retained_v1.as_ref());
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

    #[test]
    fn recovery_admission_uses_revision_stamps_before_copying_text() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            RecoveryWriter::new(RecoveryConfig::new("stamps".into(), dir.path().into(), 60));
        let id = BufferId::new();
        let revision = Revision::new(3);
        assert!(writer.should_write_revision(id, revision));
        writer.last_write.insert(id, Instant::now());
        assert!(!writer.should_write_revision(id, revision));
        writer
            .last_write
            .insert(id, Instant::now() - Duration::from_secs(61));
        assert!(writer.should_write_revision(id, revision));
        writer.covered_revision.insert(id, revision);
        assert!(!writer.should_write_revision(id, revision));
        assert!(writer.should_write_revision(id, revision.next()));
    }
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

        let source_bytes = recovery_file.to_source_bytes().unwrap();
        assert_eq!(source_bytes, b"hello\nworld");
    }

    #[test]
    fn recovery_writer_write_read() {
        let dir = tempdir().unwrap();
        let config = RecoveryConfig::new("test-vault".to_string(), dir.path().to_path_buf(), 2);
        let mut writer = RecoveryWriter::new(config);

        let buffer = Buffer::from_text("hello world");
        let path = writer.write_snapshot_now(
            b"notes/hello.md",
            RecoveryFile::from_buffer(&buffer, 0, false),
        );

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
        writer
            .write_snapshot_now(
                note_key,
                RecoveryFile::from_buffer(&first_process, 0, false),
            )
            .unwrap();
        drop(writer);

        let second_process = Buffer::from_text("disk");
        assert_ne!(first_process.buffer_id(), second_process.buffer_id());
        let restarted_writer = RecoveryWriter::new(config);
        let recovered = restarted_writer.read_for_note_key(note_key).unwrap();
        assert_eq!(recovered.to_source_bytes().unwrap(), b"recovered");
    }

    #[test]
    fn recovery_parser_rejects_tampered_or_invalid_utf8_payloads() {
        let buffer = Buffer::from_text("payload");
        let recovery = RecoveryFile::from_buffer(&buffer, 0, false);
        let mut tampered = recovery.to_bytes();
        // v2 framing: header(38) | u32 len + format json | u32 len + eol json
        // | u32 len + text.
        let format_len = serde_json::to_vec(&format_record_of(&recovery))
            .unwrap()
            .len();
        let eol_len = serde_json::to_vec(&recovery.eol_table).unwrap().len();
        let text_start = 38 + 4 + format_len + 4 + eol_len + 4;
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

    fn format_record_of(recovery: &RecoveryFile) -> FormatRecord {
        FormatRecord {
            encoding: recovery.source_format.encoding(),
            bom: recovery.source_format.bom(),
            codec_version: recovery.codec_version,
            hint_generation: recovery.hint_generation,
        }
    }

    #[test]
    fn recovery_writer_atomic_write() {
        let dir = tempdir().unwrap();
        let config = RecoveryConfig::new("test-vault".to_string(), dir.path().to_path_buf(), 2);
        let mut writer = RecoveryWriter::new(config);

        let buffer = Buffer::from_text("hello world");
        let path = writer.write_snapshot_now(
            b"notes/hello.md",
            RecoveryFile::from_buffer(&buffer, 0, false),
        );

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
        writer.write_snapshot_now(
            b"notes/hello.md",
            RecoveryFile::from_buffer(&buffer, 0, false),
        );

        // Second write within interval should not happen
        assert!(!writer.should_write(&buffer));
    }

    #[test]
    fn recovery_writer_checkpoint() {
        let dir = tempdir().unwrap();
        let config = RecoveryConfig::new("test-vault".to_string(), dir.path().to_path_buf(), 2);
        let mut writer = RecoveryWriter::new(config);

        let buffer = Buffer::from_text("hello world");
        let path = writer.write_checkpoint_durable(b"notes/hello.md", &buffer);

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
        writer.write_snapshot_now(
            b"notes/one.md",
            RecoveryFile::from_buffer(&buffer1, 0, false),
        );
        writer.write_snapshot_now(
            b"notes/two.md",
            RecoveryFile::from_buffer(&buffer2, 0, false),
        );

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
        assert_eq!(restored.to_source_bytes().unwrap(), b"base edit");
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

        // The periodic batch writes only the dirty buffers, gated by the
        // revision stamps.
        let written = [
            writer.write_snapshot(b"first.md", RecoveryFile::from_buffer(&first, 0, false)),
            writer.write_snapshot(b"second.md", RecoveryFile::from_buffer(&second, 0, false)),
        ];
        assert!(written.iter().all(|path| path.is_some()));
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
        writer
            .write_snapshot_now(b"first.md", RecoveryFile::from_buffer(&first, 0, false))
            .unwrap();

        assert_eq!(writer.delete_for_buffer(second.buffer_id()), 1);
        assert!(writer.read_for_note_key(b"second.md").is_none());
        assert!(writer.delete_for_note_key(b"first.md"));
        let extra = writer
            .write_snapshot_now(b"extra.md", RecoveryFile::from_buffer(&first, 0, false))
            .unwrap();
        assert!(writer.delete_all() >= 1);
        assert!(!extra.exists());
        assert_eq!(writer.list_recovery_files().len(), 0);
        writer.set_interval(0);
        assert_eq!(writer.interval(), Duration::ZERO);
    }
}
