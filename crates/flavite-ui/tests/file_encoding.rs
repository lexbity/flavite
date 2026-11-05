//! RFC-0006 S00 — UI host behavior for file encodings (§6.10).
//!
//! Host-state assertions only: an undecodable open must surface the typed
//! encoding decision per tab (never a blank editable note), and a UTF-16
//! note's descriptor is exposed through the tab state. Native rendering
//! evidence is separate (the view function runs headless in the visual
//! suites, not here).

#[path = "../../flavite-api/tests/support/links.rs"]
mod support;

use flavite_fs::Fs;
use flavite_ui::app::{App, Message};
use flavite_workspace::NoteLocator;
use std::time::{Duration, Instant};

fn wait(app: &mut App, done: impl Fn(&App) -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let _ = app.update(Message::Pump);
        if done(app) {
            return;
        }
        assert!(Instant::now() < until, "{}", app.status_text());
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn app_with_bytes(name: &str, bytes: &[u8]) -> (App, std::path::PathBuf) {
    let f = support::fixture(&[]);
    let path = f.root.join(name);
    f.fs.write_new(&path, bytes).expect("seed note bytes");
    (App::from_flavite(f.flavite), path)
}

fn open_note(app: &mut App, name: &str) -> flavite_workspace::TabId {
    let _ = app.update(Message::OpenNote {
        locator: NoteLocator::from_components([name]).unwrap(),
        group: None,
    });
    wait(app, |a| !a.notes.any_pending());
    app.workspace.focused_tab()
}

/// An undecodable note opens into a typed per-tab encoding decision with the
/// observed raw hash in the payload — never a blank editable buffer.
#[test]
fn undecodable_open_surfaces_encoding_choice_host_state() {
    let (mut app, path) = app_with_bytes("bad.md", b"plain \xff bytes\n");
    let tab = open_note(&mut app, "bad.md");

    let pending = app.encoding.pending(tab);
    assert!(
        pending.is_some(),
        "encoding decision expected for tab; status was {:?}",
        app.status_text()
    );
    let pending = pending.unwrap();
    assert_eq!(pending.code, "encoding-choice-required");
    let payload = pending.payload.as_deref().expect("sanitized payload");
    assert!(payload.contains("invalid-utf8"), "typed kind in payload: {payload}");
    // The observed raw hash binds the reopen request to exactly these bytes.
    let payload_json: serde_json::Value =
        serde_json::from_str(payload).expect("payload is compact JSON");
    let raw_hash = payload_json["raw_hash"]
        .as_str()
        .expect("raw_hash field present");
    assert_eq!(raw_hash.len(), 64, "blake3 hex hash");
    assert_eq!(pending.path, path);

    // No editable buffer was installed behind the dialog.
    assert!(app.note_state_text(tab).is_none());
    // The workspace still renders with the dialog state present.
    let _ = flavite_ui::workspace_shell::view(&app);
}

/// A UTF-16LE+BOM note opens normally and the tab state exposes its exact
/// descriptor.
#[test]
fn utf16le_bom_note_opens_with_exposed_descriptor() {
    let mut bytes = vec![0xFF_u8, 0xFE];
    for unit in "u16 app note\n".encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let (mut app, _path) = app_with_bytes("u16.md", &bytes);
    let tab = open_note(&mut app, "u16.md");

    assert_eq!(
        app.note_source_format(tab),
        Some(flavite_api::SourceFormat::utf16le_with_bom()),
        "descriptor exposed through the host state"
    );
    assert_eq!(app.note_state_text(tab).as_deref(), Some("u16 app note\n"));
    let _ = flavite_ui::workspace_shell::view(&app);
}
