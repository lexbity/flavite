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

/// Write to the clipboard (the join already happened at the copy source).
pub fn write(text: String) -> Task<Message> {
    iced::clipboard::write(text).map(|()| Message::Pump)
}

/// Interpret one clipboard read at the effect boundary.
pub fn handle(app: &mut App, message: ClipboardMessage) -> Task<Message> {
    match message {
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
            super::input::handle(app, tab, incarnation, input)
        }
    }
}
