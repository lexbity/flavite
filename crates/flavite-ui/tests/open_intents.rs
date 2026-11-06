//! RFC-0006 S04 open-intent routing (D-03), end to end in the real shell:
//! three movable groups with duplicate notes, visible/hidden matches, a
//! closed preferred group, the narrow fallback, and a dirty same-tab
//! quick switch — asserting exact `TabId`/`GroupId`/buffer identities.

#[path = "support/mod.rs"]
mod support;

#[path = "support/graph_fixture.rs"]
mod graph_fixture;

use flavite_editor::EditorAction;
use flavite_ui::app::{App, Message};
use flavite_ui::host::open_intents::{OpenDestination, OpenIntent};
use flavite_workspace::{GroupId, LayoutAction, NoteLocator, TabId, ViewKind};

fn locator(name: &str) -> NoteLocator {
    NoteLocator::from_components(name.split('/')).expect("locator")
}

/// Note tabs showing `name`, ascending TabId.
fn tabs_of(app: &App, name: &str) -> Vec<TabId> {
    let want = locator(name);
    app.workspace
        .tabs()
        .filter_map(|(tab, session)| {
            matches!(session.kind(), ViewKind::Note(note) if note.locator() == &want)
                .then_some(*tab)
        })
        .collect()
}

fn group_of(app: &App, tab: TabId) -> GroupId {
    app.workspace.group_of(tab).expect("tab has a group")
}

fn open_in_group(app: &mut App, group: GroupId, name: &str) -> TabId {
    let before = app.workspace.focused_tab();
    app.dispatch_layout(|revision| LayoutAction::OpenNote {
        expected_revision: revision,
        locator: locator(name),
        group,
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while app.notes.any_pending() && std::time::Instant::now() < deadline {
        let _ = app.update(Message::Pump);
    }
    let tab = app.workspace.focused_tab();
    assert_ne!(tab, before, "{name} opened and became active");
    tab
}

/// Focus a note tab: the workspace document anchor follows focused ready
/// note views (D-01), so this also makes the tab's group the preferred
/// document group for the D-03 destination order.
fn focus(app: &mut App, tab: TabId) {
    app.dispatch_layout(|revision| LayoutAction::ActivateTab {
        expected_revision: revision,
        tab,
    });
    assert_eq!(app.workspace.focused_tab(), tab);
    assert_eq!(app.workspace.document_anchor(), Some(tab));
}

/// Split the focused group; returns the created neighbor group.
fn split_focused(app: &mut App) -> GroupId {
    let before: std::collections::BTreeSet<GroupId> =
        app.workspace.group_order().into_iter().collect();
    let source = group_of(app, app.workspace.focused_tab());
    let committed = app.dispatch_layout_result(|revision| LayoutAction::SplitGroup {
        expected_revision: revision,
        group: source,
        arrangement: flavite_workspace::Arrangement::SideBySide,
    });
    assert!(committed, "the fixture split fits");
    app.workspace
        .group_order()
        .into_iter()
        .find(|group| !before.contains(group))
        .expect("split created a group")
}

fn wide(app: &mut App) {
    app.workspace_bounds =
        iced::Rectangle::new(iced::Point::ORIGIN, iced::Size::new(4000.0, 2000.0));
}

fn send(app: &mut App, intent: OpenIntent) {
    let _ = app.update(Message::OpenIntent(intent));
}

fn open_intents_fixture() -> (App, tempfile::TempDir) {
    let (app, _fs, cache) = graph_fixture::fixture_with(&[
        ("dup.md", "Shared note.\n"),
        ("solo.md", "Solo note.\n"),
        ("nested.md", "Nested note.\n"),
    ]);
    (app, cache)
}

#[test]
fn visible_match_in_the_preferred_group_wins() {
    let (mut app, _cache) = open_intents_fixture();
    wide(&mut app);
    let group_a = app.workspace.first_group().unwrap();
    let dup_a = open_in_group(&mut app, group_a, "dup.md");
    let group_b = split_focused(&mut app);
    let dup_b = open_in_group(&mut app, group_b, "dup.md");
    // Re-establish document focus in A: opening dup_b moved the anchor.
    focus(&mut app, dup_a);

    // Preferred group A holds a visible match; the newer tab in B must
    // not win over it (destination order step 1).
    send(&mut app, OpenIntent::reuse(locator("dup.md"), Some(dup_b)));
    assert_eq!(app.workspace.focused_tab(), dup_a);
    assert_eq!(group_of(&app, dup_a), group_a);
}

#[test]
fn visible_match_outside_the_preferred_group_is_reused_not_duplicated() {
    let (mut app, _cache) = open_intents_fixture();
    wide(&mut app);
    let group_a = app.workspace.first_group().unwrap();
    let _dup_a = open_in_group(&mut app, group_a, "dup.md");
    let solo_a = open_in_group(&mut app, group_a, "solo.md");
    // dup.md is now a *hidden* tab in A.

    let group_b = split_focused(&mut app);
    let dup_b = open_in_group(&mut app, group_b, "dup.md");
    assert!(app.is_tab_visible(dup_b));
    // Document focus returns to A: opening dup_b had moved the anchor to B.
    focus(&mut app, solo_a);

    // No visible match in the preferred group; the visible match in B is
    // reused — the hidden first-in-map tab in A is never activated.
    send(&mut app, OpenIntent::reuse(locator("dup.md"), None));
    assert_eq!(app.workspace.focused_tab(), dup_b);
    assert_eq!(
        tabs_of(&app, "dup.md").len(),
        2,
        "no duplicate tab was created"
    );
}

/// D-03: a hidden matching note must not be activated over the source
/// graph merely because it was first in a map; a neighboring document
/// group is created instead and the graph keeps its own visible group.
#[test]
fn hidden_match_in_the_source_graph_group_is_never_activated() {
    let (mut app, _cache) = open_intents_fixture();
    wide(&mut app);
    let group_a = app.workspace.first_group().unwrap();
    let hidden = open_in_group(&mut app, group_a, "dup.md");
    let solo = open_in_group(&mut app, group_a, "solo.md");
    app.dispatch_layout(|revision| LayoutAction::OpenGraph {
        expected_revision: revision,
        group: group_a,
    });
    // Close the anchor's note: no acknowledged document focus anywhere.
    app.dispatch_layout(|revision| LayoutAction::CloseTab {
        expected_revision: revision,
        tab: solo,
    });
    let graph_tab = app.workspace.focused_tab();
    assert!(matches!(
        app.workspace.tab(&graph_tab).map(|s| s.kind()),
        Some(ViewKind::Graph(_))
    ));
    // No anchor anywhere: preferred is None; the hidden dup.md in the
    // source graph group is the only match.
    assert_eq!(app.workspace.document_anchor(), None);

    send(
        &mut app,
        OpenIntent::reuse(locator("dup.md"), Some(graph_tab)),
    );
    // A neighboring document group was created (split of the source) and
    // the note opened there — never over the graph, never the hidden tab.
    let opened = app.workspace.focused_tab();
    assert_eq!(
        tabs_of(&app, "dup.md"),
        vec![hidden, opened],
        "a new tab was created; the hidden one untouched"
    );
    let opened_group = group_of(&app, opened);
    assert_ne!(opened_group, group_a, "opened over the source graph group");
    // The graph is still there and still visible in its own group.
    assert!(matches!(
        app.workspace.tab(&graph_tab).map(|s| s.kind()),
        Some(ViewKind::Graph(_))
    ));
    assert!(app.is_tab_visible(graph_tab));
}

#[test]
fn narrow_mode_falls_back_to_the_source_group_only() {
    let (mut app, _cache) = open_intents_fixture();
    // Adaptive narrow: the workspace no longer fits the window (the
    // smallest view minimums exceed this, so one group shows at a time).
    app.workspace_bounds = iced::Rectangle::new(iced::Point::ORIGIN, iced::Size::new(80.0, 60.0));
    let group_a = app.workspace.first_group().unwrap();
    app.dispatch_layout(|revision| LayoutAction::OpenGraph {
        expected_revision: revision,
        group: group_a,
    });
    let graph_tab = app.workspace.focused_tab();
    send(
        &mut app,
        OpenIntent::reuse(locator("solo.md"), Some(graph_tab)),
    );
    // The only narrow fallback: a new document tab in the source group.
    let opened = app.workspace.focused_tab();
    assert_eq!(tabs_of(&app, "solo.md"), vec![opened]);
    assert_eq!(group_of(&app, opened), group_a);
}

#[test]
fn closed_preferred_group_falls_to_the_mru_document_group() {
    let (mut app, _cache) = open_intents_fixture();
    wide(&mut app);
    let group_a = app.workspace.first_group().unwrap();
    let dup_a = open_in_group(&mut app, group_a, "dup.md");
    let group_b = split_focused(&mut app);
    let solo_b = open_in_group(&mut app, group_b, "solo.md");
    // Document focus back on dup.md in A before the close.
    focus(&mut app, dup_a);

    // Closing group A clears the anchor: preferred is recomputed from
    // acknowledged document focus, never persisted stale (D-03).
    app.dispatch_layout(|revision| LayoutAction::CloseGroup {
        expected_revision: revision,
        group: group_a,
    });
    assert_eq!(app.workspace.document_anchor(), None);
    assert!(app.workspace.group(&group_a).is_none());

    send(&mut app, OpenIntent::reuse(locator("dup.md"), Some(solo_b)));
    let opened = app.workspace.focused_tab();
    assert_eq!(tabs_of(&app, "dup.md"), vec![opened]);
    assert_eq!(group_of(&app, opened), group_b, "MRU document group wins");
}

#[test]
fn new_tab_bypasses_view_dedup_but_shares_the_buffer() {
    let (mut app, _cache) = open_intents_fixture();
    wide(&mut app);
    let group_a = app.workspace.first_group().unwrap();
    let dup_a = open_in_group(&mut app, group_a, "dup.md");
    let buffer_a = app.notes.buffer(&dup_a).expect("buffer attached");

    send(
        &mut app,
        OpenIntent {
            locator: locator("dup.md"),
            destination: OpenDestination::NewTab,
            source: None,
        },
    );
    let dup_a2 = app.workspace.focused_tab();
    assert_ne!(dup_a2, dup_a, "view dedup is bypassed");
    let mut both = tabs_of(&app, "dup.md");
    both.sort();
    assert_eq!(
        both,
        {
            let mut expected = vec![dup_a, dup_a2];
            expected.sort();
            expected
        },
        "exactly one new tab"
    );
    assert_eq!(
        group_of(&app, dup_a2),
        group_a,
        "preferred group receives it"
    );
    assert_eq!(
        app.notes.buffer(&dup_a2),
        Some(buffer_a),
        "buffer sharing survives the dedup bypass"
    );
}

/// Explicit same-tab quick switch goes through the in-tab navigation/
/// close-safety contract: the tab identity and mode are retained, the
/// replaced note enters Back, and the dirty draft stays recoverable.
#[test]
fn dirty_same_tab_quick_switch_keeps_tab_identity_and_history() {
    let (mut app, _cache) = open_intents_fixture();
    wide(&mut app);
    let group_a = app.workspace.first_group().unwrap();
    let dup_a = open_in_group(&mut app, group_a, "dup.md");

    // Make the buffer dirty through the acknowledged editor path.
    let incarnation = app.notes.incarnation(dup_a).expect("incarnation");
    let view = app.note_editor_view(dup_a).expect("editor view");
    let position = view.text.len();
    let _ = app.update(Message::EditorAction {
        tab: dup_a,
        incarnation,
        action: EditorAction::replace(&view, position, position, " draft".into()),
    });
    assert_eq!(
        app.note_buffer_state(dup_a),
        Some(flavite_api::BufferStatus::Dirty),
    );
    let original_buffer = app.notes.buffer(&dup_a).expect("buffer attached");

    send(
        &mut app,
        OpenIntent {
            locator: locator("solo.md"),
            destination: OpenDestination::ReplaceTab(dup_a),
            source: Some(dup_a),
        },
    );
    // Same tab identity and group; the note was replaced in place.
    assert_eq!(app.workspace.focused_tab(), dup_a);
    assert_eq!(group_of(&app, dup_a), group_a);
    assert_eq!(
        app.notes.locator_of(dup_a),
        Some(locator("solo.md")),
        "the tab now shows the quick-switch target"
    );
    // The replaced note entered this tab's Back history.
    assert!(app.nav.can_go_back(dup_a), "the replaced note entered Back");
    // The dirty draft's Session buffer survives the in-tab replacement
    // for recovery (last-view contract, D-06); the lease itself moved to
    // the quick-switch target.
    assert_ne!(app.notes.buffer(&dup_a), Some(original_buffer));
    assert!(
        app.flavite
            .as_ref()
            .expect("vault attached")
            .get_shared_buffer_view(flavite_buffer::BufferId::from(original_buffer), None)
            .is_ok()
    );
}
