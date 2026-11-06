# RFC-0006 S04 — Graph exploration opens editors without losing the workbench

**Status:** complete. Native AC-4/5/6 receipts on both renderers and the
full common-check chain are recorded in
`devdocs/verification/RFC0006-S04.md`. This file is the tracked amendment
required by RFC-0006 §10: earlier RFCs live in gitignored scratch, so the
amended contract is restated here self-contained.

## Amended contract (supersedes RFC-0003 D-05 and §9.3 double-click insertion)

**D-02 — graph activation navigates; it never mutates.**

- Single click selects a node and shows its actions in a reserved overlay
  band. Selecting, deselecting, inspecting occurrence details, or showing
  the gesture notice changes the canvas bounds by zero logical pixels.
- Double-click (or Enter on a selection, or the accessible list row's
  Open) **activates** the node: it opens through the D-03 destination
  policy. `GestureOutcome::InsertLink` is deleted; the arbiter emits
  `ActivateNode` and no gesture path can touch a document.
- Insertion is only the explicit `Ctrl+Enter` / Insert intent. It requires
  an acknowledged collapsed writing caret, a resolvable note node, and a
  current (not stale, not Updating) paired frame, and it flows through the
  existing semantic prepare/consume-once plan with the confirmation
  preference unchanged.
- Pairing is keyed by view incarnation + identity epoch + entity — not by
  every RenderKey generation. A completed drag or Escape cancels
  recognition; a newer identity epoch cancels the pending pair; a removed
  node fails activation against the current records even while its old dot
  is still painted.
- During a click pair the hit geometry is frozen for at most 500 ms
  (the configured pair interval): entity-moving frames (new base
  publication, position ticks, shared pins, node visibility, drag preview)
  defer until the hold ends, while emphasis-only updates (selection,
  writing-target accent, palette) still apply. Accepted newer frames stay
  pending for presentation, labelled Updating. The camera keeps its
  manual-UX stability: auto-fit does not move while a hold is pending and
  never resets a user-held camera.
- The first launch after upgrade shows one dismissible explanation of the
  changed gesture (`graph_activation_acknowledged`, device preferences).
  No preference preserves the old double-click-insert interpretation.

**D-03 — one typed `OpenIntent` controls every navigation surface.**

`ReuseDocument` order: (1) a visible matching note in the preferred
document group (the last live group with acknowledged document focus — the
document anchor, independent of current graph focus), (2) another visible
matching note, (3) a matching note in the MRU live document group (never
the source group of a graph origin, so a hidden match cannot be activated
over the source graph), (4) a new tab in that group. Tie-break: highest
last-focus sequence, then ascending TabId. No live document group: under
narrow adaptation the source group is the only fallback; otherwise create
a neighboring document group when geometry permits; otherwise a new tab in
the source group with the return path exposed. `NewTab` bypasses view
dedup but not buffer sharing; `ReplaceTab` is the explicit same-tab quick
switch through the existing close-safety contract. Nothing implicitly
closes a dirty tab. Destination memory is session-local (recomputed after
restore; stale GroupIds never survive merge/close/restore/vault switch).
Multi-selected nodes offer Open-in-tabs: native-key order, max 20 per
invocation, honest opened-of-requested counts.

**Non-note intents (D-02).** Activating an attachment node drives the
existing local attachment inspector (`QueryPayload::AttachmentOpen`, core
validated open, honest typed refusal; never a note open). Activating an
unresolved node offers explicit Create (the prefilled, validated new-note
dialog via `offer_create_for_link`) or Locate (the prefilled picker) —
navigation never creates a note (FR-19).

## Files and debt cleared

| Area | Change | Debt |
|---|---|---|
| new `flavite-ui/src/host/open_intents.rs` | D-03 destination policy, session-local `OpenMemory`, `ReuseDocument`/`NewTab`/`ReplaceTab`, multi-open ceiling, pure `choose_reuse` decision | TD-4 |
| `app.rs` | global first-map-match `request_open` deleted; every surface (graph, reading links, backlinks/context, search rows, notes rows, picker same-tab, palette, creation, accessibility `OpenNote`) routes `Message::OpenIntent`; navigation pairing split from insertion/drag pairing (`handle_graph_action`, `GraphNodeCommand`); `CreateFromNode`/`LocateFromNode` messages; destination memory observes every committed transaction | TD-4 |
| `graph/events.rs` | `ActivateNode` outcome (no insert); pairing keyed by epoch+entity; bounded hold + `expire()`; drag/escape cancel; triple-click restarts | TD-5 |
| `host/graph.rs` | D-02 frozen-hold enforcement in `pump` (`emphasis_only_change`, deferred entity frames, bounded expiry, auto-fit pause); `open_node` per-kind routing (note → intent, attachment → inspector, unresolved → Create/Locate offer, removed/blocked → honest refusal); shared `insert_available` predicate; `OpenSelectedInTabs` (20 max, native-key order, honest counts); retired W09 deferrals | TD-5, TD-13 |
| `views/graph.rs` | selection band + occurrence details + notice are canvas overlays (zero-bounds chrome, fixed clipped footer); chips share catalog predicates; D-02 notice; writing-target banner and list hints now state the amended contract | TD-5 |
| `host/occurrences.rs`, `host/reading.rs`, `views/{notes,note,search}.rs`, `accessibility/` | all opens routed through typed intents; occurrence inspection opens its source through the destination policy; graph-owned attachment completions route honestly | TD-4 |
| `host/commands.rs` | keyboard Enter / Ctrl+Enter and palette run the same `GraphCommand`s; availability predicates (`graph.open_selected`, `graph.insert_selected`, `graph.create_from_node`, `graph.locate_node`) match execution | TD-13 |
| `host/appearance.rs`, `flavite-api/src/device_preferences.rs` | persisted one-time gesture-contract acknowledgement marker (schema-tolerant round-trip) | — |
| docs | `docs/APP-FEATURES.md` click semantics, list mode, non-note intents, provenance amended | — |
| gates/scripts | S04 cumulative gates (typed-intent presence, dead-pattern absence incl. retired gesture copy), xtask negative fixtures, native runner `workbench-visual-linux.sh` + `graph-editor-loop` scenario + shared `common.py`; `workspace_graph.py` retained-coordinate bug fixed and extended to assert zero-bounds selection and activation-never-inserts | — |

## Verification ownership

AC-4 (zero-pixel selection geometry + activation never inserts), AC-5
(destination order incl. hidden-match refusal, graph visibility, NewTab
buffer sharing), AC-6 (edit→graph→open→edit loop with identity, camera,
scope and Preview stability), plus the D-02 hold/freeze, stale-Insert
refusal, removed-node refusal, unresolved Create/Locate and attachment
inspector behaviors are owned by `flavite-ui/tests/open_intents.rs`,
`workbench_loop.rs`, `graph::events` unit tests, and the
`graph-editor-loop` native scenario on both renderers. See
`devdocs/verification/RFC0006-S04.md` for command receipts and evidence
classes.

## Explicitly not claimed by S04

- Completion/link catalog richness (S05), saved Boolean search (S06),
  vault Find/Replace (S07–S09), shell presets/tree (S10), retained
  operations workbench (S11).
- The §1.3 baseline `shared-editors` visual assertion
  (`writing_target.tab == document_anchor`) remains an S02/S04 investigation
  item under D-01's distinct authorities; S04 neither forces authority
  equality nor drops the assertion.
- Full NFR latency/p99 receipts (NFR-2/4 measurement) belong to the load
  receipts of their slices; S04 adds the gesture-hold distribution to the
  native scenario's evidence obligations rather than claiming p99 numbers.
