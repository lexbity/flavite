# Workspace shell foundation

Date: 2026-10-29. Scope: corrections to the implemented W02/W03 shell;
this is not completion evidence for RFC-0003 or W01's native platform matrix.

The shell retains the application-owned binary layout tree and shared Session
buffers/history. No document bodies, toolkit identities, or writing capabilities
have been added to persistence. `scratch-devdocs/` remains ignored and no test
depends on the concept image.

## Implemented behavior

- Shared flat tab, toolbar, icon-button and panel surfaces use neutral dark
  tokens with a violet focus/writing accent. Icons are vector primitives, not
  font-dependent symbols. Source headers and per-note durability status replace
  the inset editor card and metadata pills. This does not implement W11's full
  light/System/preferences contract.
- Tab presses start a real pointer gesture. Strip drops reorder/insert; body
  centre drops append; body edges split. All transactions go through the
  workspace reducer. The drag captures its structural revision, refuses stale
  drops and insufficient split space, and cancels on Escape/window blur.
- The strip computes a contiguous visible window from mounted width and keeps
  the active tab visible. All tabs remain reachable in an anchored scrollable
  menu. New view and More also use overlays: opening a menu cannot resize the
  editor. Arrow keys/Enter/Escape operate menus; Shift+F10 opens group actions,
  and all three menus have command-palette entries.
- Actual mounted workspace bounds feed the existing minimum-size solver.
  Its effective rectangles project into disposable PaneGrid ratios, including
  the divider-centre/usable-space conversion. The host captures the full 8px
  splitter corridor before editor input. Pointer movement paints a preview;
  release commits once; Escape/blur restores desired geometry. Narrow windows
  temporarily maximize with a complete scrollable group switcher.
- Editor runtimes distinguish `(TabId, incarnation)`, including two views of
  one buffer. Controlled carets/focus restore when moved; overlays suppress
  background editor input. Text/history ownership remains in Session.
- Accessibility exposes per-view editable text, line text runs, current caret
  and selection, tab/group controls, splitters, and overlay scopes/actions.
  Native text selections enter the normal host acknowledgement path and check
  view incarnation, source revision, run identity, composition and Unicode
  boundaries. Modal background routes and removed/old-revision runs reject.

## Verification

`cargo test -p flavite-ui --test workspace_layout` includes pointer edge-drop,
stale drag, popup geometry, resize cancellation, and solver-versus-toolkit
rectangle checks. Existing shared-view/history tests remain in place. UI unit
tests cover all active positions through 128 tabs at widths 200–1200px.

`runtime_accessibility` exercises real Session text, native action decoding,
caret acknowledgement, a guarded edit, old-revision rejection and modal
background rejection. Editor tests exercise a reused widget slot with a lower
interaction sequence in another view/incarnation. Unicode tests cover combining
characters, joined emoji and CRLF.

Run the actual window checks with:

```sh
just workspace-visual shell-foundation
just workspace-visual shared-editors
just workspace-visual layout
just workspace-gate W03
just gate
```

The foundation OS scenario types into the real editor, then checks edge docking,
centre docking, reorder, unchanged TabId/buffer/revision/caret, live splitter
preview before release, narrow/restore desired-ratio preservation, anchored menu
geometry and overflow selection. Visual runs use an isolated Xvfb display when
available, retaining screenshots, semantic observations, assertions and a
per-run manifest under `target/workspace-evidence/`. The manifest records the
commit, lock hash and source-worktree fingerprint; fixture-only observations
are compiled behind `ui-test-support`. CI runs this scenario and retains its
artifacts on failure. Render backend identification remains the existing
`gpu-or-software` value; this is not GPU/canvas parity evidence.

Two obsolete test assertions were replaced explicitly: stale UI transactions
must be rejected unchanged rather than re-stamped, and splitter tests use the
host's pointer/usable-space contract rather than assuming an iced ratio equals
the persisted ratio.

## Native evidence still required

The current native runner was attempted, but returned nonzero because the
desktop reported `AT-SPI IsEnabled=false`. No accessibility setting was changed.
The native script now requires a real Focus/SetTextSelection action followed by
OS typing and an exact saved-byte assertion. It must be rerun with an active
screen reader. Headless routing checks and Xvfb pointer input do not satisfy
screen-reader or IME acceptance. Linux Wayland, macOS, Windows and the required
native IME combinations remain incomplete as recorded in
[workspace-native-input.md](workspace-native-input.md).
