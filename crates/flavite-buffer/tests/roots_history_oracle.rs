//! RFC-0011 S2-T1 (AC-3): root-based history equivalence.
//!
//! Randomized sequences of typing (appends), isolated replaces, undo, and
//! redo where the model's chunk-root history outcomes are byte-identical to
//! an independent sequential-undo oracle at every step. The op language is
//! chosen so coalescing semantics are unambiguous: consecutive same-origin
//! typing coalesces (continuous run), isolated edits and undo/redo break
//! the run — exactly the implemented rules.

use flavite_buffer::{
    EditorModel,
    history::{EditKind, EditorEdit},
    undo::{EditOp, UndoPolicy},
};
use flavite_syntax::text::{Span, TextByte};
use proptest::prelude::*;

#[derive(Debug, Clone)]
enum Op {
    /// Append `s` at the end of the document (typing, context origin 1).
    TypeEnd(String),
    /// Replace `[start, end)` with `s` (isolated boundary).
    Isolated(usize, usize, String),
    Undo,
    Redo,
}

/// Sequential-undo oracle: a stack of states with a pointer plus the
/// continuous-typing run (mirrors `can_coalesce`/`merge_mapped` and the
/// context-mode continuous merge).
#[derive(Debug)]
struct Oracle {
    states: Vec<String>,
    ptr: usize,
    run: bool,
}

impl Oracle {
    fn new(text: String) -> Self {
        Self {
            states: vec![text],
            ptr: 0,
            run: false,
        }
    }

    fn current(&self) -> &str {
        &self.states[self.ptr]
    }

    fn apply(&mut self, op: &Op) {
        match op {
            Op::TypeEnd(s) => {
                if self.run {
                    let mut next = self.current().to_owned();
                    next.push_str(s);
                    let _ = self.states.split_off(self.ptr + 1);
                    *self.states.last_mut().expect("current state") = next;
                } else {
                    let mut next = self.current().to_owned();
                    next.push_str(s);
                    self.states.truncate(self.ptr + 1);
                    self.states.push(next);
                    self.ptr += 1;
                    self.run = true;
                }
            }
            Op::Isolated(start, end, s) => {
                let mut next = self.current().to_owned();
                next.replace_range(*start..*end, s);
                self.states.truncate(self.ptr + 1);
                self.states.push(next);
                self.ptr += 1;
                self.run = false;
            }
            Op::Undo => {
                if self.ptr > 0 {
                    self.ptr -= 1;
                }
                self.run = false;
            }
            Op::Redo => {
                if self.ptr + 1 < self.states.len() {
                    self.ptr += 1;
                }
                self.run = false;
            }
        }
    }
}

fn apply_to_model(model: &mut EditorModel, op: &Op) {
    match op {
        Op::TypeEnd(s) => {
            let at = model.len();
            let record = EditorEdit {
                before_primary: 0,
                origin: format!("{:032x}", 1),
                kind: EditKind::Typing,
                before: vec![[0, at, at]],
                after: vec![[0, at + s.len(), at + s.len()]],
                primary: 0,
            };
            model
                .try_apply_recorded_batch(
                    &[EditOp::new(
                        Span::new(TextByte(at), TextByte(at)),
                        s.clone(),
                    )],
                    UndoPolicy::Context { origin: 1 },
                    record,
                )
                .expect("typing applies");
        }
        Op::Isolated(start, end, s) => {
            model
                .try_apply_batch(
                    &[EditOp::new(
                        Span::new(TextByte(*start), TextByte(*end)),
                        s.clone(),
                    )],
                    UndoPolicy::Isolated,
                )
                .expect("isolated applies");
        }
        Op::Undo => {
            let _ = model.undo();
        }
        Op::Redo => {
            let _ = model.redo();
        }
    }
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: 512, ..proptest::test_runner::Config::default()
    })]

    #[test]
    fn roots_match_sequential_oracle_at_every_step(
        base in "[a-zA-Z0-9 \n.,!?]{0,64}",
        ops in proptest::collection::vec(
            proptest::prop_oneof![
                "[a-zA-Z0-9 \n.,!?]{0,8}".prop_map(Op::TypeEnd),
                (0usize..96, 0usize..96, "[a-zA-Z0-9 \n.,!?]{0,8}")
                    .prop_map(|(a, b, s)| Op::Isolated(a, b, s)),
                proptest::strategy::Just(Op::Undo),
                proptest::strategy::Just(Op::Redo),
            ],
            1..80,
        ),
    ) {
        let mut model = EditorModel::from_text(&base);
        let mut oracle = Oracle::new(base.to_string());
        for raw in ops.iter() {
            // Normalize isolated ranges against the live model text.
            let current = model.text().to_owned();
            let op = match raw {
                Op::TypeEnd(s) => Op::TypeEnd(s.clone()),
                Op::Isolated(a, b, s) => {
                    let mut a = a % (current.len() + 1);
                    while !current.is_char_boundary(a) {
                        a -= 1;
                    }
                    let mut b = (b % (current.len() + 1)).max(a);
                    while b < current.len() && !current.is_char_boundary(b) {
                        b += 1;
                    }
                    Op::Isolated(a, b, s.clone())
                }
                other => other.clone(),
            };
            apply_to_model(&mut model, &op);
            oracle.apply(&op);
            proptest::prop_assert_eq!(
                model.text(),
                oracle.current(),
                "step: model text diverged from the sequential oracle"
            );
            // Byte-identical source bytes (all-LF fixture): the root must
            // materialize to exactly the oracle text.
            proptest::prop_assert_eq!(
                model.try_to_source_bytes().ok(),
                Some(oracle.current().as_bytes().to_vec()),
                "step: source bytes diverged"
            );
        }
        // Exhaustive undo all the way down, then redo all the way up.
        let mut guard = 0;
        while model.undo().is_some() {
            guard += 1;
            proptest::prop_assert!(guard < 10_000, "undo runaway");
        }
        proptest::prop_assert_eq!(model.text(), oracle.states[0].as_str());
        guard = 0;
        while model.redo().is_some() {
            guard += 1;
            proptest::prop_assert!(guard < 10_000, "redo runaway");
        }
        proptest::prop_assert_eq!(
            model.text(),
            oracle.states.last().expect("tip").as_str(),
            "redo-up must land on the recorded tip"
        );
    }
}
