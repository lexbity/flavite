//! Vault source Find and sealed replacement preview execution (RFC-0006
//! S07, D-08/D-09). The scan runs on bounded vault workers off the host
//! turn; no UI view calls it. Source matching is the canonical
//! `flavite-text-search` matcher (literal in-process; regex through the
//! isolated helper with the Bulk permit class — never a silent in-process
//! fallback), over decoded, LF-normalized `TextSnapshot` text. Full-text
//! index hits never provide source offsets here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use flavite_buffer::model::EditorModel;
use flavite_buffer::revision::Revision;
use flavite_buffer::state::BufferState;
use flavite_index::NoteCatalogEntry;
use flavite_syntax::text::TextSnapshot;
use flavite_text_search::matcher::{MatchError, MatchLimits, MatchMode, MatchOptions, Matcher};
use flavite_text_search::template::{ReplacementTemplate, TemplateGrammar};

use super::plan::OPERATIONS_DIR;
pub use super::plan::PreviewManifest;
use super::plan::{
    DraftLineage, ENGINE_VERSION, ExclusionReason, MAX_PREVIEW_PAYLOAD_BYTES, MAX_SCOPE_NOTES,
    MAX_SOURCE_MATCHES, NoteCandidateRecord, NoteExclusion, OpenLineage, PreviewTotals,
    ScopeVersion, SourceKind,
};
use super::store::{OpStoreIo, ReplaceStore, StoreError};

/// Source-find spool subdirectory under `.flavite/operations`.
const SOURCE_FIND_DIR: &str = "source-find";
/// Catalog page size for frozen scope enumeration.
const CATALOG_PAGE: usize = 500;
/// Cancellation check granularity (notes per flag probe).
const CANCEL_CHECK_INTERVAL: usize = 16;

/// One vault source-find request (S07).
#[derive(Debug, Clone)]
pub struct SourceFindRequest {
    pub folder: Option<PathBuf>,
    pub include_drafts: bool,
    /// Literal or regex query. Empty literal is Idle (refused).
    pub query: String,
    /// Match mode (D-05).
    pub mode: MatchMode,
    pub case_sensitive: bool,
    pub whole_word: bool,
}

/// One replacement-preview request (S07/D-09). Draft inclusion is absent
/// until S09; `include_drafts` must remain false in S07.
#[derive(Debug, Clone)]
pub struct PreviewRequest {
    pub folder: Option<PathBuf>,
    pub query: String,
    pub replacement: String,
    pub mode: MatchMode,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub include_drafts: bool,
}

/// Typed whole-operation failure (never a partial "complete" claim).
#[derive(Debug, Clone)]
pub enum PreviewError {
    /// Empty literal query is Idle (D-05).
    EmptyQuery,
    /// Pattern/template fault before any scan (redacted reason).
    InvalidInput(&'static str),
    /// The matcher refused the options/pattern.
    Matcher(MatchError),
    /// The replacement template was refused before any edit exists.
    Template(flavite_text_search::template::TemplateError),
    /// The frozen catalog scope changed while enumerating (retryable).
    StaleScope {
        expected: ScopeVersion,
        actual: ScopeVersion,
    },
    /// A hard budget was exceeded; the whole preview is non-applicable.
    LimitExceeded {
        kind: &'static str,
        observed: u64,
        limit: u64,
    },
    /// Regex requires the isolated helper (D-14) and it is unavailable.
    RegexUnavailable { reason: String },
    /// The store/seal persistence failed.
    Store(StoreError),
    /// Worker I/O failed.
    Io(String),
    /// The caller's cancellation flag fired.
    Cancelled,
}

impl PreviewError {
    /// Stable, redacted user-facing message (NFR-17).
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::EmptyQuery => crate::text_edit::IDLE_QUERY.to_owned(),
            Self::InvalidInput(message) => (*message).to_owned(),
            Self::Matcher(MatchError::EmptyQuery) => crate::text_edit::IDLE_QUERY.to_owned(),
            Self::Matcher(MatchError::PatternTooLong { .. }) => {
                "pattern exceeds the 8 KiB bound".to_owned()
            }
            Self::Matcher(MatchError::Regex(_)) => "invalid regular expression".to_owned(),
            Self::Matcher(MatchError::RegexTooManyGroups { .. }) => {
                "pattern exceeds the capture-group bound".to_owned()
            }
            Self::Matcher(MatchError::LimitExceeded { .. }) => {
                "too many matches for one operation".to_owned()
            }
            Self::Matcher(_) => "the find pattern is invalid".to_owned(),
            Self::Template(flavite_text_search::template::TemplateError::TooLong { .. }) => {
                "replacement exceeds the 64 KiB bound".to_owned()
            }
            Self::Template(flavite_text_search::template::TemplateError::DanglingDollar {
                ..
            }) => "replacement template has a dangling '$'".to_owned(),
            Self::Template(flavite_text_search::template::TemplateError::MalformedBraces {
                ..
            }) => "replacement template has malformed braces".to_owned(),
            Self::Template(flavite_text_search::template::TemplateError::UnknownGroup {
                ..
            }) => "replacement references an unknown capture group".to_owned(),
            Self::Template(_) => "invalid replacement template".to_owned(),
            Self::StaleScope { .. } => {
                "the vault index changed while scanning; retry the source find".to_owned()
            }
            Self::LimitExceeded {
                kind,
                observed,
                limit,
            } => format!("{kind} {observed} exceeds the {limit} bound"),
            Self::RegexUnavailable { reason } => {
                format!("regex is unavailable ({reason}); literal matching still works")
            }
            Self::Store(error) => error.to_string(),
            Self::Io(message) => format!("vault operation I/O failed: {message}"),
            Self::Cancelled => "the source scan was cancelled".to_owned(),
        }
    }
}

impl From<StoreError> for PreviewError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

/// One frozen-scope enumeration result.
struct ScopeScan {
    scope: ScopeVersion,
    entries: Vec<NoteCatalogEntry>,
    /// Enumeration stopped at the note bound while more entries existed.
    truncated: bool,
}

/// Everything the vault scan needs from the host (RFC-0006 S07): bounded
/// actor reads, the buffer manager for open-state exclusions, the vault
/// root and identity. This keeps the scan independent of the Session
/// driving-turn object while remaining a first-class vault worker surface.
#[derive(Clone)]
pub struct PreviewCtx {
    /// Bound actor handle (contained reads/catalog/op-store).
    pub actor: crate::actor::VaultActorHandle,
    /// Buffer manager (open-state classification).
    pub manager: std::sync::Arc<std::sync::Mutex<flavite_buffer::manager::BufferManager>>,
    /// Vault root.
    pub root: PathBuf,
    /// Vault UUID.
    pub vault_uuid: uuid::Uuid,
}

impl PreviewCtx {
    /// Build a context from a live Session (workers and tests).
    #[must_use]
    pub fn from_session(session: &crate::session::Session) -> Self {
        session.preview_ctx()
    }
}

/// Frozen note-catalog enumeration (D-08): one snapshot, ordered by native
/// identity; later-created notes are excluded and marked truncated.
fn enumerate_scope(
    ctx: &PreviewCtx,
    cancelled: &AtomicBool,
    folder: Option<&Path>,
) -> Result<ScopeScan, PreviewError> {
    if folder.is_some_and(|path| {
        path.components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
            || path
                .components()
                .next()
                .is_some_and(|c| c.as_os_str() == ".flavite")
    }) {
        return Err(PreviewError::InvalidInput(
            "folder must be inside the vault",
        ));
    }
    let mut entries = Vec::new();
    let mut after = None;
    let mut scope: Option<ScopeVersion> = None;
    let mut truncated = false;
    loop {
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(PreviewError::Cancelled);
        }
        let page = ctx
            .actor
            .note_catalog(CATALOG_PAGE, after.clone())
            .map_err(PreviewError::Io)?;
        let page_scope = ScopeVersion {
            epoch: page.epoch,
            generation: page.generation,
        };
        match scope {
            None => scope = Some(page_scope),
            Some(expected) if expected != page_scope => {
                return Err(PreviewError::StaleScope {
                    expected,
                    actual: page_scope,
                });
            }
            _ => {}
        }
        for entry in page.items {
            if folder.is_some_and(|folder| !Path::new(&entry.path).starts_with(folder)) {
                continue;
            }
            if entries.len() >= MAX_SCOPE_NOTES {
                truncated = true;
                break;
            }
            entries.push(entry);
        }
        if truncated {
            break;
        }
        let Some(next) = page.next else {
            break;
        };
        after = Some(next);
    }
    Ok(ScopeScan {
        scope: scope.unwrap_or(ScopeVersion {
            epoch: 0,
            generation: 0,
        }),
        entries,
        truncated,
    })
}

/// Open-buffer states keyed by vault-relative path (for default exclusion
/// and clean-lineage records).
fn open_buffer_map(ctx: &PreviewCtx) -> HashMap<PathBuf, (u64, Revision, BufferState)> {
    let mut map = HashMap::new();
    let manager = ctx
        .manager
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for buffer in manager.all_buffers() {
        let Some(path) = manager.path_for_id(buffer.buffer_id()).cloned() else {
            continue;
        };
        let relative = path.strip_prefix(&ctx.root).unwrap_or(&path).to_path_buf();
        map.insert(
            relative,
            (
                buffer.buffer_id().as_u64(),
                buffer.revision(),
                buffer.state(),
            ),
        );
    }
    map
}

/// A successfully opened eligible note.
struct NoteRead {
    /// Raw payload bytes (disk bytes, or the serialized current draft).
    bytes: Vec<u8>,
    /// Decoded snapshot.
    snapshot: TextSnapshot,
    /// Open buffer lineage when the note was open and Clean.
    lineage: Option<OpenLineage>,
    /// Draft-candidate lineage when the note was an opted-in Dirty buffer.
    draft: Option<DraftLineage>,
}

/// Read one scoped note under the max-note bound, classifying skips as
/// typed exclusions. With `include_drafts` (S09/D-09), an open Dirty
/// buffer becomes a draft candidate whose payload is its serialized
/// current content (under its own descriptor), and whose disk base is
/// recorded for later guards; Conflicted/Orphaned buffers remain excluded.
fn read_note(
    ctx: &PreviewCtx,
    entry: &NoteCatalogEntry,
    open: &HashMap<PathBuf, (u64, Revision, BufferState)>,
    include_drafts: bool,
) -> Result<NoteRead, NoteExclusion> {
    let relative = PathBuf::from(&entry.path);
    let full = ctx.root.join(&relative);
    if include_drafts && let Some((buffer_id, revision, BufferState::Dirty)) = open.get(&relative) {
        let manager = ctx
            .manager
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(buffer) =
            manager.get_buffer_by_id(flavite_buffer::revision::BufferId::from(*buffer_id))
        else {
            return Err(NoteExclusion {
                path: entry.path.clone(),
                reason: ExclusionReason::DirtyBuffer,
            });
        };
        if buffer.state() != BufferState::Dirty {
            // The buffer moved on between enumeration and read; fall
            // through to the ordinary disk classification.
        } else {
            let bytes = match buffer.model().try_to_source_bytes() {
                Ok(bytes) => bytes,
                Err(flavite_encoding::CodecError::Unencodable { .. }) => {
                    return Err(NoteExclusion {
                        path: entry.path.clone(),
                        reason: ExclusionReason::UnencodableDraft,
                    });
                }
                Err(_) => {
                    return Err(NoteExclusion {
                        path: entry.path.clone(),
                        reason: ExclusionReason::OutputTooLarge,
                    });
                }
            };
            let snapshot = TextSnapshot::new(&bytes).map_err(|_| NoteExclusion {
                path: entry.path.clone(),
                reason: ExclusionReason::Undecodable,
            })?;
            let disk_base_hash = buffer
                .base_hash()
                .map(|hash| hex::encode(hash.as_bytes()))
                .unwrap_or_default();
            return Ok(NoteRead {
                bytes,
                snapshot,
                lineage: None,
                draft: Some(DraftLineage {
                    buffer_id: *buffer_id,
                    preview_revision: revision.as_u64(),
                    disk_base_hash,
                    applied_revision: None,
                    checkpointed: false,
                }),
            });
        }
    }
    let lineage = match open.get(&relative) {
        Some((buffer_id, revision, BufferState::Clean)) => Some(OpenLineage {
            buffer_id: *buffer_id,
            revision: revision.as_u64(),
        }),
        Some((_, _, state)) => {
            return Err(NoteExclusion {
                path: entry.path.clone(),
                reason: match state {
                    BufferState::Dirty => ExclusionReason::DirtyBuffer,
                    BufferState::Conflicted => ExclusionReason::ConflictedBuffer,
                    BufferState::Orphaned => ExclusionReason::OrphanedBuffer,
                    BufferState::Clean => unreachable!("clean handled above"),
                },
            });
        }
        None => None,
    };
    let bytes = match ctx
        .actor
        .read_bounded(full.clone(), flavite_index::MAX_NOTE_BYTES as u64)
    {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return Err(NoteExclusion {
                path: entry.path.clone(),
                reason: ExclusionReason::Missing,
            });
        }
        Err(_) => {
            // Distinguish oversize (bounded read refused) from other
            // unreadable sources without unbounded reads on the happy path.
            match ctx.actor.read_file(full) {
                Ok(Some(bytes)) if bytes.len() > flavite_index::MAX_NOTE_BYTES => {
                    return Err(NoteExclusion {
                        path: entry.path.clone(),
                        reason: ExclusionReason::Oversize,
                    });
                }
                Ok(None) => {
                    return Err(NoteExclusion {
                        path: entry.path.clone(),
                        reason: ExclusionReason::Missing,
                    });
                }
                _ => {
                    return Err(NoteExclusion {
                        path: entry.path.clone(),
                        reason: ExclusionReason::Unreadable,
                    });
                }
            }
        }
    };
    let snapshot = TextSnapshot::new(&bytes).map_err(|_| NoteExclusion {
        path: entry.path.clone(),
        reason: ExclusionReason::Undecodable,
    })?;
    Ok(NoteRead {
        bytes,
        snapshot,
        lineage,
        draft: None,
    })
}

/// Compile the canonical matcher (pattern + options + limits). Regex
/// compilation is local validation only; execution still runs in the
/// helper.
fn matcher_for(request: &SourceFindRequest, max_matches: usize) -> Result<Matcher, PreviewError> {
    if request.query.is_empty() && request.mode == MatchMode::Literal {
        return Err(PreviewError::EmptyQuery);
    }
    Matcher::new(
        &request.query,
        MatchOptions {
            mode: request.mode,
            case_sensitive: request.case_sensitive,
            whole_word: request.whole_word,
        },
        MatchLimits { max_matches },
    )
    .map_err(PreviewError::Matcher)
}

/// Helper request common fields.
fn helper_request<'a>(
    request: &'a SourceFindRequest,
    template_raw: &'a str,
    max_matches: usize,
) -> crate::text_worker::HelperRequest<'a> {
    crate::text_worker::HelperRequest {
        kind: if template_raw.is_empty() {
            flavite_text_search::protocol::RequestKind::Find
        } else {
            flavite_text_search::protocol::RequestKind::Replace
        },
        source: "",
        pattern: &request.query,
        mode: request.mode,
        case_sensitive: request.case_sensitive,
        whole_word: request.whole_word,
        scope: None,
        max_matches,
        report_matches: usize::MAX,
        skip_matches: 0,
        max_output: crate::text_edit::MAX_OUTPUT_BYTES,
        template_raw,
    }
}

/// Run one helper regex operation (Bulk class) with the given source.
fn run_helper(
    request: &SourceFindRequest,
    template_raw: &str,
    source: &str,
    max_matches: usize,
    cancelled: &AtomicBool,
) -> Result<crate::text_worker::HelperRun, PreviewError> {
    let mut req = helper_request(request, template_raw, max_matches);
    req.source = source;
    crate::text_worker::run_match_bulk(&req, cancelled).map_err(preview_worker_error)
}

/// Validate helper-reported spans against the parent's own text.
fn validate_helper_spans(
    run: &crate::text_worker::HelperRun,
    text: &str,
) -> Result<Vec<flavite_syntax::Span<flavite_syntax::text::TextByte>>, PreviewError> {
    run.matches
        .iter()
        .map(|hit| {
            let start =
                usize::try_from(hit.start).map_err(|_| PreviewError::InvalidInput("scope"))?;
            let end = usize::try_from(hit.end).map_err(|_| PreviewError::InvalidInput("scope"))?;
            if end < start
                || end > text.len()
                || !text.is_char_boundary(start)
                || !text.is_char_boundary(end)
            {
                return Err(PreviewError::InvalidInput("helper span"));
            }
            Ok(flavite_syntax::Span::new(
                flavite_syntax::text::TextByte(start),
                flavite_syntax::text::TextByte(end),
            ))
        })
        .collect()
}

/// Write one note's match spans to its spool file; returns the exact match
/// count. Literal matches stream through the canonical iterator; regex
/// matches arrive from the helper and are validated before writing. The
/// payload is bounded by the per-operation match budget (≤ 8 MiB at 1m
/// matches), so one exclusive write per note is bounded.
fn write_note_spans(
    path: &Path,
    io: &dyn OpStoreIo,
    matcher: &Matcher,
    request: &SourceFindRequest,
    text: &str,
    max_matches: usize,
    cancelled: &AtomicBool,
) -> Result<u64, PreviewError> {
    let mut count = 0u64;
    let mut payload = Vec::with_capacity(8 + 65536 * 8);
    payload.extend_from_slice(&0u64.to_le_bytes()); // count placeholder
    let mut push = |start: u64, end: u64, count: &mut u64| -> Result<(), PreviewError> {
        *count = count.saturating_add(1);
        if *count > max_matches as u64 {
            return Err(PreviewError::LimitExceeded {
                kind: "source matches",
                observed: *count,
                limit: max_matches as u64,
            });
        }
        payload.extend_from_slice(&start.to_le_bytes());
        payload.extend_from_slice(&end.to_le_bytes());
        Ok(())
    };
    if request.mode == MatchMode::Regex {
        let run = run_helper(request, "", text, max_matches, cancelled)?;
        if run.total_matches != u64::try_from(run.matches.len()).unwrap_or(u64::MAX) {
            return Err(PreviewError::RegexUnavailable {
                reason: "unsound-helper-proof".to_owned(),
            });
        }
        let spans = validate_helper_spans(&run, text)?;
        for span in spans {
            push(span.start.0 as u64, span.end.0 as u64, &mut count)?;
        }
    } else {
        for item in matcher
            .iter_matches(text, None)
            .map_err(PreviewError::Matcher)?
        {
            let hit = item.map_err(PreviewError::Matcher)?;
            push(hit.span.start.0 as u64, hit.span.end.0 as u64, &mut count)?;
        }
    }
    if count == 0 {
        return Ok(0);
    }
    payload[..8].copy_from_slice(&count.to_le_bytes());
    io.write_new(path, &payload).map_err(PreviewError::Io)?;
    Ok(count)
}

/// One note's find result metadata (spans live in a spool file).
#[derive(Debug, Clone)]
pub struct FindNoteRecord {
    /// Overall note ordinal (ascending native order).
    pub ordinal: u64,
    /// Portable vault-relative path.
    pub path: String,
    /// Indexed title.
    pub title: String,
    /// Exact match count for this note.
    pub match_count: u64,
    /// Spool file name for this note's spans (within the finder's spans dir).
    pub spans_file: String,
    pub content_hash: String,
    pub source_kind: SourceKind,
}

/// Complete vault source-find outcome.
#[derive(Debug, Clone)]
pub struct SourceFindOutcome {
    /// Frozen scope snapshot.
    pub scope: ScopeVersion,
    /// True when enumeration stopped at the note bound with more notes.
    pub truncated_scope: bool,
    /// Per-note records in native order.
    pub notes: Vec<FindNoteRecord>,
    /// Typed exclusions.
    pub exclusions: Vec<NoteExclusion>,
    /// Total scoped notes.
    pub total_notes: u64,
    /// Exact total matches across all notes.
    pub total_matches: u64,
}

/// The source-find spool base directory.
#[must_use]
pub fn source_find_base(vault_root: &Path) -> PathBuf {
    vault_root.join(OPERATIONS_DIR).join(SOURCE_FIND_DIR)
}

/// Run a vault source find: frozen enumeration, per-note guarded reads,
/// canonical matching, span spooling, typed exclusions. Read-only — no
/// payloads, no manifests, no mutation.
pub fn run_find(
    ctx: &PreviewCtx,
    request: &SourceFindRequest,
    finder_id: &str,
    io: &dyn OpStoreIo,
    cancelled: &AtomicBool,
) -> Result<SourceFindOutcome, PreviewError> {
    let matcher = matcher_for(request, MAX_SOURCE_MATCHES as usize)?;
    let scan = enumerate_scope(ctx, cancelled, request.folder.as_deref())?;
    let open = open_buffer_map(ctx);
    let spans_dir = source_find_base(&ctx.root).join(finder_id).join("spans");
    io.mkdir_all(&spans_dir).map_err(PreviewError::Io)?;
    let mut notes = Vec::new();
    let mut exclusions = Vec::new();
    let mut total_matches = 0u64;
    let mut retained_source_bytes = 0usize;
    for (index, entry) in scan.entries.iter().enumerate() {
        if index % CANCEL_CHECK_INTERVAL == 0
            && cancelled.load(std::sync::atomic::Ordering::Relaxed)
        {
            return Err(PreviewError::Cancelled);
        }
        let read = match read_note(ctx, entry, &open, request.include_drafts) {
            Ok(read) => read,
            Err(exclusion) => {
                exclusions.push(exclusion);
                continue;
            }
        };
        let file = format!("{index:08}.bin");
        let count = write_note_spans(
            &spans_dir.join(&file),
            io,
            &matcher,
            request,
            read.snapshot.text(),
            MAX_SOURCE_MATCHES as usize,
            cancelled,
        )?;
        total_matches = total_matches.saturating_add(count);
        if total_matches > MAX_SOURCE_MATCHES {
            return Err(PreviewError::LimitExceeded {
                kind: "source matches",
                observed: total_matches,
                limit: MAX_SOURCE_MATCHES,
            });
        }
        if count == 0 {
            io.remove(&spans_dir.join(&file))
                .map_err(PreviewError::Io)?;
            continue;
        }
        retained_source_bytes = retained_source_bytes.saturating_add(read.snapshot.text().len());
        if retained_source_bytes > 128 * 1024 * 1024 {
            return Err(PreviewError::LimitExceeded {
                kind: "find source bytes",
                observed: retained_source_bytes as u64,
                limit: 128 * 1024 * 1024,
            });
        }
        io.write_new(
            &spans_dir.join(format!("{file}.text")),
            read.snapshot.text().as_bytes(),
        )
        .map_err(PreviewError::Io)?;
        notes.push(FindNoteRecord {
            ordinal: u64::try_from(index).unwrap_or(u64::MAX),
            path: entry.path.clone(),
            title: entry.title.clone(),
            match_count: count,
            spans_file: file,
            content_hash: blake3::hash(read.snapshot.text().as_bytes())
                .to_hex()
                .to_string(),
            source_kind: if read.draft.is_some() {
                SourceKind::Draft
            } else {
                SourceKind::Saved
            },
        });
    }
    Ok(SourceFindOutcome {
        scope: scan.scope,
        truncated_scope: scan.truncated,
        notes,
        exclusions,
        total_notes: u64::try_from(scan.entries.len()).unwrap_or(u64::MAX),
        total_matches,
    })
}

/// Compute the exact edit ops for one note (canonical matcher semantics;
/// regex expansions come from the helper), returning the ops, exact match
/// count and per-note output bytes.
fn compute_note_ops(
    matcher: &Matcher,
    template: &ReplacementTemplate,
    request: &PreviewRequest,
    text: &str,
    max_matches: usize,
    cancelled: &AtomicBool,
) -> Result<(Vec<flavite_buffer::undo::EditOp>, u64), PreviewError> {
    if request.mode == MatchMode::Regex {
        let run = run_helper(
            &SourceFindRequest {
                folder: request.folder.clone(),
                include_drafts: request.include_drafts,
                query: request.query.clone(),
                mode: request.mode,
                case_sensitive: request.case_sensitive,
                whole_word: request.whole_word,
            },
            &request.replacement,
            text,
            max_matches,
            cancelled,
        )?;
        if run.total_matches != u64::try_from(run.matches.len()).unwrap_or(u64::MAX) {
            return Err(PreviewError::RegexUnavailable {
                reason: "unsound-helper-proof".to_owned(),
            });
        }
        let spans = validate_helper_spans(&run, text)?;
        if run.output_len > crate::text_edit::MAX_OUTPUT_BYTES as u64 {
            return Err(PreviewError::InvalidInput("output bound"));
        }
        let count = u64::try_from(spans.len()).unwrap_or(u64::MAX);
        let ops = spans
            .into_iter()
            .zip(run.matches.iter())
            .map(|(span, hit)| {
                let expansion = String::from_utf8(hit.expansion.clone())
                    .map_err(|_| PreviewError::InvalidInput("expansion was not UTF-8"))?;
                Ok(flavite_buffer::undo::EditOp::new(span, expansion))
            })
            .collect::<Result<Vec<_>, PreviewError>>()?;
        Ok((ops, count))
    } else {
        let hits = matcher.find_in(text, None).map_err(PreviewError::Matcher)?;
        if hits.len() >= max_matches {
            return Err(PreviewError::LimitExceeded {
                kind: "source matches",
                observed: hits.len() as u64 + 1,
                limit: max_matches as u64,
            });
        }
        let count = u64::try_from(hits.len()).unwrap_or(u64::MAX);
        let replacement = template.raw().to_owned();
        let ops = hits
            .into_iter()
            .map(|hit| flavite_buffer::undo::EditOp::new(hit.span, replacement.clone()))
            .collect();
        Ok((ops, count))
    }
}

/// Release only the generated files of a source-finder spool. Directory
/// removal is not part of the filesystem's file-remove contract.
pub(crate) fn cleanup_find(root: &Path, finder_id: &str, io: &dyn OpStoreIo) {
    let directory = source_find_base(root).join(finder_id).join("spans");
    if let Ok(Some(names)) = io.list_dir(&directory) {
        for name in names {
            let number = name
                .strip_suffix(".bin.text")
                .or_else(|| name.strip_suffix(".bin"));
            if number.is_some_and(|number| {
                number.len() == 8 && number.bytes().all(|b| b.is_ascii_digit())
            }) {
                let _ = io.remove(&directory.join(name));
            }
        }
    }
}

/// Run a vault replacement preview: full scan, exact output computation and
/// a sealed manifest with content-addressed before/after payloads. The
/// preview never applies anything; a failure leaves inspection explicitly
/// incomplete and Apply unavailable.
pub fn run_preview(
    ctx: &PreviewCtx,
    request: &PreviewRequest,
    job_id: &str,
    store: &ReplaceStore,
    io: &dyn OpStoreIo,
    cancelled: &AtomicBool,
) -> Result<PreviewManifest, PreviewError> {
    let find_request = SourceFindRequest {
        folder: request.folder.clone(),
        include_drafts: request.include_drafts,
        query: request.query.clone(),
        mode: request.mode,
        case_sensitive: request.case_sensitive,
        whole_word: request.whole_word,
    };
    let matcher = matcher_for(&find_request, MAX_SOURCE_MATCHES as usize)?;
    let template = ReplacementTemplate::new(
        &request.replacement,
        match request.mode {
            MatchMode::Literal => TemplateGrammar::Literal,
            MatchMode::Regex => TemplateGrammar::Dollar,
        },
    )
    .map_err(|error| match error {
        flavite_text_search::template::TemplateError::TooLong { .. } => {
            PreviewError::InvalidInput("replacement exceeds the 64 KiB bound")
        }
        other => PreviewError::Template(other),
    })?;
    // Regex probe: fail the whole operation typed before scanning any note
    // when the isolated helper is unusable (D-14).
    if request.mode == MatchMode::Regex {
        run_helper(
            &find_request,
            &request.replacement,
            "",
            MAX_SOURCE_MATCHES as usize,
            cancelled,
        )?;
    }
    let scan = enumerate_scope(ctx, cancelled, request.folder.as_deref())?;
    let open = open_buffer_map(ctx);
    let mut notes = Vec::new();
    let mut exclusions = Vec::new();
    let mut total_matches = 0u64;
    let mut payload_bytes = 0u64;
    let mut drafts = 0u64;
    for (index, entry) in scan.entries.iter().enumerate() {
        if index % CANCEL_CHECK_INTERVAL == 0
            && cancelled.load(std::sync::atomic::Ordering::Relaxed)
        {
            return Err(PreviewError::Cancelled);
        }
        let read = match read_note(ctx, entry, &open, request.include_drafts) {
            Ok(read) => read,
            Err(exclusion) => {
                if matches!(
                    exclusion.reason,
                    ExclusionReason::DirtyBuffer
                        | ExclusionReason::ConflictedBuffer
                        | ExclusionReason::OrphanedBuffer
                        | ExclusionReason::UnencodableDraft
                ) {
                    drafts = drafts.saturating_add(1);
                }
                exclusions.push(exclusion);
                continue;
            }
        };
        let (ops, count) = compute_note_ops(
            &matcher,
            &template,
            request,
            read.snapshot.text(),
            MAX_SOURCE_MATCHES as usize,
            cancelled,
        )?;
        total_matches = total_matches.saturating_add(count);
        if total_matches > MAX_SOURCE_MATCHES {
            return Err(PreviewError::LimitExceeded {
                kind: "source matches",
                observed: total_matches,
                limit: MAX_SOURCE_MATCHES,
            });
        }
        if ops.is_empty() {
            continue;
        }
        // Build the exact serialized output through the canonical buffer
        // model: same edits, EOL splices and descriptor the real editor
        // would produce; output bound check included.
        let after_bytes = match compute_after_bytes(&read.snapshot, &ops) {
            Ok(bytes) => bytes,
            Err(reason) => {
                exclusions.push(NoteExclusion {
                    path: entry.path.clone(),
                    reason,
                });
                continue;
            }
        };
        let no_change = after_bytes == read.bytes;
        let before_ref = store.write_payload(io, job_id, &read.bytes)?;
        let after_ref = store.write_payload(io, job_id, &after_bytes)?;
        payload_bytes = payload_bytes
            .saturating_add(u64::try_from(read.bytes.len()).unwrap_or(u64::MAX))
            .saturating_add(u64::try_from(after_bytes.len()).unwrap_or(u64::MAX));
        if payload_bytes > MAX_PREVIEW_PAYLOAD_BYTES {
            return Err(PreviewError::LimitExceeded {
                kind: "preview payload bytes",
                observed: payload_bytes,
                limit: MAX_PREVIEW_PAYLOAD_BYTES,
            });
        }
        let format = read.snapshot.format();
        let format_stamp = format.map(flavite_encoding::FormatStamp::detected);
        notes.push(NoteCandidateRecord {
            item_id: u64::try_from(index).unwrap_or(u64::MAX),
            path: entry.path.clone(),
            path_native: hex::encode(&entry.path_native),
            source_kind: if read.draft.is_some() {
                SourceKind::Draft
            } else {
                SourceKind::Saved
            },
            disk_hash: if read.draft.is_some() {
                // The disk base hash was recorded on the draft lineage; the
                // `before` payload for a draft is its serialized current
                // content, not the disk file.
                read.draft
                    .as_ref()
                    .map(|lineage| lineage.disk_base_hash.clone())
                    .unwrap_or_default()
            } else {
                hex::encode(blake3::hash(&read.bytes).as_bytes())
            },
            content_hash: hex::encode(blake3::hash(read.snapshot.text().as_bytes()).as_bytes()),
            format: super::plan::PersistedSourceFormat::from_format(
                format_stamp
                    .map(|s| s.format)
                    .unwrap_or(flavite_encoding::SourceFormat::utf8()),
            ),
            codec_version: format_stamp
                .map(|s| s.codec_version)
                .unwrap_or(flavite_encoding::CODEC_VERSION),
            hint_generation: format_stamp.and_then(|s| s.hint_generation),
            before: before_ref,
            after: after_ref,
            match_count: count,
            output_bytes: u64::try_from(after_bytes.len()).unwrap_or(u64::MAX),
            no_change,
            open_lineage: read.lineage,
            draft_lineage: read.draft,
        });
    }
    let matched = notes.iter().filter(|n| !n.no_change).count() as u64;
    let now_ms = unix_ms_now();
    let mut digest = blake3::Hasher::new();
    digest.update(&matcher.digest().to_le_bytes());
    digest.update(&template.digest().to_le_bytes());
    digest.update(&[u8::from(request.include_drafts)]);
    if let Some(folder) = &request.folder {
        digest.update(folder.as_os_str().as_encoded_bytes());
    }
    let manifest = PreviewManifest {
        schema: super::plan::PREVIEW_SCHEMA_VERSION,
        engine_version: ENGINE_VERSION,
        job_id: job_id.to_owned(),
        vault_uuid: ctx.vault_uuid.to_string(),
        plan_digest: digest.finalize().to_hex().to_string(),
        scope_version: scan.scope,
        source_policy: "disk_with_explicit_drafts".to_owned(),
        state: "ready".to_owned(),
        totals: PreviewTotals {
            scoped: u64::try_from(scan.entries.len()).unwrap_or(u64::MAX),
            notes: u64::try_from(notes.len()).unwrap_or(u64::MAX),
            excluded: u64::try_from(exclusions.len()).unwrap_or(u64::MAX),
            matched,
            matches: total_matches,
            drafts,
            payload_bytes,
        },
        payload_bytes,
        created_at_unix_ms: now_ms,
        expires_at_unix_ms: now_ms.saturating_add(super::plan::PREVIEW_TTL.as_secs() * 1000),
        selection_revision: None,
        selection_digest: None,
        include_drafts: request.include_drafts,
        notes,
        exclusions,
    };
    let digest = manifest.record_digest();
    let _ = digest;
    store.seal_preview(io, &manifest)?;
    Ok(manifest)
}

/// Wall-clock unix milliseconds (conservative expiry anchoring; monotonic
/// in-process time is used for operation deadlines).
pub(crate) fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Compute the exact serialized output bytes for one note through the
/// canonical buffer model (same edits, EOL splices and descriptor the real
/// editor would produce; output bound check included).
fn compute_after_bytes(
    snapshot: &TextSnapshot,
    ops: &[flavite_buffer::undo::EditOp],
) -> Result<Vec<u8>, ExclusionReason> {
    let mut model = EditorModel::from_snapshot(snapshot.clone());
    model.apply_edit_ops(ops);
    model
        .try_to_source_bytes()
        .map_err(|_| ExclusionReason::OutputTooLarge)
}

fn preview_worker_error(error: crate::text_worker::TextWorkerError) -> PreviewError {
    use crate::text_worker::TextWorkerError as Worker;
    match error {
        Worker::Failure(flavite_text_search::protocol::FailureCode::Pattern) => {
            PreviewError::InvalidInput("invalid regular expression")
        }
        Worker::Failure(flavite_text_search::protocol::FailureCode::Template) => {
            PreviewError::InvalidInput("invalid replacement template")
        }
        Worker::Failure(flavite_text_search::protocol::FailureCode::Limit) => {
            PreviewError::LimitExceeded {
                kind: "source matches",
                observed: MAX_SOURCE_MATCHES + 1,
                limit: MAX_SOURCE_MATCHES,
            }
        }
        Worker::Failure(flavite_text_search::protocol::FailureCode::Source) => {
            PreviewError::InvalidInput("source")
        }
        Worker::Unavailable { reason } => PreviewError::RegexUnavailable {
            reason: reason.to_owned(),
        },
        Worker::Busy => PreviewError::RegexUnavailable {
            reason: "busy".to_owned(),
        },
        Worker::HelperDied => PreviewError::RegexUnavailable {
            reason: "helper-died".to_owned(),
        },
        Worker::Deadline => PreviewError::RegexUnavailable {
            reason: "deadline".to_owned(),
        },
        Worker::Cancelled => PreviewError::Cancelled,
        Worker::Frame(_) => PreviewError::RegexUnavailable {
            reason: "frame".to_owned(),
        },
        Worker::Failure(code) => PreviewError::RegexUnavailable {
            reason: code.code().to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_regex_pattern_is_a_typed_matcher_fault() {
        let request = SourceFindRequest {
            folder: None,
            include_drafts: false,
            query: "(".to_owned(),
            mode: MatchMode::Regex,
            case_sensitive: false,
            whole_word: false,
        };
        let err = matcher_for(&request, 10).expect_err("invalid regex");
        assert!(matches!(err, PreviewError::Matcher(MatchError::Regex(_))));
    }

    #[test]
    fn empty_literal_is_idle() {
        let request = SourceFindRequest {
            folder: None,
            include_drafts: false,
            query: String::new(),
            mode: MatchMode::Literal,
            case_sensitive: false,
            whole_word: false,
        };
        let err = matcher_for(&request, 10).expect_err("idle");
        assert!(matches!(err, PreviewError::EmptyQuery));
    }

    #[test]
    fn error_messages_are_stable_and_redacted() {
        assert_eq!(
            PreviewError::EmptyQuery.message(),
            crate::text_edit::IDLE_QUERY
        );
        assert!(
            PreviewError::RegexUnavailable { reason: "x".into() }
                .message()
                .contains("literal matching still works")
        );
        assert!(
            PreviewError::LimitExceeded {
                kind: "source matches",
                observed: 11,
                limit: 10,
            }
            .message()
            .contains("11 exceeds the 10 bound")
        );
    }

    #[test]
    fn draft_inclusion_flag_is_an_explicit_opt_in() {
        // S09/D-09: draft inclusion is an explicit opt-in carried by the
        // request and sealed into the manifest; the engine-level behavior
        // is exercised through real Session preview tests.
        let request = PreviewRequest {
            folder: None,
            query: "x".into(),
            replacement: "y".into(),
            mode: MatchMode::Literal,
            case_sensitive: false,
            whole_word: false,
            include_drafts: true,
        };
        assert!(request.include_drafts);
        assert!(
            !PreviewRequest {
                folder: None,
                include_drafts: false,
                ..request
            }
            .include_drafts
        );
    }

    #[test]
    fn unencodable_draft_reason_is_typed() {
        assert_eq!(
            ExclusionReason::UnencodableDraft.label(),
            "draft cannot be encoded under its descriptor"
        );
    }
}
