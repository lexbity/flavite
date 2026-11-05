//! Per-tab encoding action host (RFC-0006 §6.10, S00).
//!
//! The core never silently reinterprets or re-encodes a source: malformed or
//! ambiguous input, a stale hint, an unencodable strict-ASCII draft, or a
//! refused reopen arrive here as typed outcomes. This host owns the per-tab
//! view state for those outcomes — the choice dialog, the convert repair and
//! dismissal — and dispatches the typed `note.reopen_encoding` /
//! `note.convert_encoding` commands through the facade. It holds no note
//! text and performs no IO.

use std::collections::BTreeMap;

use flavite_api::Origin;
use flavite_api::command::Invocation;
use flavite_workspace::TabId;

use crate::app::{App, Message};

/// One pending encoding decision for a tab.
#[derive(Debug, Clone)]
pub struct PendingEncoding {
    /// Stable core code: `encoding-choice-required`, `encoding-unencodable`,
    /// `encoding-dirty-buffer`, `encoding-save-pending`, …
    pub code: String,
    /// Compact sanitized JSON payload from the core (reason, observed raw
    /// hash, allowed descriptors). Never note content.
    pub payload: Option<String>,
    /// Vault-relative path of the affected note.
    pub path: std::path::PathBuf,
}

/// Reason code parsed out of a choice payload (fallback: the raw string).
#[must_use]
pub fn payload_reason(payload: Option<&String>) -> String {
    payload
        .and_then(|payload| serde_json::from_str::<serde_json::Value>(payload).ok())
        .and_then(|value| {
            value
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unspecified".into())
}

/// The observed raw hash parsed out of a choice payload.
#[must_use]
pub fn payload_raw_hash(payload: Option<&String>) -> Option<String> {
    payload
        .and_then(|payload| serde_json::from_str::<serde_json::Value>(payload).ok())
        .and_then(|value| {
            value
                .get("raw_hash")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
}

/// Per-tab encoding decision state.
#[derive(Debug, Default)]
pub struct EncodingHost {
    pending: BTreeMap<TabId, PendingEncoding>,
}

impl EncodingHost {
    /// Record a typed core outcome for a tab, replacing any previous one.
    pub fn record(
        &mut self,
        tab: TabId,
        code: String,
        payload: Option<String>,
        path: std::path::PathBuf,
    ) {
        self.pending.insert(
            tab,
            PendingEncoding {
                code,
                payload,
                path,
            },
        );
    }

    /// Clear the pending decision for a tab (resolved or dismissed).
    pub fn clear(&mut self, tab: TabId) {
        self.pending.remove(&tab);
    }

    /// The pending decision for a tab, if any.
    #[must_use]
    pub fn pending(&self, tab: TabId) -> Option<&PendingEncoding> {
        self.pending.get(&tab)
    }
}

/// Handle the encoding messages routed from [`crate::app`].
pub fn handle(app: &mut App, message: EncodingMessage) -> iced::Task<Message> {
    match message {
        EncodingMessage::Reopen { tab, encoding, bom } => reopen(app, tab, &encoding, &bom),
        EncodingMessage::Convert { tab, encoding, bom } => convert(app, tab, &encoding, &bom),
        EncodingMessage::Dismiss { tab } => {
            app.encoding.clear(tab);
            iced::Task::none()
        }
    }
}

fn record_from_result(
    app: &mut App,
    tab: TabId,
    code: String,
    data: Option<&serde_json::Value>,
    path: std::path::PathBuf,
) {
    let payload = data.and_then(serde_json::Value::as_str).map(str::to_owned);
    app.encoding.record(tab, code, payload, path);
}

fn reopen(app: &mut App, tab: TabId, encoding: &str, bom: &str) -> iced::Task<Message> {
    let Some(pending) = app.encoding.pending(tab).cloned() else {
        return iced::Task::none();
    };
    let Some(raw_hash) = payload_raw_hash(pending.payload.as_ref()) else {
        app.status = "Reopen requires the observed raw hash; reload the note list".into();
        return iced::Task::none();
    };
    let Some(flavite) = &app.flavite else {
        return iced::Task::none();
    };
    let request_id = flavite.command_registry().next_request_id();
    let invocation = Invocation::new(
        request_id,
        "note.reopen_encoding",
        serde_json::json!({
            "path": pending.path,
            "raw_hash": raw_hash,
            "encoding": encoding,
            "bom": bom,
        }),
        Origin::Gui,
    );
    match flavite.execute(invocation) {
        flavite_api::ExecutionResult::Pending { request_id } => {
            app.operations.accept(request_id, "note.reopen_encoding");
        }
        flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Success(success)) => {
            app.encoding.clear(tab);
            let buffer = success
                .data
                .get("buffer_id")
                .and_then(serde_json::Value::as_u64);
            app.notes.ready(tab, buffer);
            app.status = format!(
                "Reopened as {encoding}{}",
                if bom == "present" { "+bom" } else { "" }
            );
        }
        flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Error(error)) => {
            app.status = format!("Reopen failed: {}", error.message);
        }
        flavite_api::ExecutionResult::Rejected(rejected) => {
            app.status = format!("Reopen rejected: {:?}", rejected.reason);
        }
        flavite_api::ExecutionResult::ActionRequired { code, data, .. } => {
            // Still undecodable under the confirmed descriptor: keep the
            // dialog open with the fresh payload.
            record_from_result(app, tab, code, data.as_ref(), pending.path);
        }
        _ => {
            app.status = "Reopen is unavailable right now".into();
        }
    }
    iced::Task::none()
}

fn convert(app: &mut App, tab: TabId, encoding: &str, bom: &str) -> iced::Task<Message> {
    let Some(pending) = app.encoding.pending(tab).cloned() else {
        return iced::Task::none();
    };
    let Some(flavite) = &app.flavite else {
        return iced::Task::none();
    };
    // Conversion operates on the live buffer at its exact revision.
    let Some(state) = app.notes.state(&tab) else {
        return iced::Task::none();
    };
    let Some(buffer_id) = state.buffer else {
        return iced::Task::none();
    };
    let Ok(view) = flavite.get_shared_buffer_view(flavite_api::BufferId::from(buffer_id), None)
    else {
        app.status = "Buffer unavailable for conversion".into();
        return iced::Task::none();
    };
    let request_id = flavite.command_registry().next_request_id();
    let invocation = Invocation::new(
        request_id,
        "note.convert_encoding",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": view.revision,
            "encoding": encoding,
            "bom": bom,
        }),
        Origin::Gui,
    );
    match flavite.execute(invocation) {
        flavite_api::ExecutionResult::Pending { request_id } => {
            app.operations.accept(request_id, "note.convert_encoding");
        }
        flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Success(_)) => {
            // Conversion is buffer-local (disk is untouched until save); the
            // unencodable condition clears after validation, per §6.10.
            app.encoding.clear(tab);
            app.status = format!(
                "Converted to {encoding}{} — save to persist the format",
                if bom == "present" { "+bom" } else { "" }
            );
        }
        flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Error(error)) => {
            app.status = format!("Conversion failed: {}", error.message);
        }
        flavite_api::ExecutionResult::Rejected(rejected) => {
            app.status = format!("Conversion rejected: {:?}", rejected.reason);
        }
        flavite_api::ExecutionResult::ActionRequired { code, data, .. } => {
            record_from_result(app, tab, code, data.as_ref(), pending.path);
        }
        _ => {
            app.status = "Conversion is unavailable right now".into();
        }
    }
    iced::Task::none()
}

/// Messages owned by the encoding host.
#[derive(Debug, Clone)]
pub enum EncodingMessage {
    /// Reopen the tab's note under an explicit descriptor.
    Reopen {
        tab: TabId,
        encoding: String,
        bom: String,
    },
    /// Convert the tab's buffer to an explicit save format.
    Convert {
        tab: TabId,
        encoding: String,
        bom: String,
    },
    /// Dismiss the pending decision.
    Dismiss { tab: TabId },
}
