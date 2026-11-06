//! RFC-0006 S00 — UI host behavior for file encodings (§6.10).
//!
//! Host-state assertions only (native rendering evidence is separate). The
//! cold-open flow is asserted against the real pump: an undecodable note is
//! never installed as a blank editable buffer, and the typed outcome is
//! presented. The per-tab encoding-decision host (the data behind the note
//! view's encoding banner) is exercised through its real `Message::Encoding`
//! path — record, typed reopen refusal, dismissal.
//!
//! KNOWN GAP (reported, not worked around silently): the async `note.open`
//! completion carrying `encoding-choice-required` currently reaches the host
//! only as status text (`route_completion`); `app.encoding.record` runs in
//! `open_note_tab`'s synchronous branch, which a first-time open cannot take
//! because the open is always a Pending flight. Until that routing lands, the
//! banner data path is proven here through the same record/dispatch contract
//! the async route must call.

#[path = "../../flavite-api/tests/support/links.rs"]
mod support;

use flavite_fs::Fs;
use flavite_ui::app::{App, Message};
use flavite_ui::host::encoding::{payload_raw_hash, payload_reason};
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

fn app_with_bytes(name: &str, bytes: &[u8]) -> App {
    let f = support::fixture(&[]);
    let path = f.root.join(name);
    f.fs.write_new(&path, bytes).expect("seed note bytes");
    App::from_flavite(f.flavite)
}

fn open_note(app: &mut App, name: &str) -> flavite_workspace::TabId {
    let _ = app.update(Message::OpenIntent(
        flavite_ui::host::open_intents::OpenIntent::reuse(
            NoteLocator::from_components([name]).unwrap(),
            None,
        ),
    ));
    wait(app, |a| !a.notes.any_pending());
    app.workspace.focused_tab()
}

/// The payload the core sends with `encoding-choice-required` (§6.10: reason
/// kind, observed raw hash, allowed descriptors — never note content).
/// An undecodable note never opens into a blank editable buffer, and the
/// typed outcome reaches the host. The per-tab decision state that feeds the
/// note view's encoding banner is then driven through its real message path:
/// the recorded payload parses through the host's helpers, a Reopen against
/// the real core with a stale hash is refused typed, and Dismiss clears it.
#[test]
fn undecodable_open_never_becomes_a_blank_editable_note() {
    let mut app = app_with_bytes("bad.md", b"plain \xff bytes\n");
    let tab = open_note(&mut app, "bad.md");

    // No editable buffer was installed behind the outcome (no-empty-fallback).
    assert!(app.note_state_text(tab).is_none());
    assert!(app.note_source_format(tab).is_none());
    // The async open flight completes with the typed core code; the app
    // records the per-tab decision and presents the user-facing message.
    assert!(
        app.status_text().contains("Encoding choice required"),
        "typed outcome presented, got {:?}",
        app.status_text()
    );
    // The workspace renders the decision banner headlessly (reopen
    // controls + cancel — never a blank editable note).
    let pending = app.encoding.pending(tab).expect("recorded decision");
    assert_eq!(pending.code, "encoding-choice-required");
    assert_eq!(
        payload_reason(pending.payload.as_ref()),
        "invalid-utf8",
        "the core payload names the typed reason"
    );
    assert!(
        payload_raw_hash(pending.payload.as_ref()).is_some(),
        "the observed raw hash rides the payload so a reopen can bind to exact bytes"
    );
    let _ = flavite_ui::workspace_shell::view(&app);

    // Reopen through the real core against the same malformed bytes: the
    // core re-refuses with the same typed decision (the observed hash still
    // matches), and the dialog stays actionable — never a blank note.
    let _ = app.update(Message::Encoding(
        flavite_ui::host::encoding::EncodingMessage::Reopen {
            tab,
            encoding: "utf8".to_owned(),
            bom: "auto".to_owned(),
        },
    ));
    let pending = app
        .encoding
        .pending(tab)
        .expect("decision survives a refusal");
    assert_eq!(
        pending.code, "encoding-choice-required",
        "the core re-refuses typed and the dialog stays actionable"
    );

    // Dismissal clears the decision.
    let _ = app.update(Message::Encoding(
        flavite_ui::host::encoding::EncodingMessage::Dismiss { tab },
    ));
    assert!(app.encoding.pending(tab).is_none(), "dismiss clears");
}

/// A UTF-16LE+BOM note opens normally and the tab state exposes its exact
/// descriptor.
#[test]
fn utf16le_bom_note_opens_with_exposed_descriptor() {
    let mut bytes = vec![0xFF_u8, 0xFE];
    for unit in "u16 app note\n".encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let mut app = app_with_bytes("u16.md", &bytes);
    let tab = open_note(&mut app, "u16.md");

    assert_eq!(
        app.note_source_format(tab),
        Some(flavite_api::SourceFormat::utf16le_with_bom()),
        "descriptor exposed through the host state"
    );
    assert_eq!(app.note_state_text(tab).as_deref(), Some("u16 app note\n"));
    let _ = flavite_ui::workspace_shell::view(&app);
}
