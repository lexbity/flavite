//! RFC-0008 S1-T1 — the keystroke path emits spans for every model-phase
//! FR-10 phase. Drives a real scripted keystroke through the App
//! (`Message::EditorInput` → host turn → api turn → session turn →
//! `driving_turn` wait → buffer apply → undo coalesce) under a recording
//! subscriber installed with `tracing::subscriber::with_default` — no
//! environment dependency in tests.
//!
//! The render-side phases (`render.shaping`, `render.scan`, `render.paint`)
//! are asserted by the widget unit test in flavite-editor (scan/shaping emit
//! in `reconcile_view`/`shaped`, both drivable headlessly; `render.paint`
//! emits on the real render path and is driven by the S3 headless-renderer
//! double).

#[path = "support/mod.rs"]
mod support;

use flavite_editor::input::CapturedInput;
use flavite_editor::keymap::KeyAction;
use flavite_ui::app::Message;
use flavite_workspace::{LayoutAction, NoteLocator};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tracing::{Event, Metadata, Subscriber, span};

/// Records span names (and ignores everything else). `Interest::always()`
/// makes every callsite enabled so spans are actually created.
#[derive(Clone)]
struct RecordingSubscriber {
    span_names: Arc<Mutex<Vec<&'static str>>>,
}

impl RecordingSubscriber {
    fn new() -> Self {
        Self {
            span_names: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn names(&self) -> Vec<&'static str> {
        self.span_names.lock().expect("recorder lock").clone()
    }
}

impl Subscriber for RecordingSubscriber {
    fn register_callsite(
        &self,
        _metadata: &'static Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::always()
    }

    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, attrs: &span::Attributes<'_>) -> span::Id {
        self.span_names
            .lock()
            .expect("recorder lock")
            .push(attrs.metadata().name());
        span::Id::from_u64(1)
    }

    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

    fn event(&self, _event: &Event<'_>) {}

    fn enter(&self, _span: &span::Id) {}

    fn exit(&self, _span: &span::Id) {}

    fn clone_span(&self, _span: &span::Id) -> span::Id {
        span::Id::from_u64(1)
    }
}

fn fixture(seed: u64) -> (support::UiFixture, flavite_workspace::TabId) {
    let mut f = support::ui_fixture(
        seed,
        &format!("/test/keystroke-spans-{seed}"),
        &[support::SeedNote {
            components: &["note.md"],
            body: "hello world\n",
        }],
    );
    let group = f.app.focused_group().expect("focused group");
    f.app
        .dispatch_layout(|expected_revision| LayoutAction::OpenNote {
            expected_revision,
            group,
            locator: NoteLocator::from_components(["note.md"]).expect("note locator"),
        });
    let tab = f.app.workspace.focused_tab();
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.app.note_editor_view(tab).is_none() {
        assert!(Instant::now() < deadline, "{}", f.app.status_text());
        let _ = f.app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(2));
    }
    (f, tab)
}

#[test]
fn keystroke_path_emits_all_model_phase_spans() {
    let (mut f, tab) = fixture(12001);
    let recorder = RecordingSubscriber::new();
    let mut sequence = 200u64;

    tracing::subscriber::with_default(recorder.clone(), || {
        for ch in ["a", "b", "c"] {
            let view = f.app.note_editor_view(tab).expect("editor view");
            sequence += 1;
            let _ = f.app.update(Message::EditorInput {
                tab,
                incarnation: f.app.notes.incarnation(tab).expect("incarnation"),
                input: CapturedInput {
                    buffer: view.buffer_id,
                    observed_revision: view.revision,
                    sequence,
                    action: KeyAction::Insert,
                    text: ch.to_owned(),
                    extend: false,
                    document_boundary: false,
                    vertical: None,
                    preferred_x: Vec::new(),
                    clipboard_acquired: false,
                },
            });
            assert!(
                f.app.note_editor_view(tab).expect("editor view").revision > view.revision,
                "typed edit applied"
            );
        }
    });

    let names = recorder.names();
    for required in [
        "edit.host_turn",
        "edit.api_turn",
        "edit.session_turn",
        "edit.lock_wait",
        "edit.model_apply",
        "edit.undo_coalesce",
    ] {
        assert!(
            names.contains(&required),
            "missing span {required}; observed {names:?}"
        );
    }
}
