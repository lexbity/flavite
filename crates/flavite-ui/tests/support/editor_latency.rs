//! CPU-stage measurements through the real captured-input host path.
//! Run independently of other gates for useful timings:
//! `cargo test -p flavite-ui --test shared_views editor_latency -- --ignored --nocapture`
//! Add `--release` before the filter for release measurements. This does not
//! measure physical key dispatch, inactive AT clients, or GPU presentation.

use flavite_api::{Flavite, VaultConfig};
use flavite_editor::{input::CapturedInput, keymap::KeyAction, widget::EditorState};
use flavite_fs::{Fs, MemFs};
use flavite_index::IndexActor;
use flavite_ui::app::{App, Message};
use flavite_vault::{actor::VaultActorBuilder, channels::ChannelSet, session::Session};
use std::{
    hint::black_box,
    time::{Duration, Instant},
};

fn measure(label: &str, size: usize, mut f: impl FnMut()) {
    let mut samples = Vec::new();
    for _ in 0..12 {
        let start = Instant::now();
        f();
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    println!("{label},{size},12,{:.3},{:.3}", samples[6], samples[11]);
}

#[test]
#[ignore = "manual CPU latency probe; run with --ignored --nocapture"]
fn editor_latency_probe() {
    println!("operation,bytes,samples,median_ms,max_ms");
    for size in [1024, 64 * 1024, 1024 * 1024] {
        let text = "# Heading\n\nSome ordinary text with [[target|a link]] and **bold** words.\n\n"
            .repeat(size / 71 + 1);
        let text = &text[..size.min(text.len())];
        let fs = MemFs::new();
        let root = std::path::PathBuf::from("/test/investigation-perf");
        fs.create_dir_all(&root).unwrap();
        fs.write_new(&root.join("note.md"), text.as_bytes())
            .unwrap();
        let cache = tempfile::tempdir().unwrap();
        let db = cache.path().join("index.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let identity = flavite_api::VaultIdentity::new(uuid::Uuid::new_v4(), root, vec![7, 3, 1]);
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
            Default::default(),
            channels.pulse,
            channels.notice,
            VaultConfig::default(),
        );
        let api = Flavite::open(identity, session, VaultConfig::default()).unwrap();
        let mut app = App::from_flavite(api);
        let group = app.focused_group().unwrap();
        app.dispatch_layout(|revision| flavite_workspace::LayoutAction::OpenNote {
            expected_revision: revision,
            locator: flavite_workspace::NoteLocator::from_components(["note.md"]).unwrap(),
            group,
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.notes.any_pending() {
            assert!(Instant::now() < deadline, "opening timed out");
            let _ = app.update(Message::Pump);
            std::thread::sleep(Duration::from_millis(5));
        }
        let tab = app.workspace.focused_tab();
        let incarnation = app.notes.incarnation(tab).unwrap();
        let mut sequence = 100;
        measure("app_captured_character_update", size, || {
            let view = app.note_editor_view(tab).unwrap();
            sequence += 1;
            let _ = black_box(app.update(Message::EditorInput {
                tab,
                incarnation,
                input: CapturedInput {
                    buffer: view.buffer_id,
                    observed_revision: view.revision,
                    sequence,
                    action: KeyAction::Insert,
                    text: "x".into(),
                    extend: false,
                    document_boundary: false,
                    vertical: None,
                    preferred_x: Vec::new(),
                    clipboard_acquired: false,
                },
            }));
            assert!(
                app.note_editor_view(tab).unwrap().revision > view.revision,
                "edit not applied"
            );
        });
        let view = app.note_editor_view(tab).unwrap();
        let mut state = EditorState::default();
        let mut changing = view.clone();
        let links = std::collections::HashMap::new();
        measure("widget_reconcile_new_revision", size, || {
            changing.revision += 1;
            state.reconcile(&changing, &links);
        });
        measure("accesskit_full_tree", size, || {
            black_box(flavite_ui::accessibility::tree::shell_tree(&app));
        });
        measure("idle_app_pump", size, || {
            let _ = black_box(app.update(Message::Pump));
        });
    }
}
