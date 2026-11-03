//! Modal dialogs (W10): explicit note creation, rename/move with strategy
//! preview, about, and the no-vault start screen (FR-22).
//!
//! Dialogs are ordinary interface state reduced through `App::update`; a
//! dialog never mutates a note on hover or selection, only on explicit
//! submit. Creation goes through the core `note.create` command, renames
//! through the journaled `note.rename` — no GUI-side file mutation.

use crate::app::{App, Message};
use crate::style;
use crate::workspace::tab_strip;
use flavite_workspace::NoteLocator;
use iced::widget::{button, column, container, row, text, text_input};
use iced::{Element, Length};

/// Maximum dialog text input length (NFR-12: 8 KiB per text setting).
const MAX_INPUT_BYTES: usize = 8192;

/// Widget ids of the dialog inputs, for focus and semantic tests.
pub const NEW_NOTE_INPUT_ID: &str = "dialog-new-note-input";
pub const RENAME_INPUT_ID: &str = "dialog-rename-input";

/// The one open modal dialog, if any.
#[derive(Debug, Clone)]
pub enum Dialog {
    /// Create a new empty note at an exact native path. `link_source`
    /// carries the unresolved-link activation that offered creation, for
    /// status attribution; creation itself is always explicit.
    NewNote { path: String },
    /// Rename/move one note with a strategy preview. `tier` labels an
    /// externally detected rename (T0–T2 authoritative, T3/T4 uncertain).
    Rename {
        from: NoteLocator,
        to: String,
        tier: Option<String>,
    },
    /// Version and command surface summary.
    About,
}

impl Dialog {
    /// The dialog headline.
    #[must_use]
    pub fn title(&self) -> &'static str {
        match self {
            Self::NewNote { .. } => "New note",
            Self::Rename { .. } => "Rename or move note",
            Self::About => "About Flavite",
        }
    }
}

/// No-vault start screen state (FR-22): real open/create actions, a typed
/// path field, and the device-local remembered vault.
#[derive(Debug, Default)]
pub struct StartState {
    /// Typed vault root path.
    pub path: String,
    /// The remembered last vault, when device data is readable.
    pub last_vault: Option<std::path::PathBuf>,
    /// Honest failure text from the last open attempt.
    pub error: Option<String>,
}

/// Open the new-note dialog pre-filled for one unresolved link activation.
pub fn offer_create_for_link(raw: &str) -> Dialog {
    // Canonical default: vault-root relative, extensionless wikilink text
    // becomes a `.md` path. The user edits before anything is created.
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).take(200).collect();
    let path = if cleaned.to_ascii_lowercase().ends_with(".md") {
        cleaned
    } else {
        format!("{cleaned}.md")
    };
    Dialog::NewNote { path }
}

/// Render the active dialog as a modal card body.
pub fn view(app: &App) -> Element<'_, Message> {
    let Some(dialog) = &app.dialog else {
        return column![].into();
    };
    match dialog {
        Dialog::NewNote { path } => {
            let error = validate_new_note(app, path);
            let mut body = column![
                text("New note").size(style::HEADING),
                text("Exact vault-relative path; missing folders are created on save.")
                    .size(style::TEXT_SM)
                    .style(style::dim_text),
                text_input("folder/note.md", path)
                    .id(iced::widget::Id::new(NEW_NOTE_INPUT_ID))
                    .on_input(Message::DialogInput)
                    .on_submit(Message::DialogSubmit)
                    .padding(tab_strip::chip_padding()),
            ]
            .spacing(style::SPACING_SM);
            let submittable = error.is_none();
            if let Some(error) = &error {
                body = body.push(
                    text(error.clone())
                        .size(style::TEXT_SM)
                        .style(style::dim_text),
                );
            }
            body = body.push(
                row![
                    button(text("Create note"))
                        .padding(tab_strip::chip_padding())
                        .style(style::action())
                        .on_press_maybe(submittable.then_some(Message::DialogSubmit)),
                    cancel_button(),
                ]
                .spacing(style::SPACING_SM),
            );
            dialog_card(body)
        }
        Dialog::Rename { from, to, tier } => {
            let error = validate_rename(app, from, to);
            let mut body = column![
                text("Rename or move note").size(style::HEADING),
                text(format!("From: {}", from.components().join("/")))
                    .size(style::TEXT_SM)
                    .style(style::dim_text),
                text(match tier.as_deref() {
                    Some("T0") | Some("T1") | Some("T2") => {
                        "The system detected this rename with high confidence."
                    }
                    Some("T3") | Some("T4") => {
                        "This rename was detected with lower confidence — verify the identity."
                    }
                    _ => "Inbound links are rewritten through the durable rename journal.",
                })
                .size(style::TEXT_SM)
                .style(style::dim_text),
                text_input("new/folder/note.md", to)
                    .id(iced::widget::Id::new(RENAME_INPUT_ID))
                    .on_input(Message::DialogInput)
                    .on_submit(Message::DialogSubmit)
                    .padding(tab_strip::chip_padding()),
            ]
            .spacing(style::SPACING_SM);
            let submittable = error.is_none();
            if let Some(error) = &error {
                body = body.push(
                    text(error.clone())
                        .size(style::TEXT_SM)
                        .style(style::dim_text),
                );
            }
            body = body.push(
                row![
                    button(text("Rename"))
                        .padding(tab_strip::chip_padding())
                        .style(style::action())
                        .on_press_maybe(submittable.then_some(Message::DialogSubmit)),
                    cancel_button(),
                ]
                .spacing(style::SPACING_SM),
            );
            dialog_card(body)
        }
        Dialog::About => dialog_card(
            column![
                text("Flavite").size(style::HEADING),
                text(concat!(
                    "A local-first plain-text knowledge vault.\nVersion ",
                    env!("CARGO_PKG_VERSION"),
                ))
                .style(style::dim_text),
                cancel_button(),
            ]
            .spacing(style::SPACING_SM),
        ),
    }
}

fn cancel_button() -> iced::widget::Button<'static, Message> {
    button(text("Cancel"))
        .padding(tab_strip::chip_padding())
        .style(style::ghost())
        .on_press(Message::DialogCancel)
}

fn dialog_card(body: iced::widget::Column<'_, Message>) -> Element<'_, Message> {
    container(body)
        .padding(style::SPACING_LG)
        .max_width(520)
        .style(style::palette_card)
        .into()
}

/// Typed path validation for creation; `None` means submittable.
fn validate_new_note(app: &App, path: &str) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Some("Enter a vault-relative path.".into());
    }
    if trimmed.len() > MAX_INPUT_BYTES {
        return Some("Path is too long.".into());
    }
    if NoteLocator::from_components(trimmed.split('/')).is_err() {
        return Some("Path must be relative, without empty parts or traversal.".into());
    }
    let exists = app
        .note_candidates()
        .all()
        .iter()
        .any(|candidate| candidate.path().to_string() == trimmed);
    exists.then(|| "A note already exists at this path.".into())
}

/// Typed rename validation; `None` means submittable.
fn validate_rename(app: &App, from: &NoteLocator, to: &str) -> Option<String> {
    let trimmed = to.trim();
    if trimmed.is_empty() {
        return Some("Enter the new vault-relative path.".into());
    }
    if trimmed.len() > MAX_INPUT_BYTES {
        return Some("Path is too long.".into());
    }
    let Ok(target) = NoteLocator::from_components(trimmed.split('/')) else {
        return Some("Path must be relative, without empty parts or traversal.".into());
    };
    if &target == from {
        return Some("The note already has this path.".into());
    }
    let exists = app
        .note_candidates()
        .all()
        .iter()
        .any(|candidate| candidate.path().to_string() == trimmed);
    exists.then(|| "A note already exists at the destination.".into())
}

/// Render the no-vault start screen (FR-22).
pub fn start_view(app: &App) -> Element<'_, Message> {
    let mut body = column![
        text("Flavite").size(style::HEADING),
        text("Open a folder of Markdown notes to begin. A folder is adopted as a vault on first open; no note content is created for you.")
            .style(style::dim_text),
    ]
    .spacing(style::SPACING_SM);
    if let Some(error) = &app.start.error {
        body = body.push(text(error.clone()).style(style::dim_text));
    }
    body = body.push(
        row![
            text_input("Vault folder path", &app.start.path)
                .id(iced::widget::Id::new("start-vault-input"))
                .on_input(Message::VaultPathInput)
                .on_submit(Message::StartOpenVault(std::path::PathBuf::from(
                    app.start.path.trim()
                )))
                .padding(tab_strip::chip_padding())
                .width(Length::Fill),
            button(text("Open"))
                .padding(tab_strip::chip_padding())
                .style(style::action())
                .on_press_maybe((!app.start.path.trim().is_empty()).then(|| {
                    Message::StartOpenVault(std::path::PathBuf::from(app.start.path.trim()))
                })),
            button(text("Choose folder…"))
                .padding(tab_strip::chip_padding())
                .style(style::ghost())
                .on_press(Message::VaultDialog),
        ]
        .spacing(style::SPACING_SM),
    );
    if let Some(last) = app.start.last_vault.clone() {
        body = body.push(
            button(text(format!("Open last vault: {}", last.display())))
                .padding(tab_strip::chip_padding())
                .style(style::ghost())
                .on_press(Message::StartOpenVault(last)),
        );
    }
    container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .padding(style::SPACING_LG * 4.0)
        .style(style::content)
        .into()
}

/// Open the native file dialog for one note (W10). The portal dialog is
/// asynchronous; the task resolves into [`Message::NotePicked`]. On hosts
/// without a dialog service the typed-path paths remain available.
pub fn pick_note_file() -> iced::Task<Message> {
    iced::Task::perform(
        async move {
            rfd::AsyncFileDialog::new()
                .add_filter("Markdown", &["md", "markdown"])
                .pick_file()
                .await
                .and_then(|handle| handle.path().to_path_buf().into())
        },
        Message::NotePicked,
    )
}

/// Open the native folder dialog for vault selection (FR-22).
pub fn pick_vault_folder() -> iced::Task<Message> {
    iced::Task::perform(
        async move {
            rfd::AsyncFileDialog::new()
                .pick_folder()
                .await
                .and_then(|handle| handle.path().to_path_buf().into())
        },
        Message::VaultPicked,
    )
}
