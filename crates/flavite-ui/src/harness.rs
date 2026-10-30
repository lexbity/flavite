//! Visual-runner harness (RFC-0003 §16.2, compiled only with the
//! `ui-test-support` feature).
//!
//! After every acknowledged state change the shell writes one semantic
//! snapshot line to an inherited file descriptor named by
//! `FLAVITE_HARNESS_OUT_FD`. The production build never contains this
//! module, so no environment variable can enable test behavior there.
//! Transport is an inherited anonymous pipe or file, never a socket.

use crate::app::App;
use std::fs::File;
use std::sync::{Mutex, OnceLock};

fn sink() -> Option<&'static Mutex<File>> {
    static SINK: OnceLock<Option<Mutex<File>>> = OnceLock::new();
    let holder = SINK.get_or_init(|| {
        let fd = std::env::var("FLAVITE_HARNESS_OUT_FD").ok()?;
        let raw = fd.parse::<i32>().ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::io::FromRawFd;
            // SAFETY: the fd is inherited from the test runner for exactly
            // this purpose; ownership transfers to the File.
            Some(Mutex::new(unsafe { File::from_raw_fd(raw) }))
        }
        #[cfg(not(unix))]
        {
            let _ = raw;
            None
        }
    });
    holder.as_ref()
}

/// Whether the harness is attached in this process.
#[must_use]
pub fn enabled() -> bool {
    sink().is_some()
}

/// Fixture injection: omit a named tab from snapshots so the visual gate
/// can prove it fails on hidden required state. Read from the environment
/// at first use; the production build has no call site for this module.
fn injected_hidden_tab() -> Option<String> {
    static HIDDEN: OnceLock<Option<String>> = OnceLock::new();
    HIDDEN
        .get_or_init(|| std::env::var("FLAVITE_HARNESS_INJECT_HIDE_TAB").ok())
        .clone()
}

/// Emit one semantic snapshot of the shell after a state change.
pub fn observe(app: &App) {
    let Some(sink) = sink() else {
        return;
    };
    let mut snapshot = semantic_snapshot(app);
    if let Some(hidden) = injected_hidden_tab() {
        for group in snapshot["groups"].as_array_mut().expect("groups") {
            let tabs = group["tabs"].as_array_mut().expect("tabs");
            tabs.retain(|tab| tab["title"].as_str() != Some(hidden.as_str()));
        }
    }
    let mut line = serde_json::to_string(&snapshot).expect("snapshot serializes");
    line.push('\n');
    let mut file = sink
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    use std::io::Write;
    let _ = file.write_all(line.as_bytes());
    let _ = file.flush();
}

/// The semantic state the visual runner asserts on: group structure, tab
/// identities, focus, load states, and the focused note's text length.
#[must_use]
pub fn semantic_snapshot(app: &App) -> serde_json::Value {
    let focused = app.workspace.focused_tab();
    let groups: Vec<serde_json::Value> = app
        .workspace
        .group_order()
        .into_iter()
        .filter_map(|group| {
            let state = app.workspace.group(&group)?;
            let tabs: Vec<serde_json::Value> = state
                .tabs()
                .iter()
                .filter_map(|tab| {
                    let session = app.workspace.tab(tab)?;
                    let load = match session.kind() {
                        flavite_workspace::ViewKind::Note(note) => match note.load() {
                            flavite_workspace::NoteLoad::Reserved => "reserved",
                            flavite_workspace::NoteLoad::Ready => "ready",
                            flavite_workspace::NoteLoad::Failed => "failed",
                        },
                        flavite_workspace::ViewKind::Empty => "empty",
                        flavite_workspace::ViewKind::Notes => "notes",
                    };
                    Some(serde_json::json!({
                        // JSON numbers cannot losslessly represent every
                        // u128; expose opaque identity in its stable string
                        // form so the fixture protocol remains valid.
                        "tab": tab.to_string(),
                        "title": session.title(),
                        "load": load,
                        "active": *tab == state.active(),
                    }))
                })
                .collect();
            Some(serde_json::json!({
                "group": group.to_string(),
                "label": app
                    .workspace
                    .tab(&state.active())
                    .map(|session| session.title())
                    .unwrap_or_default(),
                "focused": state.active() == focused,
                "tabs": tabs,
            }))
        })
        .collect();
    let focused_text = app
        .note_state_text(focused)
        .map(|text| text.chars().count())
        .unwrap_or(0);
    serde_json::json!({
        "event": "workspace",
        "revision": app.workspace.revision(),
        "status": app.status_text(),
        "groups": groups,
        "focused_tab": focused.to_string(),
        "focused_text_chars": focused_text,
        "maximized": app.maximized_group().map(|group| group.to_string()),
        "catalog": app.catalog().len(),
    })
}
