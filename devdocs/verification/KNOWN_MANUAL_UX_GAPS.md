# Known manual-UX gaps (RFC-0003)

These require human/manual testing (pointer, rendering, IME, assistive tech):

- **Editor selection feel**: drag-selection now renders on the next frame
  (redraw request + shaping cache + gated host rebuilds); click-drag,
  shift-click, and double-click need human confirmation, including with
  palette/picker/menus open (pointer input is no longer gated by overlay
  flags).
- **Editor/Read link navigation**: plain click on a link navigates in Edit
  mode; Read-mode links navigate without the catalog-freshness gate. The
  message paths have integration tests (`tests/link_navigation.rs`); real
  click/pointer targeting still needs manual verification.
- **Graph settle & camera**: first geometry auto-fits, the camera follows
  while the layout runs, one final fit on settle, and a pan/zoom hands the
  camera to the user. Delta batches keep simulation momentum. Verify visually
  with GPU and canvas backends on a vault being edited.
- **Graph chrome layout**: compact toolbar + writing-target banner + corner
  "Fit to view"; verify spacing/clipping at narrow widths and the
  neighborhood depth/direction pickers on the toolbar row.
- **Menu / dropdown**: "Flavite" menu and context menus depend on
  pointer/overlay interactions.
- **Splits via drag**: commands work; drag/drop affordances need manual
  testing (drag ghost and background-drop feedback remain open).
- **Right-click context menus**: not implemented anywhere yet (deferred).
- **Perceived latency**: per-event shaping passes were eliminated and
  per-move host rebuilds gated; actual latency needs measurement by feel on
  real hardware.
- **Narrow window adaptation / visuals**: responsive behavior and visual
  polish require manual review.

Deferred this round (explicitly): right-click context menus, drag ghost /
empty-space drop feedback, vault file browser (folder tree notes list),
note lifecycle UI (create/rename/delete).

Evidence classes: native, rendered_os_input, headless_integration,
deterministic_model, fault_injected, source_inspection. Lower classes cannot
substitute for required native/rendered evidence.
