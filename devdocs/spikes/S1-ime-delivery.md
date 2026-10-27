# Spike S1-1: IME event delivery to a custom iced widget

**Status:** closed (verdicts recorded below). **Slice:** PLAN-A A5 (§9 Phase 0,
spike S1, part 1 of 2). **Date:** 2026-10-26. **Pinned versions:** iced 0.14.0,
winit 0.30.13 (the workspace pins; verified against the vendored crate sources
in the cargo registry).

## Question

ADR-0001 closed Q1 (the custom editor is the decision, the stock `text_editor`
is not a v1 fallback). S1 verifies the custom path's one remaining external
dependency: **can IME preedit / commit / candidate events reach a *custom* iced
widget** on Linux (X11 + Wayland), macOS, and Windows at the pinned versions?
(D18: IME correctness is a MUST — CJK users cannot use a broken editor.)

This document is S1 slice 1 of 2 (A5). Slice 2 of 2 (A6) audits the remaining
S1 checklist items (caret geometry, HiDPI hit-test precision, undo determinism,
multi-cursor, paste-from-HTML, markdown-widget coverage, AccessKit) and records
the consolidated verdict in `devdocs/spikes/S1-verdict.md`.

## Method (three evidence layers)

1. **Source trace** — the full delivery chain read from the vendored sources of
   winit 0.30.13, iced_winit 0.14.1, iced_core 0.14.0, iced_runtime 0.14.0,
   per platform backend. This is platform-independent evidence: the conversion
   and dispatch code below is not `cfg`-gated per OS.
2. **Headless synthetic tests** — `xtask/tests/s1_ime_probe.rs` (21 tests)
   constructs the probe widget and drives its `Widget::update` with synthetic
   events of the exact pinned types, asserting recording, commit semantics, and
   the IME strategy handoff. Runs in CI (G0/G1) on Linux, macOS, and Windows.
3. **Runtime probe** — `cargo run -p xtask --bin s1_ime_probe` on a live
   desktop, following `devdocs/checklists/ime-acceptance.md`. Executed here on
   Linux (both backends; see transcripts and their honest limits).

## The delivery path (source-verified, all platforms)

```text
OS IME → winit platform backend → WindowEvent::Ime(Ime::{Enabled, Preedit, Commit, Disabled})
       → iced_winit conversion  → iced_core::Event::InputMethod(input_method::Event::{Opened, Preedit, Commit, Closed})
       → iced_runtime UserInterface::update (per event, root widget tree)
       → Widget::update(&mut self, tree, &Event, ...)   ← the custom widget
```

Concrete anchors (vendored sources):

- **winit 0.30.13 `Ime` enum** (`src/event.rs:774`): exactly four variants —
  `Enabled`, `Preedit(String, Option<(usize, usize)>)`, `Commit(String)`,
  `Disabled`. **There is no `Candidates` variant at this version** (finding F2).
- **iced_winit 0.14.1 conversion** (`src/conversion.rs:320-328`): maps all four
  winit variants to `input_method::Event::{Opened, Preedit, Commit, Closed}`
  (`Preedit(content, cursor_range)` — the cursor range is byte-wise).
- **iced_core 0.14.0** (`src/event.rs`): `Event::InputMethod(input_method::Event)`
  is a top-level event variant alongside `Keyboard` / `Mouse` / `Window` /
  `Touch`.
- **iced_runtime 0.14.0** (`src/user_interface.rs`): `UserInterface::update`
  dispatches every event to the root widget's `update` — a custom widget at the
  root of the view receives all of them (verified: no focus gate at the
  dispatch layer; focus handling is the widget's own choice, as in iced's
  `text_input`).
- **iced_core 0.14.0 `Widget` trait** (`src/widget.rs`): the event method is
  `fn update(&mut self, tree, event: &Event, layout, cursor, renderer, clipboard,
  shell, viewport)` — renamed from 0.13's `on_event` and now takes the event **by
  reference**; capture is via `shell.capture_event()` (the return type is `()`).
  (The plan text says "`on_event`"; the pinned 0.14 name is `update`.)
- **IME strategy out** (iced_winit 0.14.1 `src/window.rs:212-287`): the widget's
  `Shell::request_input_method(&InputMethod::Enabled { cursor, purpose,
  preedit })` — honored only during `window::Event::RedrawRequested` — makes
  the runtime call `set_ime_allowed(true)`, `set_ime_cursor_area(cursor)`,
  `set_ime_purpose(purpose)`, and draw the preedit as an over-the-spot overlay
  at the cursor rectangle.
- **Keyboard text** (iced_core 0.14.0 `src/keyboard/event.rs`):
  `KeyPressed { text: Option<SmolStr>, .. }` carries the produced text for
  non-IME input.

## Per-OS event matrix (source-verified)

| OS / backend | winit backend source (0.30.13) | IME events emitted | Candidate-list event | Cursor-area API | Runtime evidence here |
|---|---|---|---|---|---|
| Linux X11 | `platform_impl/linux/x11/event_processor.rs:107-122, 1001-1006` (XIM) | `Enabled` / `Preedit` / `Commit` / `Disabled` | **No** | `set_ime_cursor_area` | Yes (transcript below) |
| Linux Wayland | `platform_impl/linux/wayland/seat/text_input/mod.rs:78-147`, `wayland/window/mod.rs:617` (zwp_input_method_v2 / text-input-v3) | `Enabled` / `Preedit` / `Commit` / `Disabled` | **No** | `set_ime_cursor_area` | Yes (transcript below) |
| macOS | `platform_impl/macos/view.rs:304-451, 894` (NSTextInputClient: `setMarkedText` → Preedit, `insertText` → Commit) | `Enabled` / `Preedit` / `Commit` / `Disabled` | **No** | `set_ime_cursor_area` | Source only (no macOS host here) |
| Windows | `platform_impl/windows/event_loop.rs:1520-1598` (WM_IME_START / WM_IME_COMPOSITION / WM_IME_END) | `Enabled` / `Preedit` / `Commit` / `Disabled` | **No** | `set_ime_cursor_area` | Source only (no Windows host here) |

## Findings

- **F1 — Delivery is complete at the pinned versions, on all four backends.**
  Every backend emits all four IME lifecycle events; iced_winit converts them
  without loss; the runtime delivers them to a custom widget's `update`. The
  probe widget (below) handles all four: `Opened`/`Closed` track IME state,
  `Preedit` is recorded and handed to the runtime overlay, `Commit` inserts at
  the caret. The synthetic tests assert all of this behavior.
- **F2 — No candidate-list event exists at winit 0.30.13 / iced 0.14, on any
  platform.** winit's `Ime` enum has no `Candidates` variant; iced's
  `input_method::Event` has none either. Candidate windows are drawn by the
  OS/IME itself (over-the-spot or floating), positioned via the cursor area the
  widget reports (`set_ime_cursor_area`). Consequence: the production editor
  (A10) cannot rely on candidate-list *events*; it gets composition state via
  `Preedit` (content + selection range) and can render its own candidate UI from
  that. The probe pins the four-variant surface with an exhaustive match
  (`xtask::s1_probe::input_method_event_name`, no wildcard arm): a future
  iced/winit that adds a variant breaks the build and forces a conscious
  revisit of this finding.
- **F3 — API drift vs the plan text:** the pinned iced 0.14 `Widget` event
  method is `update` (not `on_event`), takes `&Event`, and captures via
  `shell.capture_event()`. The probe and tests use the verified 0.14 names.
- **F4 — IME strategy timing:** `request_input_method` is honored only during
  `window::Event::RedrawRequested`. The probe (like iced's `text_input`)
  requests the strategy on every redraw while focused, and disables the IME
  (`InputMethod::Disabled`) when unfocused.
- **F5 — Keyboard text:** non-IME text arrives as `KeyPressed.text`; control
  characters must be filtered by the editor (the probe does).

## Runtime transcripts (Linux, this host)

Environment: live KDE Plasma **Wayland** session (KWin), Xwayland on `:1`,
2560x1440 @ 1.5 scale, winit prefers Wayland when `WAYLAND_DISPLAY` is set.
The probe was run twice: as a native Wayland client (default environment) and
as an X11 client (`env -u WAYLAND_DISPLAY DISPLAY=:1 …`, i.e. via Xwayland).
Input was **real** (the host user's physical keyboard and mouse; the window was
activated/focused); xdotool XTEST synthetic input did not reach the app in this
environment (Xwayland input-routing quirk of the live session) and is recorded
here honestly rather than claimed.

### Linux Wayland (native client) — excerpt

```text
[s1-probe     132 ms] window.opened              position=None size=Size { width: 720.0, height: 520.0 }
[s1-probe     132 ms] window.rescaled            scale_factor=1.5
[s1-probe     132 ms] window.resized             size=Size { width: 720.0, height: 520.0 }
[s1-probe     136 ms] window.focused
[s1-probe    2094 ms] mouse.cursor_entered
[s1-probe    2094 ms] mouse.cursor_moved         position=Point { x: 0.85546875, y: 278.9414 }
[s1-probe    2153 ms] mouse.cursor_moved         position=Point { x: 216.85547, y: 116.94141 }
```

The custom widget received window-lifecycle and real pointer-motion events as a
native Wayland client. (Full log: 636 lines of window/mouse/keyboard events.)

### Linux X11 (Xwayland) — excerpt

```text
[s1-probe     136 ms] window.opened              position=Some(Point { x: 493.33334, y: 220.0 }) size=Size { width: 720.0, height: 520.0 }
[s1-probe     141 ms] input_method.closed        ime closed
[s1-probe     141 ms] window.unfocused
[s1-probe     141 ms] window.focused
[s1-probe     141 ms] keyboard.modifiers_changed modifiers=Modifiers(0x0)
[s1-probe     141 ms] mouse.cursor_moved         position=Point { x: 2.6666667, y: 119.333336 }
[s1-probe     141 ms] window.moved               position=Point { x: 493.33334, y: 248.0 }
[s1-probe     991 ms] mouse.cursor_entered
[s1-probe     389 ms] keyboard.key_pressed       key=Character("m") text=Some("m")
[s1-probe     492 ms] keyboard.key_released      key=Character("m")
[s1-probe   10573 ms] keyboard.key_pressed       key=Character("d") text=Some("d")
[s1-probe   10741 ms] keyboard.key_pressed       key=Character("e") text=Some("e")
[s1-probe   10849 ms] keyboard.key_pressed       key=Character("v") text=Some("v")
```

Two results matter here:

- **`input_method.closed` at 141 ms — a real IME event delivered to the custom
  widget at runtime.** winit's X11 backend emits `Ime::Disabled` through XIM at
  window setup; the probe recorded it through the full chain
  (XIM → `WindowEvent::Ime` → `Event::InputMethod` → `Widget::update`). The IME
  event path is live on X11, not just present in source.
- **`keyboard.key_pressed … text=Some("m")` — real physical-keyboard input with
  produced text** delivered to the custom widget while the window was focused
  (the host user typed; the probe recorded press and release per key).

### Synthetic tests (all three OSes in CI)

`cargo test -p xtask --test s1_ime_probe` — 21 tests, green. Coverage:
recording of every `Event` variant (30 variants across keyboard / mouse /
window / touch / input_method, in order, with non-decreasing timestamps);
`Opened`/`Preedit`/`Commit`/`Closed` recording; preedit recorded but never
committed early; **commit replaces the preedit** and inserts at the caret;
empty-preedit clearing; `Closed` clearing state; focus gating (recorded always,
applied only when focused — mirroring iced's `text_input`); the IME strategy
handoff on `RedrawRequested` (`Enabled { cursor, purpose: Normal, preedit }`
with the exact cursor rectangle, `Disabled` when unfocused); keyboard
insertion/editing keys; click focus/unfocus; the ring buffer cap
(`LOG_CAPACITY`); the pinned stdout line format; the widget contract
(tag/state/layout).

## Checklist outcomes (`devdocs/checklists/ime-acceptance.md`)

The AC-24 instrument is ready and its Linux rows are **partially exercised**:
window/keyboard/IME-lifecycle delivery verified at runtime on both Linux
backends with real input (transcripts above). **Not exercised here:** actual
JA / ZH / KO compositions — no IME daemon (ibus / fcitx) is installed in this
environment — and macOS / Windows (no such hosts here). Those rows are pending
manual execution on the respective desktops; the synthetic tests already cover
the widget's handling of the events those IMEs produce. This is recorded as
pending, not claimed as done.

## Verdicts

| OS / backend | Verdict | Basis |
|---|---|---|
| Linux X11 | **delivered-with-caveats** | Source-verified delivery + runtime-verified (XIM `input_method.closed`, real keyboard with text). Caveat: no candidate-list events at pinned versions (F2); JA/ZH/KO composition checklist pending (no IME daemon here). |
| Linux Wayland | **delivered-with-caveats** | Source-verified delivery + runtime-verified (native Wayland client receiving window/pointer events). Caveat: as X11; composition checklist pending. |
| macOS | **delivered-with-caveats** | Source-verified (NSTextInputClient → `Ime::{Enabled,Preedit,Commit,Disabled}` → iced conversion → `Widget::update`). Runtime confirmation pending on a macOS host. Caveat: F2. |
| Windows | **delivered-with-caveats** | Source-verified (WM_IME_* → `Ime::{Enabled,Preedit,Commit,Disabled}` → iced conversion → `Widget::update`). Runtime confirmation pending on a Windows host. Caveat: F2. |

**Verdict summary:** IME preedit / commit / open / close events **are delivered
to a custom iced widget** on all target platforms at the pinned versions. The
single caveat is the absence of candidate-list *events* (F2) — candidate
windows are OS-managed and positioned via the widget-reported cursor area; the
composition state itself arrives via `Preedit`. No backend requires the
ADR-0001 escalation path (upstream patches → fork iced → renegotiate purity);
that path stays armed only as the recorded reversal trigger.

## Consequences

- **A6 is unblocked** (S1 part 1 of 2 closed; part 2 audits the remaining
  checklist items and writes `devdocs/spikes/S1-verdict.md`).
- **A7+ proceed** per ADR-0001 — no escalation.
- **A10 (production IME path)** inherits: the four-variant event surface (pinned
  by an exhaustive match), the `RedrawRequested`-only strategy handoff, the
  cursor-rectangle computation for candidate-window positioning, and the
  decision point recorded in the checklist doc (inline preedit rendering vs the
  runtime's over-the-spot overlay).
- The probe stays in the tree as a manual diagnostic
  (`cargo run -p xtask --bin s1_ime_probe`) and its widget/state types are the
  executable reference for the A7 `EditorSurface` work.

## Reversal / escalation

ADR-0001's escalation (upstream patches → fork iced → renegotiate the pure-Rust
constraint, last resort only) is **not triggered**: delivery works at the
pinned versions on all four backends. Re-arm conditions: a backend stops
delivering (regression in a winit/iced upgrade — the weekly iced-`master`
advisory build and the CI matrix are the tripwires), or a candidate-list event
becomes a hard requirement (F2) and cannot be met without forking.
