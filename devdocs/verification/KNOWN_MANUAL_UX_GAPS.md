# Known manual-UX gaps (RFC-0003)

These require human/manual testing (pointer, rendering, IME, assistive tech):

- **Graph empty state / rendering**: graph may show no nodes/edges depending on index/graph service state; verify with canvas/GPU backends and check `graph_service` status/stale discards.
- **Link click navigation**: clicking links in Read mode and editor navigate paths depend on catalog resolution and message routing; end-to-end click behavior needs manual verification.
- **Menu / dropdown**: "Flavite" menu and context menus depend on pointer/overlay interactions.
- **Splits via drag**: commands may work while drag/drop affordances need manual testing.
- **Text selection / lag**: editor selection hit-testing and perceived latency require manual interaction.
- **Narrow window adaptation / visuals**: responsive behavior and visual polish require manual review.

Evidence classes: native, rendered_os_input, headless_integration, deterministic_model, fault_injected, source_inspection. Lower classes cannot substitute for required native/rendered evidence.
