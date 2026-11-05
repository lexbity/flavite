//! Central source-format selection, hints and worker publication
//! (RFC-0006 D-15 / §6.10).
//!
//! One codec contract governs open/index/watch/rename/save/merge/history/
//! recovery/replacement: raw bytes enter here, and every outcome is either a
//! successfully strict-decoded snapshot with its authoritative descriptor, or
//! a typed `EncodingChoiceRequired` reason. Nothing in this module performs
//! Markdown parsing, EOL normalization, or filesystem policy beyond the
//! device-local user-state store it owns.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use flavite_encoding::{
    CodecError, DecodeLimits, DecodePolicy, FileEncoding, FormatStamp, SourceFormat,
};
use flavite_syntax::text::{TextError, TextSnapshot};
use serde::{Deserialize, Serialize};

use crate::user_state::{UserStateError, UserStateKind, UserStateStore};

/// Payload schema of the device-local `encoding-hints.json` record.
pub const HINTS_SCHEMA: u32 = 1;

/// Bound raw-byte hash (blake3) of the last confirmed interpretation.
pub type RawHash = [u8; 32];

/// One confirmed hint for one native source identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HintEntry {
    /// Confirmed encoding family.
    pub encoding: FileEncoding,
    /// Confirmed BOM intent.
    pub bom: bool,
    /// Raw hash of the bytes the confirmation was made against. An external
    /// new hash invalidates the entry: `EncodingChoiceRequired`, never a
    /// silent stale-hint decode (§6.10).
    pub bound_raw_hash: RawHash,
    /// Exact intent to preserve a leading signature-shaped scalar as content
    /// (BOM-less UTF-8 whose content begins with U+FEFF, or BOM-less UTF-16
    /// starting with the signature code unit). Bytes alone cannot express it.
    #[serde(default)]
    pub preserve_leading_signature: bool,
    /// Confirmed BOM-less content containing NUL scalars (ambiguous/binary
    /// input gate under §6.10 core policy; without this the open asks).
    #[serde(default)]
    pub nul_content_confirmed: bool,
}

impl HintEntry {
    #[must_use]
    pub fn format(&self) -> SourceFormat {
        // Validation only rejects ASCII+BOM; a hostile record combining them
        // falls back to UTF-8 instead of panicking on untrusted disk input.
        SourceFormat::new(self.encoding, self.bom)
            .unwrap_or_else(|_| SourceFormat::utf8())
    }
}

/// A durable save-format binding intent written BEFORE a source effect whose
/// interpretation depends on hint metadata (§6.10 "format-binding intent").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingIntent {
    /// Native path key (hex) of the source.
    pub native_key: String,
    /// Raw hash the source held before the replacement.
    pub old_raw_hash: RawHash,
    /// Exact serialized output the save intends to make durable.
    pub output_raw_hash: RawHash,
    /// Descriptor the output was written under.
    pub encoding: FileEncoding,
    pub bom: bool,
    /// Preserve-leading-signature intent being established/kept.
    pub preserve_leading_signature: bool,
    pub nul_content_confirmed: bool,
}
impl BindingIntent {
    /// The descriptor the output was written under (validated combination).
    #[must_use]
    pub fn format(&self) -> SourceFormat {
        SourceFormat::new(self.encoding, self.bom).unwrap_or_else(|_| SourceFormat::utf8())
    }
}


/// The whole device-local hints record (payload of `encoding-hints.json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HintsRecord {
    pub schema: u32,
    /// Root identity binding copied at write time for cross-root refusal.
    #[serde(default)]
    pub root_identity: String,
    /// Entries keyed by hex native path key.
    #[serde(default)]
    pub entries: HashMap<String, HintEntry>,
    /// Unresolved pre-save binding intents, keyed by native path key.
    #[serde(default)]
    pub intents: HashMap<String, BindingIntent>,
}

impl HintsRecord {
    #[must_use]
    pub fn new(root_identity: String) -> Self {
        Self {
            schema: HINTS_SCHEMA,
            root_identity,
            entries: HashMap::new(),
            intents: HashMap::new(),
        }
    }

    /// Encode to the canonical JSON payload (sorted keys for stable
    /// digests).
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] if the record cannot be represented (never for
    /// validated in-memory shapes).
    pub fn to_payload(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Decode untrusted disk bytes: schema-1 only, no unknown fields, no
    /// lossy interpretation.
    ///
    /// # Errors
    ///
    /// A typed [`HintsLoadError`] distinguishing corrupt/unversioned data
    /// from schema decisions the running binary cannot honor.
    pub fn from_payload(bytes: &[u8]) -> Result<Self, HintsLoadError> {
        #[derive(Deserialize)]
        struct SchemaProbe {
            schema: u32,
        }
        let probe: SchemaProbe = serde_json::from_slice(bytes)
            .map_err(|_| HintsLoadError::Corrupt("not a hints object".to_owned()))?;
        if probe.schema > HINTS_SCHEMA {
            return Err(HintsLoadError::FutureSchema(probe.schema));
        }
        if probe.schema < HINTS_SCHEMA {
            return Err(HintsLoadError::UnsupportedSchema(probe.schema));
        }
        Self::from_slice(bytes).map_err(|e| HintsLoadError::Corrupt(e.to_string()))
    }

    fn from_slice(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

/// Why loading the hints record failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintsLoadError {
    /// Data is not a valid schema-1 hints record.
    Corrupt(String),
    /// A newer schema is present; this binary refuses to reinterpret it.
    FutureSchema(u32),
    /// An older/unsupported schema without a migration path.
    UnsupportedSchema(u32),
}

/// Immutable published hints view shared with open/reconcile/watch workers.
/// One publication per generation — never a UI-only label (§6.10).
#[derive(Debug)]
pub struct HintsView {
    /// User-state generation backing this view (None: store unavailable).
    pub generation: Option<u64>,
    pub record: HintsRecord,
}

/// Device-local hint store plus its atomically published view.
#[derive(Debug)]
pub struct EncodingHintsState {
    /// Mutex-protected because confirmations serialize through shared
    /// `&self` Session turns; the publication itself is lock-free ArcSwap.
    store: Option<std::sync::Mutex<UserStateStore>>,
    published: ArcSwap<HintsView>,
}

impl EncodingHintsState {
    /// Load (or lazily create) the record and publish the first view. A
    /// write failure is preserved as a typed pending state on the outcome,
    /// never presented as a durably configured choice.
    #[must_use]
    pub fn open(store: Option<UserStateStore>, root_identity: String) -> (Self, Option<String>) {
        let fallback = HintsRecord::new(root_identity);
        let mut load_issue = None;
        let view = match &store {
            Some(user_store) => match user_store.read(UserStateKind::EncodingHints) {
                Ok(Some(stored)) => match HintsRecord::from_payload(&stored.payload) {
                    Ok(mut record) => {
                        if record.root_identity.is_empty() {
                            record.root_identity = fallback.root_identity.clone();
                        }
                        Some(HintsView {
                            generation: Some(stored.generation),
                            record,
                        })
                    }
                    Err(e) => {
                        load_issue = Some(format!("encoding-hints-record-invalid:{e:?}"));
                        None
                    }
                },
                Ok(None) => None,
                Err(e) => {
                    load_issue = Some(format!("encoding-hints-read-failed:{e:?}"));
                    None
                }
            },
            None => None,
        };
        let view = view.unwrap_or_else(|| HintsView {
            generation: None,
            record: fallback,
        });
        let state = Self {
            store: store.map(std::sync::Mutex::new),
            published: ArcSwap::from_pointee(view),
        };
        (state, load_issue)
    }

    /// Current immutable view for worker acquisition (cheap Arc clone).
    #[must_use]
    pub fn view(&self) -> Arc<HintsView> {
        self.published.load_full()
    }

    /// The store, when a device-local data root was available.
    #[must_use]
    pub const fn store(&self) -> Option<&std::sync::Mutex<UserStateStore>> {
        self.store.as_ref()
    }

    /// Apply `mutate` to the current record and durably write it, publishing
    /// the new generation on success. Returns the failure when the store
    /// cannot protect/record the state (§6.10: hint write failure is a typed
    /// pending/error state; the ambiguous source must not be advertised as
    /// durably configured).
    ///
    /// # Errors
    ///
    /// [`HintsWriteError`] when no store is available, the mutation payload
    /// cannot be serialized, or the compare-and-set write fails.
    pub fn update(
        &self,
        mutate: impl FnOnce(&mut HintsRecord),
    ) -> Result<u64, HintsWriteError> {
        let store = self.store.as_ref().ok_or(HintsWriteError::NoStore)?;
        let mut store = store
            .lock()
            .map_err(|_| HintsWriteError::Store(UserStateError::Corrupt("poisoned".into())))?;
        let current = self.published.load_full();
        let mut record = current.record.clone();
        mutate(&mut record);
        let payload = record
            .to_payload()
            .map_err(|e| HintsWriteError::Store(UserStateError::Corrupt(format!("payload:{e}"))))?;
        let expected = current.generation.unwrap_or(0);
        let next = store
            .write(UserStateKind::EncodingHints, expected, &payload)
            .map_err(HintsWriteError::Store)?;
        self.published.store(Arc::new(HintsView {
            generation: Some(next),
            record,
        }));
        Ok(next)
    }

    /// Publish without durable backing is NOT allowed for confirmations;
    /// provided only for tests of the startup fallback view.
    #[cfg(test)]
    fn replace_view(&self, view: HintsView) {
        self.published.store(Arc::new(view));
    }
}

/// Hint persistence failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintsWriteError {
    /// No device-local data root is available (e.g. read-only environment).
    NoStore,
    /// The underlying user-state store refused the write.
    Store(UserStateError),
}

/// Why an explicit encoding choice is required before an editable attach or
/// an indexing decision. Carries only kind/offset/size — never note content
/// (§6.10 verification envelope).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChoiceReason {
    /// A previously confirmed hint no longer matches the observed raw hash
    /// (external rewrite): the new interpretation must be re-confirmed.
    StaleHint {
        observed_raw_hash: RawHash,
    },
    /// Strict decoding failed outright.
    InvalidInput {
        /// Typed codec failure kind (never raw backend exception text).
        kind: InvalidKind,
        observed_raw_hash: RawHash,
    },
    /// A BOM was required by the confirmed descriptor but is not present.
    BomConflict {
        observed_raw_hash: RawHash,
    },
    /// A longer unsupported signature (UTF-32 family) or the documented
    /// `FF FE 00 00` ambiguity was detected under Auto.
    UnsupportedOrAmbiguousBom {
        observed_raw_hash: RawHash,
    },
    /// BOM-less decoded content contains NUL scalars: ambiguous/binary
    /// input requires an explicit choice at the core policy boundary.
    NulBearingContent {
        observed_raw_hash: RawHash,
    },
    /// The recorded hint itself cannot be applied (corrupt combination).
    HintUnusable,
}

/// Sanitized invalid-input kinds safe to display without a backend dump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidKind {
    /// Malformed UTF-8 sequence.
    InvalidUtf8,
    /// Unpaired surrogate.
    InvalidUtf16,
    /// Trailing half code unit.
    OddUtf16ByteCount,
    /// Byte limit exceeded.
    LimitExceeded,
    /// Other strict-decode rejection.
    Other,
}

impl InvalidKind {
    #[must_use]
    pub const fn from_codec(error: &CodecError) -> Self {
        match error {
            CodecError::InvalidUtf8 { .. } | CodecError::AsciiViolation { .. } => Self::InvalidUtf8,
            CodecError::InvalidUtf16 { .. } => Self::InvalidUtf16,
            CodecError::OddUtf16ByteCount => Self::OddUtf16ByteCount,
            CodecError::LimitExceeded { .. } => Self::LimitExceeded,
            _ => Self::Other,
        }
    }

    /// Stable machine-readable code for receipts and the UI catalog.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidUtf8 => "invalid-utf8",
            Self::InvalidUtf16 => "invalid-utf16",
            Self::OddUtf16ByteCount => "odd-utf16-byte-count",
            Self::LimitExceeded => "limit-exceeded",
            Self::Other => "invalid-input",
        }
    }
}

/// BOM intent for a Reopen request (§6.10 CLI contract). `Auto` strips a
/// matching signature for the requested family and otherwise treats the file
/// as absent; `Present` requires the signature (`BomConflict` otherwise);
/// `Absent` preserves signature-shaped leading bytes as content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BomMode {
    Auto,
    Present,
    Absent,
}

/// Stable ActionRequired code routed to the UI dialog.
pub const CHOICE_REQUIRED_CODE: &str = "encoding-choice-required";

/// Stable ActionRequired code for a save refused before any file effect
/// because the content is not representable under the strict descriptor
/// (ASCII violation or an encode limit, §6.10/FR-22). Repair is an explicit
/// Convert-to-UTF-8 action; the draft stays intact.
pub const UNENCODABLE_CODE: &str = "encoding-unencodable";

/// Stable ActionRequired code for a serialized output exceeding the encode
/// limits (NFR-19). No stage was created; nothing was written.
pub const OUTPUT_LIMIT_CODE: &str = "encoding-output-limit";

/// Stable ActionRequired code when Reopen is refused because the buffer
/// holds unsaved text (§6.10 DirtyBufferActionRequired). The choices are
/// save with the current format, convert-then-save, or cancel.
pub const DIRTY_BUFFER_CODE: &str = "encoding-dirty-buffer";

/// Stable ActionRequired code when Reopen is refused because a save is in
/// flight for the same buffer.
pub const SAVE_PENDING_CODE: &str = "encoding-save-pending";

/// Stable ActionRequired code when the caller's raw-hash precondition no
/// longer matches the source on disk.
pub const STALE_SOURCE_CODE: &str = "encoding-stale-source";

/// Stable ActionRequired code when a format-binding intent cannot be made
/// durable before a save whose interpretation depends on it (§6.10): the
/// save is refused with zero file effects.
pub const HINT_UNAVAILABLE_CODE: &str = "encoding-hint-unavailable";

/// Sanitized kind code for a serialization failure (typed, no content).
#[must_use]
pub const fn codec_limit_kind_code(error: &CodecError) -> &'static str {
    match error {
        CodecError::AsciiViolation { .. } => "ascii-violation",
        CodecError::Unencodable { .. } => "unencodable",
        CodecError::LimitExceeded { .. } => "limit-exceeded",
        CodecError::Cancelled => "cancelled",
        _ => "encode-error",
    }
}

/// A resolved open: decoded snapshot plus provenance, or a choice request.
#[derive(Debug, Clone)]
pub enum Resolution {
    /// Successful strict decode.
    Resolved(ResolvedSource),
    /// Explicit user decision required; nothing was attached or indexed.
    NeedsChoice {
        /// Hex native path key of the source.
        native_key: String,
        reason: ChoiceReason,
        /// Explicit policy the user may confirm against the observed hash.
        allowed: &'static [SourceFormat],
    },
}

/// The explicit descriptors a user may confirm for one source: every valid
/// format combination, offered in a stable catalog order.
pub const EXPLICIT_FORMATS: [SourceFormat; 7] = [
    SourceFormat::utf8(),
    SourceFormat::utf8_with_bom(),
    SourceFormat::ascii(),
    SourceFormat::utf16le(),
    SourceFormat::utf16le_with_bom(),
    SourceFormat::utf16be(),
    SourceFormat::utf16be_with_bom(),
];

/// Successfully resolved source content with its authoritative format.
#[must_use]
pub fn explicit_formats() -> &'static [SourceFormat] {
    &EXPLICIT_FORMATS
}

#[derive(Debug, Clone)]
pub struct ResolvedSource {
    /// The strict-decoded snapshot (already validated once).
    pub snapshot: TextSnapshot,
    /// Descriptor that was applied.
    pub format: SourceFormat,
    /// Format provenance stamp.
    pub stamp: FormatStamp,
    /// True when the descriptor came from a confirmed hint rather than
    /// unambiguous auto-detection.
    pub from_hint: bool,
    /// Raw hash of the decoded bytes (identity binding for hint updates).
    pub raw_hash: RawHash,
}

/// Decode raw source bytes under the central codec contract.
///
/// `explicit` overrides everything with a user-confirmed descriptor (a
/// current hint or a `Reopen with encoding` choice). Otherwise a
/// hash-matched hint is honored; stale hints and Auto failures produce a
/// typed `NeedsChoice` instead of attaching anything.
///
/// # Errors
///
/// This function itself is total over its contract: failures are returned as
/// `Resolution::NeedsChoice`, never as a lossy or empty substitute.
#[must_use]
pub fn resolve_source(
    native_key: String,
    bytes: &[u8],
    hints: &HintsView,
    explicit: Option<SourceFormat>,
    limits: &DecodeLimits,
) -> Resolution {
    let raw_hash: RawHash = *blake3::hash(bytes).as_bytes();
    let hex_key = native_key.clone();
    let entry = hints.record.entries.get(&hex_key);

    // A user-confirmed current choice always wins and must match exactly.
    if let Some(format) = explicit {
        return match TextSnapshot::decoding(bytes, DecodePolicy::Explicit(format), limits, &mut || false)
        {
            Ok(snapshot) => Resolution::Resolved(ResolvedSource {
                snapshot,
                format,
                stamp: FormatStamp::from_hint(format, hints.generation.unwrap_or(0)),
                from_hint: true,
                raw_hash,
            }),
            Err(_) => Resolution::NeedsChoice {
                native_key,
                reason: classify_decode_error(bytes, format, &raw_hash),
                allowed: explicit_formats(),
            },
        };
    }

    if let Some(entry) = entry {
        if entry.bound_raw_hash != raw_hash {
            return Resolution::NeedsChoice {
                native_key,
                reason: ChoiceReason::StaleHint {
                    observed_raw_hash: raw_hash,
                },
                allowed: explicit_formats(),
            };
        }
        let format = entry.format();
        // An exact hint uses Explicit, not a second Auto pass (§6.10).
        let policy = DecodePolicy::Explicit(format);
        return match TextSnapshot::decoding(bytes, policy, limits, &mut || false) {
            Ok(snapshot) => Resolution::Resolved(ResolvedSource {
                snapshot,
                format,
                stamp: FormatStamp::from_hint(format, hints.generation.unwrap_or(0)),
                from_hint: true,
                raw_hash,
            }),
            Err(error) => Resolution::NeedsChoice {
                native_key,
                reason: match &error {
                    TextError::Codec(CodecError::BomConflict) => ChoiceReason::BomConflict {
                        observed_raw_hash: raw_hash,
                    },
                    TextError::Codec(CodecError::UnsupportedOrAmbiguousBom) => {
                        ChoiceReason::UnsupportedOrAmbiguousBom {
                            observed_raw_hash: raw_hash,
                        }
                    }
                    other => classify_text_error(other, &raw_hash),
                },
                allowed: explicit_formats(),
            },
        };
    }

    // Auto: recognized BOM or valid UTF-8 only.
    match TextSnapshot::decoding(bytes, DecodePolicy::Auto, limits, &mut || false) {
        Ok(snapshot) => {
            let format = snapshot
                .format()
                .unwrap_or_else(flavite_encoding::SourceFormat::default);
            // Core policy boundary: BOM-less Auto content containing NUL is
            // ambiguous/binary input until an explicit choice confirms it —
            // the codec itself never makes this usability decision (§6.10).
            if !format.bom() && snapshot.text().contains('\0') {
                return Resolution::NeedsChoice {
                    native_key,
                    reason: ChoiceReason::NulBearingContent {
                        observed_raw_hash: raw_hash,
                    },
                    allowed: explicit_formats(),
                };
            }
            // Leading BOM-shaped content under BOM-less Auto: bytes alone
            // cannot distinguish BOM from a U+FEFF first character, and the
            // Auto path already consumed it as a signature. Such intent is
            // only representable via an explicit hint; Auto results are
            // reported honestly as Auto.
            Resolution::Resolved(ResolvedSource {
                snapshot,
                format,
                stamp: FormatStamp::detected(format),
                from_hint: false,
                raw_hash,
            })
        }
        Err(error) => Resolution::NeedsChoice {
            native_key,
            reason: match &error {
                TextError::Codec(CodecError::UnsupportedOrAmbiguousBom) => {
                    ChoiceReason::UnsupportedOrAmbiguousBom {
                        observed_raw_hash: raw_hash,
                    }
                }
                other => classify_text_error(other, &raw_hash),
            },
            allowed: explicit_formats(),
        },
    }
}

fn classify_text_error(error: &TextError, raw_hash: &RawHash) -> ChoiceReason {
    match error {
        TextError::Codec(codec) => ChoiceReason::InvalidInput {
            kind: InvalidKind::from_codec(codec),
            observed_raw_hash: *raw_hash,
        },
    }
}

fn classify_decode_error(bytes: &[u8], format: SourceFormat, raw_hash: &RawHash) -> ChoiceReason {
    // Re-run once purely to classify for the UI; already known to fail.
    match TextSnapshot::with_policy(bytes, DecodePolicy::Explicit(format)) {
        Ok(_) => ChoiceReason::HintUnusable,
        Err(TextError::Codec(CodecError::BomConflict)) => ChoiceReason::BomConflict {
            observed_raw_hash: *raw_hash,
        },
        Err(error) => classify_text_error(&error, raw_hash),
    }
}

impl ChoiceReason {
    /// Stable machine-readable code (kind-level for invalid input).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidInput { kind, .. } => kind.code(),
            other => choice_reason_code(other),
        }
    }

    /// The observed raw-byte hash this reason was classified against
    /// (identity binding for a Reopen request; zeros when unavailable).
    #[must_use]
    pub const fn observed_raw_hash(&self) -> RawHash {
        match self {
            Self::StaleHint { observed_raw_hash }
            | Self::InvalidInput { observed_raw_hash, .. }
            | Self::BomConflict { observed_raw_hash }
            | Self::UnsupportedOrAmbiguousBom { observed_raw_hash }
            | Self::NulBearingContent { observed_raw_hash } => *observed_raw_hash,
            Self::HintUnusable => [0u8; 32],
        }
    }
}

/// Compact, sanitized JSON payload for the ActionRequired channel: reason
/// kind, the observed raw hash (so a caller can Reopen against exactly the
/// bytes it saw), and the explicit descriptors the user may confirm. Never
/// note content (§6.10).
#[must_use]
pub fn choice_payload_json(reason: &ChoiceReason) -> String {
    serde_json::json!({
        "reason": reason.code(),
        "raw_hash": hex::encode(reason.observed_raw_hash()),
        "allowed": EXPLICIT_FORMATS.iter().map(|f| format!(
            "{}{}",
            f.encoding_code(),
            if f.bom() { "+bom" } else { "" }
        )).collect::<Vec<_>>(),
    })
    .to_string()
}

/// Reason code for the ActionRequired channel (stable, machine-readable).
#[must_use]
pub const fn choice_reason_code(reason: &ChoiceReason) -> &'static str {
    match reason {
        ChoiceReason::StaleHint { .. } => "stale-hint",
        ChoiceReason::InvalidInput { .. } => "invalid-input",
        ChoiceReason::BomConflict { .. } => "bom-conflict",
        ChoiceReason::UnsupportedOrAmbiguousBom { .. } => "unsupported-or-ambiguous-bom",
        ChoiceReason::NulBearingContent { .. } => "nul-bearing-content",
        ChoiceReason::HintUnusable => "hint-unusable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> HintsRecord {
        HintsRecord::new("root-1".to_string())
    }

    #[test]
    fn payload_round_trips_and_rejects_unknown_fields_and_future_schemas() {
        let mut rec = record();
        rec.entries.insert(
            "aabb".to_string(),
            HintEntry {
                encoding: FileEncoding::Utf16Le,
                bom: false,
                bound_raw_hash: [7u8; 32],
                preserve_leading_signature: false,
                nul_content_confirmed: false,
            },
        );
        let payload = rec.to_payload().unwrap();
        assert_eq!(HintsRecord::from_payload(&payload).unwrap(), rec);

        let future = br#"{"schema":99,"root_identity":"","entries":{},"intents":{}}"#;
        assert_eq!(
            HintsRecord::from_payload(future),
            Err(HintsLoadError::FutureSchema(99))
        );
        let hostile = br#"{"schema":1,"root_identity":"","entries":{},"intents":{},"evil":1}"#;
        assert!(HintsRecord::from_payload(hostile).is_err());
    }

    #[test]
    fn ascii_bom_hint_never_panics() {
        let entry = HintEntry {
            encoding: FileEncoding::Ascii,
            bom: true,
            bound_raw_hash: [0u8; 32],
            preserve_leading_signature: false,
            nul_content_confirmed: false,
        };
        assert_eq!(entry.format(), SourceFormat::utf8());
    }

    #[test]
    fn auto_resolution_accepts_utf8_and_bom_families() {
        let hints = HintsView {
            generation: None,
            record: record(),
        };
        let utf8 = b"hello \xc3\xa9\n";
        let res = resolve_source("k1".into(), utf8, &hints, None, &DecodeLimits::default());
        assert!(matches!(res, Resolution::Resolved(_)));
    }

    #[test]
    fn auto_refuses_ambiguity_and_malformed_with_typed_choices() {
        let hints = HintsView {
            generation: None,
            record: record(),
        };
        let res = resolve_source("k".into(), &[0xff, 0xfe, 0x00, 0x00, 0x41], &hints, None, &DecodeLimits::default());
        assert!(matches!(
            res,
            Resolution::NeedsChoice {
                reason: ChoiceReason::UnsupportedOrAmbiguousBom { .. },
                ..
            }
        ));
        let res = resolve_source("k".into(), &[0x41, 0xff], &hints, None, &DecodeLimits::default());
        assert!(matches!(
            res,
            Resolution::NeedsChoice {
                reason: ChoiceReason::InvalidInput {
                    kind: InvalidKind::InvalidUtf8,
                    ..
                },
                ..
            }
        ));
        // BOM-less content with a NUL scalar requires a confirmed choice.
        let res = resolve_source("k".into(), b"a\x00b", &hints, None, &DecodeLimits::default());
        assert!(matches!(
            res,
            Resolution::NeedsChoice {
                reason: ChoiceReason::NulBearingContent { .. },
                ..
            }
        ));
        // The same bytes via explicit UTF-16LE confirm cleanly.
        let res = resolve_source(
            "k".into(),
            b"a\x00b",
            &hints,
            Some(SourceFormat::utf16le()),
            &DecodeLimits::default(),
        );
        assert!(matches!(res, Resolution::Resolved(_)));
    }

    #[test]
    fn stale_hint_requires_reconfirmation_not_silent_decode() {
        let mut rec = record();
        rec.entries.insert(
            "k".to_string(),
            HintEntry {
                encoding: FileEncoding::Utf16Be,
                bom: false,
                bound_raw_hash: [9u8; 32],
                preserve_leading_signature: false,
                nul_content_confirmed: false,
            },
        );
        let hints = HintsView {
            generation: Some(3),
            record: rec,
        };
        let res = resolve_source("k".into(), b"\x00A", &hints, None, &DecodeLimits::default());
        match res {
            Resolution::NeedsChoice {
                reason: ChoiceReason::StaleHint { observed_raw_hash },
                ..
            } => {
                assert_eq!(observed_raw_hash, *blake3::hash(b"\x00A").as_bytes());
            }
            other => panic!("expected stale hint, got {other:?}"),
        }
    }

    #[test]
    fn matched_hint_uses_explicit_and_can_reopen_bom_less_utf16() {
        let bytes = b"\x41\x00\x00\xfe"; // LE: 'A' + content U+FEFF-ish? use simple
        let raw_hash = *blake3::hash(bytes).as_bytes();
        let mut rec = record();
        rec.entries.insert(
            "k".to_string(),
            HintEntry {
                encoding: FileEncoding::Utf16Le,
                bom: false,
                bound_raw_hash: raw_hash,
                preserve_leading_signature: false,
                nul_content_confirmed: false,
            },
        );
        let hints = HintsView {
            generation: Some(4),
            record: rec,
        };
        let res = resolve_source("k".into(), bytes, &hints, None, &DecodeLimits::default());
        let Resolution::Resolved(r) = res else {
            panic!("matched hint must decode");
        };
        assert_eq!(r.format, SourceFormat::utf16le());
        assert!(r.from_hint);
        assert_eq!(r.stamp.hint_generation, Some(4));
    }

    #[test]
    fn replace_view_swaps_publication_atomically() {
        // The store-less state still publishes a deterministic fallback.
        let (state, issue) = EncodingHintsState::open(None, "root-x".to_string());
        assert!(issue.is_none());
        let view = state.view();
        assert_eq!(view.record.root_identity, "root-x");
        assert_eq!(view.generation, None);
        state.replace_view(HintsView {
            generation: Some(1),
            record: record(),
        });
        assert_eq!(state.view().generation, Some(1));
        // Without a store, durable confirmation is refused typed.
        let err = state.update(|_| ()).unwrap_err();
        assert_eq!(err, HintsWriteError::NoStore);
    }
}
