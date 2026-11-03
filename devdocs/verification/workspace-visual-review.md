# W11 — Visual preferences and constrained layout: implementation and verification record

**Date:** 2026-11-03. **Branch:** `uxfixes`. **Slice:** RFC-0003 §27 (W11), following the W10 record.

**Status:** Implemented on Linux X11 with real OS input evidence. macOS/Windows native evidence and IME remain deferred per the tracked release scope notes; NFR-16 combinations that were not run are listed explicitly below and are not claimed.

## Scope delivered

1. **Semantic tokens and contrast (TD-15).** `crates/flavite-ui/src/style/tokens.rs` owns the light/dark palettes and graph/editor data-color roles; `style/contrast.rs` implements the WCAG 2.x luminance/ratio/compositing arithmetic; `style/type_scale.rs` derives scale-aware type and chrome metrics. Component styles in `style.rs` select AA-compliant text/fill pairs themselves (`chip`, `dim_text`, `accent_text`) instead of trusting toolkit-generated pairs. `crates/flavite-ui/tests/visual_contract.rs` pins text ≥ 4.5:1, non-text ≥ 3:1, chip pairs, the ≥24 px pointer-target floor, the ≥2 px focus ring, the ≥8 px splitter corridor, and non-color cues for selected/focused/active states.
2. **Preferences.** The validated codec for the device-local `DevicePreferences` `DevicePreferences` user-state document lives in `flavite-api/src/device_preferences.rs`, beside the user-state kinds and the app-launch codec it travels with — one schema authority for every host (the shell re-exports it; `flavite-ui/src/preferences.rs` is a re-export shim). Document shape (schema 1): theme System/Light/Dark, interface scale 75–200%, editor font size 12–28, line width 40–160/full, reduced motion, and shortcut overrides. Unknown additive fields are preserved; invalid fields fall back with warnings; unreadable/future documents disable saving instead of being overwritten (D-12). `host/appearance.rs` is the runtime host: resolved scheme (System follows `iced::system::theme_changes()` via mundy, dark when the OS reports nothing), metrics cache, persistence through `Flavite::read_user_state/write_user_state`, and the shortcut table.
3. **Render adapters.** The editor consumes `flavite_editor::TypeMetrics` (font size + derived line height, cached-shaping keys include the metrics) and `flavite_editor::EditorColors` from the active scheme — no color literals remain in the editor draw path. Line width caps the wrap width (left-aligned; a wrap constraint, not a reflow anchor). Graph node/edge/label/selection colors derive from token roles for the active scheme; the WGSL edge color moved into the 64-byte camera uniform (offset 48, validated with naga); a scheme change rebuilds geometry from the same publication frame without touching camera, positions, pins, or the layout worker (no forced re-layout).
4. **Scale plumbing.** Interface scale reaches the solver at every call site (`Adapter::project`, `workspace_region`, `split_hits`, `is_tab_visible`, `split_fits`, harness `adaptive_maximized`) and all chrome/type metrics in views, tab strip, chrome icons, dialogs, and the shell. At 100% the layout is byte-identical to the pre-W11 constants (`Metrics::default()` reproduces them exactly).
5. **Constrained layout (D-13).** Adaptive maximize and the group switcher existed from W02; W11 adds the full NFR-14 geometry matrix with maximum-depth trees (`crates/flavite-workspace/tests/geometry_matrix.rs`), keyboard splitter increments (`Mod+Alt+Shift+Arrow` grows/shrinks the split adjacent to the focused group, 2.5% steps, normal `ResizeSplit` transactions), scale-aware split admission, and the exact canonical `ratio_f32` (the pre-W11 adapter conversion silently truncated ppm to hundredths — repaired and pinned by test).
6. **Shortcut catalog (D-15).** `DEFAULT_CHORDS` is the single binding table; overrides from preferences replace defaults; `keyboard_message` resolves chords exactly (modifiers and key) through the effective table, which also fixed two latent defects: `Alt+Left`/`Alt+Right` navigation could never fire (it sat inside the `command()`-gated branch) and two `Alt+Shift+Right` arms collided. The Preferences dialog provides a capture-based collision editor (Escape cancels, an unreadable/colliding/OS-reserved chord is refused with a named owner and an alternate action) and restore-defaults. Palette commands `workspace.toggle_theme` and `workspace.cycle_interface_scale` expose quick edits.
7. **UX and storage fixes found during evidence runs.** `flavite-vault/src/user_state.rs` wrote the `.bak` last-good copy with create-new semantics, so the third save of any user-state kind failed once a backup existed (layouts included). The backup is now written atomically. `xtask` negative gates extended for W11; `just workspace-gate` accepts W01–W11 with stage 11 running the three new suites.

## Verification evidence

~~~sh
cargo test -p flavite-ui --test appearance_preferences   # 9 pass
cargo test -p flavite-workspace --test geometry_matrix   # 7 pass
cargo test -p flavite-ui --test visual_contract          # 6 pass
cargo test -p xtask --test workspace_gates               # negative gates incl. W11
bash scripts/check-workspace-contracts.sh W11            # pass
bash scripts/workspace-visual-linux.sh --scenario appearance-matrix
~~~

Two defects were caught by the evidence run itself and fixed in the same slice: Escape did not close a modal dialog (the dialog leaked into the narrow-window capture; Escape now closes the dialog before any gesture cancellation), and the dialog card clipped its shortcut list on small windows (the card now scrolls within a bounded max height).

The appearance-matrix run produces `target/workspace-evidence/appearance-matrix-*/` with PNGs (dark-100, light-100, light-150, preferences dialog, light-200 narrow adaptive, restored dark-100), `harness.jsonl` semantic snapshots carrying `theme`/`theme_choice`/`interface_scale_pct`/`editor_font_size`/`reduced_motion`, and a manifest (schema 1, build hash, worktree hash, `input_evidence: rendered_os_input`). Checks: dark default → light via real palette input → 150% scale → dialog → 640×480 adaptive maximize with **desired ratios byte-identical** across the narrow window → restore to dark/100%.

## Known incompleteness (explicit, per NFR-16)

- **Native screen-reader evidence:** this record contains no macOS/Windows AT runs. The Linux AT-SPI runner (`just workspace-native`) is unchanged by W11 and was not re-executed for this record; the preferences dialog and shortcut editor are reachable through the existing accessibility tree but native OS verification is unmet evidence, not a pass.
- **IME:** unchanged by this slice; still deferred with scaffolding required (tracked scope note).
- **Reduced motion:** the preference is stored, editable, and exposed; no animated transition exists in the shell today, so there is nothing to gate yet. Any future animation MUST consult `preferences.reduced_motion` (graph camera easing included).
- **Interface scale vs. device pixel ratio:** interface scale multiplies logical chrome/type metrics only (NFR-14's device scales are the toolkit's own hidpi factor, untested here beyond the solver's logical model).
- The `rfd` portal dialogs, and therefore "Choose folder…", are unchanged.
- **Cosmetic finding (recorded for W13 polish):** at the extreme corner of the supported matrix (640 px window at 200% interface scale), a long writing-target chip and the status text can visually overlap in the status bar; every control remains reachable and the semantic checks are unaffected.

## Review notes for the next slice

- W12 should reuse `AppearanceHost::persist`'s generation-CAS handling pattern for close/recovery receipts.
- The shortcut table's exact-match semantics (no firing when extra modifiers are held) is intentional and is now the documented contract for the editor UI.
- `views/note.rs` wires editor metrics/colors; read-mode body text size intentionally keeps its own density (D-15 names editor font/size/line-width, not reading typography).
