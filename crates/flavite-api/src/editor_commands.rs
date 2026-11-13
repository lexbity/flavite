//! Editor kernel commands (FEUX-0002 X03): operation tickets, the planner-
//! backed `editor.delete`, and the admission-guarded selection-only set
//! commands (`editor.keep_primary`, `editor.collapse`,
//! `editor.clear_selection`). The api is the sole translation point between
//! canonical `flavite-buffer::selection` values and view state.

use crate::command::Invocation;
use crate::flavite::decode;
use flavite_buffer::revision::{BufferId, Revision};
use flavite_buffer::selection::{RawSelection, SelectionSet};
use flavite_vault::editor_receipts::{OperationId, Outcome};
use flavite_vault::session::{Session, SessionCommand, SessionResult};
use serde::Deserialize;

/// Wire form of a Session-issued operation ticket.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationArg {
    pub buffer_incarnation: u64,
    pub sequence: u64,
    /// Hex-encoded 128-bit ticket.
    pub ticket: String,
}

impl OperationArg {
    /// Decode into an [`OperationId`] bound to `buffer_id`.
    pub fn decode(&self, buffer_id: u64) -> Result<OperationId, String> {
        let bytes =
            hex::decode(&self.ticket).map_err(|_| "operation ticket must be 32 hex characters")?;
        let bytes: [u8; 16] = bytes
            .try_into()
            .map_err(|_| "operation ticket must be 32 hex characters")?;
        Ok(OperationId {
            buffer_id,
            buffer_incarnation: self.buffer_incarnation,
            sequence: self.sequence,
            ticket: u128::from_be_bytes(bytes),
        })
    }
}

/// Wire form of the initiating editor view.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorOriginArg {
    /// 32-hex view token (tab identity).
    pub view: String,
    pub incarnation: u64,
}

/// Wire form of a canonical selection set.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionSetArg {
    /// Id of the intended primary member (hint only).
    #[serde(default)]
    pub primary: Option<u32>,
    /// `[id, anchor, head]` triples.
    pub items: Vec<[usize; 3]>,
}

/// Decode a selection set and normalize it against `text`.
fn normalize_set(text: &str, args: &SelectionSetArg) -> Result<SelectionSet, String> {
    let candidates: Vec<RawSelection> = args
        .items
        .iter()
        .map(|triple| RawSelection {
            id: u32::try_from(triple[0]).ok(),
            anchor: triple[1],
            head: triple[2],
        })
        .collect();
    SelectionSet::normalize(text, candidates, args.primary)
        .map(|(set, _)| set)
        .map_err(|error| error.to_string())
}

fn triples(set: &SelectionSet) -> Vec<[usize; 3]> {
    set.to_triples()
}

/// The index within `set` of the primary member (FEUX-0002 E-4/FR-4):
/// primary identity is explicit in every set-bearing handoff.
fn primary_index(set: &SelectionSet) -> usize {
    set.members()
        .iter()
        .position(|member| member.id() == set.primary_id())
        .unwrap_or(0)
}

/// `editor.issue_operation`: allocate one Session-issued operation ticket.
/// Non-mutating; the pending cap (32 per buffer) is enforced.
pub(crate) fn handle_issue_operation(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: IssueOperationArgs = decode(invocation)?;
    match session.issue_editor_operation(BufferId::from(args.buffer_id)) {
        Some(op) => Ok(SessionResult::Success(
            crate::session::CommandResult::OperationIssued {
                buffer_id: BufferId::from(args.buffer_id),
                operation: op,
            },
        )),
        None => Err("Too many pending operations for this buffer".into()),
    }
}

/// `editor.cancel_operation`: cancel a pending activation. A terminal
/// operation is refused with the typed outcome.
pub(crate) fn handle_cancel_operation(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: CancelOperationArgs = decode(invocation)?;
    let op = args.operation.decode(args.buffer_id)?;
    if session.cancel_editor_operation(&op) {
        return Ok(SessionResult::Success(
            crate::session::CommandResult::OperationCancelled {
                buffer_id: BufferId::from(args.buffer_id),
                operation: op,
            },
        ));
    }
    // Not pending: report the terminal state.
    match session.admit_editor_operation(&op) {
        flavite_vault::editor_receipts::TicketStatus::AlreadyApplied => {
            Err("AlreadyApplied".into())
        }
        flavite_vault::editor_receipts::TicketStatus::Cancelled => Err("Cancelled".into()),
        _ => Err("UnknownOperation".into()),
    }
}

/// `editor.cancel_selection`: cancel one pending match collection (E-11).
/// Non-mutating; the worker stops and the dropped outcome never reaches a
/// caller.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelSelectionArgs {
    request_id: u64,
}

pub(crate) fn handle_cancel_selection(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: CancelSelectionArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::TextCancel {
        request_id: args.request_id,
    }))
}

/// `editor.delete`: the planner-backed, ticketed delete over the canonical
/// selection set (FEUX-0002 X03 proof verb). Runs entirely through the
/// Session single-writer path; zero-effect no-ops (empty line) return an
/// unchanged revision with no history node.
pub(crate) fn handle_editor_delete(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: EditorDeleteArgs = decode(invocation)?;
    let op = args.operation.decode(args.buffer_id)?;
    validate_origin(&args.origin)?;
    // Ticket admission happens before any snapshot/revision work so duplicate
    // delivery replays the terminal outcome regardless of the buffer state.
    match session.admit_editor_operation(&op) {
        flavite_vault::editor_receipts::TicketStatus::Pending => {}
        flavite_vault::editor_receipts::TicketStatus::AlreadyApplied => {
            return Err("AlreadyApplied".into());
        }
        flavite_vault::editor_receipts::TicketStatus::Cancelled => {
            return Err("Cancelled".into());
        }
        flavite_vault::editor_receipts::TicketStatus::Retired => {
            return Err("ReceiptExpired".into());
        }
        flavite_vault::editor_receipts::TicketStatus::Unknown => {
            return Err("UnknownOperation".into());
        }
    }
    let snapshot = match session.shared_buffer_snapshot(
        BufferId::from(args.buffer_id),
        Some(Revision::new(args.revision)),
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            session.complete_editor_operation(&op, Outcome::Cancelled);
            return Err(match error {
                flavite_vault::session::BufferSnapshotError::NotFound(_) => {
                    "Buffer not found".to_string()
                }
                flavite_vault::session::BufferSnapshotError::Stale { expected, actual } => {
                    format!("StaleRevision: expected {expected}, current {actual}")
                }
            });
        }
    };
    let text = snapshot.text;
    let set = match normalize_set(&text, &args.selections) {
        Ok(set) => set,
        Err(error) => {
            session.complete_editor_operation(&op, Outcome::Cancelled);
            return Err(error);
        }
    };
    let prepared = match flavite_buffer::commands::plan(
        &text,
        &set,
        &flavite_buffer::commands::EditOperation::Delete,
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            session.complete_editor_operation(&op, Outcome::Cancelled);
            return Err(error.to_string());
        }
    };
    let kind = match args.kind.as_deref() {
        None | Some("delete") => flavite_buffer::history::EditKind::Delete,
        Some("backspace") => flavite_buffer::history::EditKind::Backspace,
        Some(other) => {
            session.complete_editor_operation(&op, Outcome::Cancelled);
            return Err(format!("kind must be `delete` or `backspace`, got {other}"));
        }
    };
    if prepared.ops.is_empty() {
        // Zero-effect: no history node, no revision bump (AC-8). The
        // activation is still consumed terminally.
        session.complete_editor_operation(&op, Outcome::Applied);
        return Ok(SessionResult::Success(
            crate::session::CommandResult::EditsApplied {
                buffer_id: BufferId::from(args.buffer_id),
                new_revision: Revision::new(args.revision),
                movement: Some(flavite_buffer::history::HistoryMovement {
                    selections: Some(triples(&set)),
                    spans: Vec::new(),
                    truncated: false,
                    primary: Some(primary_index(&set)),
                }),
                after_selections: Some(triples(&set)),
            },
        ));
    }
    let record = flavite_buffer::history::EditorEdit {
        origin: args.origin.view.clone(),
        kind,
        before: triples(&set),
        after: triples(&prepared.after_set),
        primary: primary_index(&prepared.after_set),
    };
    Ok(session.execute(SessionCommand::ApplyEdits {
        buffer_id: BufferId::from(args.buffer_id),
        revision: Revision::new(args.revision),
        ops: prepared.ops,
        undo_policy: flavite_buffer::undo::UndoPolicy::Context {
            origin: parse_origin(&args.origin.view)?,
        },
        editor: Some(record),
        operation: Some(op),
    }))
}

/// `editor.keep_primary`: drop every member but the primary. Selection-only:
/// no history node, no revision bump.
pub(crate) fn handle_keep_primary(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: EditorSelectionOnlyArgs = decode(invocation)?;
    let snapshot = session
        .shared_buffer_snapshot(
            BufferId::from(args.buffer_id),
            Some(Revision::new(args.revision)),
        )
        .map_err(|error| match error {
            flavite_vault::session::BufferSnapshotError::NotFound(_) => {
                "Buffer not found".to_string()
            }
            flavite_vault::session::BufferSnapshotError::Stale { expected, actual } => {
                format!("StaleRevision: expected {expected}, current {actual}")
            }
        })?;
    let set = normalize_set(&snapshot.text, &args.selections)?;
    let primary_id = set.primary_id().as_raw();
    let kept = set
        .members()
        .into_iter()
        .map(|member| RawSelection {
            id: Some(member.id().as_raw()),
            anchor: member.anchor(),
            head: member.head(),
        })
        .filter(|member| member.id == Some(primary_id))
        .collect::<Vec<_>>();
    let (kept, _) = SelectionSet::normalize(&snapshot.text, kept, Some(primary_id))
        .map_err(|error| error.to_string())?;
    Ok(selections_changed(args.buffer_id, args.revision, &kept))
}

/// `editor.collapse`: every range becomes a caret at its head (the delivered
/// collapse-to-heads behavior, reassigned per FEUX-0002 E-7).
pub(crate) fn handle_collapse(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: EditorSelectionOnlyArgs = decode(invocation)?;
    let snapshot = session
        .shared_buffer_snapshot(
            BufferId::from(args.buffer_id),
            Some(Revision::new(args.revision)),
        )
        .map_err(|error| match error {
            flavite_vault::session::BufferSnapshotError::NotFound(_) => {
                "Buffer not found".to_string()
            }
            flavite_vault::session::BufferSnapshotError::Stale { expected, actual } => {
                format!("StaleRevision: expected {expected}, current {actual}")
            }
        })?;
    let set = normalize_set(&snapshot.text, &args.selections)?;
    let carets = set
        .members()
        .into_iter()
        .map(|member| RawSelection {
            id: Some(member.id().as_raw()),
            anchor: member.head(),
            head: member.head(),
        })
        .collect::<Vec<_>>();
    let (collapsed, _) =
        SelectionSet::normalize(&snapshot.text, carets, None).map_err(|error| error.to_string())?;
    Ok(selections_changed(
        args.buffer_id,
        args.revision,
        &collapsed,
    ))
}

/// `editor.clear_selection`: one caret at the primary head (the reassigned
/// meaning of the delivered id; trivial at N=1).
pub(crate) fn handle_clear_selection(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: EditorSelectionOnlyArgs = decode(invocation)?;
    let snapshot = session
        .shared_buffer_snapshot(
            BufferId::from(args.buffer_id),
            Some(Revision::new(args.revision)),
        )
        .map_err(|error| match error {
            flavite_vault::session::BufferSnapshotError::NotFound(_) => {
                "Buffer not found".to_string()
            }
            flavite_vault::session::BufferSnapshotError::Stale { expected, actual } => {
                format!("StaleRevision: expected {expected}, current {actual}")
            }
        })?;
    let set = normalize_set(&snapshot.text, &args.selections)?;
    let head = set.primary().head();
    let caret = RawSelection {
        id: Some(set.primary_id().as_raw()),
        anchor: head,
        head,
    };
    let (cleared, _) = SelectionSet::normalize(&snapshot.text, vec![caret], None)
        .map_err(|error| error.to_string())?;
    Ok(selections_changed(args.buffer_id, args.revision, &cleared))
}

fn selections_changed(buffer_id: u64, revision: u64, set: &SelectionSet) -> SessionResult {
    SessionResult::Success(crate::session::CommandResult::SelectionsChanged {
        buffer_id: BufferId::from(buffer_id),
        revision: Revision::new(revision),
        selections: triples(set),
        primary: primary_index(set),
    })
}

fn validate_origin(origin: &EditorOriginArg) -> Result<(), String> {
    parse_origin(&origin.view).map(|_| ())
}

fn parse_origin(view: &str) -> Result<u128, String> {
    if view.len() != 32
        || !view
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("editor origin view must be 32 lowercase hex characters".into());
    }
    u128::from_str_radix(view, 16).map_err(|_| "invalid editor origin view".into())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueOperationArgs {
    buffer_id: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelOperationArgs {
    buffer_id: u64,
    operation: OperationArg,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorDeleteArgs {
    buffer_id: u64,
    revision: u64,
    operation: OperationArg,
    origin: EditorOriginArg,
    selections: SelectionSetArg,
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorSelectionOnlyArgs {
    buffer_id: u64,
    revision: u64,
    selections: SelectionSetArg,
}

// ---- FEUX-0002 X04/X05: the set-aware verb family ----
//
// Every remaining catalogue verb translates through this module: ticketed
// mutating verbs share one snapshot→normalize→plan→ApplyEdits path, and the
// selection-only family validates through the canonical buffer constructors.
// The api is the sole translation point (E-1); no handler re-implements the
// algebra.

/// Ticketed mutating verb arguments (`editor.change`, `editor.cut`,
/// `editor.open_above`, `editor.open_below`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorMutateArgs {
    buffer_id: u64,
    revision: u64,
    operation: OperationArg,
    origin: EditorOriginArg,
    selections: SelectionSetArg,
}

/// `editor.paste` adds the broadcast clipboard text.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorPasteArgs {
    buffer_id: u64,
    revision: u64,
    operation: OperationArg,
    origin: EditorOriginArg,
    selections: SelectionSetArg,
    text: String,
}

/// The shared ticketed mutating path: admit the ticket, take the frozen
/// snapshot, normalize the set, plan, and dispatch through the Session
/// single-writer apply. Zero-effect plans (no ops) consume the activation
/// and return an unchanged revision (AC-8).
#[allow(clippy::too_many_arguments)]
fn handle_ticketed_edit(
    session: &Session,
    buffer_id: u64,
    revision: u64,
    op: OperationArg,
    origin: EditorOriginArg,
    selections: SelectionSetArg,
    edit_operation: flavite_buffer::commands::EditOperation,
    kind: flavite_buffer::history::EditKind,
) -> Result<SessionResult, String> {
    let operation = op.decode(buffer_id)?;
    validate_origin(&origin)?;
    match session.admit_editor_operation(&operation) {
        flavite_vault::editor_receipts::TicketStatus::Pending => {}
        flavite_vault::editor_receipts::TicketStatus::AlreadyApplied => {
            return Err("AlreadyApplied".into());
        }
        flavite_vault::editor_receipts::TicketStatus::Cancelled => {
            return Err("Cancelled".into());
        }
        flavite_vault::editor_receipts::TicketStatus::Retired => {
            return Err("ReceiptExpired".into());
        }
        flavite_vault::editor_receipts::TicketStatus::Unknown => {
            return Err("UnknownOperation".into());
        }
    }
    let snapshot = match session
        .shared_buffer_snapshot(BufferId::from(buffer_id), Some(Revision::new(revision)))
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            session.complete_editor_operation(&operation, Outcome::Cancelled);
            return Err(match error {
                flavite_vault::session::BufferSnapshotError::NotFound(_) => {
                    "Buffer not found".to_string()
                }
                flavite_vault::session::BufferSnapshotError::Stale { expected, actual } => {
                    format!("StaleRevision: expected {expected}, current {actual}")
                }
            });
        }
    };
    let text = snapshot.text;
    let set = match normalize_set(&text, &selections) {
        Ok(set) => set,
        Err(error) => {
            session.complete_editor_operation(&operation, Outcome::Cancelled);
            return Err(error);
        }
    };
    let prepared = match flavite_buffer::commands::plan(&text, &set, &edit_operation) {
        Ok(prepared) => prepared,
        Err(error) => {
            session.complete_editor_operation(&operation, Outcome::Cancelled);
            return Err(error.to_string());
        }
    };
    if prepared.ops.is_empty() {
        // Zero-effect: no history node, no revision bump (AC-8).
        session.complete_editor_operation(&operation, Outcome::Applied);
        return Ok(SessionResult::Success(
            crate::session::CommandResult::EditsApplied {
                buffer_id: BufferId::from(buffer_id),
                new_revision: Revision::new(revision),
                movement: Some(flavite_buffer::history::HistoryMovement {
                    selections: Some(triples(&set)),
                    spans: Vec::new(),
                    truncated: false,
                    primary: Some(primary_index(&set)),
                }),
                after_selections: Some(triples(&set)),
            },
        ));
    }
    let record = flavite_buffer::history::EditorEdit {
        origin: origin.view.clone(),
        kind,
        before: triples(&set),
        after: triples(&prepared.after_set),
        primary: primary_index(&prepared.after_set),
    };
    Ok(session.execute(SessionCommand::ApplyEdits {
        buffer_id: BufferId::from(buffer_id),
        revision: Revision::new(revision),
        ops: prepared.ops,
        undo_policy: flavite_buffer::undo::UndoPolicy::Context {
            origin: parse_origin(&origin.view)?,
        },
        editor: Some(record),
        operation: Some(operation),
    }))
}

/// `editor.change`: delete each member's D-11 effective range; the host
/// enters typing after the receipt.
pub(crate) fn handle_editor_change(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: EditorMutateArgs = decode(invocation)?;
    handle_ticketed_edit(
        session,
        args.buffer_id,
        args.revision,
        args.operation,
        args.origin,
        args.selections,
        flavite_buffer::commands::EditOperation::Delete,
        flavite_buffer::history::EditKind::Change,
    )
}

/// `editor.cut`: the D-11 cut ranges delete atomically; the fragments rode
/// the earlier `editor.copy` resolution at the host clipboard seam.
pub(crate) fn handle_editor_cut(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: EditorMutateArgs = decode(invocation)?;
    handle_ticketed_edit(
        session,
        args.buffer_id,
        args.revision,
        args.operation,
        args.origin,
        args.selections,
        flavite_buffer::commands::EditOperation::Cut,
        flavite_buffer::history::EditKind::Cut,
    )
}

/// `editor.paste`: broadcast one string to every member (`Typing` op: ranges
/// replace whole, carets receive insertions).
pub(crate) fn handle_editor_paste(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: EditorPasteArgs = decode(invocation)?;
    handle_ticketed_edit(
        session,
        args.buffer_id,
        args.revision,
        args.operation,
        args.origin,
        args.selections,
        flavite_buffer::commands::EditOperation::Typing(args.text.replace("\r\n", "\n")),
        flavite_buffer::history::EditKind::Paste,
    )
}

/// `editor.open_above`/`editor.open_below`: open one line per unique head
/// line; the host enters typing after the receipt.
fn open_line(
    session: &Session,
    invocation: &Invocation,
    above: bool,
) -> Result<SessionResult, String> {
    let args: EditorMutateArgs = decode(invocation)?;
    let kind = if above {
        flavite_buffer::history::EditKind::OpenAbove
    } else {
        flavite_buffer::history::EditKind::OpenBelow
    };
    handle_ticketed_edit(
        session,
        args.buffer_id,
        args.revision,
        args.operation,
        args.origin,
        args.selections,
        flavite_buffer::commands::EditOperation::OpenLine { above },
        kind,
    )
}

pub(crate) fn handle_open_above(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    open_line(session, invocation, true)
}

pub(crate) fn handle_open_below(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    open_line(session, invocation, false)
}

/// `editor.copy`: the D-11 copy fragments joined with LF in document order
/// (§5.3). Read-only; the host owns the clipboard write at the seam.
pub(crate) fn handle_editor_copy(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: EditorSelectionOnlyArgs = decode(invocation)?;
    let snapshot = shared_snapshot(session, args.buffer_id, args.revision)?;
    let set = normalize_set(&snapshot.text, &args.selections)?;
    let fragments = flavite_buffer::commands::copy_fragments(&snapshot.text, &set)
        .map_err(|error| error.to_string())?;
    Ok(SessionResult::Success(
        crate::session::CommandResult::TextCopied {
            buffer_id: BufferId::from(args.buffer_id),
            revision: Revision::new(args.revision),
            text: fragments.join("\n"),
        },
    ))
}

/// Shared snapshot fetch for the selection-only family.
fn shared_snapshot(
    session: &Session,
    buffer_id: u64,
    revision: u64,
) -> Result<flavite_vault::session::SharedBufferSnapshot, String> {
    session
        .shared_buffer_snapshot(BufferId::from(buffer_id), Some(Revision::new(revision)))
        .map_err(|error| match error {
            flavite_vault::session::BufferSnapshotError::NotFound(_) => {
                "Buffer not found".to_string()
            }
            flavite_vault::session::BufferSnapshotError::Stale { expected, actual } => {
                format!("StaleRevision: expected {expected}, current {actual}")
            }
        })
}

/// `editor.select_word`: each member selects its containing word (grammar
/// word bounds), merged through normalization.
pub(crate) fn handle_select_word(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: EditorSelectionOnlyArgs = decode(invocation)?;
    let snapshot = shared_snapshot(session, args.buffer_id, args.revision)?;
    let set = normalize_set(&snapshot.text, &args.selections)?;
    let candidates: Vec<RawSelection> = set
        .members()
        .iter()
        .map(|member| {
            let (anchor, head) = flavite_edit_context::word_bounds(&snapshot.text, member.head());
            RawSelection {
                id: Some(member.id().as_raw()),
                anchor,
                head,
            }
        })
        .collect();
    let hint = set.primary_id().as_raw();
    let (selected, _) = SelectionSet::normalize(&snapshot.text, candidates, Some(hint))
        .map_err(|error| error.to_string())?;
    Ok(selections_changed(args.buffer_id, args.revision, &selected))
}

/// `editor.select_line`: each member selects its containing line including
/// its line break, merged.
pub(crate) fn handle_select_line(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: EditorSelectionOnlyArgs = decode(invocation)?;
    let snapshot = shared_snapshot(session, args.buffer_id, args.revision)?;
    let set = normalize_set(&snapshot.text, &args.selections)?;
    let candidates: Vec<RawSelection> = set
        .members()
        .iter()
        .map(|member| {
            let (anchor, head) =
                flavite_buffer::selection::containing_line(&snapshot.text, member.head());
            RawSelection {
                id: Some(member.id().as_raw()),
                anchor,
                head,
            }
        })
        .collect();
    let hint = set.primary_id().as_raw();
    let (selected, _) = SelectionSet::normalize(&snapshot.text, candidates, Some(hint))
        .map_err(|error| error.to_string())?;
    Ok(selections_changed(args.buffer_id, args.revision, &selected))
}

/// The `editor.insert_*` family: a zero-effect plan collapses every member
/// onto the requested boundary; the returned set is the typing entry state.
fn insert_at(
    session: &Session,
    invocation: &Invocation,
    edit_operation: flavite_buffer::commands::EditOperation,
) -> Result<SessionResult, String> {
    let args: EditorSelectionOnlyArgs = decode(invocation)?;
    let snapshot = shared_snapshot(session, args.buffer_id, args.revision)?;
    let set = normalize_set(&snapshot.text, &args.selections)?;
    let prepared = flavite_buffer::commands::plan(&snapshot.text, &set, &edit_operation)
        .map_err(|error| error.to_string())?;
    Ok(selections_changed(
        args.buffer_id,
        args.revision,
        &prepared.after_set,
    ))
}

pub(crate) fn handle_insert_start(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    insert_at(
        session,
        invocation,
        flavite_buffer::commands::EditOperation::InsertAtStarts(String::new()),
    )
}

pub(crate) fn handle_insert_end(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    insert_at(
        session,
        invocation,
        flavite_buffer::commands::EditOperation::InsertAtEnds(String::new()),
    )
}

pub(crate) fn handle_insert_line_start(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    insert_at(
        session,
        invocation,
        flavite_buffer::commands::EditOperation::InsertAtLineStarts(String::new()),
    )
}

pub(crate) fn handle_insert_line_end(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    insert_at(
        session,
        invocation,
        flavite_buffer::commands::EditOperation::InsertAtLineEnds(String::new()),
    )
}

/// Which boundary the `editor.insert_*` family collapses onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtPosition {
    Starts,
    Ends,
    LineStarts,
    LineEnds,
}

/// `editor.toggle_caret`: a11y/programmatic parity with Mod-click (E-14).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorToggleCaretArgs {
    buffer_id: u64,
    revision: u64,
    selections: SelectionSetArg,
    position: usize,
}

pub(crate) fn handle_toggle_caret(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: EditorToggleCaretArgs = decode(invocation)?;
    let snapshot = shared_snapshot(session, args.buffer_id, args.revision)?;
    let set = normalize_set(&snapshot.text, &args.selections)?;
    let toggled = flavite_buffer::selection::toggle_caret(&snapshot.text, &set, args.position)
        .map_err(|error| error.to_string())?;
    Ok(selections_changed(args.buffer_id, args.revision, &toggled))
}

// ---- X05: motions, find-char, and the pure set-op family ----

/// The `editor.move` motion vocabulary (§6).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MotionArg {
    CharLeft,
    CharRight,
    LineUp,
    LineDown,
    WordNext,
    WordPrev,
    WordEnd,
    LineStart,
    LineEnd,
    DocStart,
    DocEnd,
    PageUp,
    PageDown,
}

impl From<MotionArg> for flavite_buffer::motion::Motion {
    fn from(arg: MotionArg) -> Self {
        match arg {
            MotionArg::CharLeft => Self::CharLeft,
            MotionArg::CharRight => Self::CharRight,
            MotionArg::LineUp => Self::LineUp,
            MotionArg::LineDown => Self::LineDown,
            MotionArg::WordNext => Self::WordNext,
            MotionArg::WordPrev => Self::WordPrev,
            MotionArg::WordEnd => Self::WordEnd,
            MotionArg::LineStart => Self::LineStart,
            MotionArg::LineEnd => Self::LineEnd,
            MotionArg::DocStart => Self::DocStart,
            MotionArg::DocEnd => Self::DocEnd,
            MotionArg::PageUp => Self::PageUp,
            MotionArg::PageDown => Self::PageDown,
        }
    }
}

/// `editor.move {motion, extend, positions}`: one typed motion per member.
/// Visual motions (line up/down, page up/down) carry editor-computed target
/// heads; text-pure motions compute here and refuse supplied positions.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorMoveArgs {
    buffer_id: u64,
    revision: u64,
    selections: SelectionSetArg,
    motion: MotionArg,
    #[serde(default)]
    extend: bool,
    #[serde(default)]
    positions: Option<Vec<usize>>,
}

pub(crate) fn handle_editor_move(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: EditorMoveArgs = decode(invocation)?;
    let snapshot = shared_snapshot(session, args.buffer_id, args.revision)?;
    let set = normalize_set(&snapshot.text, &args.selections)?;
    let motion: flavite_buffer::motion::Motion = args.motion.into();
    let positions = match (motion.is_visual(), &args.positions) {
        (true, Some(positions)) => Some(positions.as_slice()),
        (true, None) => {
            return Err("visual motions need one target position per selection".into());
        }
        (false, Some(_)) => {
            return Err("positions only apply to visual motions".into());
        }
        (false, None) => None,
    };
    let moved = flavite_buffer::motion::apply(&snapshot.text, &set, motion, args.extend, positions)
        .map_err(|error| error.to_string())?;
    Ok(selections_changed(args.buffer_id, args.revision, &moved))
}

/// `editor.find_char {direction, char, extend}`: scan for one literal char
/// per member (E-8); no match on any member is a typed no-op.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorFindCharArgs {
    buffer_id: u64,
    revision: u64,
    selections: SelectionSetArg,
    direction: String,
    char: String,
    #[serde(default)]
    extend: bool,
}

pub(crate) fn handle_find_char(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: EditorFindCharArgs = decode(invocation)?;
    let forward = match args.direction.as_str() {
        "fwd" => true,
        "back" => false,
        other => return Err(format!("direction must be `fwd` or `back`, got {other}")),
    };
    let mut chars = args.char.chars();
    let (target, extra) = (chars.next(), chars.next());
    let Some(target) = target.filter(|_| extra.is_none()) else {
        return Err("char must be exactly one character".into());
    };
    let snapshot = shared_snapshot(session, args.buffer_id, args.revision)?;
    let set = normalize_set(&snapshot.text, &args.selections)?;
    let found =
        flavite_buffer::selection::find_char(&snapshot.text, &set, forward, target, args.extend)
            .map_err(|error| error.to_string())?;
    Ok(selections_changed(args.buffer_id, args.revision, &found))
}

/// `editor.add_caret_above`/`below`: the host computes the visual-row target
/// heads (layout is editor-side); the pure half validates and merges.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorAddCaretsArgs {
    buffer_id: u64,
    revision: u64,
    selections: SelectionSetArg,
    positions: Vec<usize>,
}

fn handle_add_carets(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let args: EditorAddCaretsArgs = decode(invocation)?;
    eprintln!("[add_carets] positions={:?}", args.positions);
    let snapshot = shared_snapshot(session, args.buffer_id, args.revision)?;
    let set = normalize_set(&snapshot.text, &args.selections)?;
    let merged = flavite_buffer::selection::add_carets(&snapshot.text, &set, &args.positions)
        .map_err(|error| error.to_string())?;
    eprintln!(
        "[add_carets] merged={:?}",
        merged
            .members()
            .iter()
            .map(|m| (m.start(), m.end()))
            .collect::<Vec<_>>()
    );
    Ok(selections_changed(args.buffer_id, args.revision, &merged))
}

pub(crate) fn handle_add_caret_above(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    handle_add_carets(session, invocation)
}

pub(crate) fn handle_add_caret_below(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    handle_add_carets(session, invocation)
}

/// `editor.rotate_primary {direction}`: selection-only primary advance.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorRotateArgs {
    buffer_id: u64,
    revision: u64,
    selections: SelectionSetArg,
    direction: String,
}

pub(crate) fn handle_rotate_primary(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: EditorRotateArgs = decode(invocation)?;
    let snapshot = shared_snapshot(session, args.buffer_id, args.revision)?;
    let set = normalize_set(&snapshot.text, &args.selections)?;
    let forward = match args.direction.as_str() {
        "next" => true,
        "previous" => false,
        other => {
            return Err(format!(
                "direction must be `next` or `previous`, got {other}"
            ));
        }
    };
    let rotated = flavite_buffer::selection::rotate_primary(&set, forward);
    Ok(selections_changed(args.buffer_id, args.revision, &rotated))
}

macro_rules! selection_only_set_op {
    ($name:ident, $method:expr) => {
        pub(crate) fn $name(
            session: &Session,
            invocation: &Invocation,
        ) -> Result<SessionResult, String> {
            let args: EditorSelectionOnlyArgs = decode(invocation)?;
            let snapshot = shared_snapshot(session, args.buffer_id, args.revision)?;
            let set = normalize_set(&snapshot.text, &args.selections)?;
            let next = $method(&snapshot.text, &set).map_err(|error| error.to_string())?;
            Ok(selections_changed(args.buffer_id, args.revision, &next))
        }
    };
}

selection_only_set_op!(
    handle_flip_selections,
    flavite_buffer::selection::flip_selections
);
selection_only_set_op!(
    handle_trim_selections,
    flavite_buffer::selection::trim_selections
);
selection_only_set_op!(
    handle_invert_selections,
    flavite_buffer::selection::invert_selections
);

// ---- FEUX-0002 X06: match-driven selection (E-10/E-11) ----

/// The shared query argument for `editor.select_matches` /
/// `editor.split_selection` (§5.9): literal or regex, case flag.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchQueryArg {
    pub text: String,
    #[serde(default)]
    pub regex: bool,
    #[serde(default)]
    pub case_sensitive: bool,
}

/// Both match-driven selection commands share one wire shape: a draft
/// (`apply` omitted/false) requests collection and returns the candidate
/// spans as a read-only outcome; an apply (`apply: true`) installs the
/// supplied candidate spans as one canonical set through the selection-only
/// path (no ticket, no history node, `selection_version` bump). An apply
/// must carry the collection's exact observed totals (`total_matches`,
/// `zero_width_count`): the candidate span list is capped at the selection
/// bound, so only the totals prove the exact observed counts (E-10/R13).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorSelectMatchesArgs {
    buffer_id: u64,
    revision: u64,
    selections: SelectionSetArg,
    query: MatchQueryArg,
    #[serde(default)]
    apply: bool,
    /// Canonical candidate spans (half-open byte ranges) echoed back on
    /// apply. Refused when absent on apply or present on draft.
    #[serde(default)]
    spans: Option<Vec<(usize, usize)>>,
    /// The collection's exact match total (required on apply).
    #[serde(default)]
    total_matches: Option<u64>,
    /// The collection's exact zero-width hit count (required on apply).
    #[serde(default)]
    zero_width_count: Option<u64>,
}

fn decode_mode(regex: bool) -> flavite_text_search::matcher::MatchMode {
    if regex {
        flavite_text_search::matcher::MatchMode::Regex
    } else {
        flavite_text_search::matcher::MatchMode::Literal
    }
}

/// `editor.select_matches`: draft collection (async, through the existing
/// `text.find` engine) or atomic apply of the candidate spans.
pub(crate) fn handle_select_matches(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    handle_match_selection(session, invocation, true)
}

/// `editor.split_selection`: the Kakoune `S` complement — draft collection
/// or atomic apply of the gaps between the candidate matches.
pub(crate) fn handle_split_selection(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    handle_match_selection(session, invocation, false)
}

fn handle_match_selection(
    session: &Session,
    invocation: &Invocation,
    select: bool,
) -> Result<SessionResult, String> {
    let args: EditorSelectMatchesArgs = decode(invocation)?;
    let buffer_id = BufferId::from(args.buffer_id);
    // Both paths validate the committed set against the live snapshot; a
    // stale capture is refused before any collection or install.
    let snapshot = shared_snapshot(session, args.buffer_id, args.revision)?;
    normalize_set(&snapshot.text, &args.selections)?;
    let mode = decode_mode(args.query.regex);
    if !args.apply {
        if args.spans.is_some() {
            return Err("spans only apply to an apply".into());
        }
        if args.query.text.len() > flavite_text_search::matcher::PATTERN_MAX_BYTES {
            return Err("query is too long".into());
        }
        return Ok(session.execute(SessionCommand::SelectMatches {
            request_id: invocation.request_id,
            request: flavite_vault::text_select::SelectMatchesRequest {
                buffer_id,
                revision: Revision::new(args.revision),
                query: args.query.text,
                mode,
                case_sensitive: args.query.case_sensitive,
                whole_word: false,
            },
        }));
    }
    let Some(spans) = args.spans else {
        return Err("apply needs the candidate spans".into());
    };
    // The exact observed totals are the apply's authority (E-10/R13): the
    // span list is capped at the selection bound, so a truncated apply
    // without them would report a dishonest count.
    let Some(total_matches) = args.total_matches else {
        return Err("apply needs the exact match total".into());
    };
    let Some(zero_width_count) = args.zero_width_count else {
        return Err("apply needs the exact zero-width count".into());
    };
    if select {
        apply_select_matches(
            &snapshot.text,
            &spans,
            total_matches,
            zero_width_count,
            args.buffer_id,
            args.revision,
        )
    } else {
        apply_split_selection(
            &snapshot.text,
            &spans,
            total_matches,
            args.buffer_id,
            args.revision,
        )
    }
}

/// The select apply: refuse zero-width and oversize with the collection's
/// exact observed counts; otherwise canonicalize the spans into the new set
/// (E-10). Within the cap the collector materializes every span, so the
/// apply additionally proves the span list is the complete collection.
fn apply_select_matches(
    text: &str,
    spans: &[(usize, usize)],
    total_matches: u64,
    zero_width_count: u64,
    buffer_id: u64,
    revision: u64,
) -> Result<SessionResult, String> {
    if total_matches == 0 || spans.is_empty() {
        return Err("NoMatches".into());
    }
    if zero_width_count > 0 {
        return Err(format!("ZeroWidthMatches {zero_width_count}"));
    }
    if total_matches > flavite_buffer::selection::MAX_SELECTIONS as u64 {
        return Err(format!("TooManySelections {total_matches}"));
    }
    if spans.len() as u64 != total_matches {
        return Err("collection spans do not match the observed total".into());
    }
    let set = flavite_buffer::selection::selection_set_from_spans(text, spans, None)
        .map_err(|error| error.to_string())?;
    Ok(selections_changed(buffer_id, revision, &set))
}

/// The split apply: refuse an oversize match set with the exact observed
/// count (a truncated span list cannot derive exact gaps), canonicalize,
/// compute the document-wide gaps, refuse an empty gap set (`NoGaps`), then
/// install the gap set.
fn apply_split_selection(
    text: &str,
    spans: &[(usize, usize)],
    total_matches: u64,
    buffer_id: u64,
    revision: u64,
) -> Result<SessionResult, String> {
    if total_matches == 0 || spans.is_empty() {
        return Err("NoMatches".into());
    }
    if total_matches > flavite_buffer::selection::MAX_SELECTIONS as u64 {
        return Err(format!("TooManySelections {total_matches}"));
    }
    if spans.len() as u64 != total_matches {
        return Err("collection spans do not match the observed total".into());
    }
    let set = flavite_buffer::selection::selection_set_from_spans(text, spans, None)
        .map_err(|error| error.to_string())?;
    let members: Vec<(usize, usize)> = set
        .members()
        .into_iter()
        .map(|member| (member.start(), member.end()))
        .collect();
    let gaps = flavite_buffer::selection::split_gaps(text, &members);
    if gaps.is_empty() {
        return Err("NoGaps".into());
    }
    if gaps.len() > flavite_buffer::selection::MAX_SELECTIONS {
        return Err(format!("TooManySelections {}", gaps.len()));
    }
    let gap_set = flavite_buffer::selection::selection_set_from_spans(text, &gaps, None)
        .map_err(|error| error.to_string())?;
    Ok(selections_changed(buffer_id, revision, &gap_set))
}
