//! Minimal end-to-end host for the custom editor and Session save path.

use flavite_api::lattice::session::{CommandResult, SessionCommand, SessionResult};
use flavite_api::{BufferView, Lattice, VaultConfig};
use flavite_editor::{EditorAction, EditorWidget};
use iced::widget::{button, column, container, text};
use iced::{Element, Length, Subscription, time};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn main() -> iced::Result {
    iced::application(App::boot, App::update, App::view)
        .title("Lattice editor demo")
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
    lattice: Option<Lattice>,
    view: Option<BufferView>,
    status: String,
    last_edit: Instant,
    pending_save: Option<u64>,
}

impl App {
    fn boot() -> Self {
        match Self::open() {
            Ok(app) => app,
            Err(error) => Self {
                lattice: None,
                view: None,
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
        let note = root.join("demo.md");
        if !note.exists() {
            std::fs::write(
                &note,
                "# Lattice editor demo\n\nType here. Changes autosave after one idle second.\n",
            )
            .map_err(|error| error.to_string())?;
        }
        let config = VaultConfig {
            watcher_enabled: false,
            ..VaultConfig::default()
        };
        let lattice = Lattice::open_root(root, config).map_err(|error| error.to_string())?;
        let path = PathBuf::from("demo.md");
        match lattice.execute_session(SessionCommand::OpenNote { path }) {
            SessionResult::Success(CommandResult::NoteOpened { buffer_id, .. }) => {
                let view = lattice
                    .get_buffer_view(buffer_id, None)
                    .map_err(|error| error.to_string())?;
                Ok(Self {
                    lattice: Some(lattice),
                    view: Some(view),
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
                let Some(lattice) = &self.lattice else { return };
                let buffer_id = action.buffer_id;
                let result = lattice.execute_session(action.into_session_command());
                self.status = format!("Edit: {result:?}");
                self.refresh(buffer_id);
                self.last_edit = Instant::now();
            }
            Message::Save => self.save(),
            Message::Pump => self.pump(),
        }
    }

    fn refresh(&mut self, buffer_id: u64) {
        if let Some(lattice) = &self.lattice {
            match lattice.get_buffer_view(buffer_id.into(), None) {
                Ok(view) => self.view = Some(view),
                Err(error) => self.status = format!("Could not refresh buffer: {error}"),
            }
        }
    }

    fn save(&mut self) {
        let (Some(lattice), Some(view)) = (&self.lattice, &self.view) else {
            return;
        };
        match lattice.execute_session(SessionCommand::SaveNote {
            path: view.path.clone(),
        }) {
            SessionResult::Pending { request_id } => {
                self.pending_save = Some(request_id);
                self.status = format!("Save pending (request {request_id})");
            }
            result => {
                self.status = format!("Save: {result:?}");
                self.refresh(view.buffer_id);
            }
        }
    }

    fn pump(&mut self) {
        let Some(lattice) = &self.lattice else { return };
        let completions = lattice.session().drain_operation_completions();
        for completion in completions {
            if self.pending_save == Some(completion.request_id) {
                self.pending_save = None;
                self.status = format!("Save complete: {:?}", completion.result);
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
            text("Lattice • custom iced editor"),
            text(&self.status).size(14),
            button("Save now").on_press(Message::Save),
        ]
        .spacing(8);
        if let Some(view) = &self.view {
            container(
                column![
                    header,
                    EditorWidget::new(view, Message::Edit)
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
