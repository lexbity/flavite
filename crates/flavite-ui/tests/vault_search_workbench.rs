//! Vault search uses frozen source, guarded navigation and sealed replacement.
#[path = "support/mod.rs"]
mod support;
use flavite_fs::Fs;
use flavite_ui::{
    app::{App, Message},
    host::{
        replace::{ReplaceField, ReplacePanelMode, SelectAction},
        vault_search::{SIDEBAR_SEARCH, VaultSearchMessage as Search},
    },
};
use flavite_workspace::{LayoutAction, TabId};
use std::{
    path::Path,
    time::{Duration, Instant},
};

fn wait(app: &mut App, ready: impl Fn(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready(app) {
        assert!(
            Instant::now() < deadline,
            "{}; {:?}",
            app.status,
            app.replace.tabs
        );
        let _ = app.update(Message::Pump);
        std::thread::yield_now();
    }
}
fn fixture(seed: u64) -> support::UiFixture {
    let mut fixture = support::ui_fixture(
        seed,
        "/test/vault-search",
        &[
            support::SeedNote {
                components: &["dir", "a.md"],
                body: "heading\nx α\n",
            },
            support::SeedNote {
                components: &["dir", "c.md"],
                body: "another α\n",
            },
            support::SeedNote {
                components: &["dir-other", "b.md"],
                body: "outside α\n",
            },
        ],
    );
    wait(&mut fixture.app, |app| app.catalog().len() == 3);
    fixture
}
fn open(app: &mut App) -> TabId {
    let group = app.focused_group().unwrap();
    app.dispatch_layout(|expected_revision| LayoutAction::OpenReplace {
        expected_revision,
        group,
    });
    app.workspace.focused_tab()
}
fn edit(app: &mut App, tab: TabId, field: ReplaceField, value: &str) {
    let _ = app.update(Message::ReplaceEdit {
        tab,
        field,
        value: value.into(),
    });
}
fn find(app: &mut App, tab: TabId) {
    let _ = app.update(Message::ReplaceSubmitNow { tab });
    wait(app, |app| {
        app.replace.view_state(tab).finder.is_some() && !app.replace.view_state(tab).updating
    });
    assert!(app.replace.view_state(tab).error.is_none());
}

#[test]
fn folder_scope_unicode_excerpts_and_exact_jump_revalidate_the_source() {
    let mut f = fixture(39100);
    let tab = open(&mut f.app);
    edit(&mut f.app, tab, ReplaceField::Folder, "dir");
    edit(&mut f.app, tab, ReplaceField::Query, "α");
    find(&mut f.app, tab);
    let state = f.app.replace.view_state(tab);
    assert_eq!(state.total_matches, Some(2));
    let row = state
        .rows
        .iter()
        .find(|row| row.path == "dir/a.md")
        .unwrap()
        .clone();
    let hit = &row.contexts[0];
    assert_eq!((hit.line, hit.column), (2, 3));
    assert_eq!(hit.before, "x ");
    assert_eq!(hit.matched, "α");
    let action = Search::OpenMatch {
        owner: tab,
        finder: state.finder.clone().unwrap(),
        path: row.path.clone(),
        hash: row.content_hash.clone(),
        start: hit.start,
        end: hit.end,
    };
    let _ = f.app.update(Message::VaultSearch(action.clone()));
    wait(&mut f.app, |app| app.status == "Opened exact source match");
    let target = f.app.workspace.focused_tab();
    let interaction = f.app.workspace.tab(&target).unwrap().interaction();
    assert_eq!(
        (
            interaction.selections[0].anchor,
            interaction.selections[0].head
        ),
        (hit.start as usize, hit.end as usize)
    );
    // A real edit makes the old frozen match unusable, even at a valid offset.
    let view = f.app.note_editor_view(target).unwrap();
    let _ = f.app.update(Message::EditorInput {
        tab: target,
        incarnation: f.app.notes.incarnation(target).unwrap(),
        input: flavite_editor::input::CapturedInput {
            buffer: view.buffer_id,
            observed_revision: view.revision,
            sequence: interaction.sequence + 1,
            action: flavite_editor::keymap::KeyAction::Insert,
            text: "changed".into(),
            extend: false,
            document_boundary: false,
            vertical: None,
            preferred_x: Vec::new(),
            clipboard_acquired: false,
        },
    });
    let current = f.app.note_editor_view(target).unwrap();
    let _ = f.app.update(Message::VaultSearch(action));
    wait(&mut f.app, |app| app.status.contains("match is stale"));
    assert_eq!(
        f.app.note_editor_view(target).unwrap().revision,
        current.revision
    );
    assert_eq!(f.app.note_editor_view(target).unwrap().text, current.text);
}

#[test]
fn sealed_readable_changes_and_note_selection_apply_only_the_scoped_choice() {
    let mut f = fixture(39101);
    let tab = open(&mut f.app);
    edit(&mut f.app, tab, ReplaceField::Folder, "dir");
    edit(&mut f.app, tab, ReplaceField::Query, "α");
    edit(&mut f.app, tab, ReplaceField::Replacement, "Ω");
    let _ = f.app.update(Message::ReplaceSetMode {
        tab,
        mode: ReplacePanelMode::Preview,
    });
    let _ = f.app.update(Message::ReplaceRunPreview { tab });
    wait(&mut f.app, |app| {
        app.replace.view_state(tab).preview_id.is_some() && !app.replace.view_state(tab).updating
    });
    let candidates = f.app.replace.view_state(tab).candidates.clone();
    assert_eq!(candidates.len(), 2);
    let chosen = candidates
        .iter()
        .find(|candidate| candidate.path == "dir/a.md")
        .unwrap();
    let _ = f.app.update(Message::ReplaceInspect {
        tab,
        item_id: chosen.item_id,
    });
    wait(&mut f.app, |app| {
        app.replace
            .view_state(tab)
            .diffs
            .contains_key(&chosen.item_id)
    });
    let diff = f.app.replace.view_state(tab).diffs[&chosen.item_id]
        .as_ref()
        .unwrap();
    assert!(diff.before.contains('α'));
    assert!(diff.after.contains('Ω'));
    assert_eq!(
        f.fs.read_if_exists(Path::new("/test/vault-search/dir/a.md"))
            .unwrap()
            .unwrap(),
        "heading\nx α\n".as_bytes()
    );
    let _ = f.app.update(Message::ReplaceSelect {
        target: f.app.replace.view_state(tab).preview_target().unwrap(),
        tab,
        selection: SelectAction::All,
    });
    wait(&mut f.app, |app| {
        app.replace.view_state(tab).selected_count() == 2 && app.replace.view_state(tab).can_apply()
    });
    let _ = f.app.update(Message::ReplaceSelect {
        target: f.app.replace.view_state(tab).preview_target().unwrap(),
        tab,
        selection: SelectAction::Keys(vec![chosen.path_native.clone()]),
    });
    wait(&mut f.app, |app| {
        app.replace.view_state(tab).selected_count() == 1 && app.replace.view_state(tab).can_apply()
    });
    let _ = f.app.update(Message::ReplaceApply {
        target: f.app.replace.view_state(tab).preview_target().unwrap(),
        tab,
    });
    wait(&mut f.app, |app| {
        app.replace
            .view_state(tab)
            .job
            .as_ref()
            .is_some_and(|job| job.saved == 1)
    });
    assert_eq!(
        f.fs.read_if_exists(Path::new("/test/vault-search/dir/a.md"))
            .unwrap()
            .unwrap(),
        "heading\nx Ω\n".as_bytes()
    );
    assert_eq!(
        f.fs.read_if_exists(Path::new("/test/vault-search/dir/c.md"))
            .unwrap()
            .unwrap(),
        "another α\n".as_bytes()
    );
    assert_eq!(
        f.fs.read_if_exists(Path::new("/test/vault-search/dir-other/b.md"))
            .unwrap()
            .unwrap(),
        "outside α\n".as_bytes()
    );
}

#[test]
fn docking_preserves_the_live_query_and_pending_request() {
    let mut f = fixture(39102);
    let tab = open(&mut f.app);
    edit(&mut f.app, tab, ReplaceField::Query, "α");
    let _ = f.app.update(Message::ReplaceSubmitNow { tab });
    let _ = f.app.update(Message::VaultSearch(Search::Dock(tab)));
    wait(&mut f.app, |app| {
        app.replace.view_state(SIDEBAR_SEARCH).total_matches == Some(3)
    });
    let finder = f.app.replace.view_state(SIDEBAR_SEARCH).finder.clone();
    assert_eq!(f.app.replace.view_state(SIDEBAR_SEARCH).query, "α");
    assert!(f.app.workspace.tab(&tab).is_none());
    let _ = f.app.update(Message::VaultSearch(Search::Undock));
    let destination = f.app.workspace.focused_tab();
    assert_eq!(f.app.replace.view_state(destination).finder, finder);
    assert_eq!(f.app.replace.view_state(destination).total_matches, Some(3));
    assert!(!f.app.vault_search.indexed(&f.app, destination));
}

#[test]
fn stale_apply_controls_cannot_apply_a_newer_sealed_preview() {
    let mut f = fixture(39103);
    let tab = open(&mut f.app);
    edit(&mut f.app, tab, ReplaceField::Query, "α");
    edit(&mut f.app, tab, ReplaceField::Replacement, "first");
    let _ = f.app.update(Message::ReplaceSetMode {
        tab,
        mode: ReplacePanelMode::Preview,
    });
    let _ = f.app.update(Message::ReplaceRunPreview { tab });
    wait(&mut f.app, |app| {
        app.replace.view_state(tab).preview_id.is_some() && !app.replace.view_state(tab).updating
    });
    let target = f.app.replace.view_state(tab).preview_target().unwrap();
    let _ = f.app.update(Message::ReplaceSelect {
        tab,
        target,
        selection: SelectAction::All,
    });
    wait(&mut f.app, |app| app.replace.view_state(tab).can_apply());
    let old_control = f.app.replace.view_state(tab).preview_target().unwrap();
    edit(&mut f.app, tab, ReplaceField::Replacement, "newer");
    let _ = f.app.update(Message::ReplaceRunPreview { tab });
    wait(&mut f.app, |app| {
        app.replace.view_state(tab).preview_id.is_some() && !app.replace.view_state(tab).updating
    });
    let _ = f.app.update(Message::ReplaceApply {
        tab,
        target: old_control,
    });
    assert!(
        f.app
            .replace
            .view_state(tab)
            .error
            .as_ref()
            .unwrap()
            .contains("Preview changed")
    );
    assert!(f.app.replace.view_state(tab).job.is_none());
    assert_eq!(
        f.fs.read_if_exists(Path::new("/test/vault-search/dir/a.md"))
            .unwrap()
            .unwrap(),
        "heading\nx α\n".as_bytes()
    );
}

#[test]
fn changing_queries_and_previews_releases_the_bounded_session_slots() {
    let mut f = fixture(39104);
    let tab = open(&mut f.app);
    for query in ["α", "heading", "another", "outside", "α", "heading", "α"] {
        edit(&mut f.app, tab, ReplaceField::Query, query);
        find(&mut f.app, tab);
    }
    let _ = f.app.update(Message::ReplaceSetMode {
        tab,
        mode: ReplacePanelMode::Preview,
    });
    for replacement in ["one", "two", "three", "four", "five"] {
        edit(&mut f.app, tab, ReplaceField::Replacement, replacement);
        let _ = f.app.update(Message::ReplaceRunPreview { tab });
        wait(&mut f.app, |app| {
            app.replace.view_state(tab).preview_id.is_some()
                || app.replace.view_state(tab).error.is_some()
        });
        assert!(
            f.app.replace.view_state(tab).error.is_none(),
            "{:?}",
            f.app.replace.view_state(tab).error
        );
    }
}

#[test]
fn accessibility_exposes_search_and_rejects_retired_preview_controls() {
    use accesskit::{Action, ActionData, ActionRequest, TreeId};
    use flavite_ui::accessibility::{route_action, tree};
    let mut f = fixture(39106);
    let tab = open(&mut f.app);
    let request = |id, action, data| ActionRequest {
        action,
        target_tree: TreeId::ROOT,
        target_node: id,
        data,
    };
    let query = tree::shell_tree(&f.app)
        .nodes
        .into_iter()
        .find(|(_, node)| node.label() == Some("Find in Markdown source"))
        .unwrap()
        .0;
    let message = route_action(
        &f.app,
        request(query, Action::SetValue, Some(ActionData::Value("α".into()))),
    )
    .expect("accessible query input");
    let _ = f.app.update(message);
    find(&mut f.app, tab);
    let projection = tree::shell_tree(&f.app);
    assert!(projection.nodes.iter().any(|(_, node)| {
        node.label()
            .is_some_and(|label| label.contains("line 2 column 3"))
    }));
    let _ = f.app.update(Message::ReplaceSetMode {
        tab,
        mode: ReplacePanelMode::Preview,
    });
    edit(&mut f.app, tab, ReplaceField::Replacement, "NEW");
    let _ = f.app.update(Message::ReplaceRunPreview { tab });
    wait(&mut f.app, |app| {
        app.replace.view_state(tab).preview_id.is_some() && !app.replace.view_state(tab).updating
    });
    let old = tree::shell_tree(&f.app)
        .nodes
        .into_iter()
        .find(|(_, node)| node.label() == Some("Select all notes"))
        .unwrap()
        .0;
    assert!(route_action(&f.app, request(old, Action::Click, None)).is_some());
    edit(&mut f.app, tab, ReplaceField::Replacement, "OTHER");
    assert!(route_action(&f.app, request(old, Action::Click, None)).is_none());
}
