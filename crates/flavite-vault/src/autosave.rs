//! Staged autosave worker (RFC-0011 S7).
//!
//! The host-pump autosave admission must never encode a document while it
//! holds the `driving_turn` / buffer-manager locks: a 16 MiB note would
//! serialize (twice) on the UI thread inside the keystroke-critical pump.
//! Instead the pump captures an O(1) staged snapshot ([`StagedAutosave`])
//! under the lock and hands it to a dedicated worker thread; the worker
//! materializes text, derives the decoded layer, runs the strict codec
//! serialization, computes the FEFF/NUL probes, and hands a complete
//! [`SaveRequest`] back to the Session turn, which submits it to the actor
//! exactly as a synchronous save would. The UI pump never encodes.

use std::path::PathBuf;
use std::sync::Arc;

use flavite_buffer::revision::BaseHash;
use flavite_buffer::undo::VersionState;

use crate::save::{DiskPrecondition, SaveRequest};

/// An O(1) capture of everything an autosave needs, taken under the
/// buffer-manager lock. Cloning the `Arc`s is O(1); the text is materialized
/// and encoded later, on the worker thread.
#[derive(Debug, Clone)]
pub struct StagedAutosave {
    /// Pre-allocated request id (the Session's counter).
    pub request_id: u64,
    /// The note path.
    pub path: PathBuf,
    /// Native note key (for the actor's history pinning).
    pub note_key: Vec<u8>,
    /// Buffer identity at capture time.
    pub buffer_id: u64,
    /// Revision at capture time.
    pub revision: u64,
    /// Immutable version: chunk-tree root + EOL side state + descriptor.
    pub version: Arc<VersionState>,
    /// Disk base hash at capture time (save precondition).
    pub base_hash: Option<BaseHash>,
    /// Exact merge-base bytes (disk state at last save); O(1) Arc clone.
    pub merge_base: Arc<[u8]>,
    /// Disk precondition derived at capture time (O(1)).
    pub precondition: DiskPrecondition,
}

/// The outcome of the worker's off-turn serialization.
#[derive(Debug)]
pub enum AutosaveWork {
    /// The encode succeeded; the request is ready for the Session turn to
    /// submit to the actor. The hint flags are carried so the format-binding
    /// intent persistence stays on the turn (single writer to session state).
    Ready {
        /// Complete actor-ready save request (bytes encoded off-turn).
        request: SaveRequest,
        /// Whether the source needs a persisted binding intent (non-UTF-8
        /// descriptor, BOM, leading signature, or NUL-bearing content).
        needs_hint: bool,
        /// BOM-less content beginning with U+FEFF.
        leading_signature: bool,
        /// NUL-bearing content within the bounded probe windows.
        nul_content: bool,
        /// Disk base hash at capture time (binding-intent old hash).
        base_hash: Option<BaseHash>,
        /// Authoritative save-format descriptor at capture time.
        source_format: flavite_encoding::SourceFormat,
    },
    /// Strict-descriptor refusal (mirrors `Session::save_note`'s typed
    /// codes: `encoding-unencodable` / `encoding-output-limit`).
    EncodeRefused {
        /// The note path (for the surfacing notice).
        path: PathBuf,
        /// The typed action-required code.
        code: &'static str,
        /// Optional structured data (codec limit kind).
        data: Option<String>,
    },
    /// The worker panicked while serializing (contained by the worker loop).
    /// The admitted autosave must not vanish silently: the pump records a
    /// failure so the draft stays visible and retryable.
    Panicked {
        /// The note path.
        path: PathBuf,
    },
}

/// Run the off-turn autosave serialization: materialize, derive the decoded
/// layer, strict-encode under the captured descriptor, probe FEFF/NUL.
///
/// This is the exact byte-for-byte analog of the synchronous
/// `EditorModel::try_to_source_bytes` path used by manual saves — the same
/// decoded layer, limits, and refusal semantics — so an autosave and a
/// manual save of the same revision always serialize identically.
pub fn run_autosave_work(staged: StagedAutosave) -> AutosaveWork {
    let text = staged.version.materialize();
    // Same strict descriptor serialization as `EditorModel::try_to_source_bytes`:
    // decoded layer first (physical EOLs restored), then codec encode.
    let decoded = flavite_buffer::model::decoded_layer(&text, &staged.version.eol_table());
    let bytes = match flavite_encoding::encode(
        &decoded,
        staged.version.format,
        &flavite_encoding::EncodeLimits::default(),
        &flavite_encoding::NOT_CANCELLED,
    ) {
        Ok(bytes) => bytes,
        Err(error) => {
            let unencodable = matches!(error, flavite_encoding::CodecError::Unencodable { .. });
            let code = if unencodable {
                crate::encoding::UNENCODABLE_CODE
            } else {
                crate::encoding::OUTPUT_LIMIT_CODE
            };
            return AutosaveWork::EncodeRefused {
                path: staged.path,
                code,
                data: Some(crate::encoding::codec_limit_kind_code(&error).to_owned()),
            };
        }
    };
    let leading_signature = text.starts_with('\u{FEFF}');
    let nul_content = crate::encoding::contains_nul_in_probe_windows(&text);
    let source_format = staged.version.format;
    let needs_hint = source_format.encoding() != flavite_encoding::FileEncoding::Utf8
        || source_format.bom()
        || leading_signature
        || nul_content;
    let request = SaveRequest {
        request_id: staged.request_id,
        path: staged.path,
        note_key: staged.note_key,
        bytes,
        merge_base: staged.merge_base.as_ref().to_vec(),
        precondition: staged.precondition,
        buffer_id: staged.buffer_id,
        revision: staged.revision,
    };
    AutosaveWork::Ready {
        request,
        needs_hint,
        leading_signature,
        nul_content,
        base_hash: staged.base_hash,
        source_format,
    }
}
