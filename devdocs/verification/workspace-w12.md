# W12 — Final-view close, recovery, conflicts, and quit: implementation and verification record

**Date:** 2026-11-03. **Branch:** `uxfixes`. **Slice:** RFC-0003 §28 (W12), following the W11 record.

**Status:** Implemented on Linux with real OS-input evidence for the recovery flow. macOS/Windows native evidence and IME remain deferred per the tracked release scope; combinations not run are listed explicitly and are not claimed.

## Scope delivered

1. **One core close policy, no GUI bypass.** `Session::close_note` (flavite-vault) is the single authority: a pending save flight refuses every close; a dirty, conflicted, or orphaned buffer refuses a plain close (`dirty-buffer-close` action required) and may close only by retaining a durable recovery snapshot that provably covers its revision and content hash. The pre-W12 hole where `Conflicted`/`Orphaned` buffers (not `is_dirty()`) could be closed silently is fixed.
2. **API surface (flavite-api).** New modules, one canonical command ID per action: `recovery.rs` (`recovery.restore`, `recovery.discard`, `recovery.flush` with the revision-bound `RecoveryFlushed` receipt), `documents.rs` (`note.close` with the typed `CloseArgs{path, policy: clean|retain_recovery}` — the raw path payload is rejected as invalid args; `note.discard_edits`; `conflict.apply_merge` applying a confirmed merge against the `MergeReady` receipt's disk hash and revision). `conflict.resolve` is the sole reload/keep-mine/merge-prepare command; `save-as` stays an honest `save-as-confirm` action-required code (not implemented in core, never faked).
3. **Close plans (host/closing.rs).** Per-buffer final-view lease accounting (`DocumentRegistry::other_leases`); shared views release only their lease; the final view of an unsaved document captures a `ClosePlan` (keyed by tab id, so decisions survive tab movement) and the dialog offers Save and close / Close keeping a recoverable copy / Discard (separate destructive action, armed against the revision the user saw; a later edit invalidates the confirmation) / Keep open. No view removal happens before the safety outcome: save-close waits for the save terminal, rechecks the buffer, closes through the core policy, and a failed or rejected save never closes the view (AC-W13). Group close closes clean tabs immediately and queues decision tabs; one refusal retains the undecided tabs.
4. **§8.3 last-view close.** A final clean view now closes its Session buffer through `note.close` before the tab is removed; the obsolete buffer-reuse assertion in `workspace_layout.rs` was replaced by the fresh-buffer policy.
5. **Quit schedule (host/shutdown.rs).** `CloseRequested` is intercepted (`exit_on_close_request: false`): composition and open dialogs block with honest status; document decisions run through the close machinery (origin Quit); accepted operations drain against a bounded 5 s window; the layout flush is durable. The timeout never mislabels pending work as saved — the user gets a typed concern and the explicit choices Keep waiting / Quit anyway (consequential) / Cancel quit. Cancellation returns the workspace to its actual current state. Layout-mutation guard while quitting.
6. **Dialogs (host/dialogs/{close,conflict,recovery,merge}.rs).** The monolith became a module with the W12 surfaces: close decisions, conflict resolution (reload/keep-mine/prepare-merge, save-as truthfully unavailable), merge confirmation with the structured hunks, recovery offer (notice-driven presentation only — restoring/discarding is always explicit), and the quit-timeout choice. Escape cancels, Enter runs only the non-destructive primary action.
7. **Core defects found and fixed by the slice's own evidence.**
   - `instance.rs` kept a private data-dir resolver that ignored `FLAVITE_DATA_DIR`, so the instance IPC socket landed in the default user data directory while identity/recovery/preferences honored the override — every isolated run (visual scenarios included) silently leaked device state to the real user data directory, and an absolute override failed outright. One authority now serves all of them.
   - `pulldown_glue`'s indexer parse buffer skipped structural bytes (paragraph separators, inline markup markers — only soft/hard breaks were compensated), so its scan window desynchronized from the snapshot and **every wikilink after the first paragraph was silently dropped from the index**. The buffer is now gap-filled from the snapshot; a regression test pins the multi-paragraph case and span coordinates. This bug hid real links from the graph, backlinks, and search for any note with prose before a link — the crash test's graph assertion exposed it.
   - `Session::wait_for_operation` re-acquired the non-reentrant driving turn when called under `execute` (the `recovery.restore` implicit open), self-deadlocking the first API-level restore. The wait is split into a holding-turn inner form.
   - UI: `save_layout_now` could report "saved" while the debounced autosave deferred the actual write (durability acknowledgement was a false report — explicit flushes now always attempt the write); Keep-open during a quit leaked the shutdown schedule ("zombie quit"); the quit schedule stalled forever after the last queued decision; layout mutations are refused while quitting; and the close-dialog discard confirmation was dead after any edit while the dialog stayed open (arming now re-presents the document, per D-07).
8. **Operation pump.** Close-plan completions route before generic handling; terminal results continue to process while dialogs are visible; completions for closed tabs stay correlated through the plan; results are described from typed values only (the `result.contains/starts_with` gate is enforced).

## Verification evidence

~~~sh
cargo test -p flavite-ui --test close_safety
cargo test -p flavite-api --test recovery_close
cargo test -p flavite-ui --test shutdown
cargo test -p flavite-ui --test knowledge_workflow crash
just fault-tests
bash scripts/check-workspace-contracts.sh W12
bash scripts/workspace-visual-linux.sh --scenario conflict-recovery
just workspace-gate W12
just gate
~~~

(Results recorded at the bottom of this file.)

The `conflict-recovery` visual run produces `target/workspace-evidence/conflict-recovery-*/`: edit → `kill -9` (no destructors; the fixture knob stretches the autosave idle window so the newest bytes exist only as a retained recovery copy) → relaunch → the recovery-offer dialog appears (`dialog_kind` in the semantic snapshot) → Enter restores → Ctrl+S persists the recovered bytes; the disk file never contained the unsaved edit before the restore. Manifest schema 1 with build/worktree hashes and `input_evidence: rendered_os_input`.

## Known incompleteness (explicit)

- No macOS/Windows native AT evidence; no IME evidence (deferred, unchanged).
- `shutdown.rs` models delay via the real 5 s drain window plus white-box stage setup; there is no injectable clock in the test stack (fault/error injection comes from `FaultFs`).
- The conflict dialog's "compare" is the merge view's structured hunks; a side-by-side byte compare is not a core-permitted surface yet (disk bytes are core-owned).

## Results

- `cargo test -p flavite-ui --test close_safety` — 8/8 pass (two-view lease release, decision+save-close persistence, retain-recovery with offer on reopen, revision-bound discard refusing after a later edit, cancellation, group close queueing, plan surviving tab movement, failed save never closing the final view).
- `cargo test -p flavite-api --test recovery_close` — 5/5 pass (`--features fault`): retain-recovery receipt, plain close refusing dirty/pending-save, discard keeping history, restore/discard round trip, and a fault-injected crash at the durable snapshot write with post-revive consistency.
- `cargo test -p flavite-ui --test shutdown` — 8/8 pass (clean quit flushing layout, decision-first quit, save-then-exit, truthful timeout choices with Keep-waiting/Quit-anyway/Cancel, layout mutations refused while quitting, layout-flush failure offering exit-with-unsaved-layout, composition blocking quit).
- `cargo test -p flavite-ui --test knowledge_workflow` — 8/8 pass, including `crash_recovery_matches_the_recovered_note_in_the_workspace`: child edit → durable recovery checkpoint → parent `kill -9` (signal 9 verified, disk never held the edit) → reopen → RecoveryOffer → `recovery.restore` → save → the recovered `[[b]]` link appears in the saved graph projection.
- `just fault-tests` — 11/11 pass; `cargo test --workspace` — clean; `cargo clippy --workspace --all-targets -- -D warnings` — clean; `cargo fmt --check` — clean.
- `bash scripts/check-workspace-contracts.sh W12` — pass; `cargo test -p xtask --test workspace_gates` — 9/9 (including the W12 negative gates seeding forbidden close patterns and missing inputs).
- `bash scripts/workspace-visual-linux.sh --scenario conflict-recovery` — all checks pass with real OS input (edit → `kill -9` → relaunch → recovery-offer dialog → Enter restores → Ctrl+S persists), now hermetic: device state lives under a short per-run `/tmp` directory (the instance socket's `SUN_LEN` bound rules out deep evidence paths) and the run no longer touches the user's real data directory.
- `just workspace-gate W12` — pass (contracts W01–W12, xtask gates, runtime accessibility, all staged slice suites through stage 12, `just fault-tests`). One earlier staged run failed `navigation_identity::authoritative_rename_relocates_views_bindings_and_history` under full parallel load immediately before the target-cache budget purge; the test passes consistently in isolation and in the clean rerun, and is a pre-existing W10 load-sensitivity, not a W12 regression (the binary sets no data-dir override; the instance fix does not alter its paths).
- `just gate` — **G0 passed** (fmt + clippy -D warnings + full workspace test + fault-tests + lint-arch + arch check), including after a full target rebuild.
