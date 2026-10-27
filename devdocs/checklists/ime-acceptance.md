# IME acceptance checklist — AC-24 instrument

**Spike:** S1 (editor viability), slice A5. **Spec basis:** D18 (IME correctness is a
MUST; composition rendering, commit, and candidate-window positioning are
acceptance-tested per platform), ADR-0001 (the custom editor is the decision; S1
verifies the custom path), §9 Phase 0. **Owner:** PLAN-A (A5 creates this
instrument; A10 automates it as far as the platform allows and owns the
production IME path).

**Instrument:** `cargo run -p xtask --bin s1_ime_probe` — a minimal iced
application hosting one custom widget (`xtask::s1_probe::ImeProbe`) that records
**every** runtime event it receives to stdout, one line per event:

```text
[s1-probe      42 ms] input_method.preedit    preedit="かな" selection=Some(0..2)
[s1-probe      87 ms] input_method.commit     commit="かな"
```

The widget keeps committed text, a caret, and the current preedit; it requests
the IME strategy (cursor area + purpose + preedit) from the shell on redraw,
exactly like iced's own `text_input`. Preedit handed to the shell is drawn by
the iced runtime as an over-the-spot overlay at the cursor area.

## What "pass" means

For each OS × IME below, a run passes when **all** of the following hold:

1. **Opened.** After switching to the IME and starting a composition, the
   stdout log contains `input_method.opened`.
2. **Preedit.** While composing, the log contains `input_method.preedit` lines
   whose `preedit=` text tracks the composition (e.g. `かな` for JA kana input,
   the pinyin letters for ZH, the jamo for KO), and the preedit is visible on
   screen at the caret (runtime overlay).
3. **Candidate window.** The OS candidate window appears near the caret
   (positioned via the cursor area the widget reports). **Known platform fact
   (pinned versions):** winit 0.30 / iced 0.14 deliver **no candidate-list
   event** — the candidate window is OS-managed. Record the *visual* placement
   here; there is no `candidates` line to grep. See
   `devdocs/spikes/S1-ime-delivery.md` (finding F2).
4. **Commit.** On commit (Enter / number selection), the log contains
   `input_method.commit` with the final text, the committed text appears in the
   input line, and the preedit is cleared (an empty `preedit=""` line precedes
   the commit, per the winit contract).
5. **Closed.** After switching the IME off, the log contains
   `input_method.closed`.
6. **Caret tracking.** Moving the caret (arrow keys, click) moves the reported
   cursor area; a new composition starts at the new caret.

A run with any failure is a **finding**, not a checklist edit: record it in
`devdocs/spikes/S1-ime-delivery.md` (or a new spike doc) and do not tune the
checklist to pass.

## Execution status

| OS / backend | Environment | Status |
|---|---|---|
| Linux X11 | KDE Plasma Wayland session, Xwayland (`env -u WAYLAND_DISPLAY`) | Partially exercised — see below |
| Linux Wayland | KDE Plasma Wayland session (KWin) | Partially exercised — see below |
| macOS | — | Pending (requires a macOS desktop) |
| Windows | — | Pending (requires a Windows desktop) |

Partially-exercised notes (Linux, this repository's CI container host — a live
KDE Plasma Wayland session, 2026-10-26):

- The probe ran as a native **Wayland** client (winit prefers Wayland when
  `WAYLAND_DISPLAY` is set) and as an **X11** client via Xwayland; both received
  real window-lifecycle, pointer-motion, and keyboard events (physical keyboard
  with produced text, e.g. `keyboard.key_pressed key=Character("m")
  text=Some("m")`), and X11 delivered a real XIM IME lifecycle event
  (`input_method.closed` at startup). Full transcripts:
  `devdocs/spikes/S1-ime-delivery.md`.
- **No IME daemon is installed in this environment** (no ibus / fcitx), so
  actual JA / ZH / KO compositions could not be produced here. The headless
  synthetic tests (`cargo test -p xtask --test s1_ime_probe`, 21 tests, run in
  CI on Linux, macOS, and Windows) drive the widget's `update` with synthetic
  `Opened` / `Preedit` / `Commit` / `Closed` events and assert recording,
  commit-replaces-preedit, caret insertion, and the IME strategy handoff.
- xdotool (XTEST synthetic input) did not reach the app in this environment
  (Xwayland input-routing quirk of the live session); the manual steps below
  use a real keyboard and mouse.

## Manual procedure (per OS)

0. Build and launch: `cargo run -p xtask --bin s1_ime_probe`
   (on Linux X11 under a Wayland session: `env -u WAYLAND_DISPLAY cargo run -p
   xtask --bin s1_ime_probe`).
1. Click into the input area (the bordered strip at the top). The widget is
   focused; the caret is drawn.
2. Switch to the IME under test and type a composition:
   - **JA (e.g. Anthy / Mozc / Kotoeri):** type romaji, e.g. `nihongo` → expect
     preedit `にほ…` evolving, commit `にほんご`.
   - **ZH (Pinyin):** type `nihao` → expect preedit `nihao` → `你好` on selection.
   - **KO (e.g. fcitx-hangul / Mozc-ko):** type jamo, e.g. `dkssud` → `안녕`.
3. Watch the stdout log (checklist items 1–2) and the screen (preedit overlay at
   the caret, candidate window near the caret — item 3).
4. Commit (Enter or candidate number). Check items 4.
5. Switch the IME off. Check item 5.
6. Move the caret (arrow keys; click elsewhere in the input area) and compose
   again. Check item 6.

## Checklists

Legend: ☐ pending · ✅ pass · ❌ fail (record as a finding). "Log line" columns
refer to the stdout lines above.

### Linux — X11

| # | IME | Opened logged | Preedit logged + visible | Candidate window at caret | Commit logged + inserted | Closed logged | Caret tracking | Result |
|---|---|---|---|---|---|---|---|---|
| 1 | JA | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ |
| 2 | ZH (Pinyin) | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ |
| 3 | KO | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ |

### Linux — Wayland

| # | IME | Opened logged | Preedit logged + visible | Candidate window at caret | Commit logged + inserted | Closed logged | Caret tracking | Result |
|---|---|---|---|---|---|---|---|---|
| 1 | JA | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ |
| 2 | ZH (Pinyin) | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ |
| 3 | KO | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ |

### macOS

| # | IME | Opened logged | Preedit logged + visible | Candidate window at caret | Commit logged + inserted | Closed logged | Caret tracking | Result |
|---|---|---|---|---|---|---|---|---|
| 1 | JA (Kotoeri) | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ |
| 2 | ZH (Pinyin) | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ |
| 3 | KO | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ |

### Windows

| # | IME | Opened logged | Preedit logged + visible | Candidate window at caret | Commit logged + inserted | Closed logged | Caret tracking | Result |
|---|---|---|---|---|---|---|---|---|
| 1 | JA (MS-IME) | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ |
| 2 | ZH (Pinyin) | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ |
| 3 | KO | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ | ☐ |

## Notes for A10 (production IME path)

- The probe hands the preedit to the iced runtime via
  `Shell::request_input_method` (honored only during
  `window::Event::RedrawRequested`), and the runtime draws it as an over-the-spot
  overlay at the reported cursor rectangle. A10 decides whether the production
  editor renders preedit inline itself (D14: "preedit rendered inline at the
  caret") or keeps the runtime overlay; both paths are proven to receive the
  events by this instrument and by the synthetic tests.
- Keyboard text for non-IME input arrives as
  `keyboard.key_pressed ... text=Some(...)`; control-key text is filtered by the
  probe and must be filtered by the production editor too.
- `input_method::Event` at the pinned versions has exactly four variants
  (`Opened`, `Preedit`, `Commit`, `Closed`). The probe pins this with an
  exhaustive match (`xtask::s1_probe::input_method_event_name`) that stops
  compiling if a future iced/winit adds a variant (e.g. candidate lists) —
  a conscious update point, not a silent gap.
