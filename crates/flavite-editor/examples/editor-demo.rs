//! Minimal end-to-end host for the custom editor and Session save path.

use flavite_api::{
    BufferView, ExecutionResult, Flavite, Invocation, LocalResult, Origin, VaultConfig,
};
use flavite_editor::{EditorAction, EditorWidget, LinkTarget};
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
    OpenLink(String),
    Pump,
}

struct App {
    flavite: Option<Flavite>,
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
                flavite: None,
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
        let root = std::fs::canonicalize(root).map_err(|error| error.to_string())?;
        let note = root.join("demo.md");
        if !note.exists() {
            std::fs::write(
                &note,
                "# Flavite editor demo\n\nType here. Changes autosave after one idle second.\n",
            )
            .map_err(|error| error.to_string())?;
        }
        let concepts = root.join("Concepts.md");
        if !concepts.exists() {
            std::fs::write(&concepts, "# Concepts\n\nA note opened from a wikilink.\n")
                .map_err(|error| error.to_string())?;
        }
        let config = VaultConfig {
            watcher_enabled: false,
            ..VaultConfig::default()
        };
        let flavite =
            Flavite::open_root(root.clone(), config).map_err(|error| error.to_string())?;
        let path = root.join("demo.md");
        let request_id = flavite.command_registry().next_request_id();
        let opened = flavite.execute(Invocation::new(
            request_id,
            "open_note",
            serde_json::json!(path),
            Origin::Internal,
        ));
        match opened {
            ExecutionResult::Local(LocalResult::Success(success)) => {
                let buffer_id = success.data["buffer_id"]
                    .as_u64()
                    .ok_or("open response omitted buffer_id")?;
                let view = flavite
                    .get_buffer_view(buffer_id.into(), None)
                    .map_err(|error| error.to_string())?;
                Ok(Self {
                    flavite: Some(flavite),
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
                let Some(flavite) = &self.flavite else { return };
                let buffer_id = action.buffer_id;
                let request_id = flavite.command_registry().next_request_id();
                let result = flavite.execute(action.into_invocation(request_id));
                self.status = match result {
                    ExecutionResult::Local(LocalResult::Success(_)) => "Editing".into(),
                    other => format!("Edit failed: {other:?}"),
                };
                self.refresh(buffer_id);
                self.last_edit = Instant::now();
            }
            Message::OpenLink(target) => self.open_link(&target),
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
        let request_id = flavite.command_registry().next_request_id();
        match flavite.execute(Invocation::new(
            request_id,
            "save_note",
            serde_json::json!(view.path),
            Origin::Gui,
        )) {
            ExecutionResult::Pending { request_id } => {
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
        let completions = flavite.drain_operation_completions();
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

    fn open_link(&mut self, target: &str) {
        let Some(flavite) = &self.flavite else { return };
        let mut path = PathBuf::from(target);
        if path.extension().is_none() {
            path.set_extension("md");
        }
        let path = flavite.vault_ref().root.join(path);
        let id = flavite.command_registry().next_request_id();
        let result = flavite.execute(Invocation::new(
            id,
            "open_note",
            serde_json::json!(path),
            Origin::Gui,
        ));
        match result {
            ExecutionResult::Local(LocalResult::Success(success)) => {
                if let Some(buffer_id) = success.data["buffer_id"].as_u64() {
                    self.refresh(buffer_id);
                }
                self.status = format!("Opened {target}");
            }
            other => self.status = format!("Could not open {target}: {other:?}"),
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
                        .on_open_link(Message::OpenLink)
                        .with_link_targets(vec![LinkTarget {
                            target: "Concepts".into(),
                            title: "Concepts".into()
                        }])
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

fn terminal_save_status(result: &ExecutionResult) -> String {
    match result {
        ExecutionResult::Local(LocalResult::Success(success))
            if success.command_id == "save_note" =>
        {
            let hash = success.data["hash"].as_str().unwrap_or("");
            let short_hash = hash.chars().take(8).collect::<String>();
            format!("Saved · {} · {}", success.data["generation"], short_hash)
        }
        other => format!("Save failed: {other:?}"),
    }
}
