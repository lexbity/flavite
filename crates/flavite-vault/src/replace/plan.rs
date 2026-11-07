//! Sealed replacement preview types and manifest schema (RFC-0006 S07,
//! D-09/§6.4). The manifest is schema 1, checksummed and rejects unknown
//! fields; every before/after payload is content-addressed by raw BLAKE3
//! and stored under the vault-local `.flavite/operations/replace/<job-id>/`
//! namespace (excluded from note indexing and watcher scans by the whole
//! `.flavite` skip).

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Preview manifest schema version (schema 1, §6.4).
pub const PREVIEW_SCHEMA_VERSION: u32 = 1;
/// Engine version of the preview/seal semantics.
pub const ENGINE_VERSION: u32 = 1;
/// D-08 corpus bound: at most this many notes are scoped in one operation.
/// Later-created notes are excluded and the UI says so.
pub const MAX_SCOPE_NOTES: usize = 50_000;
/// NFR-8/D-08: total source matches per operation (1,000,000). Exceeding it
/// aborts the preview before application — never a silent truncation.
pub const MAX_SOURCE_MATCHES: u64 = 1_000_000;
/// NFR-9: aggregate before+after payload bound per preview (2 GiB).
pub const MAX_PREVIEW_PAYLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// S07: a sealed ready preview expires after 30 minutes (NFR-14; GC and
/// abandoned-preview removal are S11 work, but expiry is enforced on access).
pub const PREVIEW_TTL: Duration = Duration::from_secs(30 * 60);
/// The private operation-store subdirectory under `.flavite` (S07).
pub const OPERATIONS_DIR: &str = ".flavite/operations";
/// The replace preview subdirectory under the operations dir.
pub const REPLACE_DIR: &str = "replace";

/// Frozen note-catalog snapshot version (D-08): enumeration is frozen in
/// one catalog snapshot ordered by native identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeVersion {
    /// Index epoch.
    pub epoch: u64,
    /// Index generation.
    pub generation: u64,
}

/// The source kind of a candidate (D-09). `Saved` candidates operate on
/// the disk bytes; `Draft` candidates are open buffers with unsaved edits,
/// explicitly opted in via `PreviewRequest::include_drafts` (S09).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// A clean saved-disk note; its disk bytes are the working source.
    Saved,
    /// An open buffer with unsaved edits; the buffer snapshot is the
    /// working source and a durable recovery checkpoint seals the effect.
    Draft,
}

/// Why a scoped note was excluded from the preview (D-09: known exclusions
/// are listed with counts and reasons).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionReason {
    /// Open buffer has unsaved edits; default-excluded until explicit
    /// draft opt-in (S09).
    DirtyBuffer,
    /// Open buffer conflicts with external changes; excluded until resolved.
    ConflictedBuffer,
    /// Open buffer's file was deleted; excluded until resolved.
    OrphanedBuffer,
    /// The saved note no longer exists on disk.
    Missing,
    /// The note could not be read within containment.
    Unreadable,
    /// The note exceeds the max-note byte bound (NFR-19/§6.5).
    Oversize,
    /// Strict decode failed or the encoding is ambiguous/undetermined.
    Undecodable,
    /// The per-note replacement output would exceed its bound.
    OutputTooLarge,
    /// The draft cannot be serialized under its own strict descriptor
    /// (e.g. a strict-ASCII draft holding non-ASCII text, §6.10).
    UnencodableDraft,
}

impl ExclusionReason {
    /// Stable, redacted user-facing label (NFR-17: no note text).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::DirtyBuffer => "unsaved draft",
            Self::ConflictedBuffer => "conflicted buffer",
            Self::OrphanedBuffer => "orphaned buffer",
            Self::Missing => "note missing",
            Self::Unreadable => "unreadable",
            Self::Oversize => "oversize",
            Self::Undecodable => "encoding undetermined or malformed",
            Self::OutputTooLarge => "output exceeds bound",
            Self::UnencodableDraft => "draft cannot be encoded under its descriptor",
        }
    }
}

/// One excluded note row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteExclusion {
    /// Portable vault-relative display path.
    pub path: String,
    /// The exclusion reason (typed, countable).
    pub reason: ExclusionReason,
}

/// A content-addressed payload reference (raw BLAKE3 hex + checked length).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedPayloadRef {
    /// BLAKE3 hex of the exact object bytes.
    pub hash: String,
    /// Object byte length (checked against the hash object).
    pub len: u64,
}

/// Open-buffer lineage recorded on a candidate when its note is open and
/// Clean at the matching disk base (D-09: later Apply revalidates it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenLineage {
    /// Buffer id (session-local; never a restart identity).
    pub buffer_id: u64,
    /// Exact revision observed at preview time.
    pub revision: u64,
}

/// Draft-candidate lineage (RFC-0006 S09/D-09/D-11): the exact buffer
/// revision, source hash and disk base captured at preview time, plus the
/// revision the replacement install produced once applied (`applied_revision`)
/// and whether a durable recovery checkpoint covers it. `applied_revision`
/// is filled by the coordinator, never by the preview; the record is
/// persisted beside the job (sidecar) and read at restart/revert time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftLineage {
    /// Buffer id (session-local; never a restart identity).
    pub buffer_id: u64,
    /// Exact revision observed at preview time.
    pub preview_revision: u64,
    /// Disk base hash (hex) at preview time — the draft's disk precondition.
    pub disk_base_hash: String,
    /// Revision immediately after the replacement install (None until Apply).
    pub applied_revision: Option<u64>,
    /// Whether a durable recovery checkpoint covers the applied revision.
    pub checkpointed: bool,
}

/// Compact source-format descriptor as persisted in the manifest. The
/// canonical `flavite_encoding::SourceFormat` is authoritative in code;
/// the manifest keeps a portable, validated projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedSourceFormat {
    /// `ascii` | `utf8` | `utf16le` | `utf16be`.
    pub encoding: String,
    /// Whether the descriptor requires a leading BOM signature.
    pub bom: bool,
}

impl PersistedSourceFormat {
    /// Project the canonical descriptor.
    #[must_use]
    pub fn from_format(format: flavite_encoding::SourceFormat) -> Self {
        Self {
            encoding: match format.encoding() {
                flavite_encoding::FileEncoding::Ascii => "ascii",
                flavite_encoding::FileEncoding::Utf8 => "utf8",
                flavite_encoding::FileEncoding::Utf16Le => "utf16le",
                flavite_encoding::FileEncoding::Utf16Be => "utf16be",
            }
            .to_owned(),
            bom: format.bom(),
        }
    }
}

/// One sealed candidate note (D-09): exact identity, disk/normalized
/// hashes, format stamp, sealed payloads and exact output hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteCandidateRecord {
    /// Stable ordinal within the plan (canonical native order).
    pub item_id: u64,
    /// Portable vault-relative display path.
    pub path: String,
    /// Exact native path bytes, hex-encoded (authoritative identity — never
    /// a display title/path).
    pub path_native: String,
    /// Source kind.
    pub source_kind: SourceKind,
    /// Raw disk bytes hash (BLAKE3 hex).
    pub disk_hash: String,
    /// Normalized decoded text hash (BLAKE3 hex).
    pub content_hash: String,
    /// Source format descriptor at preview time.
    pub format: PersistedSourceFormat,
    /// Codec version of the stamp.
    pub codec_version: u32,
    /// Applicable encoding-hint generation, when hint-confirmed.
    pub hint_generation: Option<u64>,
    /// Sealed original payload.
    pub before: SealedPayloadRef,
    /// Sealed exact serialized output payload.
    pub after: SealedPayloadRef,
    /// Exact match count in the original source.
    pub match_count: u64,
    /// Serialized output byte length.
    pub output_bytes: u64,
    /// True when before and after bytes are identical (NoChange row: not a
    /// mutation, excluded from selection totals).
    pub no_change: bool,
    /// Clean open-buffer lineage, when open at preview time.
    pub open_lineage: Option<OpenLineage>,
    /// Draft-candidate lineage; `Some` only when `source_kind` is `Draft`
    /// (S09). Older plans without it remain readable (serde default).
    #[serde(default)]
    pub draft_lineage: Option<DraftLineage>,
}

/// Aggregate preview totals (derived from per-item records on loads).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewTotals {
    /// Total scoped notes enumerated (frozen scope).
    pub scoped: u64,
    /// Notes with a candidate record (eligible, matching).
    pub notes: u64,
    /// Notes excluded with a typed reason.
    pub excluded: u64,
    /// Notes with an exact match (after NoChange filtering).
    pub matched: u64,
    /// Sum of exact source matches across candidates.
    pub matches: u64,
    /// Excluded draft (Dirty/Conflicted/Orphaned) note count.
    pub drafts: u64,
    /// Sum of serialized after-payload bytes.
    pub payload_bytes: u64,
}

/// The sealed preview plan (S07). `state` is always `"ready"` in S07:
/// Apply/Cancel/Resume/Revert/forget are S08/S11 commands and are never
/// exposed as placeholders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewManifest {
    /// Manifest schema version.
    pub schema: u32,
    /// Seal engine version.
    pub engine_version: u32,
    /// Job id (32 lowercase hex; core-issued, unguessable).
    pub job_id: String,
    /// Vault UUID hex.
    pub vault_uuid: String,
    /// Digest over options + matcher semantics + pattern + replacement
    /// (hex). Re-validated on any later read.
    pub plan_digest: String,
    /// Frozen scope version.
    pub scope_version: ScopeVersion,
    /// Source policy. `"disk_with_explicit_drafts"` is the only accepted
    /// value in S07 (draft opt-in lands with S09).
    pub source_policy: String,
    /// Sealed plan state: `"ready"` (non-applying) in S07.
    pub state: String,
    /// Derived totals.
    pub totals: PreviewTotals,
    /// Total sealed payload bytes (before+after, after dedup).
    pub payload_bytes: u64,
    /// Sealing wall-clock ms (unix).
    pub created_at_unix_ms: u64,
    /// Expiry wall-clock ms (unix).
    pub expires_at_unix_ms: u64,
    /// Selection revision produced by `replace.select`; `None` until the
    /// first explicit selection (never a capability).
    pub selection_revision: Option<u64>,
    /// Digest of the current selection (hex); `None` until selected.
    pub selection_digest: Option<String>,
    /// Whether the preview explicitly opted dirty buffers in as draft
    /// candidates (S09/D-09). Absent in older plans (serde default false).
    #[serde(default)]
    pub include_drafts: bool,
    /// Candidate records in canonical native order.
    pub notes: Vec<NoteCandidateRecord>,
    /// Typed exclusions.
    pub exclusions: Vec<NoteExclusion>,
}

impl PreviewManifest {
    /// A stable digest over the plan identity (options/scope/source policy/
    /// selection). The plan digest itself is computed by the matcher layer;
    /// this seals the plan record so an altered payload/checksum recompute
    /// cannot fake identity.
    #[must_use]
    pub fn record_digest(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"flavite-replace-preview-v1");
        hasher.update(self.job_id.as_bytes());
        hasher.update(self.plan_digest.as_bytes());
        hasher.update(&self.scope_version.epoch.to_le_bytes());
        hasher.update(&self.scope_version.generation.to_le_bytes());
        hasher.update(self.source_policy.as_bytes());
        hasher.update(&self.payload_bytes.to_le_bytes());
        hasher.update(&self.totals.matches.to_le_bytes());
        hex::encode(hasher.finalize().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_rejects_unknown_fields() {
        let manifest = PreviewManifest {
            schema: PREVIEW_SCHEMA_VERSION,
            engine_version: ENGINE_VERSION,
            job_id: "a".repeat(32),
            vault_uuid: "b".repeat(32),
            plan_digest: "c".repeat(64),
            scope_version: ScopeVersion {
                epoch: 1,
                generation: 2,
            },
            source_policy: "disk_with_explicit_drafts".to_owned(),
            state: "ready".to_owned(),
            totals: PreviewTotals::default(),
            payload_bytes: 0,
            created_at_unix_ms: 0,
            expires_at_unix_ms: 0,
            selection_revision: None,
            selection_digest: None,
            include_drafts: false,
            notes: vec![],
            exclusions: vec![],
        };
        let json = serde_json::to_string(&manifest).expect("serialize");
        let parsed: PreviewManifest = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(parsed.schema, PREVIEW_SCHEMA_VERSION);
        let mut unknown = serde_json::from_str::<serde_json::Value>(&json).expect("value");
        unknown
            .as_object_mut()
            .expect("object")
            .insert("sneaky".into(), serde_json::json!(1));
        assert!(
            serde_json::from_str::<PreviewManifest>(&unknown.to_string()).is_err(),
            "unknown manifest fields are rejected"
        );
    }

    #[test]
    fn exclusion_reasons_are_typed_labels() {
        assert_eq!(ExclusionReason::DirtyBuffer.label(), "unsaved draft");
        assert_eq!(
            ExclusionReason::Undecodable.label(),
            "encoding undetermined or malformed"
        );
    }

    #[test]
    fn record_digest_is_deterministic_and_binds_identity() {
        let base = PreviewManifest {
            schema: PREVIEW_SCHEMA_VERSION,
            engine_version: ENGINE_VERSION,
            job_id: "a".repeat(32),
            vault_uuid: "b".repeat(32),
            plan_digest: "c".repeat(64),
            scope_version: ScopeVersion {
                epoch: 1,
                generation: 2,
            },
            source_policy: "disk_with_explicit_drafts".to_owned(),
            state: "ready".to_owned(),
            totals: PreviewTotals::default(),
            payload_bytes: 0,
            created_at_unix_ms: 0,
            expires_at_unix_ms: 0,
            selection_revision: None,
            selection_digest: None,
            include_drafts: false,
            notes: vec![],
            exclusions: vec![],
        };
        assert_eq!(base.record_digest(), base.record_digest());
        let mut changed = base.clone();
        changed.payload_bytes = 7;
        assert_ne!(changed.record_digest(), base.record_digest());
    }
}
