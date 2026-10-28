//! Minimal end-to-end host for the custom editor and Session save path.

use flavite_api::session::{CommandResult, SessionCommand, SessionResult};
use flavite_api::{BufferView, ExecutionResult, Flavite, LocalResult, VaultConfig};
use flavite_editor::{EditorAction, EditorWidget, NoteCandidate};
use iced::widget::{button, column, container, text};
use iced::{Element, Length, Subscription, time};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn main() -> iced::Result {
    iced::application(App::boot, App::update, App::view)
        .title("Flavite editor demo")
        .subscription(App::subscription)
        .window_size(iced::Size::new(900.0, 650.0))
        .run()
}

#[derive(Debug, Clone)]
enum Message {
    Edit(EditorAction),
    Save,
    Pump,
}

struct App {
    flavite: Option<Flavite>,
    view: Option<BufferView>,
    candidates: Vec<NoteCandidate>,
    status: String,
    last_edit: Instant,
    pending_save: Option<u64>,
}

impl App {
    fn boot() -> Self {
        match Self::open() {
            Ok(app) => app,
            Err(error) => Self {
                flavite: None,
                view: None,
                candidates: Vec::new(),
                status: format!("Could not open demo vault: {error}"),
                last_edit: Instant::now(),
                pending_save: None,
            },
        }
    }

    fn open() -> Result<Self, String> {
        let root = std::env::args()
            .nth(1)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("flavite-demo-vault"));
        std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        let root = std::fs::canonicalize(root).map_err(|error| error.to_string())?;
        let note = root.join("demo.md");
        if !note.exists() {
            std::fs::write(
                &note,
                "# Flavite editor demo\n\nType here. Changes autosave after one idle second.\n",
            )
            .map_err(|error| error.to_string())?;
        }
        let config = VaultConfig {
            watcher_enabled: false,
            ..VaultConfig::default()
        };
        let flavite =
            Flavite::open_root(root.clone(), config).map_err(|error| error.to_string())?;
        let path = root.join("demo.md");
        match flavite.execute_session(SessionCommand::OpenNote { path }) {
            SessionResult::Success(CommandResult::NoteOpened { buffer_id, .. }) => {
                let view = flavite
                    .get_buffer_view(buffer_id, None)
                    .map_err(|error| error.to_string())?;
                let candidates = flavite
                    .completion_candidates()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|candidate| NoteCandidate {
                        target: candidate.target,
                        title: candidate.title,
                        aliases: candidate.aliases,
                        stable_id: candidate.stable_id,
                    })
                    .collect();
                Ok(Self {
                    flavite: Some(flavite),
                    view: Some(view),
                    candidates,
                    status: "Ready".into(),
                    last_edit: Instant::now(),
                    pending_save: None,
                })
            }
            other => Err(format!("open note did not complete: {other:?}")),
        }
    }

    fn subscription(_state: &Self) -> Subscription<Message> {
        time::every(Duration::from_millis(50)).map(|_| Message::Pump)
    }

    fn update(&mut self, message: Message) {
        match message {
            Message::Edit(action) => {
                let Some(flavite) = &self.flavite else { return };
                if let Some(navigation) = &action.navigation {
                    let Some(path) =
                        safe_note_target(&flavite.vault_ref().root, &navigation.target)
                    else {
                        self.status = "Link target is not a safe vault-relative note path".into();
                        return;
                    };
                    let target = navigation.target.clone();
                    let invocation = action.into_link_invocation(
                        flavite.command_registry().next_request_id(),
                        path.clone(),
                    );
                    let Some(invocation) = invocation else { return };
                    match flavite.execute(invocation) {
                        ExecutionResult::Local(LocalResult::Success(_)) => {
                            self.status = format!("Opened link: {target}");
                            if let Some(buffer_id) = flavite.session().get_buffer_id(&path) {
                                self.refresh(buffer_id.as_u64());
                            }
                        }
                        other => self.status = format!("Could not open link: {other:?}"),
                    }
                    return;
                }
                let buffer_id = action.buffer_id;
                if action.command_id == "link.insert" {
                    let invocation =
                        action.into_invocation(flavite.command_registry().next_request_id());
                    let Some(invocation) = invocation else { return };
                    match flavite.execute(invocation) {
                        ExecutionResult::Local(LocalResult::Success(_)) => {
                            self.status = "Link inserted".into();
                            self.refresh(buffer_id);
                        }
                        other => self.status = format!("Link insertion failed: {other:?}"),
                    }
                    self.last_edit = Instant::now();
                    return;
                }
                let result = flavite.execute_session(action.into_session_command());
                self.status = match result {
                    SessionResult::Success(_) => "Editing".into(),
                    other => format!("Edit failed: {other:?}"),
                };
                self.refresh(buffer_id);
                self.last_edit = Instant::now();
            }
            Message::Save => self.save(),
            Message::Pump => self.pump(),
        }
    }

    fn refresh(&mut self, buffer_id: u64) {
        if let Some(flavite) = &self.flavite {
            match flavite.get_buffer_view(buffer_id.into(), None) {
                Ok(view) => self.view = Some(view),
                Err(error) => self.status = format!("Could not refresh buffer: {error}"),
            }
        }
    }

    fn save(&mut self) {
        let (Some(flavite), Some(view)) = (&self.flavite, &self.view) else {
            return;
        };
        match flavite.execute_session(SessionCommand::SaveNote {
            path: view.path.clone(),
        }) {
            SessionResult::Pending { request_id } => {
                self.pending_save = Some(request_id);
                self.status = format!("Save pending (request {request_id})");
            }
            result => {
                self.status = terminal_save_status(&result);
                self.refresh(view.buffer_id);
            }
        }
    }

    fn pump(&mut self) {
        let Some(flavite) = &self.flavite else { return };
        let completions = flavite.session().drain_operation_completions();
        for completion in completions {
            if self.pending_save == Some(completion.request_id) {
                self.pending_save = None;
                self.status = terminal_save_status(&completion.result);
                if let Some(view) = &self.view {
                    let id = view.buffer_id;
                    self.refresh(id);
                }
            }
        }
        if self.pending_save.is_none()
            && self.view.as_ref().is_some_and(|view| view.state == "Dirty")
            && self.last_edit.elapsed() >= Duration::from_secs(1)
        {
            self.save();
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let header = column![
            text("Flavite • custom iced editor"),
            text(&self.status).size(14),
            button("Save now").on_press(Message::Save),
        ]
        .spacing(8);
        if let Some(view) = &self.view {
            container(
                column![
                    header,
                    EditorWidget::new(view, Message::Edit)
                        .with_candidates(self.candidates.clone())
                        .width(Length::Fill)
                        .height(Length::Fill),
                ]
                .spacing(12),
            )
            .padding(18)
            .into()
        } else {
            container(header).padding(18).into()
        }
    }
}

fn safe_note_target(root: &std::path::Path, target: &str) -> Option<PathBuf> {
    use std::path::Component;
    let root = std::fs::canonicalize(root).ok()?;
    let candidate = std::path::Path::new(target);
    if candidate.as_os_str().is_empty()
        || candidate
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || target
            .chars()
            .any(|character| character == ':' || character == '\\' || character.is_control())
    {
        return None;
    }
    let mut relative = candidate.to_path_buf();
    if relative.extension().is_none() {
        relative.set_extension("md");
    } else if !relative
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
    {
        return None;
    }
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return None;
        };
        let name = name.to_string_lossy();
        if name.ends_with(['.', ' '])
            || name.chars().any(|character| "<>\"|?*".contains(character))
            || is_windows_reserved_name(&name)
        {
            return None;
        }
    }
    let path = root.join(relative);
    let parent = std::fs::canonicalize(path.parent()?).ok()?;
    if !parent.starts_with(&root) {
        return None;
    }
    if std::fs::symlink_metadata(&path).is_ok() {
        let resolved = std::fs::canonicalize(&path).ok()?;
        if !resolved.starts_with(&root) {
            return None;
        }
    }
    Some(path)
}

fn is_windows_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or_default();
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn terminal_save_status(result: &SessionResult) -> String {
    match result {
        SessionResult::Success(CommandResult::NoteSaved {
            hash,
            epoch,
            generation,
            ..
        }) => {
            let short_hash = hash
                .iter()
                .take(4)
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            format!("Saved · {epoch} · {generation} · {short_hash}")
        }
        other => format!("Save failed: {other:?}"),
    }
}
