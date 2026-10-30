//! Iced application shell and host adapter for Flavite.
//!
//! The shell renders persistent window chrome (sidebar, pane header, status
//! bar) plus a keyboard-driven command palette overlay. Styles come from
//! [`crate::style`]; every surface reads the theme palette.

use crate::operation::OperationPump;
use crate::palette::{keybinding, matching_commands};
use crate::pane::Pane;
use crate::settings::{RenameBehavior, Settings};
use crate::style::{self, Tone};
use crate::subscription;
use flavite_api::{
    BufferView, ExecutionResult, Flavite, Invocation, LocalResult, Origin, VaultConfig,
};
use flavite_editor::{EditorAction, EditorInteractionSnapshot, EditorWidget};
use iced::widget::{
    Space, button, column, container, opaque, row, rule, scrollable, stack, text, text_input,
};
use iced::{Element, Font, Length, Padding, Subscription, Task, alignment, keyboard};
use std::path::PathBuf;

/// Widget id of the palette query input, used to focus it when the overlay opens.
const PALETTE_INPUT_ID: &str = "palette-query-input";

/// Run-ready application state. `Flavite` owns the only Session and buffer authority.
pub struct App {
    flavite: Option<Flavite>,
    root: PathBuf,
    pane: Pane,
    palette_open: bool,
    palette_query: String,
    palette_selected: usize,
    settings: Settings,
    settings_dirty: bool,
    settings_valid: bool,
    window_size: iced::Size,
    status: String,
    recent_events: Vec<String>,
    operations: OperationPump,
    active_buffer: Option<u64>,
    editor_interaction: Option<EditorInteractionSnapshot>,
    reconcile_progress: Option<(u32, u32)>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Navigate(Pane),
    Edit(EditorAction),
    EditorInteraction(EditorInteractionSnapshot),
    EditorFocusRequested,
    EditorSelectionRequested {
        revision: u64,
        anchor: usize,
        focus: usize,
    },
    EditorSelectionAcknowledged(Option<EditorInteractionSnapshot>),
    Keyboard(keyboard::Event),
    PaletteOpen,
    PaletteClose,
    PaletteQuery(String),
    PaletteNext,
    PalettePrevious,
    PaletteSubmit,
    RunCommand(&'static str),
    ChangeRenameBehavior,
    ChangeRecovery(i8),
    ChangeVerification(i8),
    ChangeLogBudget(i8),
    SetIgnoreGlobs(String),
    SaveSettings,
    Pump,
    WindowEvent(iced::window::Event),
}

impl App {
    /// Open the configured vault and construct the iced shell.
    pub fn boot() -> Self {
        let root = std::env::args_os()
            .nth(1)
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("LATTICE_VAULT").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("target/ui-visual/app-vault"));
        Self::boot_from_root(root)
    }

    /// Construct the shell against an explicit vault root. Examples and
    /// integration harnesses use this to avoid process-global path state.
    pub fn boot_from_root(root: PathBuf) -> Self {
        let mut app = Self::empty_at(root.clone());
        match Flavite::open_root(root, VaultConfig::default()) {
            Ok(flavite) => app.attach_vault(flavite),
            Err(error) => app.status = format!("Vault unavailable: {error}"),
        }
        app
    }

    /// Build the shell around an already-authorized Session facade. This is
    /// also the fixture seam used by integration tests with an isolated Fs.
    pub fn from_flavite(flavite: Flavite) -> Self {
        let root = flavite.vault_ref().root.clone();
        let mut app = Self::empty_at(root);
        app.attach_vault(flavite);
        app
    }

    fn empty_at(root: PathBuf) -> Self {
        let status = format!("Opening {}…", root.display());
        Self {
            flavite: None,
            root,
            pane: Pane::Workspace,
            palette_open: false,
            palette_query: String::new(),
            palette_selected: 0,
            settings: Settings::default(),
            settings_dirty: false,
            settings_valid: true,
            window_size: iced::Size::new(1180.0, 760.0),
            status,
            recent_events: Vec::new(),
            operations: OperationPump::default(),
            active_buffer: None,
            editor_interaction: None,
            reconcile_progress: None,
        }
    }

    fn attach_vault(&mut self, flavite: Flavite) {
        self.root = flavite.vault_ref().root.clone();
        self.settings = match flavite.read_settings() {
            Ok(contents) => Settings::parse(contents.as_deref()).unwrap_or_else(|error| {
                self.settings_valid = false;
                self.status = format!("Settings could not be parsed; saving is disabled: {error}");
                Settings::default()
            }),
            Err(error) => {
                self.settings_valid = false;
                self.status = format!("Settings could not be read: {error}");
                Settings::default()
            }
        };
        self.flavite = Some(flavite);
        if self.settings_valid {
            self.status = "Vault ready".into();
        }
        self.run_command("note.open");
    }

    /// Return a fresh semantic snapshot of the current shell document. This
    /// is deliberately derived from the Session view on each publication;
    /// assistive technology never receives a stale copy owned by the widget.
    pub fn accessibility_document(&self) -> (String, String, Option<EditorInteractionSnapshot>) {
        let title = self
            .active_buffer
            .and_then(|buffer| {
                self.flavite
                    .as_ref()?
                    .get_buffer_view(buffer.into(), None)
                    .ok()
                    .map(|view| view.path.display().to_string())
            })
            .unwrap_or_else(|| format!("{:?}", self.pane));
        let contents = self
            .active_buffer
            .and_then(|buffer| {
                self.flavite
                    .as_ref()?
                    .get_buffer_view(buffer.into(), None)
                    .ok()
                    .map(|view| view.text)
            })
            .unwrap_or_else(|| self.status.clone());
        let interaction = self.editor_interaction.filter(|snapshot| {
            self.pane == Pane::Editor && self.active_buffer == Some(snapshot.buffer_id)
        });
        (title, contents, interaction)
    }

    /// Current legacy shell destination, projected only for its navigation
    /// tabs while W02 replaces this shell with the workspace model.
    pub fn accessibility_pane(&self) -> Pane {
        self.pane
    }

    /// Current logical window size, used to keep native accessibility bounds
    /// aligned with the shell after resize events.
    pub fn accessibility_window_size(&self) -> iced::Size {
        self.window_size
    }

    /// Bridge vault channels and operation replies without coupling completion to presentation.
    pub fn subscription(_state: &Self) -> Subscription<Message> {
        Subscription::batch([
            subscription::pump().map(|_| Message::Pump),
            keyboard::listen().map(Message::Keyboard),
            iced::window::events().map(|(_, event)| Message::WindowEvent(event)),
        ])
    }

    /// Apply one host event.
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Keyboard(event) => {
                if let Some(message) = self.keyboard_message(event) {
                    return self.update(message);
                }
            }
            Message::WindowEvent(iced::window::Event::Focused) => {
                if let Some(flavite) = &self.flavite
                    && let Err(error) = flavite.focus()
                {
                    self.status = format!("Vault focus reconcile failed: {error}");
                }
            }
            Message::WindowEvent(iced::window::Event::Unfocused) => {
                self.editor_interaction = None;
            }
            Message::WindowEvent(iced::window::Event::Resized(size)) => {
                self.window_size = size;
            }
            Message::WindowEvent(_) => {}
            Message::Navigate(pane) => {
                self.pane = pane;
                self.editor_interaction = None;
            }
            Message::Edit(action) => self.apply_editor_action(action),
            Message::EditorInteraction(snapshot) => {
                if self.pane == Pane::Editor && self.active_buffer == Some(snapshot.buffer_id) {
                    self.editor_interaction = Some(snapshot);
                }
            }
            Message::EditorFocusRequested => {
                let Some(buffer_id) = self.active_buffer else {
                    self.status = "Open a note before focusing the editor.".into();
                    return iced::Task::none();
                };
                let Some(view) = self
                    .flavite
                    .as_ref()
                    .and_then(|flavite| flavite.get_buffer_view(buffer_id.into(), None).ok())
                else {
                    self.status = "The current editor view is unavailable.".into();
                    return iced::Task::none();
                };
                return iced_runtime::task::widget(flavite_editor::focus_editor(
                    buffer_id,
                    view.revision,
                ))
                .map(Message::EditorSelectionAcknowledged);
            }
            Message::EditorSelectionRequested {
                revision,
                anchor,
                focus,
            } => {
                let active_buffer = self.active_buffer;
                let valid = active_buffer
                    .zip(self.editor_interaction)
                    .filter(|(buffer, interaction)| {
                        *buffer == interaction.buffer_id && interaction.revision == revision
                    })
                    .and_then(|(buffer, _)| {
                        self.flavite
                            .as_ref()?
                            .get_buffer_view(buffer.into(), None)
                            .ok()
                    })
                    .is_some_and(|view| {
                        anchor <= view.text.len()
                            && focus <= view.text.len()
                            && view.text.is_char_boundary(anchor)
                            && view.text.is_char_boundary(focus)
                    });
                if valid && let Some(buffer_id) = active_buffer {
                    return iced_runtime::task::widget(
                        flavite_editor::set_focused_editor_selection(
                            buffer_id, revision, anchor, focus,
                        ),
                    )
                    .map(Message::EditorSelectionAcknowledged);
                }
            }
            Message::EditorSelectionAcknowledged(Some(snapshot)) => {
                self.editor_interaction = Some(snapshot);
            }
            Message::EditorSelectionAcknowledged(None) => {
                self.status =
                    "Accessibility selection could not be applied to the current editor.".into();
            }
            Message::PaletteOpen => {
                self.palette_open = true;
                self.palette_selected = 0;
                return iced::widget::operation::focus(PALETTE_INPUT_ID);
            }
            Message::PaletteClose => self.palette_open = false,
            Message::PaletteQuery(query) => {
                self.palette_query = query;
                self.palette_selected = 0;
            }
            Message::PaletteNext => {
                let last = self.palette_match_count().saturating_sub(1);
                self.palette_selected = (self.palette_selected + 1).min(last);
            }
            Message::PalettePrevious => {
                self.palette_selected = self.palette_selected.saturating_sub(1);
            }
            Message::PaletteSubmit => {
                let selected = self.palette_selected;
                return self.run_palette_selection(selected);
            }
            Message::RunCommand(id) => {
                self.close_palette();
                self.run_command(id);
            }
            Message::ChangeRenameBehavior => {
                self.settings.rename_behavior = match self.settings.rename_behavior {
                    RenameBehavior::Rewrite => RenameBehavior::Alias,
                    RenameBehavior::Alias => RenameBehavior::None,
                    RenameBehavior::None => RenameBehavior::Rewrite,
                };
                self.settings_dirty = true;
            }
            Message::ChangeRecovery(delta) => {
                self.settings.recovery_interval_secs =
                    adjust(self.settings.recovery_interval_secs, delta, 1, 60);
                self.settings_dirty = true;
            }
            Message::ChangeVerification(delta) => {
                self.settings.verification_interval_mins =
                    adjust(self.settings.verification_interval_mins, delta, 1, 1440);
                self.settings_dirty = true;
            }
            Message::ChangeLogBudget(delta) => {
                self.settings.change_log_budget_mib =
                    adjust(self.settings.change_log_budget_mib, delta, 8, 1024);
                self.settings_dirty = true;
            }
            Message::SetIgnoreGlobs(value) => {
                self.settings.ignore_globs = value
                    .split('\n')
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned)
                    .collect();
                self.settings_dirty = true;
            }
            Message::SaveSettings => self.save_settings(),
            Message::Pump => self.pump(),
        }
        Task::none()
    }

    /// Draw the persistent shell: sidebar, pane header, content, status bar.
    pub fn view(&self) -> Element<'_, Message> {
        let shell = self.shell();
        if !self.palette_open {
            return shell;
        }
        let backdrop = button(Space::new().width(Length::Fill).height(Length::Fill))
            .style(style::backdrop)
            .on_press(Message::PaletteClose);
        stack![
            shell,
            opaque(backdrop),
            container(self.palette_card())
                .align_x(alignment::Horizontal::Center)
                .align_y(alignment::Vertical::Top)
                .padding(Padding {
                    top: 72.0,
                    ..Padding::default()
                })
                .width(Length::Fill)
                .height(Length::Fill)
        ]
        .into()
    }

    /// The chrome shared by every pane: navigation sidebar, pane header and
    /// content, and the status bar.
    fn shell(&self) -> Element<'_, Message> {
        column![
            row![self.sidebar(), self.main_region()].height(Length::Fill),
            rule::horizontal(1),
            self.status_bar(),
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    fn sidebar(&self) -> Element<'_, Message> {
        let vault_name = self.root.file_name().map_or_else(
            || self.root.display().to_string(),
            |name| name.to_string_lossy().to_string(),
        );
        let mut nav = column![];
        for pane in Pane::NAV {
            let selected = *pane == self.pane;
            nav = nav.push(
                button(text(pane.title()).width(Length::Fill))
                    .width(Length::Fill)
                    .padding(Padding {
                        top: 7.0,
                        bottom: 7.0,
                        left: 10.0,
                        right: 10.0,
                    })
                    .style(style::list_row(selected))
                    .on_press(Message::Navigate(*pane)),
            );
        }
        let (dot_tone, hint) = if self.flavite.is_some() {
            (Tone::Success, "Vault connected")
        } else {
            (Tone::Danger, "Vault unavailable")
        };
        container(
            column![
                text("LATTICE").size(style::BRAND).style(style::accent_text),
                text(vault_name).size(style::TEXT_SM).style(style::dim_text),
                text(self.root.display().to_string())
                    .size(style::TEXT_SM)
                    .style(style::dim_text),
                Space::new()
                    .width(Length::Fixed(0.0))
                    .height(Length::Fixed(style::SPACING_LG)),
                nav.spacing(style::SPACING_SM),
                button(
                    text("Command palette  Ctrl+P")
                        .width(Length::Fill)
                        .style(style::dim_text),
                )
                .width(Length::Fill)
                .padding(Padding {
                    top: 7.0,
                    bottom: 7.0,
                    left: 10.0,
                    right: 10.0,
                })
                .style(style::ghost())
                .on_press(Message::PaletteOpen),
                Space::new().width(Length::Fill).height(Length::Fill),
                row![
                    container(
                        Space::new()
                            .width(Length::Fixed(8.0))
                            .height(Length::Fixed(8.0))
                    )
                    .style(move |theme| style::dot(theme, dot_tone)),
                    text(hint).size(style::TEXT_SM).style(style::dim_text),
                ]
                .spacing(style::SPACING_SM)
                .align_y(alignment::Vertical::Center),
            ]
            .spacing(style::SPACING_SM),
        )
        .width(Length::Fixed(style::SIDEBAR_WIDTH))
        .height(Length::Fill)
        .padding(style::SPACING)
        .style(style::sidebar)
        .into()
    }

    fn main_region(&self) -> Element<'_, Message> {
        let content = match self.pane {
            Pane::Workspace => self.workspace_view(),
            Pane::Editor => self.document_view(false),
            Pane::Read => self.document_view(true),
            Pane::Graph => self.placeholder_view(
                "Knowledge graph",
                "The graph view is hosted in the same pane grid and will receive its live feed in the graph integration slice.",
            ),
            Pane::Settings => self.settings_view(),
        };
        column![self.pane_header(), rule::horizontal(1), content]
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn pane_header(&self) -> Element<'_, Message> {
        let mut header = row![
            text(self.pane.title())
                .size(style::HEADING)
                .width(Length::Fill),
        ]
        .spacing(style::SPACING)
        .align_y(alignment::Vertical::Center);
        header = match self.pane {
            Pane::Workspace => header.push(
                button(text("Refresh vault"))
                    .style(style::ghost())
                    .padding(chip_padding())
                    .on_press(Message::RunCommand("vault.reconcile")),
            ),
            Pane::Editor => {
                let mut actions = header;
                if let Some(view) = self.active_view() {
                    let tone = state_tone(&view.state);
                    let state = view.state;
                    actions = actions.push(
                        container(text(state).size(style::TEXT_SM))
                            .style(move |theme| style::chip(theme, tone))
                            .padding(chip_padding()),
                    );
                }
                actions.push(
                    button(text("Save note  Ctrl+S"))
                        .style(style::action())
                        .padding(chip_padding())
                        .on_press_maybe(
                            self.active_buffer
                                .is_some()
                                .then_some(Message::RunCommand("note.save")),
                        ),
                )
            }
            Pane::Read => header.push(
                button(text("Open in editor"))
                    .style(style::ghost())
                    .padding(chip_padding())
                    .on_press(Message::Navigate(Pane::Editor)),
            ),
            Pane::Graph | Pane::Settings => header,
        };
        container(header)
            .width(Length::Fill)
            .padding(Padding {
                top: 10.0,
                bottom: 10.0,
                left: style::SPACING_LG,
                right: style::SPACING_LG,
            })
            .into()
    }

    fn status_bar(&self) -> Element<'_, Message> {
        let mut right = row![].spacing(style::SPACING_SM);
        if let Some(view) = self.active_view() {
            let conflicted = view.state == "Conflicted";
            let tone = state_tone(&view.state);
            let state = view.state;
            right = right.push(
                container(text(state).size(style::TEXT_SM))
                    .style(move |theme| style::chip(theme, tone))
                    .padding(chip_padding()),
            );
            if conflicted {
                right = right.push(
                    container(text("Autosave paused").size(style::TEXT_SM))
                        .style(|theme| style::chip(theme, Tone::Danger))
                        .padding(chip_padding()),
                );
            }
        }
        let pending = self.operations.pending().count();
        if pending > 0 {
            right = right.push(
                container(text(format!("{pending} in flight")).size(style::TEXT_SM))
                    .style(|theme| style::chip(theme, Tone::Warning))
                    .padding(chip_padding()),
            );
        }
        if let Some((done, total)) = self.reconcile_progress {
            right = right.push(
                container(text(format!("Indexing {done}/{total}")).size(style::TEXT_SM))
                    .style(|theme| style::chip(theme, Tone::Warning))
                    .padding(chip_padding()),
            );
        }
        container(
            row![
                text(&self.status).size(style::TEXT_SM).width(Length::Fill),
                right,
            ]
            .spacing(style::SPACING),
        )
        .width(Length::Fill)
        .padding(Padding {
            top: 6.0,
            bottom: 6.0,
            left: style::SPACING_LG,
            right: style::SPACING_LG,
        })
        .style(style::status_bar)
        .into()
    }

    fn workspace_view(&self) -> Element<'_, Message> {
        let content = column![
            text("Your notes remain plain Markdown files. This shell hosts the same headless Session used by the CLI."),
            row![
                button(text("Open command palette  Ctrl+P"))
                    .style(style::action())
                    .padding(chip_padding())
                    .on_press(Message::PaletteOpen),
                button(text("Save active note  Ctrl+S"))
                    .style(style::ghost())
                    .padding(chip_padding())
                    .on_press_maybe(
                        self.active_buffer.is_some().then_some(Message::RunCommand("note.save")),
                    ),
            ]
            .spacing(style::SPACING),
            container(
                column![
                    text("Recent activity").size(style::TEXT_LG),
                    self.events_view(),
                ]
                .spacing(style::SPACING_SM),
            )
            .width(Length::Fill)
            .padding(style::SPACING)
            .style(style::card),
        ]
        .spacing(style::SPACING_LG);
        container(scrollable(content))
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(style::SPACING_LG)
            .style(style::content)
            .into()
    }

    fn document_view(&self, read_mode: bool) -> Element<'_, Message> {
        let Some(flavite) = &self.flavite else {
            return self.placeholder_view("Document", "The vault is unavailable.");
        };
        let Some(buffer_id) = self.active_buffer else {
            return self.placeholder_view(
                "Document",
                "Open demo.md through the command palette to attach a revisioned buffer view.",
            );
        };
        match flavite.get_buffer_view(buffer_id.into(), None) {
            Ok(view) => {
                let title = if read_mode { "Read" } else { "Editor" };
                let meta = row![
                    container(text(view.path.display().to_string()).size(style::TEXT_SM))
                        .style(|theme| style::chip(theme, Tone::Neutral))
                        .padding(chip_padding()),
                    container(text(format!("rev {}", view.revision)).size(style::TEXT_SM))
                        .style(|theme| style::chip(theme, Tone::Neutral))
                        .padding(chip_padding()),
                    container(text(title).size(style::TEXT_SM))
                        .style(|theme| style::chip(theme, Tone::Primary))
                        .padding(chip_padding()),
                ]
                .spacing(style::SPACING_SM);
                let body: Element<'_, Message> = if read_mode {
                    container(scrollable(
                        text(view.text).font(Font::MONOSPACE).size(style::TEXT),
                    ))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .padding(style::SPACING)
                    .style(style::card)
                    .into()
                } else {
                    EditorWidget::new(&view, Message::Edit)
                        .with_interaction(Message::EditorInteraction)
                        .into()
                };
                container(column![meta, body].spacing(style::SPACING))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .padding(style::SPACING_LG)
                    .style(style::content)
                    .into()
            }
            Err(error) => {
                self.placeholder_view("Document", &format!("Buffer view unavailable: {error}"))
            }
        }
    }

    fn apply_editor_action(&mut self, action: EditorAction) {
        let Some(flavite) = &self.flavite else {
            self.status = "The vault is unavailable".into();
            return;
        };
        let buffer_id = action.buffer_id;
        if action.navigation.is_some() {
            self.status = "Link navigation is unavailable in this editor view".into();
            return;
        }
        let result = flavite.execute_session(action.into_session_command());
        self.status = match result {
            flavite_api::session::SessionResult::Success(_) => "Editing".into(),
            other => format!("Edit failed: {other:?}"),
        };
        if let Ok(view) = flavite.get_buffer_view(buffer_id.into(), None) {
            if self.active_buffer != Some(view.buffer_id) {
                self.editor_interaction = None;
            }
            self.active_buffer = Some(view.buffer_id);
        }
    }

    fn placeholder_view(&self, title: &str, description: &str) -> Element<'_, Message> {
        container(
            column![
                text(title.to_owned()).size(style::TEXT_LG),
                text(description.to_owned()).style(style::dim_text),
            ]
            .spacing(style::SPACING_SM),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(style::SPACING_LG)
        .style(style::content)
        .center_x(Length::Fill)
        .align_y(alignment::Vertical::Top)
        .into()
    }

    fn palette_card(&self) -> Element<'_, Message> {
        let matches = self.palette_matches();
        let selected = self.palette_selected.min(matches.len().saturating_sub(1));
        let mut rows = column![];
        for (index, spec) in matches.iter().enumerate() {
            let is_selected = index == selected;
            rows = rows.push(
                button(
                    row![
                        text(spec.title).width(Length::Fill),
                        text(spec.id).size(style::TEXT_SM).style(style::dim_text),
                        text(keybinding(spec).unwrap_or_default())
                            .size(style::TEXT_SM)
                            .style(style::dim_text),
                    ]
                    .spacing(style::SPACING),
                )
                .width(Length::Fill)
                .padding(Padding {
                    top: 8.0,
                    bottom: 8.0,
                    left: 10.0,
                    right: 10.0,
                })
                .style(style::list_row(is_selected))
                .on_press(Message::RunCommand(spec.id)),
            );
        }
        let body: Element<'_, Message> = if matches.is_empty() {
            text("No matching commands").style(style::dim_text).into()
        } else {
            scrollable(rows).width(Length::Fill).into()
        };
        container(
            column![
                text_input("Type a command…", &self.palette_query)
                    .id(PALETTE_INPUT_ID)
                    .on_input(Message::PaletteQuery)
                    .size(style::TEXT_LG)
                    .padding(8),
                rule::horizontal(1),
                container(body).max_height(360).width(Length::Fill),
                row![
                    Space::new().width(Length::Fill).height(Length::Fixed(0.0)),
                    text("Up/Down navigate · Enter runs · Esc dismisses")
                        .size(style::TEXT_SM)
                        .style(style::dim_text),
                ],
            ]
            .spacing(style::SPACING_SM)
            .width(Length::Fill),
        )
        .width(Length::Fixed(style::PALETTE_WIDTH))
        .padding(style::SPACING)
        .style(style::palette_card)
        .into()
    }

    fn settings_view(&self) -> Element<'_, Message> {
        let rename = match self.settings.rename_behavior {
            RenameBehavior::Rewrite => "Rewrite links",
            RenameBehavior::Alias => "Add aliases",
            RenameBehavior::None => "Do not update links",
        };
        let globs = self.settings.ignore_globs.join("\n");
        let can_save = self.settings_valid && self.settings_dirty;
        let content =
            column![
                text(format!(
                    "Settings version {} · unknown fields are preserved",
                    self.settings.version
                ))
                .style(style::dim_text),
                container(
                    column![
                        setting_row(
                            "Rename behavior",
                            button(text(rename))
                                .style(style::ghost())
                                .padding(chip_padding())
                                .on_press(Message::ChangeRenameBehavior),
                        ),
                        setting_row(
                            "Recovery checkpoint",
                            stepper(
                                self.settings.recovery_interval_secs,
                                "seconds",
                                Message::ChangeRecovery,
                            ),
                        ),
                        setting_row(
                            "Content verification",
                            stepper(
                                self.settings.verification_interval_mins,
                                "minutes",
                                Message::ChangeVerification,
                            ),
                        ),
                        setting_row(
                            "Change log budget",
                            stepper(
                                self.settings.change_log_budget_mib,
                                "MiB",
                                Message::ChangeLogBudget,
                            ),
                        ),
                        text("Ignore globs · one per line"),
                        text_input("*.draft.md", &globs)
                            .on_input(Message::SetIgnoreGlobs)
                            .padding(8),
                        row![
                        button(text(if !self.settings_valid {
                            "Settings unavailable"
                        } else if self.settings_dirty {
                            "Save settings"
                        } else {
                            "Settings saved"
                        }))
                        .style(style::action())
                        .padding(chip_padding())
                        .on_press_maybe(can_save.then_some(Message::SaveSettings)),
                        text("External symlink targets remain disabled by the vault safety policy.")
                            .size(style::TEXT_SM)
                            .style(style::dim_text),
                    ]
                        .spacing(style::SPACING)
                        .align_y(alignment::Vertical::Center),
                    ]
                    .spacing(style::SPACING),
                )
                .width(Length::Fill)
                .padding(style::SPACING)
                .style(style::card),
            ]
            .spacing(style::SPACING);
        container(scrollable(content))
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(style::SPACING_LG)
            .style(style::content)
            .into()
    }

    fn events_view(&self) -> Element<'_, Message> {
        if self.recent_events.is_empty() {
            return text("No recent events")
                .size(style::TEXT_SM)
                .style(style::dim_text)
                .into();
        }
        let mut entries = column![];
        for event in self.recent_events.iter().rev().take(12) {
            entries = entries.push(text(event.as_str()).size(style::TEXT_SM));
        }
        entries.into()
    }

    fn palette_matches(&self) -> Vec<flavite_api::CommandSpec> {
        self.flavite.as_ref().map_or_else(Vec::new, |flavite| {
            matching_commands(flavite, &self.palette_query)
        })
    }

    fn palette_match_count(&self) -> usize {
        self.palette_matches().len()
    }

    /// Execute the palette row at `index` and dismiss the overlay.
    fn run_palette_selection(&mut self, index: usize) -> Task<Message> {
        let matches = self.palette_matches();
        let Some(spec_id) = matches
            .get(index.min(matches.len().saturating_sub(1)))
            .map(|spec| spec.id)
        else {
            self.close_palette();
            return Task::none();
        };
        self.close_palette();
        self.run_command(spec_id);
        Task::none()
    }

    fn close_palette(&mut self) {
        self.palette_open = false;
        self.palette_query.clear();
        self.palette_selected = 0;
    }

    /// Current revisioned view of the active buffer, if any.
    fn active_view(&self) -> Option<BufferView> {
        let buffer_id = self.active_buffer?;
        self.flavite
            .as_ref()?
            .get_buffer_view(buffer_id.into(), None)
            .ok()
    }

    fn run_command(&mut self, command_id: &'static str) {
        let Some(flavite) = &self.flavite else {
            self.status = "Open a vault before running commands".into();
            return;
        };
        let note = self.root.join("demo.md");
        let args = match command_id {
            "note.open" | "note.close" | "note.save" => serde_json::json!(note),
            "vault.reconcile" => serde_json::json!({ "scope": "full", "mode": "fast" }),
            "buffer.undo" | "buffer.redo" => {
                let Some(buffer_id) = self.active_buffer else {
                    self.status = format!("{command_id} requires an open note");
                    return;
                };
                let Ok(view) = flavite.get_buffer_view(buffer_id.into(), None) else {
                    self.status =
                        format!("{command_id} could not read the current buffer revision");
                    return;
                };
                serde_json::json!({ "buffer_id": buffer_id, "revision": view.revision })
            }
            _ => serde_json::Value::Null,
        };
        let request_id = flavite.command_registry().next_request_id();
        let invocation = Invocation::new(request_id, command_id, args, Origin::Gui);
        match flavite.execute(invocation) {
            ExecutionResult::Local(result) => match result {
                LocalResult::Success(success) => {
                    self.status = match command_id {
                        "note.open" => "Opened demo.md".into(),
                        "note.close" => "Note closed".into(),
                        "note.save" => "Note saved".into(),
                        "vault.reconcile" => "Vault refreshed".into(),
                        _ => format!("{command_id} completed"),
                    };
                    if command_id == "note.open" {
                        let opened = success
                            .data
                            .get("buffer_id")
                            .and_then(serde_json::Value::as_u64);
                        if self.active_buffer != opened {
                            self.editor_interaction = None;
                        }
                        self.active_buffer = opened;
                    } else if command_id == "note.close" {
                        self.active_buffer = None;
                        self.editor_interaction = None;
                    }
                }
                LocalResult::Error(error) => {
                    self.status = format!("{command_id} failed: {}", error.message);
                }
                LocalResult::Void => self.status = format!("{command_id} completed"),
            },
            ExecutionResult::Pending { request_id } => {
                self.operations.accept(request_id, command_id);
                self.status = format!("{command_id} pending · request {request_id}");
            }
            ExecutionResult::ActionRequired {
                request_id, code, ..
            } => {
                self.operations.accept(request_id, command_id);
                self.status = format!("{command_id} needs a decision: {code}");
            }
            ExecutionResult::Superseded { by_request, .. } => {
                self.status = format!("{command_id} superseded by request {by_request}");
            }
            ExecutionResult::Rejected(rejected) => {
                self.status = format!("{command_id} rejected: {:?}", rejected.reason);
            }
        }
    }

    fn save_settings(&mut self) {
        if !self.settings_valid {
            self.status = "Settings were not loaded safely; repair settings.toml first".into();
            return;
        }
        let Some(flavite) = &self.flavite else { return };
        match self
            .settings
            .to_toml()
            .and_then(|text| flavite.write_settings(&text).map_err(|e| e.to_string()))
        {
            Ok(()) => {
                self.settings_dirty = false;
                self.status = "Settings saved".into();
            }
            Err(error) => self.status = format!("Settings save failed: {error}"),
        }
    }

    fn pump(&mut self) {
        let Some(flavite) = &self.flavite else { return };
        flavite.session().pump();
        for event in flavite.drain_events() {
            if let flavite_api::Event::ReconcileProgress { done, total } = event {
                self.reconcile_progress = (done < total).then_some((done, total));
            }
            self.recent_events
                .push(format!("{}: {event:?}", event.event_type()));
            flavite.acknowledge_event(&event);
        }
        for terminal in self.operations.drain(flavite) {
            self.status = format!("Request {} · {}", terminal.request_id, terminal.result);
        }
        self.recent_events.truncate(100);
    }
}

/// Map a buffer state string to its chip tone.
fn state_tone(state: &str) -> Tone {
    match state {
        "Clean" => Tone::Success,
        "Dirty" => Tone::Warning,
        "Conflicted" => Tone::Danger,
        _ => Tone::Neutral,
    }
}

/// Padding shared by chip-sized controls.
fn chip_padding() -> Padding {
    Padding {
        top: 5.0,
        bottom: 5.0,
        left: 10.0,
        right: 10.0,
    }
}

fn setting_row<'a>(
    label: &'static str,
    control: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    row![text(label).width(Length::Fill), control.into(),]
        .spacing(style::SPACING)
        .align_y(alignment::Vertical::Center)
        .into()
}

fn stepper<'a>(value: u64, unit: &'static str, change: fn(i8) -> Message) -> Element<'a, Message> {
    row![
        button(text("−"))
            .style(style::ghost())
            .padding(chip_padding())
            .on_press(change(-1)),
        text(format!("{value} {unit}"))
            .width(Length::Fixed(96.0))
            .align_x(alignment::Horizontal::Center),
        button(text("+"))
            .style(style::ghost())
            .padding(chip_padding())
            .on_press(change(1)),
    ]
    .spacing(style::SPACING_SM)
    .align_y(alignment::Vertical::Center)
    .into()
}

impl App {
    /// Resolve a raw keyboard event against the shell state (palette overlay
    /// shortcuts and navigation keys).
    fn keyboard_message(&self, event: keyboard::Event) -> Option<Message> {
        let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
            return None;
        };
        if modifiers.command() {
            let keyboard::Key::Character(character) = key.as_ref() else {
                return None;
            };
            if character.eq_ignore_ascii_case("p") {
                return Some(if self.palette_open {
                    Message::PaletteClose
                } else {
                    Message::PaletteOpen
                });
            }
            if character.eq_ignore_ascii_case("s") {
                return Some(Message::RunCommand("note.save"));
            }
            return None;
        }
        if !self.palette_open {
            return None;
        }
        let keyboard::Key::Named(named) = key.as_ref() else {
            return None;
        };
        match named {
            keyboard::key::Named::Escape => Some(Message::PaletteClose),
            keyboard::key::Named::ArrowDown => Some(Message::PaletteNext),
            keyboard::key::Named::ArrowUp => Some(Message::PalettePrevious),
            keyboard::key::Named::Enter => Some(Message::PaletteSubmit),
            _ => None,
        }
    }
}

fn adjust(value: u64, delta: i8, minimum: u64, maximum: u64) -> u64 {
    (value as i64 + i64::from(delta)).clamp(minimum as i64, maximum as i64) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_api::channels::{Notice, Pulse};

    fn empty_app() -> App {
        App {
            flavite: None,
            root: PathBuf::from("fixture-vault"),
            pane: Pane::Workspace,
            palette_open: false,
            palette_query: String::new(),
            palette_selected: 0,
            settings: Settings::default(),
            settings_dirty: false,
            settings_valid: true,
            window_size: iced::Size::new(1180.0, 760.0),
            status: "Ready".into(),
            recent_events: Vec::new(),
            operations: OperationPump::default(),
            active_buffer: None,
            editor_interaction: None,
            reconcile_progress: None,
        }
    }

    #[test]
    fn app_state_navigates_and_tracks_setting_edits_without_a_ui_runtime() {
        let mut app = empty_app();
        let _ = app.update(Message::Navigate(Pane::Settings));
        assert_eq!(app.pane, Pane::Settings);
        let _ = app.update(Message::ChangeRecovery(1));
        assert_eq!(app.settings.recovery_interval_secs, 3);
        assert!(app.settings_dirty);
    }

    #[test]
    fn palette_overlay_keeps_its_query_until_a_command_runs() {
        let mut app = empty_app();
        let _ = app.update(Message::PaletteOpen);
        assert!(app.palette_open);
        let _ = app.update(Message::PaletteQuery("save".into()));
        assert_eq!(app.palette_query, "save");
        let _ = app.update(Message::PaletteClose);
        assert!(!app.palette_open);
        assert_eq!(app.palette_query, "save", "Esc preserves the query");
        let _ = app.update(Message::RunCommand("note.save"));
        assert!(app.palette_query.is_empty());
        assert_eq!(app.palette_selected, 0);
    }

    #[test]
    fn palette_selection_walks_without_leaving_bounds() {
        let mut app = empty_app();
        app.palette_selected = 0;
        let _ = app.update(Message::PaletteNext);
        assert_eq!(app.palette_selected, 0, "no matches without a vault");
        app.palette_selected = 3;
        let _ = app.update(Message::PalettePrevious);
        assert_eq!(app.palette_selected, 2);
        let _ = app.update(Message::PalettePrevious);
        assert_eq!(app.palette_selected, 1);
    }

    #[test]
    fn running_a_command_dismisses_the_palette() {
        let mut app = empty_app();
        let _ = app.update(Message::PaletteOpen);
        let _ = app.update(Message::RunCommand("note.save"));
        assert!(!app.palette_open);
        assert!(app.status.contains("Open a vault"));
    }

    #[test]
    fn save_is_not_reported_as_completed_when_no_vault_is_open() {
        let mut app = empty_app();
        app.run_command("note.save");
        assert!(app.status.contains("Open a vault"));
        assert_eq!(app.operations.pending().count(), 0);
    }

    #[test]
    fn pump_presents_versioned_api_events_once() {
        use flavite_buffer::manager::BufferManager;
        use flavite_fs::{Fs, MemFs};
        use flavite_index::IndexActor;
        use flavite_vault::actor::VaultActorBuilder;
        use flavite_vault::channels::ChannelSet;
        use flavite_vault::session::Session;
        let fs = MemFs::new();
        let root = PathBuf::from("/test/ui-host-vault");
        fs.create_dir_all(&root).unwrap();
        let temp = tempfile::tempdir().expect("fixture cache directory");
        let db = temp.path().join("ui-host.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let identity =
            flavite_api::VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![2, 4, 6]);
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs) as Box<dyn Fs + Send + Sync>)
            .with_identity(identity.clone())
            .with_index(IndexActor::new(&db, 1).unwrap())
            .build()
            .unwrap()
            .start()
            .unwrap();
        let channels = ChannelSet::new();
        let session = Session::new(
            identity.clone(),
            actor,
            BufferManager::new(),
            channels.pulse,
            channels.notice,
            VaultConfig::default(),
        );
        let flavite = Flavite::open(identity, session, VaultConfig::default()).unwrap();
        let mut app = empty_app();
        app.flavite = Some(flavite);
        app.root = root.clone();
        let flavite = app.flavite.as_ref().expect("flavite attached");
        let _ = flavite.session().pulse_channel().send(Pulse::Invalidate {
            note: flavite_api::channels::NoteRef::new(root.join("demo.md")),
        });
        flavite
            .session()
            .notice_channel()
            .send(Notice::RecoveryOffer {
                note: flavite_api::channels::NoteRef::new(root.join("demo.md")),
            });
        flavite.session().notice_channel().send(Notice::Conflict {
            note: flavite_api::channels::NoteRef::new(root.join("other.md")),
        });
        app.pump();
        let presented = app.recent_events.clone();
        assert!(
            presented.iter().any(|event| event.contains("invalidate")),
            "pulse hint should be presented: {presented:?}"
        );
        assert!(
            presented
                .iter()
                .any(|event| event.contains("recovery_offer")),
            "recovery offer should be presented: {presented:?}"
        );
        assert!(
            presented.iter().any(|event| event.contains("conflict")),
            "conflict notice should be presented: {presented:?}"
        );
        // Lossless policy: a second pump re-delivers nothing, and the
        // presentation acks let the channel retire the pending keys.
        app.pump();
        assert_eq!(app.recent_events.len(), presented.len());
    }

    #[test]
    fn gui_and_cli_hosts_reach_the_same_document_state_for_note_open() {
        use flavite_buffer::manager::BufferManager;
        use flavite_fs::{Fs, MemFs};
        use flavite_index::IndexActor;
        use flavite_vault::actor::VaultActorBuilder;
        use flavite_vault::channels::ChannelSet;
        use flavite_vault::session::Session;

        fn fixture(label: &str) -> (Flavite, PathBuf, tempfile::TempDir) {
            let fs = MemFs::new();
            let root = PathBuf::from(format!("/test/host-parity-{label}"));
            fs.create_dir_all(&root).unwrap();
            fs.write_new(&root.join("demo.md"), b"# Shared state\n\nbody\n")
                .unwrap();
            let temp = tempfile::tempdir().unwrap();
            let db = temp.path().join("index.sqlite3");
            drop(flavite_index::open_or_init(&db).unwrap());
            let identity =
                flavite_api::VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![8, 6, 7]);
            let actor = VaultActorBuilder::new()
                .with_fs(Box::new(fs) as Box<dyn Fs + Send + Sync>)
                .with_identity(identity.clone())
                .with_index(IndexActor::new(&db, 1).unwrap())
                .build()
                .unwrap()
                .start()
                .unwrap();
            let channels = ChannelSet::new();
            let session = Session::new(
                identity.clone(),
                actor,
                BufferManager::new(),
                channels.pulse,
                channels.notice,
                VaultConfig::default(),
            );
            let flavite = Flavite::open(identity, session, VaultConfig::default()).unwrap();
            (flavite, root, temp)
        }

        let (gui_flavite, gui_root, _gui_cache) = fixture("gui");
        let mut app = empty_app();
        app.flavite = Some(gui_flavite);
        app.root = gui_root.clone();
        let _ = app.update(Message::RunCommand("note.open"));
        let gui_buffer = app.active_buffer.expect("GUI host opens the note");
        let gui_view = app
            .flavite
            .as_ref()
            .unwrap()
            .get_buffer_view(gui_buffer.into(), None)
            .unwrap();
        app.pane = Pane::Editor;
        app.editor_interaction = Some(EditorInteractionSnapshot {
            buffer_id: gui_view.buffer_id,
            revision: gui_view.revision,
            caret: 2,
            selection: (2, 2),
            focused: true,
            composing: false,
        });
        let action = accesskit::ActionRequest {
            action: accesskit::Action::SetTextSelection,
            target_tree: accesskit::TreeId::ROOT,
            target_node: crate::accessibility::tree::EDITOR,
            data: Some(accesskit::ActionData::SetTextSelection(
                accesskit::TextSelection {
                    anchor: accesskit::TextPosition {
                        node: crate::accessibility::tree::EDITOR_TEXT,
                        character_index: 2,
                    },
                    focus: accesskit::TextPosition {
                        node: crate::accessibility::tree::EDITOR_TEXT,
                        character_index: 8,
                    },
                },
            )),
        };
        match crate::accessibility::route_action(&app, action).unwrap() {
            Message::EditorSelectionRequested {
                revision,
                anchor,
                focus,
            } => {
                assert_eq!(revision, gui_view.revision);
                assert_eq!((anchor, focus), (2, 8));
            }
            _ => panic!("native selection must use the editor message path"),
        }
        let edit = EditorAction::replace(
            &flavite_editor::EditorView::from(&gui_view),
            gui_view.text.len(),
            gui_view.text.len(),
            " from shell editor".into(),
        );
        app.apply_editor_action(edit);
        let edited_gui_view = app
            .flavite
            .as_ref()
            .unwrap()
            .get_buffer_view(gui_buffer.into(), None)
            .unwrap();
        assert!(edited_gui_view.text.ends_with(" from shell editor"));

        let (cli_flavite, cli_root, _cli_cache) = fixture("cli");
        let request_id = cli_flavite.command_registry().next_request_id();
        let result = cli_flavite.execute(Invocation::new(
            request_id,
            "note.open",
            serde_json::json!(cli_root.join("demo.md")),
            Origin::Cli,
        ));
        let cli_buffer = match result {
            ExecutionResult::Local(LocalResult::Success(success)) => success
                .data
                .get("buffer_id")
                .and_then(serde_json::Value::as_u64)
                .unwrap(),
            other => panic!("CLI host failed to open note: {other:?}"),
        };
        let cli_view = cli_flavite
            .get_buffer_view(cli_buffer.into(), None)
            .unwrap();

        assert_eq!(gui_view.text, cli_view.text);
        assert_eq!(gui_view.state, cli_view.state);
        assert_eq!(gui_view.path.file_name(), cli_view.path.file_name());
    }

    #[test]
    fn palette_keys_only_respond_while_the_overlay_is_open() {
        let app = empty_app();
        let press = |named: keyboard::key::Named| keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(named),
            modified_key: keyboard::Key::Named(named),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        };
        assert!(
            app.keyboard_message(press(keyboard::key::Named::Escape))
                .is_none()
        );
        let mut open = empty_app();
        open.palette_open = true;
        assert!(matches!(
            open.keyboard_message(press(keyboard::key::Named::Escape)),
            Some(Message::PaletteClose)
        ));
        assert!(matches!(
            open.keyboard_message(press(keyboard::key::Named::ArrowDown)),
            Some(Message::PaletteNext)
        ));
        assert!(matches!(
            open.keyboard_message(press(keyboard::key::Named::Enter)),
            Some(Message::PaletteSubmit)
        ));
    }

    #[test]
    fn command_shortcuts_map_save_and_palette_keys() {
        let press_ctrl = |character: &'static str| keyboard::Event::KeyPressed {
            key: keyboard::Key::Character(character.into()),
            modified_key: keyboard::Key::Character(character.into()),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::COMMAND,
            text: None,
            repeat: false,
        };
        let app = empty_app();
        assert!(matches!(
            app.keyboard_message(press_ctrl("s")),
            Some(Message::RunCommand("note.save"))
        ));
        assert!(matches!(
            app.keyboard_message(press_ctrl("p")),
            Some(Message::PaletteOpen)
        ));
        let mut open = empty_app();
        open.palette_open = true;
        assert!(matches!(
            open.keyboard_message(press_ctrl("p")),
            Some(Message::PaletteClose)
        ));
    }
}
