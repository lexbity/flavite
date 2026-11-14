use flavite_buffer::{
    EditorModel,
    history::{EditKind, EditorEdit, HistoryError},
    undo::{EditOp, UndoPolicy},
};
use flavite_syntax::text::{Span, TextByte, TextSnapshot};

#[test]
fn history_display_time_belongs_to_the_edit_and_not_to_head_navigation() {
    let mut model = EditorModel::from_text("base");
    edit(&mut model, 0, "A", EditKind::Typing);
    model.seal_undo_group();
    let page = model.history_page(None, 100).unwrap();
    let entry = page.entries[0].clone();
    assert!(entry.changed_at_unix_ms > 0);
    edit(&mut model, 1, "B", EditKind::Typing);
    model.restore_state(entry.target).unwrap();
    model.undo().unwrap();
    model.redo().unwrap();
    let restored = model
        .history_page(None, 100)
        .unwrap()
        .entries
        .into_iter()
        .find(|e| e.target == entry.target)
        .unwrap();
    assert_eq!(restored.changed_at_unix_ms, entry.changed_at_unix_ms);
    let wire = serde_json::to_value(restored).unwrap();
    assert_eq!(wire["changed_at_unix_ms"], entry.changed_at_unix_ms);
    assert!(
        !wire.to_string().contains("base"),
        "display metadata never contains note text"
    );
}

fn edit(model: &mut EditorModel, at: usize, value: &str, kind: EditKind) {
    let record = EditorEdit {
        before_primary: 0,
        origin: format!("{:032x}", 1),
        kind,
        before: vec![[0, at, at]],
        after: vec![[0, at + value.len(), at + value.len()]],
        primary: 0,
    };
    model
        .try_apply_recorded_batch(
            &[EditOp::new(
                Span::new(TextByte(at), TextByte(at)),
                value.into(),
            )],
            UndoPolicy::Context { origin: 1 },
            record,
        )
        .unwrap();
}

#[test]
fn restore_moves_the_existing_head_and_preserves_branches_format_and_baseline() {
    let mut model = EditorModel::from_snapshot(TextSnapshot::new(b"a\r\nb\n").unwrap());
    let root = model.history_page(None, 100).unwrap().entries[0].target;
    edit(&mut model, 0, "雪", EditKind::Typing);
    model.seal_undo_group();
    let first = model
        .history_page(None, 100)
        .unwrap()
        .trail
        .last()
        .unwrap()
        .target;
    edit(&mut model, 3, "B", EditKind::Typing);
    let revision = model.revision();
    let count = model.history_page(None, 100).unwrap().entries.len();
    assert_eq!(model.restore_state(first).unwrap(), revision.next());
    assert_eq!(model.text(), "雪a\nb\n");
    assert_eq!(
        model.history_page(None, 100).unwrap().entries.len(),
        count,
        "restore must not add a node"
    );
    assert_eq!(
        model.history_movement().unwrap().selections,
        Some(vec![[0, 3, 3]])
    );
    edit(&mut model, 3, "branch", EditKind::Typing);
    assert_eq!(
        model.history_page(None, 100).unwrap().entries.len(),
        count + 1
    );
    model.restore_state(root).unwrap();
    assert_eq!(model.try_to_source_bytes().unwrap(), b"a\r\nb\n");
    assert!(
        !model.is_dirty(),
        "restore compares with the current saved baseline"
    );
    assert_eq!(
        model.history_movement().unwrap().selections,
        Some(vec![[0, 0, 0]])
    );
    model.restore_state(first).unwrap();
    assert_eq!(
        model.try_to_source_bytes().unwrap(),
        "雪a\r\nb\n".as_bytes()
    );
    assert!(model.is_dirty());
}

#[test]
fn coalescing_versions_expire_old_targets_and_reload_ends_the_epoch() {
    let mut model = EditorModel::from_text("base");
    edit(&mut model, 0, "A", EditKind::Change);
    let old = model.history_page(None, 1).unwrap();
    let target = old.entries[0].target;
    edit(&mut model, 1, "B", EditKind::Typing);
    let latest = model.history_page(None, 100).unwrap();
    assert_eq!(latest.entries[0].target.node, target.node);
    assert!(latest.entries[0].target.version > target.version);
    assert_eq!(latest.entries[0].kind, EditKind::Change);
    let revision = model.revision();
    assert_eq!(model.restore_state(target), Err(HistoryError::Expired));
    assert_eq!(model.revision(), revision);
    assert_eq!(
        model.history_page(old.next, 1),
        Err(HistoryError::StalePage)
    );
    let current = latest.entries[0].target;
    model.replace_snapshot(TextSnapshot::new(b"disk").unwrap());
    assert_eq!(model.restore_state(current), Err(HistoryError::Expired));
    assert_eq!(model.text(), "disk");
}

#[test]
fn durable_boundaries_refuse_arbitrary_jumps_but_allow_same_segment_restore() {
    let mut model = EditorModel::from_text("base");
    let root = model.history_page(None, 100).unwrap().entries[0].target;
    model.apply_journal_snapshot_boundary(
        TextSnapshot::new(b"rename").unwrap(),
        flavite_buffer::undo::JournalTag {
            txn_id: "transaction".into(),
            item_id: 1,
        },
    );
    let journal = model.history_page(None, 100).unwrap().entries[0].target;
    edit(&mut model, 0, "A", EditKind::Typing);
    assert_eq!(
        model.restore_state(root),
        Err(HistoryError::JournalBoundary)
    );
    let page = model.history_page(None, 100).unwrap();
    assert!(
        !page
            .entries
            .iter()
            .find(|e| e.target == root)
            .unwrap()
            .restorable
    );
    model.restore_state(journal).unwrap();
    assert_eq!(model.text(), "rename");
    assert!(
        model.undo().is_some(),
        "ordinary Undo still crosses the tagged transition"
    );
    assert_eq!(model.text(), "base");
}

#[test]
fn invalid_recorded_graphemes_refuse_before_any_text_or_history_change() {
    let mut model = EditorModel::from_text("e\u{301}x");
    let before = model.history_page(None, 100).unwrap();
    let result = model.try_apply_recorded_batch(
        &[EditOp::new(Span::new(TextByte(0), TextByte(0)), "z".into())],
        UndoPolicy::Typing,
        EditorEdit {
            before_primary: 0,
            origin: "1".repeat(32),
            kind: EditKind::Typing,
            before: vec![[0, 1, 1]],
            after: vec![[0, 1, 1]],
            primary: 0,
        },
    );
    assert!(result.is_err());
    assert_eq!(model.text(), "e\u{301}x");
    assert_eq!(model.history_page(None, 100).unwrap(), before);
}

#[test]
fn unrecorded_context_edits_cannot_extend_a_recorded_selection_receipt() {
    let mut model = EditorModel::from_text("base");
    edit(&mut model, 0, "A", EditKind::Typing);
    let recorded = model.history_page(None, 100).unwrap().entries[0].target;
    model
        .try_apply_batch(
            &[EditOp::new(Span::new(TextByte(1), TextByte(1)), "B".into())],
            UndoPolicy::Context { origin: 1 },
        )
        .unwrap();
    let page = model.history_page(None, 100).unwrap();
    assert_eq!(page.entries.len(), 3);
    assert_eq!(page.entries[0].selection_count, None);
    model.restore_state(recorded).unwrap();
    assert_eq!(model.text(), "Abase");
    assert_eq!(
        model.history_movement().unwrap().selections,
        Some(vec![[0, 1, 1]])
    );
}

#[test]
fn epochs_isolate_buffers_and_invalid_origins_are_atomic() {
    let mut first = EditorModel::from_text("base");
    let mut second = EditorModel::from_text("base");
    let foreign = first.history_page(None, 100).unwrap().entries[0].target;
    assert_eq!(second.restore_state(foreign), Err(HistoryError::Expired));
    let before = first.history_page(None, 100).unwrap();
    assert!(
        first
            .try_apply_recorded_batch(
                &[EditOp::new(Span::new(TextByte(0), TextByte(0)), "A".into())],
                UndoPolicy::Context { origin: 2 },
                EditorEdit {
                    before_primary: 0,
                    origin: format!("{:032x}", 1),
                    kind: EditKind::Typing,
                    before: vec![[0, 0, 0]],
                    after: vec![[0, 1, 1]],
                    primary: 0,
                },
            )
            .is_err()
    );
    assert_eq!(first.text(), "base");
    assert_eq!(first.history_page(None, 100).unwrap(), before);
}

#[test]
fn selecting_an_abandoned_branch_adopts_its_redo_continuation() {
    let mut model = EditorModel::from_text("base");
    let root = model.history_page(None, 100).unwrap().entries[0].target;
    edit(&mut model, 0, "A", EditKind::Typing);
    model.seal_undo_group();
    let a = model.history_page(None, 100).unwrap().entries[0].target;
    edit(&mut model, 1, "B", EditKind::Typing);
    model.seal_undo_group();
    let b = model.history_page(None, 100).unwrap().entries[0].target;
    model.restore_state(root).unwrap();
    edit(&mut model, 0, "C", EditKind::Typing);
    let c = model.history_page(None, 100).unwrap().entries[0].target;
    let count = model.history_page(None, 100).unwrap().entries.len();
    model.restore_state(b).unwrap();
    model.undo().unwrap();
    assert_eq!(model.text(), "Abase");
    model.redo().unwrap();
    assert_eq!(
        model.text(),
        "ABbase",
        "Redo follows the branch the user selected"
    );
    model.restore_state(a).unwrap();
    assert_eq!(
        model.history_page(None, 100).unwrap().redo.unwrap().target,
        b
    );
    model.restore_state(root).unwrap();
    model.restore_state(c).unwrap();
    model.restore_state(a).unwrap();
    model.redo().unwrap();
    assert_eq!(model.text(), "ABbase");
    assert_eq!(model.history_page(None, 100).unwrap().entries.len(), count);
}

#[test]
fn undo_redo_and_restore_keep_buffer_clean_state_in_sync_with_the_saved_baseline() {
    use flavite_buffer::state::{Buffer, BufferState};
    let mut buffer = Buffer::from_text("base");
    let root = buffer.model().history_page(None, 100).unwrap().entries[0].target;
    buffer
        .apply_local_edits_with_policy(
            &[EditOp::new(Span::new(TextByte(0), TextByte(0)), "A".into())],
            UndoPolicy::Isolated,
        )
        .unwrap();
    assert_eq!(buffer.state(), BufferState::Dirty);
    buffer.undo().unwrap();
    assert_eq!(buffer.state(), BufferState::Clean);
    buffer.redo().unwrap();
    assert_eq!(buffer.state(), BufferState::Dirty);
    buffer.restore_editing_state(root).unwrap();
    assert_eq!(buffer.state(), BufferState::Clean);
    assert!(!buffer.model().is_dirty());
}

#[test]
fn history_compares_with_the_latest_save_receipt_and_preserves_incomplete_saves() {
    use flavite_buffer::state::{Buffer, BufferState};
    let mut buffer = Buffer::from_text("base");
    let root = buffer.model().history_page(None, 100).unwrap().entries[0].target;
    buffer
        .apply_local_edits_with_policy(
            &[EditOp::new(Span::new(TextByte(0), TextByte(0)), "A".into())],
            UndoPolicy::Isolated,
        )
        .unwrap();
    let saved = buffer.model().history_page(None, 100).unwrap().entries[0].target;
    buffer.handle_save_completed_revision(buffer.revision(), b"Abase");
    buffer.restore_editing_state(root).unwrap();
    assert_eq!(
        buffer.state(),
        BufferState::Dirty,
        "the loaded root is no longer the disk baseline"
    );
    assert!(buffer.model().is_dirty());
    buffer.restore_editing_state(saved).unwrap();
    assert_eq!(buffer.state(), BufferState::Clean);
    assert!(!buffer.model().is_dirty());

    // Bytes can equal a completed baseline while a later save's durability
    // or indexing milestone still requires a retry. Matching a hash cannot
    // acknowledge that milestone on its behalf.
    buffer.record_replaced_disk_snapshot(b"Abase");
    assert_eq!(buffer.state(), BufferState::Dirty);
    buffer.restore_editing_state(root).unwrap();
    buffer.restore_editing_state(saved).unwrap();
    assert_eq!(buffer.state(), BufferState::Dirty);
    assert!(buffer.model().is_dirty());
    buffer.handle_save_completed_revision(buffer.revision(), b"Abase");
    buffer.restore_editing_state(saved).unwrap();
    assert_eq!(buffer.state(), BufferState::Clean);

    // A save can finish for an older revision while newer typing is live.
    buffer
        .apply_local_edits_with_policy(
            &[EditOp::new(Span::new(TextByte(0), TextByte(0)), "B".into())],
            UndoPolicy::Isolated,
        )
        .unwrap();
    let late_revision = buffer.revision();
    buffer
        .apply_local_edits_with_policy(
            &[EditOp::new(Span::new(TextByte(0), TextByte(0)), "C".into())],
            UndoPolicy::Isolated,
        )
        .unwrap();
    buffer.handle_save_completed_revision(late_revision, b"BAbase");
    assert_eq!(buffer.state(), BufferState::Dirty);
    buffer.undo().unwrap();
    assert_eq!(buffer.state(), BufferState::Clean);
    buffer.restore_editing_state(saved).unwrap();
    assert_eq!(buffer.state(), BufferState::Dirty);
    buffer.undo().unwrap();
    assert_eq!(buffer.state(), BufferState::Dirty);
}
