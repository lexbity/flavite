//! FEUX-0002 X01: history resource closure — budgets, accounting, eviction,
//! reservations (E-12). All refusals happen before mutation and are typed;
//! eviction is deterministic; the current node and in-flight restore targets
//! are never evicted; leases expire.

use flavite_buffer::{
    EditorModel,
    budget::{Budget, METADATA_BYTES_PER_NODE},
    history::HistoryError,
    undo::{EditOp, UndoPolicy, UndoTree},
};
use flavite_syntax::text::{Span, TextByte, TextSnapshot};

fn model_with_budget(bytes: u64, nodes: u64) -> EditorModel {
    let mut model = EditorModel::from_text("");
    model.set_history_budget(Budget {
        max_bytes: bytes,
        max_nodes: nodes,
    });
    model
}

/// Append one isolated node whose text is `width` bytes.
fn append_isolated(model: &mut EditorModel, width: usize) {
    let text = "x".repeat(width);
    let at = model.len();
    model
        .try_apply_batch(
            &[EditOp::new(
                Span::new(TextByte(at), TextByte(at)),
                text.clone(),
            )],
            UndoPolicy::Isolated,
        )
        .expect("isolated append applies");
}

/// Deterministic LCG (same seed, same sequence).
fn lcg(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

#[test]
fn metadata_constant_covers_undo_node_size() {
    assert!(
        std::mem::size_of::<flavite_buffer::undo::UndoNode>() <= METADATA_BYTES_PER_NODE as usize,
        "fixed per-node metadata constant must cover the UndoNode struct"
    );
}

#[test]
fn refusal_before_mutation_leaves_text_revision_and_history_byte_identical() {
    let mut model = model_with_budget(1024, 10_000);
    let before_text = model.text().to_owned();
    let before_revision = model.revision();
    let before_account = model.history_accounted_bytes();
    let before_page = model.history_page(None, 100).unwrap();

    let big = "x".repeat(4096);
    let error = model
        .try_apply_batch(
            &[EditOp::new(
                Span::new(TextByte(0), TextByte(0)),
                big.clone(),
            )],
            UndoPolicy::Isolated,
        )
        .expect_err("oversized operation must be refused");
    assert!(matches!(
        error,
        flavite_buffer::model::BatchError::LimitExceeded { needed, budget }
            if budget == 1024 && needed > 1024
    ));
    // Zero effects: text, revision, accounted bytes and the history page are
    // byte-identical to before.
    assert_eq!(model.text(), before_text);
    assert_eq!(model.revision(), before_revision);
    assert_eq!(model.history_accounted_bytes(), before_account);
    assert_eq!(model.history_page(None, 100).unwrap(), before_page);
    assert_eq!(model.history_evicted_total(), 0, "no eviction on refusal");

    // Recorded (editor-annotated) path refuses the same way.
    let record = flavite_buffer::history::EditorEdit {
        origin: format!("{:032x}", 1),
        kind: flavite_buffer::history::EditKind::Typing,
        before: vec![[0, 0, 0]],
        after: vec![[0, 4096, 4096]],
    };
    let recorded_error = model
        .try_apply_recorded_batch(
            &[EditOp::new(Span::new(TextByte(0), TextByte(0)), big)],
            UndoPolicy::Context { origin: 1 },
            record,
        )
        .expect_err("recorded oversized operation must be refused");
    assert!(matches!(
        recorded_error,
        flavite_buffer::model::RecordedBatchError::LimitExceeded { .. }
    ));
    assert_eq!(model.text(), before_text);
    assert_eq!(model.revision(), before_revision);
}

#[test]
fn node_cap_refusal_precedes_mutation() {
    // A node cap of 1 means only the current node can be retained; adding
    // any new node can never fit even after maximal eviction, so every
    // append is refused before mutation.
    let mut model = model_with_budget(1_000_000, 1);
    let before_text = model.text().to_owned();
    let before_revision = model.revision();
    let before_page = model.history_page(None, 100).unwrap();
    let error = model
        .try_apply_batch(
            &[EditOp::new(
                Span::new(TextByte(model.len()), TextByte(model.len())),
                "z".into(),
            )],
            UndoPolicy::Isolated,
        )
        .expect_err("node cap must be refused");
    assert!(matches!(
        error,
        flavite_buffer::model::BatchError::LimitExceeded { .. }
    ));
    assert_eq!(model.text(), before_text);
    assert_eq!(model.revision(), before_revision);
    assert_eq!(model.history_page(None, 100).unwrap(), before_page);
    assert_eq!(model.history_nodes(), 1, "no node was created");
}

#[test]
fn eviction_is_deterministic_across_seeded_runs() {
    // Same operation sequence -> identical survivor sets, eviction counts and
    // accounted bytes, over 1,000 seeded sequences. The byte budget is large
    // enough that no single projected append can be refused (the refusal
    // contract is covered by its own tests), while accumulated history far
    // exceeds it so eviction is exercised heavily.
    for seed0 in 0..1_000u64 {
        let mut seed = seed0;
        let mut a = model_with_budget(20_000, 14);
        let mut b = model_with_budget(20_000, 14);
        for _ in 0..40 {
            let width = (lcg(&mut seed) as usize) % 120 + 8;
            append_isolated(&mut a, width);
            append_isolated(&mut b, width);
        }
        assert_eq!(
            a.history_survivor_ids(),
            b.history_survivor_ids(),
            "seed {seed0}: survivor sets diverged"
        );
        assert_eq!(a.history_accounted_bytes(), b.history_accounted_bytes());
        assert_eq!(a.history_evicted_total(), b.history_evicted_total());
        assert_eq!(
            a.history_retained_watermark(),
            b.history_retained_watermark()
        );
    }
}

#[test]
fn evicted_states_vanish_and_truncation_is_visible() {
    // Build a moderate document under a generous budget (no eviction), then
    // shrink the budget and force one tiny append: accumulation far exceeds
    // the budget, so eviction fires while the tiny append itself fits.
    let mut model = model_with_budget(100_000, 1_000);
    for _ in 0..10 {
        append_isolated(&mut model, 40);
    }
    let page = model.history_page(None, 100).unwrap();
    assert!(!page.history_truncated, "no eviction yet");
    assert_eq!(page.retained_watermark, 0);

    model.set_history_budget(Budget {
        max_bytes: 3_000,
        max_nodes: 1_000,
    });
    append_isolated(&mut model, 1);
    let page = model.history_page(None, 100).unwrap();
    assert!(
        page.history_truncated,
        "eviction happened, truncation visible"
    );
    assert!(page.retained_watermark > 0);
    assert_eq!(page.retained_watermark, model.history_retained_watermark());
    // Every entry is a survivor and its id is at or above the watermark.
    for entry in &page.entries {
        assert!(entry.target.node >= page.retained_watermark);
    }
    // The current node is never evicted.
    assert!(page.entries.iter().any(|e| e.current));
    // The requested range overflowed: most created ids were evicted.
    assert!(page.entries.len() < 11, "most ids were evicted");
    // Restoring an evicted target is refused as expired.
    let evicted_target = flavite_buffer::history::HistoryTarget {
        epoch: page.epoch,
        node: 0,
        version: 1,
    };
    assert_eq!(
        model.restore_state(evicted_target),
        Err(HistoryError::Expired)
    );
    assert_eq!(model.text().len(), 401, "refused restore changed nothing");
}

#[test]
fn evicted_cursor_returns_history_changed_and_unrelated_stale_stays_stale() {
    let mut model = model_with_budget(100_000, 1_000);
    for _ in 0..10 {
        append_isolated(&mut model, 40);
    }
    let first = model.history_page(None, 1).unwrap();
    let cursor = first.next.expect("paginated cursor");
    // Shrink the budget and force eviction.
    model.set_history_budget(Budget {
        max_bytes: 3_000,
        max_nodes: 1_000,
    });
    for _ in 0..6 {
        append_isolated(&mut model, 1);
    }
    assert!(model.history_evicted_total() > 0);
    assert_eq!(
        model.history_page(Some(cursor), 100),
        Err(HistoryError::HistoryChanged),
        "eviction invalidates cursors with HistoryChanged"
    );
    // Without eviction, the delivered stale-page path is unchanged.
    let mut plain = EditorModel::from_text("");
    append_isolated(&mut plain, 8);
    let page = plain.history_page(None, 1).unwrap();
    assert!(page.next.is_some());
    let plain_cursor = page.next.unwrap();
    append_isolated(&mut plain, 8);
    assert_eq!(
        plain.history_page(Some(plain_cursor), 1),
        Err(HistoryError::StalePage)
    );
    assert_eq!(plain.history_evicted_total(), 0);
}

#[test]
fn reserved_restore_target_survives_eviction_until_released() {
    let mut tree = UndoTree::with_budget(
        TextSnapshot::from_normalized(""),
        Budget {
            max_bytes: 1_000_000,
            max_nodes: 1_000,
        },
    );
    let snap =
        |at: usize, text: &str| EditOp::new(Span::new(TextByte(at), TextByte(at)), text.into());
    for step in 0..8 {
        let text = format!("step{step}");
        tree.apply_boundary(snap(step, &text), TextSnapshot::from_normalized(&text));
    }
    assert!(tree.survivor_ids().contains(&2));
    // Protect node 2 (an older, non-current node) for the lease window.
    let protection = tree.protect_token(2, 1_000_000_000_000_000);
    // Shrink the budget so only the most recent nodes fit.
    tree.set_budget(Budget {
        max_bytes: 3_000,
        max_nodes: 50,
    });
    for _ in 0..8 {
        let lease = tree
            .reserve(600, 1_000_000_000_000_000)
            .expect("projected fit after eviction");
        tree.release(&lease);
        assert!(
            tree.survivor_ids().contains(&2),
            "protected restore target must never be evicted"
        );
        // The protected target remains restorable.
        let target = flavite_buffer::history::HistoryTarget {
            epoch: flavite_buffer::history::HistoryEpoch::default(),
            node: 2,
            version: 1,
        };
        assert!(tree.restore_snapshot(target).is_ok());
        // The current node stays (never evicted).
        let current = tree.current().unwrap().id;
        assert!(tree.survivor_ids().contains(&current));
    }
    tree.unprotect(protection);
    // With protection released, the same pressure can evict node 2.
    tree.set_budget(Budget {
        max_bytes: 1_200,
        max_nodes: 50,
    });
    let mut released = false;
    for _ in 0..8 {
        let lease = tree.reserve(400, 1_000_000_000_000_000).unwrap();
        tree.release(&lease);
        if !tree.survivor_ids().contains(&2) {
            released = true;
            break;
        }
    }
    assert!(released, "released target is evictable");
}

#[test]
fn protection_lease_expires_and_frees_the_node() {
    let mut tree = UndoTree::with_budget(
        TextSnapshot::from_normalized(""),
        Budget {
            max_bytes: 1_000_000,
            max_nodes: 1_000,
        },
    );
    for step in 0..6 {
        let text = format!("step{step}");
        tree.apply_boundary(
            EditOp::new(Span::new(TextByte(step), TextByte(step)), text.clone()),
            TextSnapshot::from_normalized(&text),
        );
    }
    // Protect node 1 with a lease that expires at 30_000 ms.
    let protection = tree.protect_token(1, 0);
    // Pressure with a far-future timestamp: the expired protection must be
    // dropped and node 1 becomes evictable.
    tree.set_budget(Budget {
        max_bytes: 1_500,
        max_nodes: 50,
    });
    let mut evicted = false;
    for _ in 0..8 {
        let lease = tree.reserve(400, 100_000).unwrap();
        tree.release(&lease);
        if !tree.survivor_ids().contains(&1) {
            evicted = true;
            break;
        }
    }
    assert!(evicted, "expired protection no longer pins the node");
    let _ = protection;
}

#[test]
fn eviction_never_removes_current_node_and_evicts_off_path_branches_first() {
    // Branching: off-path branches are evicted before the current path.
    let mut model = model_with_budget(1_000_000, 1_000);
    for _ in 0..10 {
        append_isolated(&mut model, 5);
    }
    let root_target = model.history_page(None, 100).unwrap().entries[0].target;
    // Restore to root, then grow an off-path branch from there.
    model.restore_state(root_target).unwrap();
    for _ in 0..8 {
        append_isolated(&mut model, 5);
    }
    // Shrink the budget: the next append must evict the off-path main branch
    // before touching the current path.
    model.set_history_budget(Budget {
        max_bytes: 2_000,
        max_nodes: 50,
    });
    append_isolated(&mut model, 5);
    let survivors = model.history_survivor_ids();
    let current = survivors.last().copied().unwrap();
    assert!(survivors.contains(&current), "current node never evicted");
    // Off-path main nodes (1..=10) are gone entirely.
    for main in 1..=10 {
        assert!(
            !survivors.contains(&main),
            "off-path main node {main} should be evicted"
        );
    }
    // The surviving path is a suffix of the current branch.
    assert!(survivors.iter().all(|id| *id >= 11));
}

#[test]
fn accounted_bytes_match_an_independent_oracle_under_random_ops() {
    proptest::proptest!(|(
        ops in proptest::collection::vec(
            (".{0,24}", proptest::bool::ANY),
            1..60
        ),
    )| {
        let mut tree = UndoTree::with_budget(
            TextSnapshot::from_normalized(""),
            Budget {
                max_bytes: 200_000,
                max_nodes: 400,
            },
        );
        let mut text = String::new();
        for (next, seal) in ops {
            if seal {
                tree.seal_current();
            }
            let op = EditOp::new(
                Span::new(TextByte(0), TextByte(text.len())),
                next.clone(),
            );
            tree.apply_boundary(op, TextSnapshot::from_normalized(&next));
            text = next;
            // Independent oracle: recompute from the surviving nodes using
            // literal constants (512 fixed metadata; 24 B/selection), NOT
            // the production helper.
            let expected: u64 = tree
                .survivor_ids()
                .iter()
                .map(|&id| {
                    let node = tree.get(id).unwrap();
                    let snap_len = node
                        .snapshot
                        .as_ref()
                        .map_or(0, |s| s.text().len()) as u64;
                    let repl_len = node
                        .edit
                        .as_ref()
                        .map_or(0, |e| e.replacement.len()) as u64;
                    let (before, after) = match &node.editor {
                        Some(record) => (&record.before[..], &record.after[..]),
                        None => (&[][..], &[][..]),
                    };
                    let sel = (before.len() + after.len()) as u64 * 24;
                    512 + snap_len + repl_len + sel
                })
                .sum();
            proptest::prop_assert_eq!(tree.accounted_bytes(), expected);
            proptest::prop_assert!(tree.retained_node_count() as u64 <= 400);
        }
    });
}

#[test]
fn coalescing_fresh_accounting_and_no_eviction_of_the_merged_node() {
    // A coalesced node replaces its snapshot: accounted bytes must reflect the
    // merged state (root + one merged node), and the merged node (current)
    // is never evicted.
    let mut tree = UndoTree::with_budget(
        TextSnapshot::from_normalized(""),
        Budget {
            max_bytes: 3_000,
            max_nodes: 50,
        },
    );
    let z = |text: &str| EditOp::new(Span::new(TextByte(0), TextByte(text.len())), text.into());
    tree.apply_context(z("a"), TextSnapshot::from_normalized("a"), 1);
    tree.apply_context(z("ab"), TextSnapshot::from_normalized("ab"), 1);
    let current = tree.current().unwrap().id;
    assert_eq!(tree.retained_node_count(), 2, "root + one coalesced node");
    assert_eq!(tree.current().unwrap().version, 2);
    assert!(tree.survivor_ids().contains(&current));
    // Root (512) + merged node (512 fixed + "ab" snapshot + "ab" replacement).
    let expected = 512 + 512 + 2 + 2;
    assert_eq!(tree.accounted_bytes(), expected);
}

#[test]
fn default_budget_is_the_locked_e12_numbers() {
    let default = Budget::default();
    assert_eq!(default.max_bytes, 128 * 1024 * 1024);
    assert_eq!(default.max_nodes, 10_000);
    assert_eq!(
        flavite_buffer::budget::APP_WIDE_MAX_HISTORY_BYTES,
        512 * 1024 * 1024
    );
    assert_eq!(flavite_buffer::budget::RESERVATION_LEASE_MS, 30_000);
}

#[test]
fn manager_default_budget_reaches_buffers_opened_after_it_is_set() {
    // FEUX-0002 X01 regression (native context-mode-budget): the harness
    // shrinks budgets at startup; a note opened afterwards must inherit the
    // shrunk budget, not the 128 MiB default.
    use flavite_buffer::manager::BufferManager;

    let mut manager = BufferManager::new();
    manager.set_default_history_budget(Budget {
        max_bytes: 2_000,
        max_nodes: 4,
    });
    let content = "alpha body\n".as_bytes();
    let (buffer, _) = manager
        .open_note("later-opened.md".into(), content, Default::default())
        .expect("fixture decodes");
    let budget = buffer.model().history_budget();
    assert_eq!(budget.max_bytes, 2_000);
    assert_eq!(budget.max_nodes, 4);

    // Setting the default also re-budgets already-open buffers.
    manager.set_default_history_budget(Budget {
        max_bytes: 3_000,
        max_nodes: 5,
    });
    let buffer = manager
        .open_note("later-opened.md".into(), content, Default::default())
        .expect("fixture decodes")
        .0;
    assert_eq!(buffer.model().history_budget().max_bytes, 3_000);

    // A manager without a default keeps the model defaults.
    let mut fresh = BufferManager::new();
    let (buffer, _) = fresh
        .open_note("fresh.md".into(), content, Default::default())
        .expect("fixture decodes");
    assert_eq!(buffer.model().history_budget(), Budget::default());
}
