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
