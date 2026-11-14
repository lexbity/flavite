//! The editor clipboard seam (FEUX-0002 X04, T-10). Every OS clipboard
//! effect the editor paths perform lives here — reads resume through
//! [`ClipboardMessage`] at the effect boundary, so tests can drive empty or
//! changed reads by sending the message, and no control handler touches the
//! platform clipboard directly. Copy joins fragments with LF in document
//! order (the api `editor.copy` computes the fragments under the D-11
//! policy); Paste broadcasts the one clipboard string to every member.

use crate::app::{App, Message};
use flavite_editor::input::CapturedInput;
use flavite_workspace::TabId;
use iced::Task;

use super::context_mode::EditorTarget;

/// Clipboard outcomes resume interpretation at the effect boundary.
#[derive(Debug, Clone)]
pub enum ClipboardMessage {
    WriteChecked {
        verified: bool,
    },
    CutWriteFailed,
    CopyReady {
        target: EditorTarget,
        generation: u64,
    },
    /// A Context Mode paste read completed.
    ContextPaste {
        target: EditorTarget,
        generation: u64,
        text: Option<String>,
    },
    /// A plain (or typing-mode) paste read completed; the captured key
    /// intent resumes interpretation with the clipboard text.
    TypingPaste {
        tab: TabId,
        incarnation: u64,
        input: CapturedInput,
        text: Option<String>,
    },
    /// A Context Mode cut's clipboard write completed. The source is
    /// revalidated here, before the one guarded deletion (§5.5): a stale
    /// or refused write can never delete the source text.
    CutReady {
        target: EditorTarget,
        generation: u64,
    },
    /// A plain (or typing-mode) cut's clipboard write completed; the
    /// captured intent resumes interpretation and performs its guarded
    /// deletion without writing again.
    CutCompleted {
        tab: TabId,
        incarnation: u64,
        input: CapturedInput,
    },
}

/// Read the clipboard for a Context Mode paste (generation-guarded).
pub fn read_for_context_paste(target: EditorTarget, generation: u64) -> Task<Message> {
    iced::clipboard::read().map(move |text| {
        Message::EditorClipboard(ClipboardMessage::ContextPaste {
            target: target.clone(),
            generation,
            text,
        })
    })
}

/// Read the clipboard for a captured typing intent (the widget never reads
/// the platform clipboard; the intent arrives with empty paste text).
pub fn read_for_typing(tab: TabId, incarnation: u64, input: CapturedInput) -> Task<Message> {
    iced::clipboard::read().map(move |text| {
        Message::EditorClipboard(ClipboardMessage::TypingPaste {
            tab,
            incarnation,
            input: input.clone(),
            text,
        })
    })
}

/// iced writes are effects, not value-producing tasks: a map callback on
/// `write` never runs. Chain a read after the effect and verify the text
/// before any destructive continuation. Other clipboard owners may win the
/// intervening race; refusal preserves the document in that case.
fn verified_write(text: String) -> Task<bool> {
    iced::clipboard::write(text.clone())
        .chain(iced::clipboard::read())
        .map(move |actual| actual.as_deref() == Some(text.as_str()))
}

pub fn write(text: String) -> Task<Message> {
    verified_write(text)
        .map(|verified| Message::EditorClipboard(ClipboardMessage::WriteChecked { verified }))
}

pub fn write_for_context_copy(
    target: EditorTarget,
    generation: u64,
    text: String,
) -> Task<Message> {
    verified_write(text).map(move |verified| {
        Message::EditorClipboard(if verified {
            ClipboardMessage::CopyReady {
                target: target.clone(),
                generation,
            }
        } else {
            ClipboardMessage::WriteChecked { verified: false }
        })
    })
}

pub fn write_for_cut(target: EditorTarget, generation: u64, text: String) -> Task<Message> {
    verified_write(text).map(move |verified| {
        Message::EditorClipboard(if verified {
            ClipboardMessage::CutReady {
                target: target.clone(),
                generation,
            }
        } else {
            ClipboardMessage::CutWriteFailed
        })
    })
}

pub fn write_for_cut_resume(
    tab: TabId,
    incarnation: u64,
    input: CapturedInput,
    text: String,
) -> Task<Message> {
    verified_write(text).map(move |verified| {
        Message::EditorClipboard(if verified {
            ClipboardMessage::CutCompleted {
                tab,
                incarnation,
                input: input.clone(),
            }
        } else {
            ClipboardMessage::CutWriteFailed
        })
    })
}

/// Interpret one clipboard read at the effect boundary.
pub fn handle(app: &mut App, message: ClipboardMessage) -> Task<Message> {
    match message {
        ClipboardMessage::WriteChecked { verified } => {
            app.status = if verified {
                "Copied selection"
            } else {
                "Clipboard write could not be verified; try Copy again"
            }
            .into();
            Task::none()
        }
        ClipboardMessage::CutWriteFailed => {
            app.status = "Clipboard write could not be verified; cut was not applied".into();
            app.context_mode.notice(app.status.clone());
            Task::none()
        }
        ClipboardMessage::CopyReady { target, generation } => {
            if generation != app.context_mode.generation()
                || !super::context_mode::valid(app, target.clone(), true)
            {
                return Task::none();
            }
            app.status = "Copied selection".into();
            super::context_mode::focus(app, target.tab)
        }
        ClipboardMessage::ContextPaste {
            target,
            generation,
            text,
        } => {
            if generation != app.context_mode.generation()
                || !super::context_mode::valid(app, target.clone(), true)
                || app.context_mode.history.open
            {
                app.context_mode
                    .notice("Source changed; paste was not applied");
                return Task::none();
            }
            match text {
                Some(paste) if !paste.is_empty() => super::context_mode::paste(app, target, paste),
                _ => {
                    app.context_mode.notice("Clipboard is empty");
                    app.diagnostics.record(
                        Some(target.tab),
                        crate::host::editor_diagnostics::ModeEnum::from(
                            app.context_mode.mode(target.tab),
                        ),
                        crate::host::editor_diagnostics::EventKind::OperationRefused,
                        crate::host::editor_diagnostics::Outcome::Refused,
                        Some(0),
                        None,
                    );
                    super::context_mode::focus(app, target.tab)
                }
            }
        }
        ClipboardMessage::TypingPaste {
            tab,
            incarnation,
            mut input,
            text,
        } => {
            // An empty clipboard stays a no-op paste (§5.5): interpretation
            // proceeds with empty text and collapses to a selection ack.
            input.text = text.unwrap_or_default();
            // The clipboard stage is complete: interpret the acquired text,
            // never re-acquire (FR-8/FR-9).
            input.clipboard_acquired = true;
            super::input::handle(app, tab, incarnation, input)
        }
        ClipboardMessage::CutReady { target, generation } => {
            const NOT_APPLIED: &str = "Copied; cut was not applied because the source changed";
            // The clipboard write completed. Revalidate the source and the
            // committed set/version, then perform the one guarded deletion;
            // staleness keeps the text and reports honestly (§5.5, AC-8).
            if generation != app.context_mode.generation()
                || !super::context_mode::valid(app, target.clone(), true)
            {
                app.status = NOT_APPLIED.into();
                app.context_mode.notice(NOT_APPLIED);
                return Task::none();
            }
            match super::context_mode::kernel_mutate(
                app,
                &target,
                "editor.cut",
                serde_json::json!({}),
                "Cut was refused; source changed",
            ) {
                Some(success) => {
                    super::context_mode::install_receipt(app, &target, &success);
                    super::context_mode::focus(app, target.tab)
                }
                None => Task::none(),
            }
        }
        ClipboardMessage::CutCompleted {
            tab,
            incarnation,
            mut input,
        } => {
            // The clipboard write completed; the same captured intent resumes
            // with the guarded mutation and never writes again.
            input.clipboard_acquired = true;
            super::input::handle(app, tab, incarnation, input)
        }
    }
}
