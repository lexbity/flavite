//! Multi-selection integration (FEUX-0002 X04): building sets through the
//! real acknowledged-interaction publish path, kernel edits across every
//! member, paste broadcast through the clipboard seam, cut staleness, link
//! insertion refusal at N>1, OFF-state set retention, and passive-view
//! mapping once.

#[path = "support/mod.rs"]
mod support;

use flavite_edit_context::ContextCommand as Command;
use flavite_editor::{EditorAction, EditorInteractionSnapshot};
use flavite_ui::app::{App, Message};
use flavite_ui::host::context_mode::{ContextMessage, EditorTarget};
use flavite_ui::host::editor_clipboard::ClipboardMessage;
use flavite_workspace::{LayoutAction, NoteLocator, TabId};
use std::time::{Duration, Instant};

const BODY: &str = "one café\nsecond line\nthird row\n";

fn fixture(seed: u64) -> (support::UiFixture, TabId) {
    let mut f = support::ui_fixture(
        seed,
        &format!("/test/multi-{seed}"),
        &[
            support::SeedNote {
                components: &["note.md"],
                body: BODY,
            },
            support::SeedNote {
                components: &["other.md"],
                body: "other\n",
            },
        ],
    );
    let group = f.app.focused_group().unwrap();
    f.app
        .dispatch_layout(|expected_revision| LayoutAction::OpenNote {
            expected_revision,
            group,
            locator: NoteLocator::from_components(["note.md"]).unwrap(),
        });
    let tab = f.app.workspace.focused_tab();
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.app.note_editor_view(tab).is_none() {
        assert!(Instant::now() < deadline, "{}", f.app.status_text());
        let _ = f.app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(2));
    }
    selection(&mut f.app, tab, 0, 0);
    (f, tab)
}

fn selection(app: &mut App, tab: TabId, anchor: usize, head: usize) {
    let view = app.note_editor_view(tab).unwrap();
    let _ = app.update(Message::AccessibleSelection {
        tab,
        incarnation: app.notes.incarnation(tab).unwrap(),
        revision: view.revision,
        anchor,
        head,
    });
}

/// Publish a full member set through the real acknowledged-interaction
/// publish path (the same path pointer input uses: `EditorAction::Interaction`).
fn publish_members(app: &mut App, tab: TabId, members: &[(usize, usize)]) {
    let view = app.note_editor_view(tab).unwrap();
    let sequence = app.workspace.tab(&tab).unwrap().interaction().sequence + 1;
    let primary = members[0];
    let snapshot = EditorInteractionSnapshot {
        sequence,
        buffer_id: view.buffer_id,
        revision: view.revision,
        caret: primary.1,
        selection: primary,
        selections: members
            .iter()
            .enumerate()
            .map(|(id, &(anchor, head))| [id, anchor, head])
            .collect(),
        primary: 0,
        viewport_anchor: 0,
        focused: true,
        composing: false,
    };
    let _ = app.update(Message::EditorAction {
        tab,
        incarnation: app.notes.incarnation(tab).unwrap(),
        action: EditorAction::Interaction(snapshot),
    });
    assert_eq!(
        app.workspace
            .tab(&tab)
            .unwrap()
            .interaction()
            .selections
            .len(),
        members.len(),
        "the committed set holds every member"
    );
}

fn run(app: &mut App, tab: TabId, command: Command) {
    let target = EditorTarget::capture(app, tab).unwrap();
    let _ = app.update(Message::ContextMode(ContextMessage::Run(target, command)));
}

fn committed_members(app: &App, tab: TabId) -> Vec<(usize, usize)> {
    app.workspace
        .tab(&tab)
        .unwrap()
        .interaction()
        .selections
        .iter()
        .map(|s| (s.anchor, s.head))
        .collect()
}

#[test]
fn publish_builds_a_multi_member_set_and_kernel_delete_applies_across_all() {
    let (mut f, tab) = fixture(5010);
    // Carets at the start of the first two lines ("one café\n" is 10
    // bytes, so the second line starts at 10).
    publish_members(&mut f.app, tab, &[(0, 0), (10, 10)]);
    let before_text = f.app.note_editor_view(tab).unwrap().text.clone();
    let before_revision = f.app.note_editor_view(tab).unwrap().revision;

    run(&mut f.app, tab, Command::Toggle);
    assert_eq!(
        f.app.context_mode.mode(tab),
        flavite_edit_context::ContextMode::Command
    );
    run(&mut f.app, tab, Command::Delete);

    // Both leading characters were deleted in one atomic batch.
    assert_eq!(
        &*f.app.note_editor_view(tab).unwrap().text,
        "ne café\necond line\nthird row\n"
    );
    let after_revision = f.app.note_editor_view(tab).unwrap().revision;
    assert!(after_revision > before_revision);

    // Undo restores the exact text and the recorded before-set.
    run(&mut f.app, tab, Command::Undo);
    assert_eq!(f.app.note_editor_view(tab).unwrap().text, before_text);
    assert_eq!(committed_members(&f.app, tab), vec![(0, 0), (10, 10)]);
}

#[test]
fn add_caret_below_adds_one_caret_per_member_via_the_host_row_path() {
    let (mut f, tab) = fixture(5020);
    selection(&mut f.app, tab, 0, 0);
    run(&mut f.app, tab, Command::Toggle);
    run(&mut f.app, tab, Command::AddCaretBelow);
    let members = committed_members(&f.app, tab);
    assert_eq!(members.len(), 2, "one caret added on the adjacent row");
    assert_eq!(members[0], (0, 0));
    assert_eq!(members[1], (10, 10), "the second line start");
}

#[test]
fn paste_broadcast_through_the_clipboard_seam_reaches_every_member() {
    let (mut f, tab) = fixture(5030);
    publish_members(&mut f.app, tab, &[(0, 0), (10, 10)]);
    run(&mut f.app, tab, Command::Toggle);

    let target = EditorTarget::capture(&f.app, tab).unwrap();
    let generation = f.app.context_mode.generation();
    let _ = f
        .app
        .update(Message::EditorClipboard(ClipboardMessage::ContextPaste {
            target,
            generation,
            text: Some("·X·".to_owned()),
        }));

    assert_eq!(
        &*f.app.note_editor_view(tab).unwrap().text,
        "·X·one café\n·X·second line\nthird row\n"
    );
    // Both members collapsed past their own insertion (stick right). "·X·"
    // is 5 bytes; the second member was at 10, shifted by the first
    // insertion to 15, then landed past its own insertion at 20.
    assert_eq!(committed_members(&f.app, tab), vec![(5, 5), (20, 20)]);
}

#[test]
fn copy_at_the_api_level_joins_fragments_with_lf_in_document_order() {
    let (mut f, tab) = fixture(5040);
    // "one" (0..3) and "second" (10..16).
    publish_members(&mut f.app, tab, &[(0, 3), (10, 16)]);
    let target = EditorTarget::capture(&f.app, tab).unwrap();
    let api = f.app.flavite.as_ref().unwrap();
    let result = api.execute(flavite_api::Invocation::new(
        api.command_registry().next_request_id(),
        "editor.copy",
        serde_json::json!({
            "buffer_id": target.buffer,
            "revision": target.revision,
            "selections": target.selections_json(),
        }),
        flavite_api::Origin::Gui,
    ));
    let flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Success(success)) = result
    else {
        panic!("copy must succeed: {result:?}");
    };
    assert_eq!(success.data["text"].as_str().unwrap(), "one\nsecond");
}

#[test]
fn cut_collapses_every_member_and_a_stale_target_is_a_zero_effect_refusal() {
    let (mut f, tab) = fixture(5050);
    publish_members(&mut f.app, tab, &[(0, 0), (10, 10)]);
    run(&mut f.app, tab, Command::Toggle);
    // A target captured BEFORE the cut is stale after it lands.
    let stale = EditorTarget::capture(&f.app, tab).unwrap();
    run(&mut f.app, tab, Command::Cut);
    // Clipboard ordering (\u{a7}5.5): the write completion revalidates the
    // source before the deletion. Until then the text is untouched.
    assert_eq!(
        &*f.app.note_editor_view(tab).unwrap().text,
        "one caf\u{e9}\nsecond line\nthird row\n",
        "the cut must not delete before the clipboard write completed"
    );
    let _ = f
        .app
        .update(Message::EditorClipboard(ClipboardMessage::CutReady {
            target: EditorTarget::capture(&f.app, tab).unwrap(),
            generation: f.app.context_mode.generation(),
        }));

    // D-11 cut of two carets takes each containing line including its break.
    assert_eq!(&*f.app.note_editor_view(tab).unwrap().text, "third row\n");

    // Replaying the stale target through the revalidated seam is a
    // zero-effect refusal: the source no longer matches the capture.
    let text_before = f.app.note_editor_view(tab).unwrap().text.clone();
    let _ = f
        .app
        .update(Message::EditorClipboard(ClipboardMessage::CutReady {
            target: stale,
            generation: f.app.context_mode.generation(),
        }));
    assert_eq!(f.app.note_editor_view(tab).unwrap().text, text_before);
}

#[test]
fn selection_only_command_keeps_the_acknowledged_input_chain_current() {
    use flavite_editor::input::CapturedInput;
    use flavite_editor::keymap::KeyAction;
    let (mut f, tab) = fixture(5090);
    run(&mut f.app, tab, Command::Toggle);
    run(&mut f.app, tab, Command::AddCaretBelow);
    assert_eq!(committed_members(&f.app, tab).len(), 2);
    // The next captured key (as the widget would send it, one sequence past
    // the committed interaction) must be accepted, not refused by the
    // acknowledged-input guard.
    let view = f.app.note_editor_view(tab).unwrap();
    let sequence = f.app.workspace.tab(&tab).unwrap().interaction().sequence + 1;
    let _ = f.app.update(Message::EditorInput {
        tab,
        incarnation: f.app.notes.incarnation(tab).unwrap(),
        input: CapturedInput {
            buffer: view.buffer_id,
            observed_revision: view.revision,
            sequence,
            action: KeyAction::Down,
            text: String::new(),
            extend: false,
            document_boundary: false,
            vertical: Some(flavite_editor::navigation::VerticalMotion {
                rows: 1,
                width: 500.0,
                metrics: flavite_editor::TypeMetrics::default(),
            }),
            preferred_x: vec![Some(0.0), Some(0.0)],
            clipboard_acquired: false,
        },
    });
    assert!(
        !f.app.status.contains("Input source changed"),
        "the key must not be refused: {}",
        f.app.status
    );
    assert_eq!(committed_members(&f.app, tab).len(), 2);
}

#[test]
fn link_insertion_refuses_at_n_gt_1_with_the_specified_message() {
    let (mut f, tab) = fixture(5060);
    publish_members(&mut f.app, tab, &[(0, 0), (10, 10)]);
    let _ = f.app.update(Message::InsertLink {
        locator: NoteLocator::from_components(["other.md"]).unwrap(),
    });
    assert_eq!(
        f.app.status, "Choose one caret to insert a link.",
        "multi-member sets have no one insertion point"
    );
}

#[test]
fn off_state_keeps_the_set_after_exit() {
    let (mut f, tab) = fixture(5070);
    publish_members(&mut f.app, tab, &[(0, 0), (10, 10)]);
    run(&mut f.app, tab, Command::Toggle);
    assert_eq!(committed_members(&f.app, tab).len(), 2);
    run(&mut f.app, tab, Command::Exit);
    assert_eq!(
        f.app.context_mode.mode(tab),
        flavite_edit_context::ContextMode::Off
    );
    // The set survives mode exit (FR-13); Esc-collapse is widget-local and
    // covered by the widget's vocabulary tests.
    assert_eq!(committed_members(&f.app, tab), vec![(0, 0), (10, 10)]);
}

/// Release-only performance evidence (FEUX-0002 NFR-2/NFR-3): edit
/// planning latency at rising cardinalities and the committed-set install
/// path. Run with `cargo test --release -p flavite-ui --test
/// multi_selection -- --ignored --nocapture`.
#[test]
#[ignore]
fn nfr_latency_bench_at_rising_cardinality() {
    // ~1 MiB document with many lines.
    let mut doc = String::with_capacity(1024 * 1024);
    let mut i = 0;
    while doc.len() < 1024 * 1024 {
        doc.push_str(&format!("line {i} some words here\n"));
        i += 1;
    }
    let lines: Vec<usize> = doc.match_indices('\n').map(|(at, _)| at + 1).collect();
    let _view = flavite_editor::EditorView {
        buffer_id: 7,
        revision: 1,
        text: std::sync::Arc::from(doc.as_str()),
        path: "bench.md".into(),
        state: flavite_buffer::state::BufferState::Clean,
    };
    let raw: Vec<flavite_buffer::selection::RawSelection> =
        vec![flavite_buffer::selection::RawSelection {
            id: None,
            anchor: 0,
            head: 0,
        }];
    let (base, _) = flavite_buffer::selection::SelectionSet::normalize(&doc, raw, None).unwrap();
    for count in [1usize, 100, 1_000, 10_000] {
        let members: Vec<(usize, usize)> = (0..count)
            .map(|k| {
                let at = lines[(k * (lines.len() - 1)) / count.max(1)];
                (at, at)
            })
            .collect();
        let raw: Vec<flavite_buffer::selection::RawSelection> = members
            .iter()
            .map(|&(a, h)| flavite_buffer::selection::RawSelection {
                id: None,
                anchor: a,
                head: h,
            })
            .collect();
        let t = std::time::Instant::now();
        let (set, _) = flavite_buffer::selection::SelectionSet::normalize(&doc, raw, None).unwrap();
        let normalize = t.elapsed();

        let t = std::time::Instant::now();
        let prepared = flavite_buffer::commands::plan(
            &doc,
            &set,
            &flavite_buffer::commands::EditOperation::Typing("·x·".into()),
        )
        .unwrap();
        let plan = t.elapsed();

        let t = std::time::Instant::now();
        let _ = flavite_buffer::commands::plan(
            &doc,
            &set,
            &flavite_buffer::commands::EditOperation::Delete,
        )
        .unwrap();
        let delete = t.elapsed();

        assert!(!prepared.after_set.is_empty());
        assert!(
            prepared
                .after_set
                .validate_against(&prepared.after_text)
                .is_ok()
        );
        let _ = &base;
        println!("N={count:6}: normalize {normalize:?}  typing {plan:?}  delete {delete:?}");
    }
}

#[test]
fn passive_view_maps_once_for_a_multi_member_edit() {
    let (mut f, tab_a) = fixture(5080);
    let group = f.app.focused_group().unwrap();
    f.app
        .dispatch_layout(|expected_revision| LayoutAction::OpenNote {
            expected_revision,
            group,
            locator: NoteLocator::from_components(["note.md"]).unwrap(),
        });
    let tab_b = f.app.workspace.focused_tab();
    assert_ne!(tab_a, tab_b);
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.app.note_editor_view(tab_b).is_none() {
        assert!(Instant::now() < deadline, "{}", f.app.status_text());
        let _ = f.app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(2));
    }
    // B holds a single caret at 0..2; focus A and build a multi-set there.
    selection(&mut f.app, tab_b, 0, 2);
    f.app
        .dispatch_layout(|expected_revision| LayoutAction::ActivateTab {
            expected_revision,
            tab: tab_a,
        });
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.app.workspace.focused_tab() != tab_a {
        assert!(Instant::now() < deadline, "{}", f.app.status_text());
        let _ = f.app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(2));
    }
    publish_members(&mut f.app, tab_a, &[(0, 0), (10, 10)]);
    run(&mut f.app, tab_a, Command::Toggle);
    run(&mut f.app, tab_a, Command::Delete);

    assert_eq!(
        &*f.app.note_editor_view(tab_b).unwrap().text,
        "ne café\necond line\nthird row\n"
    );
    // B's selection 0..2 ("on") maps through the deletion of [0, 1):
    // the anchor at the deletion start sticks left (0), the head shifts to
    // 1 — the surviving "n" is selected.
    assert_eq!(
        f.app.editor_interaction_snapshot(tab_b).unwrap().selection,
        (0, 1)
    );
}

#[test]
fn rotate_then_keep_primary_keeps_the_rotated_member() {
    // FEUX-0002 R7: primary identity survives the host handoffs — capture,
    // selections wire form, the kernel response, and the committed
    // interaction (E-4, FR-4/FR-5/FR-7).
    let (mut f, tab) = fixture(5060);
    publish_members(&mut f.app, tab, &[(0, 0), (10, 10)]);
    run(&mut f.app, tab, Command::Toggle);
    assert_eq!(
        f.app.workspace.tab(&tab).unwrap().interaction().primary,
        0,
        "the first member starts as the primary"
    );
    run(&mut f.app, tab, Command::RotatePrimary);
    assert_eq!(
        f.app.workspace.tab(&tab).unwrap().interaction().primary,
        1,
        "the rotated primary is the second member"
    );
    assert_eq!(
        f.app.workspace.tab(&tab).unwrap().interaction().selections,
        f.app.workspace.tab(&tab).unwrap().interaction().selections,
    );
    // Only the primary changed, so the committed selection version moved.
    let rotated_members = committed_members(&f.app, tab);
    assert_eq!(rotated_members, vec![(0, 0), (10, 10)]);
    run(&mut f.app, tab, Command::KeepPrimary);
    assert_eq!(committed_members(&f.app, tab), vec![(10, 10)]);
}

#[test]
fn command_escape_keeps_only_primary_when_n_exceeds_one() {
    // FEUX-0002 R10 / §5.4 row 10: Command Escape with N>1 keeps the
    // primary so the user can discard additional targets before typing.
    use flavite_edit_context::ContextMode;
    let (mut f, tab) = fixture(5070);
    publish_members(&mut f.app, tab, &[(0, 0), (10, 10)]);
    run(&mut f.app, tab, Command::Toggle);
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Command);
    run(&mut f.app, tab, Command::Escape);
    assert_eq!(
        f.app.context_mode.mode(tab),
        ContextMode::Command,
        "Escape stays in Command"
    );
    assert_eq!(
        committed_members(&f.app, tab),
        vec![(0, 0)],
        "the primary member survives"
    );
}

#[test]
fn ime_commit_lands_at_the_primary_only() {
    // FEUX-0002 R6 / FR-20: a composition commit joins Typing at the
    // primary caret only; the other member is retained and mapped without
    // receiving composed text.
    use flavite_edit_context::ContextMode;
    use flavite_editor::context::{ContextEvent, ContextRequest};
    use flavite_editor::input::CapturedInput;
    use flavite_editor::keymap::KeyAction;
    let (mut f, tab) = fixture(5080);
    publish_members(&mut f.app, tab, &[(0, 0), (10, 10)]);
    run(&mut f.app, tab, Command::Toggle);
    run(&mut f.app, tab, Command::InsertStart);
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Typing);
    let target = EditorTarget::capture(&f.app, tab).unwrap();
    let sequence = f.app.workspace.tab(&tab).unwrap().interaction().sequence + 1;
    let _ = f.app.update(Message::ContextNative {
        tab,
        incarnation: target.incarnation,
        request: ContextRequest {
            buffer: target.buffer,
            revision: target.revision,
            selection: target.selection,
            selections: target.selections.clone(),
            sequence,
            event: ContextEvent::Commit(CapturedInput {
                buffer: target.buffer,
                observed_revision: target.revision,
                sequence,
                action: KeyAction::Insert,
                text: "X".into(),
                extend: false,
                document_boundary: false,
                vertical: None,
                preferred_x: Vec::new(),
                clipboard_acquired: false,
            }),
        },
    });
    assert_eq!(
        &*f.app.note_editor_view(tab).unwrap().text,
        "Xone café\nsecond line\nthird row\n",
        "the composed text lands at the primary only"
    );
    let members = committed_members(&f.app, tab);
    assert_eq!(members.len(), 2, "the second member is retained");
    assert_eq!(
        members[0],
        (1, 1),
        "the primary caret sticks right of the insertion"
    );
    assert_eq!(
        members[1],
        (11, 11),
        "the second member shifts by the delta"
    );
}

#[test]
fn multi_selection_history_state_is_restorable() {
    // FEUX-0002 R8 / FR-18/FR-19: a valid, retained multi-selection state
    // is restorable through the shipped history admission control.
    let (mut f, tab) = fixture(5100);
    publish_members(&mut f.app, tab, &[(0, 0), (10, 10)]);
    run(&mut f.app, tab, Command::Toggle);
    run(&mut f.app, tab, Command::Delete);
    let _ = f.app.update(Message::ContextMode(ContextMessage::Run(
        EditorTarget::capture(&f.app, tab).unwrap(),
        Command::Undo,
    )));
    // After the undo, the newest entry (the batch delete) is a non-current
    // two-selection state.
    let source = EditorTarget::capture(&f.app, tab).unwrap();
    let page = f.app.context_mode.history.page.clone().unwrap();
    let entry = page
        .entries
        .iter()
        .find(|entry| {
            !entry.current
                && entry.restorable
                && entry.selection_count.is_some_and(|count| count > 1)
        })
        .unwrap_or_else(|| panic!("a two-selection history entry must exist"));
    assert!(
        flavite_ui::host::editor_history::can_restore(&f.app, source, entry),
        "the multi-selection state must pass admission"
    );
}

#[test]
fn completed_typing_paste_installs_the_acquired_text_without_reacquiring() {
    // FEUX-0002 R2 / FR-8/FR-9: a delivered TypingPaste completion applies
    // the prepared edit; it never loops back into clipboard acquisition.
    use flavite_editor::input::CapturedInput;
    use flavite_editor::keymap::KeyAction;
    let (mut f, tab) = fixture(5110);
    run(&mut f.app, tab, Command::Toggle);
    let target = EditorTarget::capture(&f.app, tab).unwrap();
    let sequence = f.app.workspace.tab(&tab).unwrap().interaction().sequence + 1;
    let _ = f
        .app
        .update(Message::EditorClipboard(ClipboardMessage::TypingPaste {
            tab,
            incarnation: target.incarnation,
            input: CapturedInput {
                buffer: target.buffer,
                observed_revision: target.revision,
                sequence,
                action: KeyAction::Paste,
                text: String::new(),
                extend: false,
                document_boundary: false,
                vertical: None,
                preferred_x: Vec::new(),
                clipboard_acquired: false,
            },
            text: Some("X".into()),
        }));
    assert_eq!(
        &*f.app.note_editor_view(tab).unwrap().text,
        "Xone café\nsecond line\nthird row\n",
        "the completed paste installs the clipboard text"
    );
    // An empty clipboard read terminates as a no-op.
    let view = f.app.note_editor_view(tab).unwrap();
    let sequence = f.app.workspace.tab(&tab).unwrap().interaction().sequence + 1;
    let _ = f
        .app
        .update(Message::EditorClipboard(ClipboardMessage::TypingPaste {
            tab,
            incarnation: target.incarnation,
            input: CapturedInput {
                buffer: view.buffer_id,
                observed_revision: view.revision,
                sequence,
                action: KeyAction::Paste,
                text: String::new(),
                extend: false,
                document_boundary: false,
                vertical: None,
                preferred_x: Vec::new(),
                clipboard_acquired: false,
            },
            text: None,
        }));
    assert_eq!(
        &*f.app.note_editor_view(tab).unwrap().text,
        "Xone café\nsecond line\nthird row\n",
        "an empty clipboard stays a no-op"
    );
}
