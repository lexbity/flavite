# Workspace native input and accessibility evidence

**Slice:** RFC-0003 W01. **Status:** implementation in progress; the real
Flavite shell now uses a pinned iced AccessKit runtime bridge and exposes its
current source text, destination tabs, and revisioned editor selection. Native
OS action and assistive-technology evidence has not yet been collected.

This record replaces the earlier screen-reader waiver for the proposed
workspace release. Existing keyboard tests, the Xvfb visual scripts, and the
S1 IME probe are not native accessibility evidence. Upstream iced 0.14 has no
AccessKit hooks; W01 now carries a small local runtime patch under
`vendor/iced-accessibility/`, with source revisions and archive hashes in its
`PATCHES.md`. The custom editor publishes acknowledged selection state and
accepts native SetTextSelection through the ordinary app message and widget
operation path. Headless tree tests verify the semantic projection; they do
not establish native screen-reader exposure or OS action round trips.

## Required evidence

Record one row per OS/backend and assistive technology. A row is complete only
when an actual native screen reader can read the current temporary-vault note,
report caret/selection movement, and invoke an action that changes the real
editor caret. IME evidence must separately cover Japanese, Chinese Pinyin, and
Korean commit and cancellation. Synthetic input must be labeled synthetic.

| OS / backend | Screen reader/version | IME/version | Runtime build | Result/artifact | Status |
|---|---|---|---|---|---|
| Linux X11 | No screen reader installed | — | local iced/AccessKit patch | AT-SPI API round trip passed with test bus activation; see findings | Partial; reader evidence missing |
| Linux Wayland | — | — | — | — | Not run |
| macOS | — | — | — | Deferred per user direction | Deferred |
| Windows | — | — | — | Deferred per user direction | Deferred |

## Findings and boundary

- `scripts/ui-app-visual-smoke-linux.sh` passed its real shell
  window/palette assertions earlier on 2026-10-29. It launches
  `flavite-ui/examples/app-demo`; that earlier screenshot predates W01's shell
  editor integration and is not evidence for the current build. A rerun after
  the changes was unavailable because `DISPLAY=:1` was unreachable and
  `xvfb-run` is not installed. The earlier capture is rendered UI evidence
  only.
- At the end of this W01 work session, the same script captured the current
  patched-runtime build in `target/ui-visual/app-shell.png`. Visual inspection
  confirms this is the full Flavite shell (sidebar, workspace, status bar), not
  the standalone editor widget. The first run failed its Ctrl+P check because
  the script activated but did not focus the window. Adding `xdotool windowfocus`
  made the rerun pass its palette and post-command image-change assertions.
  This is rendered OS input evidence only, not native screen-reader evidence.
  A later repeat could not start because `DISPLAY=:1` was no longer reachable
  and `xvfb-run` was unavailable; the passing run above used the display that
  was available at that time.
- The Flavite shell's Editor destination now instantiates the custom
  `flavite-editor` widget and routes its revision-guarded edits through the
  Session. `cargo test -p flavite-ui --test runtime_accessibility` constructs
  the actual shell program against a temporary vault, checks the published
  Session text and routes a shell tab action. The app host test separately
  checks text-selection action routing and the editor widget operation checks
  revision/composition guards. The standalone editor demo and its visual
  script were retired in W02; their historical failed run is not evidence for
  the current shell.
- The editor now holds IME preedit transiently, renders it underlined, replaces
  the captured selection once on Commit, and clears composition on Closed or
  window focus loss. Unit tests cover preedit/commit and cancellation. Native
  IME evidence is still required.
- IME checks started on Linux X11 on 2026-10-29. This host has no
  `ibus-daemon`, `fcitx5`, or `fcitx` executable, no matching user-bus service,
  and no `GTK_IM_MODULE`, `QT_IM_MODULE`, or `XMODIFIERS` configuration. There
  is therefore no installed JA/ZH/KO input method to produce a real
  composition. The native JA/ZH/KO rows remain unrun; synthetic widget tests
  are recorded only as behavior checks. The focused-editor operations now
  re-enable the input-method request, and `Closed` clears composition while
  retaining focused input so switching the IME back on does not require a
  second editor click. A synthetic cancel-then-reopen-then-commit regression
  test passes; it is not OS IME evidence.
- `scripts/workspace-native-accessibility-linux.sh` builds and launches the
  current app shell with an isolated fixture vault and checks the actual X11
  window before invoking its AT-SPI harness. Early runs found the desktop
  service disabled and exposed stale fixture routing, interface dispatch, and
  missing semantic bounds needed for Component.GrabFocus; the harness now
  reports app/window failures with the app log and uses the fixture vault.
- A follow-up native X11 run temporarily enabled the session's
  `org.a11y.Status.IsEnabled` flag for the harness, then restored it to false.
  AT-SPI discovered the actual Flavite app, read the open temporary note,
  dispatched Component.GrabFocus and SetTextSelection, and a native X11 key
  event inserted one marker exactly at the selected caret; Ctrl+S persisted the
  expected fixture bytes. This is native AT-SPI API plus native keyboard
  evidence for the bridge path. No screen reader was installed, so it does not
  satisfy the required real screen-reader evidence.
- The AT-SPI service does not publish applications while `IsEnabled=false` in
  this desktop session. The runner does not enable it automatically; run it
  under an active screen reader on a normal acceptance host. The successful
  bridge-only run used a temporary explicit bus toggle and is recorded as
  partial evidence, not a screen-reader pass.
- The W02 workspace visual runner exercises OS keyboard input and screenshot
  capture; it does not inspect an OS accessibility tree or dispatch native
  accessibility actions.
- `devdocs/spikes/S1-accesskit-audit.md` found no AccessKit integration in
  upstream iced 0.14. The local bridge now implements the required runtime
  event-loop seam without Flavite domain logic; native verification remains
  outstanding.
- macOS and Windows native evidence is deferred per the current user direction.
  Linux X11 bridge action/text routing has a passing native API round trip, but
  real screen-reader evidence is still outstanding. Linux Wayland and native
  Japanese, Chinese Pinyin, and Korean IME evidence also remain outstanding.

Do not mark W01 complete until the bridge, live editor semantic projection,
native action round trip, and all required platform evidence are recorded.
