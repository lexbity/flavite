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
interface delivery. This avoids an unused protocol abstraction and keeps the
UI immediately usable, but does not satisfy FEUX-001's canonical selection-set
and history-receipt target. The interface deliberately exposes no multiple
selection or history-chip behavior until those complete paths exist.

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
| `crates/flavite-buffer/src/undo.rs` tests | Explicit idle-independent groups, foreign/ordinary boundaries, branches and a Unicode snapshot-history property oracle. |
| `crates/flavite-editor/src/context.rs` tests | Physical key grammar, modifier exclusion and Unicode word/grapheme behavior. |
| `crates/flavite-settings/src/chord.rs` tests | Literal/platform modifier distinction, native-key collisions and rejection of duplicate modifiers/key-name synonyms. |
| `scripts/tests/workbench/context_mode.py` | Actual Xvfb mouse/keyboard input, backend text effects, rapid keys, clipboard, Find ownership, painted command/typing/menu controls, narrow overflow, and one-click Exit through overlays. |

Reproduce the native scenario sequentially (parallel `xvfb-run -a` startup can
collide). The runner refuses missing prerequisites. Paint checks require
Tesseract with an installed `eng` or `afr` Latin model, Pillow, and ImageMagick;
observed bounds alone are not accepted as paint evidence.

```sh
cargo test -p flavite-ui --test context_mode
cargo test -p flavite-api --test context_undo
cargo test -p flavite-buffer undo::tests
cargo test -p flavite-editor context::tests
cargo test -p flavite-settings
bash scripts/workbench-visual-linux.sh --scenario context-mode --backend canvas
bash scripts/workbench-visual-linux.sh --scenario context-mode --backend gpu
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

## Remaining specification obligations

Canonical multiple selections and atomic selection receipts; match collection
and selection-based transformations; command summaries/history chips with
guarded restoration; bounded history paging/eviction; full catalog/binding
adoption consolidation; diagnostics budgets, release-load/soak measurements and
the complete theme/scale/platform acceptance matrix are not implemented or
certified here. Gap 4's advanced verbs/automation remain deferred as agreed.
Unrelated workbench navigation/search/chrome slices were not included.

These are further implementation obligations, not claims that hidden controls,
stubs, or lower evidence classes satisfy the specification.
