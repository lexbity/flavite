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

fn desired_ratios(node: &flavite_workspace::LayoutNode) -> Vec<u32> {
    match node {
        flavite_workspace::LayoutNode::Group(_) => vec![],
        flavite_workspace::LayoutNode::Split {
            desired_ratio_ppm,
            first,
            second,
            ..
        } => {
            let mut ratios = vec![*desired_ratio_ppm];
            ratios.extend(desired_ratios(first));
            ratios.extend(desired_ratios(second));
            ratios
        }
    }
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
            let bounds = app.group_bounds.get(&group).copied();
            let slots = crate::workspace::tab_strip::plan(bounds.map_or(0.0,|r|r.width),state.tabs().len(),state.tabs().iter().position(|t| *t==state.active()).unwrap_or(0));
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
                        flavite_workspace::ViewKind::Preview(_) => "preview",
                        flavite_workspace::ViewKind::Backlinks(_) => "backlinks",
                    };
                    let binding = app.workspace.binding_of(tab);
                    let context_state = app
                        .contexts
                        .state(tab)
                        .map(|state| format!("{state:?}"))
                        .unwrap_or_default();
                    let interaction = session.interaction();
                    let buffer_view = app.note_editor_view(*tab);
                    Some(serde_json::json!({
                        // JSON numbers cannot losslessly represent every
                        // u128; expose opaque identity in its stable string
                        // form so the fixture protocol remains valid.
                        "tab": tab.to_string(),
                        "visible_in_strip": state.tabs().iter().position(|t| t==tab).is_some_and(|i| i>=slots.start && i<slots.start+slots.count),
                        "slot_index": state.tabs().iter().position(|t| t==tab).and_then(|i| i.checked_sub(slots.start)).filter(|i| *i<slots.count),
                        "title": session.title(),
                        "load": load,
                        "active": *tab == state.active(),
                        "buffer": buffer_view.as_ref().map(|view| view.buffer_id),
                        "revision": buffer_view.as_ref().map(|view| view.revision),
                        "selection": interaction.selections.first().map(|selection| [selection.anchor, selection.head]),
                        "editor_bounds": app.editor_bounds.get(tab).map(|r| [r.x,r.y,r.width,r.height]),
                        "incarnation": interaction.incarnation,
                        "interaction_focused": interaction.focused,
                        "composing": interaction.composition == flavite_workspace::CompositionState::Active,
                        "binding": binding.map(|binding| format!("{binding:?}")),
                        "effective_source": app
                            .contexts
                            .observed(tab)
                            .map(|source| source.locator.components().join("/")),
                        "context_state": context_state,
                    }))
                })
                .collect();
            Some(serde_json::json!({
                "group": group.to_string(),
                "bounds": bounds.map(|r| [r.x,r.y,r.width,r.height]),
                "tab_width": slots.tab_width,
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
    let focused_text_content = app.note_state_text(focused).unwrap_or_default();
    let writing_target = app.workspace.writing_target().map(|target| {
        serde_json::json!({
            "tab": target.tab.to_string(),
            "buffer": target.buffer_id,
            "revision": target.revision,
            "caret": target.caret_text_byte,
        })
    });
    serde_json::json!({
        "event": "workspace",
        "revision": app.workspace.revision(),
        "status": app.status_text(),
        "groups": groups,
        "menu_open": app.strip_menu.is_some(),
        "picker_open": app.picker.open,
        "palette_open": app.palette_open,
        "desired_ratios": desired_ratios(app.workspace.tree()),
        "adaptive_maximized": !flavite_workspace::fits(&app.workspace,flavite_workspace::Size {width:app.workspace_bounds.width,height:app.workspace_bounds.height},1.0),
        "window_size": [app.window_size().width,app.window_size().height],
        "drag_target": app.gesture.target().map(|target|format!("{target:?}")),
        "dragging": app.gesture.dragging(),
        "pointer": app.pointer.map(|p|[p.x,p.y]),
        "resizing": app.resizing_split.map(|id|id.to_string()),
        "split_hits": app.split_hits().into_iter().map(|(_,r,_)|[r.x,r.y,r.width,r.height]).collect::<Vec<_>>(),
        "resize_preview": app.resize_preview.values().copied().collect::<Vec<_>>(),
        "workspace_bounds": [app.workspace_bounds.x,app.workspace_bounds.y,app.workspace_bounds.width,app.workspace_bounds.height],
        "focused_tab": focused.to_string(),
        "focused_text_chars": focused_text,
        "focused_text": focused_text_content,
        "document_anchor": app.workspace.document_anchor().map(|tab| tab.to_string()),
        "writing_target": writing_target,
        "maximized": app.maximized_group().map(|group| group.to_string()),
        "catalog": app.catalog().len(),
    })
}
