//! Modal dialogs (W10): explicit note creation, rename/move with strategy
//! preview, about, and the no-vault start screen (FR-22).
//!
//! Dialogs are ordinary interface state reduced through `App::update`; a
//! dialog never mutates a note on hover or selection, only on explicit
//! submit. Creation goes through the core `note.create` command, renames
//! through the journaled `note.rename` — no GUI-side file mutation.

pub mod close;
pub mod conflict;
pub mod merge;
pub mod recovery;

use crate::app::{App, Message};
use crate::style;
use crate::workspace::chrome::Icon;
use crate::workspace::{chrome, tab_strip};
use flavite_workspace::NoteLocator;
use flavite_workspace::TabId;
use iced::widget::{
    Space, button, column, container, image, mouse_area, row, scrollable, text, text_input,
};
use iced::{Element, Length, alignment};

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
    /// Close decision for a final unsaved view (W12).
    Close { tab: TabId },
    /// Conflict resolution for a buffer whose file changed on disk (W12).
    Conflict { tab: TabId },
    /// A prepared three-way merge awaiting explicit confirmation (W12).
    Merge { tab: TabId, prepared: PreparedMerge },
    /// A retained recovery snapshot was found for a note (W12).
    RecoveryOffer { path: String },
    /// Quit stalled on unresolved work; the user chooses explicitly (W12).
    ShutdownTimeout { concern: String },
}

/// A core-prepared merge receipt plus the merged text it carries (W12).
#[derive(Debug, Clone)]
pub struct PreparedMerge {
    /// Exact native note path.
    pub path: String,
    /// The buffer id from the `MergeReady` receipt.
    pub buffer_id: u64,
    /// The expected buffer revision from the receipt.
    pub revision: u64,
    /// The disk hash the merge was prepared against (hex).
    pub disk_hash: String,
    /// The merged document text awaiting confirmation.
    pub merged: String,
    /// Structured conflict hunks for the compare view.
    pub conflicts: Vec<String>,
}

impl Dialog {
    /// The stable kind name for semantic snapshots (W12 harness).
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::NewNote { .. } => "new_note",
            Self::Rename { .. } => "rename",
            Self::About => "about",
            Self::Close { .. } => "close",
            Self::Conflict { .. } => "conflict",
            Self::Merge { .. } => "merge",
            Self::RecoveryOffer { .. } => "recovery_offer",
            Self::ShutdownTimeout { .. } => "shutdown_timeout",
        }
    }

    /// The dialog headline.
    #[must_use]
    pub fn title(&self) -> &'static str {
        match self {
            Self::NewNote { .. } => "New note",
            Self::Rename { .. } => "Rename or move note",
            Self::About => "About Flavite",
            Self::Close { .. } => "Unsaved changes",
            Self::Conflict { .. } => "Conflict",
            Self::Merge { .. } => "Confirm merge",
            Self::RecoveryOffer { .. } => "Recover unsaved changes",
            Self::ShutdownTimeout { .. } => "Quit with unresolved work?",
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
    let metrics = app.appearance.metrics();
    let Some(dialog) = &app.dialog else {
        return column![].into();
    };
    match dialog {
        Dialog::NewNote { path } => {
            let error = validate_new_note(app, path);
            let mut body = column![
                text("New note").size(metrics.heading),
                text("Exact vault-relative path; missing folders are created on save.")
                    .size(metrics.text_sm)
                    .style(style::dim_text),
                text_input("folder/note.md", path)
                    .id(iced::widget::Id::new(NEW_NOTE_INPUT_ID))
                    .on_input(Message::DialogInput)
                    .on_submit(Message::DialogSubmit)
                    .padding(tab_strip::chip_padding()),
            ]
            .spacing(metrics.spacing_sm);
            let submittable = error.is_none();
            if let Some(error) = &error {
                body = body.push(
                    text(error.clone())
                        .size(metrics.text_sm)
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
                .spacing(metrics.spacing_sm),
            );
            dialog_card(*metrics, body)
        }
        Dialog::Rename { from, to, tier } => {
            let error = validate_rename(app, from, to);
            let mut body = column![
                text("Rename or move note").size(metrics.heading),
                text(format!("From: {}", from.components().join("/")))
                    .size(metrics.text_sm)
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
                .size(metrics.text_sm)
                .style(style::dim_text),
                text_input("new/folder/note.md", to)
                    .id(iced::widget::Id::new(RENAME_INPUT_ID))
                    .on_input(Message::DialogInput)
                    .on_submit(Message::DialogSubmit)
                    .padding(tab_strip::chip_padding()),
            ]
            .spacing(metrics.spacing_sm);
            let submittable = error.is_none();
            if let Some(error) = &error {
                body = body.push(
                    text(error.clone())
                        .size(metrics.text_sm)
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
                .spacing(metrics.spacing_sm),
            );
            dialog_card(*metrics, body)
        }
        Dialog::About => about_card(*metrics),
        Dialog::Close { tab } => close::view(app, *tab, *metrics),
        Dialog::Conflict { tab } => conflict::view(app, *tab, *metrics),
        Dialog::Merge { prepared, .. } => merge::view(prepared, *metrics),
        Dialog::RecoveryOffer { path } => recovery::view(path, *metrics),
        Dialog::ShutdownTimeout { concern } => close::shutdown_view(app, concern, *metrics),
    }
}

fn cancel_button() -> iced::widget::Button<'static, Message> {
    button(text("Cancel"))
        .padding(tab_strip::chip_padding())
        .style(style::ghost())
        .on_press(Message::DialogCancel)
}

/// The flavite logo, decoded once from the packaged icon. The UI's iced
/// build ships without image codecs, so the embedded PNG decodes through
/// the API's bounded decoder and caches the RGBA handle.
fn logo_handle() -> Option<&'static iced::widget::image::Handle> {
    static HANDLE: std::sync::OnceLock<Option<iced::widget::image::Handle>> =
        std::sync::OnceLock::new();
    HANDLE
        .get_or_init(|| {
            let png = include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../packaging/icons/flavite.png"
            ));
            let decoded = flavite_api::decode_image(png).ok()?;
            Some(iced::widget::image::Handle::from_rgba(
                decoded.width,
                decoded.height,
                decoded.rgba.to_vec(),
            ))
        })
        .as_ref()
}

/// Project constants for the About card.
const ABOUT_WEBSITE: &str = "https://flavite.lexbytes.org";
const ABOUT_GITHUB: &str = "https://github.com/lexbity/flavite";
const ABOUT_LICENSE: &str = "https://github.com/lexbity/flavite/blob/main/LICENSE";
const ABOUT_CONTRIBUTORS: &str = "https://github.com/lexbity/flavite/blob/main/AUTHORS.md";

/// The About card: logo, version, license, and the project links. Every
/// link row opens the hard-coded URL through the OS handler.
fn about_card(metrics: style::Metrics) -> Element<'static, Message> {
    let link = |label: &'static str, url: &'static str| {
        button(
            row![
                text(label).size(metrics.text),
                Space::new().width(Length::Fill),
                text(url.trim_start_matches("https://"))
                    .size(metrics.text_sm)
                    .style(style::dim_text),
            ]
            .spacing(metrics.spacing)
            .align_y(alignment::Vertical::Center),
        )
        .width(Length::Fill)
        .padding(tab_strip::chip_padding())
        .style(style::list_row(false))
        .on_press(Message::OpenExternalUrl(url.to_owned()))
    };
    let mut body = column![]
        .spacing(metrics.spacing_sm)
        .align_x(alignment::Horizontal::Center);
    if let Some(logo) = logo_handle() {
        body = body.push(
            image(logo.clone())
                .width(Length::Fixed(96.0))
                .height(Length::Fixed(96.0)),
        );
    }
    body = body.push(text("Flavite").size(metrics.heading));
    body = body.push(
        text(concat!(
            "A local-first plain-text knowledge vault.\nVersion ",
            env!("CARGO_PKG_VERSION"),
            " · Apache-2.0"
        ))
        .style(style::dim_text),
    );
    let mut links = column![].spacing(2);
    for (label, url) in [
        ("Website", ABOUT_WEBSITE),
        ("GitHub", ABOUT_GITHUB),
        ("License", ABOUT_LICENSE),
        ("Contributors", ABOUT_CONTRIBUTORS),
    ] {
        links = links.push(link(label, url));
    }
    body = body.push(links);
    body = body.push(cancel_button());
    dialog_card(metrics, body)
}

fn dialog_card(
    metrics: style::Metrics,
    body: iced::widget::Column<'_, Message>,
) -> Element<'_, Message> {
    let _ = metrics;
    // The shortcut list can exceed a small window; the card scrolls
    // instead of clipping (observed in the appearance-matrix evidence).
    container(scrollable(body).width(Length::Fill).height(Length::Fill))
        .padding(style::SPACING_LG)
        .max_width(560)
        .max_height(720)
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
    let metrics = app.appearance.metrics();
    let mut body = column![
        text("Flavite").size(metrics.heading),
        text("Open a folder of Markdown notes to begin. A folder is adopted as a vault on first open; no note content is created for you.")
            .style(style::dim_text),
    ]
    .spacing(metrics.spacing_sm);
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
        .spacing(metrics.spacing_sm),
    );
    if let Some(last) = app.start.last_vault.clone() {
        body = body.push(
            button(text(format!("Open last vault: {}", last.display())))
                .padding(tab_strip::chip_padding())
                .style(style::ghost())
                .on_press(Message::StartOpenVault(last)),
        );
    }
    // The start screen owns the window before a vault exists, so it carries
    // the client-side chrome too: controls top-right, and the whole surface
    // as the drag region (its buttons and inputs capture their own presses).
    let controls = container(
        row![
            chrome::themed_icon_button(
                *metrics,
                Icon::Minimize,
                "Minimize window",
                Some(Message::WindowMinimize),
                None
            ),
            chrome::themed_icon_button(
                *metrics,
                if app.window_maximized() {
                    Icon::Restore
                } else {
                    Icon::Maximize
                },
                if app.window_maximized() {
                    "Restore window size"
                } else {
                    "Maximize window"
                },
                Some(Message::WindowToggleMaximize),
                None
            ),
            chrome::themed_icon_button(
                *metrics,
                Icon::Close,
                "Close window",
                Some(Message::WindowClose),
                None
            ),
        ]
        .spacing(metrics.spacing_sm),
    )
    .width(Length::Fill)
    .align_x(alignment::Horizontal::Right)
    .padding([4, 8]);
    iced::widget::stack![
        mouse_area(
            container(body)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .padding(metrics.spacing_lg * 4.0)
                .style(style::content)
        )
        .on_press(Message::WindowDrag)
        .on_double_click(Message::WindowToggleMaximize),
        controls,
    ]
    .into()
}

/// Open the native file dialog for one note (W10). The portal dialog is
/// asynchronous; the task resolves into [`Message::NotePicked`]. On hosts
/// without a dialog service the typed-path paths remain available.
pub fn pick_note_file() -> iced::Task<Message> {
    iced::Task::perform(
        async move {
            // The editor opens any text file regardless of extension, so the
            // dialog offers an all-files filter alongside the Markdown one.
            rfd::AsyncFileDialog::new()
                .add_filter("Markdown", &["md", "markdown"])
                .add_filter("All files", &["*"])
                .pick_file()
                .await
                .and_then(|handle| handle.path().to_path_buf().into())
        },
        Message::NotePicked,
    )
}

/// Open the native file dialog for a standard theme package (xpi themes).
/// The task resolves into [`crate::app::Message::ThemePackagePicked`].
pub fn pick_theme_file() -> iced::Task<Message> {
    iced::Task::perform(
        async move {
            rfd::AsyncFileDialog::new()
                .add_filter("Theme package", &["xpi", "zip"])
                .pick_file()
                .await
                .and_then(|handle| handle.path().to_path_buf().into())
        },
        crate::app::Message::ThemePackagePicked,
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
