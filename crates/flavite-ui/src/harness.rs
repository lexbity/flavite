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
            let metrics = app.appearance.metrics();
            let strip_x = crate::workspace::tab_strip::nav_width(*metrics);
            let slots = crate::workspace::tab_strip::plan(bounds.map_or(0.0,|r|r.width),metrics.controls_width + strip_x,state.tabs().len(),state.tabs().iter().position(|t| *t==state.active()).unwrap_or(0));
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
                        flavite_workspace::ViewKind::Graph(_) => "graph",
                        flavite_workspace::ViewKind::Empty => "empty",
                        flavite_workspace::ViewKind::Notes => "notes",
                        flavite_workspace::ViewKind::Settings => "settings",
                        flavite_workspace::ViewKind::Search => "search",
                        flavite_workspace::ViewKind::Replace => "replace",
                        flavite_workspace::ViewKind::History(_) => "history",
                        flavite_workspace::ViewKind::Preview(_) => "preview",
                        flavite_workspace::ViewKind::Backlinks(_) => "backlinks",
                        flavite_workspace::ViewKind::Outline(_) => "outline",
                        flavite_workspace::ViewKind::Properties(_) => "properties",
                    };
                    let binding = app.workspace.binding_of(tab);
                    let context_state = app
                        .contexts
                        .state(tab)
                        .map(|state| format!("{state:?}"))
                        .unwrap_or_default();
                    let interaction = session.interaction();
                    let buffer_view = app.note_editor_view(*tab);
                    let graph = if matches!(
                        session.kind(),
                        flavite_workspace::ViewKind::Graph(_)
                    ) {
                        app.graph.state(*tab).map(|state| {
                            let publication = app
                                .flavite
                                .as_ref()
                                .and_then(|flavite| flavite.graph_frame())
                                .map(|frame| frame.publication().clone());
                            // Screen positions for the visual runner: the
                            // real pointer then clicks the real widget.
                            let nodes: Vec<serde_json::Value> = state
                                .geometry
                                .as_ref()
                                .map(|geometry| {
                                    let bounds = state.bounds.unwrap_or(iced::Rectangle::new(
                                        iced::Point::ORIGIN,
                                        iced::Size::new(800.0, 600.0),
                                    ));
                                    geometry
                                        .nodes()
                                        .iter()
                                        .enumerate()
                                        .take(80)
                                        .map(|(dense, node)| {
                                            let title = publication
                                                .as_ref()
                                                .and_then(|p| geometry.node_id(dense).and_then(|id| p.node_index(id)).and_then(|index| p.record(index as usize)))
                                                .map(|r| r.title.clone())
                                                .unwrap_or_default();
                                            let screen = state.camera.world_to_screen(
                                                node.center,
                                                [0.0, 0.0],
                                                [bounds.width, bounds.height],
                                            );
                                            serde_json::json!({
                                                "id": format!("{}", geometry.node_id(dense).map(|i| i.id).unwrap_or(0)),
                                                "kind": format!("{:?}", geometry.node_id(dense).map(|i| i.kind)),
                                                "title": title,
                                                "x": bounds.x + screen[0],
                                                "y": bounds.y + screen[1],
                                            })
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            serde_json::json!({
                                "backend": app.graph.backend.label(),
                                "filter_bounds": state.filter_bounds.iter().map(|(field,r)|(format!("{field:?}"),vec![r.x,r.y,r.width,r.height])).collect::<std::collections::BTreeMap<_,_>>(),
                                "nodes": state.geometry.as_ref().map_or(0, |g| g.node_count()),
                                "edges": state.geometry.as_ref().map_or(0, |g| g.edge_count()),
                                // S04: the paired geometry key identifies the
                                // exact frame the visual evidence refers to
                                // (epoch/generation/topology/scope); opaque
                                // identity only — never private token values.
                                "geometry_key": state.geometry_key.map(|key| serde_json::json!({
                                    "epoch": key.epoch,
                                    "generation": key.generation,
                                    "topology": key.topology_version,
                                    "overlay_digest": key.overlay_digest,
                                    "scope_sequence": key.scope_sequence,
                                })),
                                "epoch": publication.as_ref().map(|p| p.epoch),
                                "generation": publication.as_ref().map(|p| p.generation),
                                "topology": publication.as_ref().map(|p| p.topology_version),
                                "selection": state.selection.map(|hit| hit.display()),
                                "selected_ids": state.selected_nodes.iter().map(|id| format!("{}:{}", format!("{:?}", id.kind).to_lowercase(), id.id)).collect::<Vec<_>>(),
                                // Gesture-freeze reporting (D-02): the
                                // bounded click-pair hold and its epoch.
                                "gesture_candidate": state.arbiter.has_candidate(),
                                "gesture_epoch": state.arbiter.candidate_epoch(),
                                "activation_notice_pending": state.controls_open && state.display_controls_open && app.appearance.graph_activation_notice_pending(),
                                "controls_open": state.controls_open,
                                "notice_bounds": state.notice_bounds.map(|r| [r.x, r.y, r.width, r.height]),
                                "frozen": state.frozen,
                                // S04 AC-6: camera authority. A user-held
                                // camera (pan/zoom taken) stops auto-follow;
                                // the scenario must hold it before asserting
                                // screen-position stability.
                                "user_camera_moved": state.user_camera_moved,
                                "list_mode": state.list_mode,
                                "stale": state.stale,
                                "updating": state.updating,
                                "scope_sequence": state.scope_sequence,
                                "overlay_sources": state.overlay_count,
                                "outside_scope": state.selection.is_some_and(|hit| state.scope_result.as_ref().is_some_and(|scope| !scope.nodes.contains(&hit.id))),
                                "bounds": state.bounds.map(|r| [r.x, r.y, r.width, r.height]),
                                "screen_nodes": nodes,
                            })
                        })
                    } else {
                        None
                    };
                    Some(serde_json::json!({
                        // JSON numbers cannot losslessly represent every
                        // u128; expose opaque identity in its stable string
                        // form so the fixture protocol remains valid.
                        "tab": tab.to_string(),
                        "visible_in_strip": state.tabs().iter().position(|t| t==tab).is_some_and(|i| i>=slots.start && i<slots.start+slots.count),
                        "slot_index": state.tabs().iter().position(|t| t==tab).and_then(|i| i.checked_sub(slots.start)).filter(|i| *i<slots.count),
                        "title": session.title(),
                        "load": load,
                        "reading_error": app.reading.notes.get(tab).and_then(|entry|entry.error.clone()),
                        "mode": match session.kind() { flavite_workspace::ViewKind::Note(note) => Some(format!("{:?}",note.mode())), _ => None },
                        "source_preview": app.reading.source_previews.contains(tab),
                        "reading": crate::host::reading::document(app,*tab).map(|doc| serde_json::json!({
                            "source": doc.path.to_string_lossy(), "buffer": doc.buffer, "current": crate::host::reading::current(app,doc), "catalog": doc.catalog_version, "published_catalog": app.flavite.as_ref().and_then(|api|api.read_catalog_version()), "unsaved": doc.unsaved, "scroll_y": app.reading.scroll.get(tab).copied().unwrap_or(0.0),
                            "outline": doc.document.outline.iter().map(|h|serde_json::json!({"title":h.title,"byte":h.byte,"level":h.level,"bounds":app.reading.blocks.get(&(*tab,h.byte)).map(|r|[r.x,r.y,r.width,r.height])})).collect::<Vec<_>>(),
                            "controls": app.reading.controls.iter().filter(|((t, source, _, _),_)| t == tab && *source == std::sync::Arc::as_ptr(doc) as usize).map(|((_,_,role,byte),r)|serde_json::json!({"role":role,"byte":byte,"bounds":[r.x,r.y,r.width,r.height]})).collect::<Vec<_>>()
                        })),
                        "find": app.note_search.finds.get(tab).map(|find|serde_json::json!({
                            "query":find.query,
                            "mode":find.mode,
                            "case_sensitive":find.case_sensitive,
                            "whole_word":find.whole_word,
                            "offset":crate::host::note_search::matches(app,*tab).map_or(find.page_offset,|page|page.offset),
                            "matches":crate::host::note_search::matches(app,*tab).map_or(0,|page|page.spans.len()),
                            "total_matches":crate::host::note_search::matches(app,*tab).map_or(0,|page|page.total_matches),
                            "next_offset":crate::host::note_search::matches(app,*tab).and_then(|page|page.next_offset),
                            "error":find.last_error,
                        })),
                        "replace": app.note_search.replaces.get(tab).map(|replace|serde_json::json!({
                            "query": replace.query,
                            "matches": crate::host::note_search::matches(app,*tab).map_or(0,|page|page.spans.len()),
                            "replace_open": true,
                            "match_count": replace.match_count,
                            "pending": replace.pending.is_some(),
                            "error": replace.last_error,
                        })),
                        "selection_search": app.selection_search.tabs.get(tab).map(|search|serde_json::json!({
                            "kind": match search.kind {
                                crate::host::selection_search::ApplyKind::Select => "select",
                                crate::host::selection_search::ApplyKind::Split => "split",
                            },
                            "query": search.query,
                            "regex": search.regex,
                            "case_sensitive": search.case_sensitive,
                            "pending": search.pending.is_some(),
                            "overlay_matches": search.overlay.as_ref().map_or(0, |overlay| overlay.spans.len()),
                            "total_matches": search.overlay.as_ref().map_or(0, |overlay| overlay.total_matches),
                            "zero_width": search.overlay.as_ref().map_or(0, |overlay| overlay.zero_width_count),
                            "error": search.error,
                        })),
                        "viewport_anchor": interaction.viewport_anchor,
                        "active": *tab == state.active(),
                        "buffer": buffer_view.as_ref().map(|view| view.buffer_id),
                        "revision": buffer_view.as_ref().map(|view| view.revision),
                        "buffer_text": buffer_view.as_ref().map(|view| view.text.to_string()),
                        "selection_count": interaction.selections.len(),
                        "selections": interaction
                            .selections
                            .iter()
                            .map(|selection| [selection.anchor, selection.head])
                            .collect::<Vec<_>>(),
                        "editor_bounds": app.editor_bounds.get(tab).map(|r| [r.x,r.y,r.width,r.height]),
                        "incarnation": interaction.incarnation,
                        "interaction_focused": interaction.focused,
                        "composing": interaction.composition == flavite_workspace::CompositionState::Active,
                        "buffer_state": buffer_view.as_ref().map(|view| format!("{:?}", view.state)),
                        "binding": binding.map(|binding| format!("{binding:?}")),
                        "effective_source": app
                            .contexts
                            .observed(tab)
                            .map(|source| source.locator.components().join("/")),
                        "context_state": context_state,
                        "graph": graph,
                    }))
                })
                .collect();
            Some(serde_json::json!({
                "group": group.to_string(),
                "bounds": bounds.map(|r| [r.x,r.y,r.width,r.height]),
                "tab_width": slots.tab_width,
                "strip_x": strip_x,
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
    let graph_service = serde_json::json!({
        "backend": app.graph.backend.label(),
        "status": app.graph.status.as_ref().map(|status| format!("{status:?}")),
        "pins": app.graph.pins.count(),
        "pins_dirty": app.graph.pins.is_dirty(),
        "pins_failure": app.graph.pins.failure.clone(),
        "pins_loaded": app.graph.pins.loaded(),
        "stale_discards": app.graph.stale_discards_total,
        "metrics": app.graph.metrics.map(|metrics| serde_json::json!({
            "wakes": metrics.wakes_total,
            "sweeps": metrics.sweeps_total,
            "watermark_skips": metrics.watermark_skips_total,
            "batches": metrics.batches_total,
            "resets": metrics.resets_total,
        })),
        "publishing": app.workspace.exploration().map(|selection| serde_json::json!({
            "source_tab": selection.source_tab.to_string(),
            "sequence": selection.sequence,
            "target": selection.target.as_ref().map(|target| target.components().join("/")),
        })),
    });
    let mut snapshot = serde_json::json!({
        "event": "workspace",
        "revision": app.workspace.revision(),
        "status": app.status_text(),
        "groups": groups,
        "menu_open": app.strip_menu.is_some(),
        "dialog_open": app.dialog.is_some(),
        "search_states": app
            .search
            .tabs
            .iter()
            .map(|(tab, state)| {
                serde_json::json!({
                    "tab": tab.to_string(),
                    "query": state.query,
                    "results": state.rows.iter().map(|row| serde_json::json!({"path": row.path, "title": row.title, "excerpt": row.excerpt})).collect::<Vec<_>>(),
                    "has_next": state.has_next(),
                    "error": state.error,
                })
            })
            .collect::<Vec<_>>(),
        "picker_open": app.picker.open,
        "palette_open": app.palette_open,
        "palette_query": app.palette_query,
        "palette_matches": if app.palette_open { app.palette_snapshot().iter().map(|command| command.id).collect::<Vec<_>>() } else { Vec::new() },
        "desired_ratios": desired_ratios(app.workspace.tree()),
        "adaptive_maximized": !flavite_workspace::fits(&app.workspace,flavite_workspace::Size {width:app.workspace_bounds.width,height:app.workspace_bounds.height},app.appearance.metrics().scale),
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
        "pending_operations": app.pending_operations(),
        "focused_text": focused_text_content,
        "context_mode": app.context_mode.session.as_ref().map(|session|serde_json::json!({
            "tab":session.tab.to_string(),"state":session.state.mode().label(),"menu":session.state.menu().map(|m|m.title()),
            "message":session.state.message().0,
            "effect":crate::host::context_mode::preview_command(app,session.tab).and_then(|command|{
                let view=app.note_editor_view(session.tab)?;
                let source=crate::host::context_mode::EditorTarget::capture(app,session.tab)?;
                let effect=flavite_edit_context::effect::plan(&view.text,source.selection,command)?;
                Some(serde_json::json!({"command":command.id(),"range":[effect.range.start,effect.range.end],"implicit":effect.implicit,"caption":effect.description(command)}))
            }),
            "history":{
                "open":app.context_mode.history.open,
                "page":app.context_mode.history.page,
                "bounds":(if app.context_mode.history.open { &app.context_mode.history.rows } else { &app.context_mode.history.bounds }).iter().map(|(node,r)|(node.to_string(),[r.x,r.y,r.width,r.height])).collect::<std::collections::BTreeMap<_,_>>(),
                "controls":app.context_mode.history.controls.iter().map(|(name,r)|(*name,[r.x,r.y,r.width,r.height])).collect::<std::collections::BTreeMap<_,_>>()
            },
            "bounds":app.context_mode.bounds.iter().filter(|((tab,_),_)|*tab==session.tab)
                .map(|((_,name),r)|(*name,[r.x,r.y,r.width,r.height])).collect::<std::collections::BTreeMap<_,_>>()
        })),
        "diagnostics": serde_json::json!({
            "burst_hint": app.diagnostics.burst_hint,
            "events": app.diagnostics.event_count(),
            "dropped": app.diagnostics.dropped(),
        }),
        "document_anchor": app.workspace.document_anchor().map(|tab| tab.to_string()),
        "writing_target": writing_target,
        "graph_service": graph_service,
        "maximized": app.maximized_group().map(|group| group.to_string()),
        "catalog": app.catalog().len(),
        "catalog_status": match app.catalog_status() {
            crate::app::CatalogStatus::Empty => "empty".to_owned(),
            crate::app::CatalogStatus::Loading => "loading".to_owned(),
            crate::app::CatalogStatus::Ready {
                epoch,
                generation,
                truncated,
            } => format!(
                "ready:{epoch}/{generation}{}",
                if *truncated { "·truncated" } else { "" }
            ),
            crate::app::CatalogStatus::Failed { .. } => "failed".to_owned(),
        },
        "dialog_kind": app.dialog.as_ref().map(|dialog| dialog.kind_name()),
        "start_error": app.start.error.clone(),
        "theme": app.appearance.scheme().name(),
        "theme_choice": app.appearance.theme_name(),
        "interface_scale_pct": app.appearance.scale_pct(),
        "editor_font_size": app.appearance.editor_font_size(),
        "reduced_motion": app.appearance.reduced_motion(),
    });
    snapshot["menu_bar_visible"] = app.menu_bar_visible.into();
    snapshot["sidebar_open"] = app.sidebar_open().into();
    snapshot["sidebar_filter"] = app.sidebar_filter.clone().into();
    snapshot["sidebar_bounds"] = serde_json::json!(
        app.sidebar_bounds
            .iter()
            .map(|(key, r)| (key, [r.x, r.y, r.width, r.height]))
            .collect::<std::collections::BTreeMap<_, _>>()
    );
    snapshot
}
