# Context Mode interface — implementation scope and verification

This change delivers the working command toolbar and temporary typing interface
over the existing source editor and Session. It does **not** certify completion
of FEUX-001 or any entire multi-capability specification slice. The specification
and references remain under the gitignored `scratch-devdocs/` directory.

## Behavior delivered

- A horizontal, in-flow toolbar replaces the source note header. Exit is the
  leading control and remains reachable above an owned dropdown/palette backdrop.
  Narrow panes use More; below 320 logical pixels, editing pauses with an Expand
  pane action while mode and selection remain intact.
- Entry by header button, resolved toggle binding, or Unicode-aware right-click.
  Command state uses physical keys, selection motions and flat menus. Typing
  survives repositioning and toolbar operations. Escape pops a menu before
  returning typing to commands; it does not exit. Tab departure and Read exit;
  OS blur seals typing without exiting. Caption/footer double-click exits.
- Real Cut/Copy/Paste, Undo/Redo, selection and movement, Change, line opening,
  palette, and current-note Find/Replace. Unsupported advanced operations have
  no advertised control. General context tools and source Context Mode retain
  distinct types and modules.
- Controls capture tab, incarnation, buffer, revision and selection. Clipboard
  completion additionally checks its mode generation. Delayed/replayed actions
  cannot retarget a different source or selection.
- Native key interpretation uses acknowledged host mode. An editor frame's old
  COMMAND/TYPING label cannot change what a queued key means. Native capture
  sequence and acknowledged interaction sequence are tracked separately because
  one command can perform several internal selection operations. Only receipts
  for this view's own edits authorize continuation across an undrawn revision.
- Text commits from an IME use the acknowledged input path, close composition,
  and advance the caret. Command-state stale text commits are refused.
- Toolbar controls, menus and expansion publish observed rectangles and route
  native accessibility actions through the same guarded handlers.
- Recent editing-state chips show the current state and one available redo.
  The Editing history chooser pages through the existing tree, including other
  branches. Restoration moves the head once, preserves branches and the saved
  baseline, restores recorded selections, and maps passive split-view selections
  and scroll anchors without taking their focus. The chooser suspends typing;
  Escape dismisses it before changing mode and Exit remains above its backdrop.

## Backend contract and architectural choices

The UI reuses `apply_edits`, `buffer.undo`, `buffer.redo`, existing input
preparation, and Session-owned persistence/conflict/recovery. It does not create
a second text buffer, undo tree, clipboard service or filesystem editing path.

Two additions are required for the temporary typing contract:

```json
{
  "command_id": "apply_edits",
  "args": {
    "buffer_id": 7,
    "revision": 12,
    "ops": [{"range": [10, 10], "replacement": "text"}],
    "undo_policy": {"context": {"origin": "00000000000000000000000000000001"}}
  }
}
```

`buffer.seal_undo_group` takes `{buffer_id, revision}`. It serializes through
the Session driving turn, checks the revision, and seals without adding a
document edit, revision or undo node. Repeating it at the same revision is safe.
The Context policy accepts exactly 32 lowercase hex characters for origin;
different origins, ordinary edits, isolated operations, save and history
navigation close the run. Idle alone does not. Existing batch validation,
format/EOL installation and monotonically advancing buffer revisions still apply.

Decision → cost: reuse the present single-selection input model for this
interface delivery. Recorded editor edits accept exactly one selection before
and after, with grapheme-boundary validation before any mutation. This prevents
an API caller from installing selections the mounted widget cannot display,
but does not satisfy FEUX-001's canonical multiple-selection target.

### Editing-history contract

`apply_edits` accepts optional `editor` metadata:

```json
{
  "origin": "00000000000000000000000000000001",
  "kind": "typing",
  "before": [[10, 10]],
  "after": [[14, 14]]
}
```

Origin must match the Context undo-policy origin when that policy is used.
Metadata and text commit together. A coalesced node keeps its first kind and
before-selection and updates its after-selection and version. Unrecorded edits
cannot extend a recorded node. Operation labels are fixed enums and contain no
note text. All edits and restores use `Session::execute`, including its
single-writer turn, panic containment, replacement leases, and autosave lifecycle.

`buffer.history_page` takes `{buffer_id, revision, cursor?, limit?}`; limit is
1–100, default 100. It returns `{buffer_id, revision, page}`. The page contains
an epoch, tree generation, current node, bounded entries, up to 12 ancestor
chips, an optional redo state, and an optional next cursor. Cursor epoch and
generation must match. The GUI freezes the actual cursor in a paging action;
replaying an old button cannot silently advance a second page.

`buffer.restore_state` takes `{buffer_id, revision, target}` where target is
`{epoch, node, version}`. Epoch is a fresh 32-character lowercase UUID hex value
for every buffer-history incarnation, including reload and process restart.
Targets expire when their recorded node is coalesced or their epoch changes.
The core validates epoch, node version, source revision, and durable-journal
segment before changing anything. Arbitrary restores across a tagged rename
transition are refused; ordinary Undo/Redo retain their journal behavior.

A successful restore returns `{buffer_id, revision, movement}`. Movement includes
optional recorded selections and a source-free replacement transform
`{start, end, inserted}` for other views. Undo/Redo return the same receipt.
Restoration installs the immutable snapshot and exact EOL/source format,
advances the buffer revision once, and adds no node. Dirty state is compared
against the current saved baseline rather than assumed dirty after navigation.
An incomplete save's durability/index milestone remains pending even if a
restored state's bytes match disk; only a successful save receipt or an explicit
disk-state adoption clears that obligation. Late save receipts retain the saved
revision's baseline while newer typing remains dirty.
No save is implicit in the command; normal autosave configuration still applies.

Decision → cost: expose head movement over the existing undo tree rather than
record a second GUI journal. This preserves one source of truth and branch
identity, but makes tree versioning and selection metadata public contracts.
Selecting an abandoned branch adopts its preferred redo continuation. Moving
backward within that continuation preserves it; Undo followed by Redo therefore
returns to the selected branch rather than the previously active branch.
Generic non-editor nodes have no recorded selection; their restores map the
requesting view's live selection through the same transform as passive views.
Selection mapping uses endpoint affinity and grapheme boundaries. The transform
is the contiguous difference between immutable states; for disjoint changes it
preserves positions outside that span and maps positions inside by affinity.

Decision → cost: bound pages and rendered summaries now while retaining the
existing tree's lifetime. This delivery does **not** implement FEUX-001's
per-buffer/app-wide snapshot budgets, eviction, checkpointing or allocation
reservation. Long editing sessions therefore retain the existing unbounded
snapshot-history memory risk. It must not be represented as completion of S07
or of the specification's history resource guarantees.

Decision → cost: add an explicit undo policy and a non-editing seal operation
to the existing tree. This preserves ordinary history and durable obligations,
but expands the public edit contract; API, buffer property and UI integration
tests therefore cover it. The legacy invalid empty `UndoTree::default` was
replaced by a valid empty-document root, and history jumps now seal a run.

Decision → cost: `Ctrl` is literal Control and `Mod` is the platform action
modifier. They serialize separately and collisions compare native modifier
bits. Existing macOS preferences that spelled `Ctrl` while intending Command
must use `Mod`. No synonym migration, compatibility alias or silent key-name
translation is introduced. Defaults use Ctrl+Space on Linux/Windows and
Ctrl+Shift+Space on macOS.

Command IDs for existing operations are reused directly. New editor commands
have one typed vocabulary shared by menus, palette and native routing; catalog
collision checks include that vocabulary. Full core/UI catalog consolidation
and effective-map adoption cleanup remain specification work.

## Verification surfaces

| Path | What it proves |
|---|---|
| `crates/flavite-ui/tests/context_mode.rs` | Real Session edits, mode lifecycle, Unicode source identity, exact selection guards, clipboard completion refusal, read-only behavior, narrow suspension, semantic action routing, queued mode transitions/replay and composition commits. |
| `crates/flavite-api/tests/context_undo.rs` | Public API policy/seal validation, repeated sealing, stale revisions, origin isolation, read-only refusal, normal Undo/Redo and no save side effect. |
| `crates/flavite-api/tests/editing_history.rs` | Real API paging, stale generation/target refusal, read-only capability gates, atomic metadata validation, one-revision restoration and no disk write. |
| `crates/flavite-buffer/tests/editing_history.rs` | Branch preservation, coalesced versions, unique epochs, journal barriers, exact mixed EOL restoration, selection receipts, unrecorded-edit isolation and clean-state synchronization. |
| `crates/flavite-vault/src/session.rs` history lifecycle test | Recorded edits and history restores rearm the existing autosave bookkeeping. |
| `crates/flavite-editor/src/input.rs` tests | Chunked grapheme planning matches a whole-string oracle across insertion/deletion/replacement and combining marks, emoji ZWJ, regional indicators, CR and LF. Clipboard CRLF is normalized before recording the caret. |
| `crates/flavite-editor/src/widget.rs` receipt test | An old pointer-release sequence cannot suppress new-revision selection/focus receipts; native capture sequences remain monotonic. |
| `crates/flavite-buffer/src/undo.rs` tests | Explicit idle-independent groups, foreign/ordinary boundaries, branches and a Unicode snapshot-history property oracle. |
| `crates/flavite-editor/src/context.rs` tests | Physical key grammar, modifier exclusion and Unicode word/grapheme behavior. |
| `crates/flavite-settings/src/chord.rs` tests | Literal/platform modifier distinction, native-key collisions and rejection of duplicate modifiers/key-name synonyms. |
| `crates/flavite-ui/tests/settings_tab.rs` | Successful native chord capture, cancelling capture, refusal of a second command taking that captured chord, and resetting it. The positive fixture uses Ctrl+Shift+Q; Mod+Shift+F is already owned by vault Replace. |
| `scripts/tests/workbench/context_mode.py` | Actual Xvfb mouse/keyboard input, backend text effects, rapid keys, clipboard, Find ownership, painted command/typing/menu controls, narrow overflow, and one-click Exit through overlays. |
| `scripts/tests/workbench/context_mode_history.py` | Actual painted chips and chooser, initial/redo restoration, branch activation, recorded selection, no added undo node, unchanged disk bytes, Escape ownership and Exit over the chooser. |

Reproduce the native scenario sequentially (parallel `xvfb-run -a` startup can
collide). The runner refuses missing prerequisites. Paint checks require
Tesseract with an installed `eng` or `afr` Latin model, Pillow, and ImageMagick;
observed bounds alone are not accepted as paint evidence.

```sh
cargo test -p flavite-ui --test context_mode
cargo test -p flavite-api --test context_undo
cargo test -p flavite-api --test editing_history
cargo test -p flavite-buffer --test editing_history
cargo test -p flavite-buffer undo::tests
cargo test -p flavite-editor context::tests
cargo test -p flavite-settings
bash scripts/workbench-visual-linux.sh --scenario context-mode --backend canvas
bash scripts/workbench-visual-linux.sh --scenario context-mode --backend gpu
bash scripts/workbench-visual-linux.sh --scenario context-mode-history --backend canvas
bash scripts/workbench-visual-linux.sh --scenario context-mode-history --backend gpu
just gate
```

Source inspection supplement (the following search must return 1, not 2):

```sh
rg -n 'todo!|unimplemented!|TODO|compat|shim' \
  crates/flavite-editor/src/context.rs \
  crates/flavite-ui/src/host/context_mode.rs \
  crates/flavite-ui/src/views/context_toolbar.rs
```

Native evidence is archived under
`scratch-devdocs/context-mode-evidence/2026-11-07/`. These Linux fixture runs do
not prove installed packaging, macOS/Windows input, native IME, or a complete
theme/scale/assistive-technology matrix. Earlier failed runs are not passing
evidence: one exposed an English OCR model missing from this machine, one a
cropped dropdown label, and a parallel pair collided during Xvfb startup.

### History continuation verification (2026-11-07)

- `cargo test --workspace`: 1,969 passed, zero failures, five existing ignores.
  The last branch-specific correction is additionally covered by the buffer
  history suite, API history tests, UI context/shared-view suites and a real
  native Undo/Redo round trip after selecting an abandoned branch.
- Complete UI suite: 315 passed, zero failures, one existing performance probe
  ignored. Buffer tests, strict workspace Clippy, rustfmt and architecture
  drift/edge/layer checks pass. The public API history and Context undo suites
  pass with real Session state.
- The `context-mode-history` scenario passes on canvas and GPU. The existing
  `context-mode` canvas scenario passes as regression coverage. History paint,
  manifests and harness observations are archived in the `history/` directory
  below the evidence root above.
- Initial sandbox runs could not write the search/recovery test state outside
  their allowed filesystem; reruns outside the sandbox passed. A loaded run
  exceeded the existing 50k-node graph-build test's one-second budget; the next
  full workspace run passed without changing that budget. Native startup also
  timed out on initial attempts; subsequent complete runs passed. Failed attempts
  are not counted as passing evidence.
- `just gate` is deferred at the user's request. No full FEUX-001 slice,
  packaging check, or complete platform/theme/load matrix is certified here.

## Remaining specification obligations

Canonical multiple selections; match collection
and selection-based transformations; history budgets/eviction/checkpointing;
full catalog/binding
adoption consolidation; diagnostics budgets, release-load/soak measurements and
the complete theme/scale/platform acceptance matrix are not implemented or
certified here. Gap 4's advanced verbs/automation remain deferred as agreed.
Unrelated workbench navigation/search/chrome slices were not included.

These are further implementation obligations, not claims that hidden controls,
stubs, or lower evidence classes satisfy the specification.
