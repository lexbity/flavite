# RFC-0006 S09 — Completion record

**Slice:** S09 — Explicit draft application, recovery lineage and guarded Revert.
**Status:** Implemented; slice gates green (S00–S09 cumulative).
**Receipt:** `devdocs/verification/RFC0006-S09.md`.

## Normative decisions implemented

- **D-09** (preview authorization): draft opt-in (`include_drafts`),
  sealed Draft candidates with exact buffer revision/source hash/disk base
  lineage, `UnencodableDraft` exclusion, conflicts/orphans remain excluded.
- **D-11** (retries/crashes/uncertain saves/Revert): draft effects are
  only terminal after a durable recovery checkpoint; a failed checkpoint is
  nonterminal; Revert is a distinct action epoch on the same authenticated
  log with after-hash/lineage preconditions, conservative invalidation on
  later typing/Undo/Redo/rename even when text is identical, and per-note
  outcomes with compare/manual-recovery skips.
- **§6.4/§6.9** applicable extensions: `AppliedDraft` persisted outcome,
  `RevertIntentDurable`/`RevertOutcome` records, sidecar applied-revision
  record, restart classification against the recovery marker.

## Files created/migrated

- `crates/flavite-buffer/src/recovery.rs` (+`write_checkpoint_durable`)
- `crates/flavite-vault/src/replace/{plan,preview,log,coordinator,mod}.rs`
- `crates/flavite-vault/src/actor.rs` (+`RevertReplaceItem`)
- `crates/flavite-vault/src/session.rs` (command/work/mode/recovery/pump)
- `crates/flavite-api/src/replace.rs`, `src/flavite.rs`
- `crates/flavite-cli/src/main.rs` (+`replace revert`, `--include-drafts`)
- `crates/flavite-ui/src/{host/replace.rs,views/replace.rs,app.rs}`
- Tests: `crates/flavite-vault/tests/replace_drafts.rs`,
  `replace_revert.rs`, `crates/flavite-api/tests/replace_draft_guards.rs`,
  `crates/flavite-ui/tests/draft_operations.rs`,
  `crates/flavite-cli/tests/operations_cli.rs`
- Gates: `scripts/check-workbench-contracts.sh` (S09), `xtask/tests/workbench_gates.rs`
- `scripts/tests/workbench/draft_replace_revert.py`
- Receipts: `devdocs/verification/RFC0006-S09.md`, this record

## Common checks (all green)

`cargo fmt --all -- --check` · `cargo clippy --workspace --all-targets -- -D warnings`
· `cargo test --workspace --no-fail-fast` (1838 passed / 0 failed / 4 ignored)
· `cargo test -p flavite-vault --features fault journal::executor::tests` (11)
· `just lint-arch` · `cargo xtask arch check`
· `bash scripts/check-workbench-contracts.sh S09` (155 gates)
· `cargo test -p xtask --test workbench_gates --test workbench_ownership` (5)

## Remaining (recorded, not hidden)

- Native `draft-replace-revert` workbench-visual run (Xvfb/OCR) — `incomplete`.
- 50k/2 GiB draft/revert load receipts; macOS/Windows displacement receipts.
- Full RFC-0006 release conformance (S00–S11, AC-1–26) — S10/S11 remain.
