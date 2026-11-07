# RFC-0006 S07 — Codebase review (S00–S06) and S07 implementation note

**Date:** 2026-11-06. **Baseline:** `73b30f7` + uncommitted working tree.
**Purpose:** record the requested review of the codebase and the RFC, the
S00–S06 completion assessment, and the S07 implementation plan before the
slice lands. The S07 receipt lives in `RFC0006-S07.md`.

## 1. Repository review

Workspace of 16 crates + `xtask` (resolver 3, edition 2024, rust-version
1.90). All S07-relevant crates exist and are compiled into the workspace:
`flavite-encoding`, `flavite-syntax`, `flavite-buffer`, `flavite-fs`,
`flavite-vault`, `flavite-index`, `flavite-api`, `flavite-ui`, `flavite-editor`,
`flavite-workspace`, `flavite-text-search`, `flavite-text-search-worker`,
`flavite-cli`, graph/theme/app crates. Architecture config is generated and
checked (`cargo xtask arch check`); `flavite-text-search` has the exact §4.1
dependency closure (syntax + regex + serde only), the editor never depends on
it, and the vault/API depend on it as the RFC allows.

## 2. Phase review vs the RFC

| Slice | Status | Verified evidence |
|---|---|---|
| S00 | Implemented | `flavite-encoding` crate (strict codecs, sparse DecodeMap, `SourceFormat`/`FormatStamp`), syntax composed maps, buffer descriptor/EOL ownership, recovery v2, `EncodingHints`, index cache 4.0, UI/CLI encoding controls; `encoding_*` suites green. |
| S01 | Implemented | `flavite-text-search` literal matcher, `text_edit.rs` prepare/apply spine, Session plan registry (TTL/retry/replay), `host/note_search.rs` extraction; `note_replace`/`prepared_edits` tests green. |
| S02 | Implemented | `layout_navigation.rs`, buffer `navigation.rs`, syntax `commands.rs`, one key decoder; `logical_navigation`/`markdown_commands`/`navigation_geometry` suites. |
| S03 | Implemented | `flavite-text-search-worker` helper + `text_worker.rs` bounded pool (Interactive/Bulk permits, 5 s deadline, sibling-only discovery), regex/empty-match/template semantics; `text_worker`/`note_regex` suites. |
| S04 | Implemented | `open_intents.rs`, `ActivateNode` gesture (no implicit insert), stable geometry; `open_intents`/`workbench_loop` suites; native GPU/canvas scenario. |
| S05 | Implemented | `catalog_service.rs` bounded pages/verdicts, editor completion via Arc-shared snapshots; `completion_catalog`/`editor_completion` suites. |
| S06 | Implemented | `search_writer.rs` (ordered envelope owner), `search_eval.rs` (full AST/NNF/bitsets), `search_session.rs` capture, API `search_sessions.rs` spools, `host/search.rs` continuation; `search_boolean`/`search_writer`/`search_sessions`/`search_paging`/`search_modes` suites; S06 receipt records deferred native/50k-load/full-gate items honestly. |

### Gaps / risks found

1. **S09 draft inclusion is intentionally absent** (D-09): the preview must
   exclude Dirty/Conflicted/Orphaned buffers by default and refuse
   `include_drafts` in S07 (that lands in S09). UI must show exclusions with
   counts and reasons.
2. **No vault-wide source-matching surface exists yet** — only per-buffer
   `text.find`; `source_find.*`/`replace.preview` commands are unregistered.
   CLI `search --mode literal|regex` currently returns typed `unsupported`
   (S06 receipt explicitly defers it to S07).
3. **The vault Session does not expose open-buffer states** for exclusion
   logic (`BufferManager::dirty_buffers` etc. exist; a small additive Session
   accessor is needed).
4. **No private operation store** exists under `.flavite/operations`; the
   watcher/reconcile walk already skips `.flavite` wholesale (reconcile_worker
   line ~566), so a store rooted there is excluded from indexing/scanning by
   construction.
5. **Regex bulk execution** must use the existing `run_match_bulk` permit
   class (NFR-10/§6.2 one bulk slot; interactive reservation preserved);
   `text_edit::prepare_ops/prepare_find` hard-code the Interactive class, so
   bulk wrappers are needed.
6. **Read-only vaults** must be able to Find/preview under read budget but
   must not persist a vault-local preview (`Flavite::set_read_only` exists).

## 3. S07 target architecture (implemented in this slice)

- `flavite-vault/src/replace/{mod,plan,preview,store}.rs`: frozen scope
  enumeration (note-catalog snapshot), per-note guarded reads, canonical
  matcher reuse (`text_edit::prepare_ops`/`prepare_find` with Bulk permits),
  exact before/after serialized payloads via `EditorModel` + `SourceFormat`,
  sealed manifest (schema 1, deny_unknown_fields) + content-addressed payload
  store under `.flavite/operations/replace/<job-id>/` through bounded actor IO.
- `source_find.start/page/close` and `replace.preview/select/status` facade
  commands (ReadNotes; `replace.select` is a local selection revision, not a
  vault mutation; Apply/Cancel/Resume/Revert/forget stay unregistered until
  S08/S11).
- UI: `host/replace.rs` + `views/replace.rs`, `ViewKind::Replace`, palette
  commands for vault Find (Ctrl/Cmd+Shift+F) and Replace preview
  (Ctrl/Cmd+Shift+H).
- CLI: `search --mode literal|regex` now runs the vault source find.
- Gates: `scripts/check-workbench-contracts.sh S07` cumulative (+ new
  invocations) and `xtask` negative fixtures; fuzz target `replace_plan`.
