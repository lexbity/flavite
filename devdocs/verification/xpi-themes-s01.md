# XPI Themes S01 — verification record

**Slice:** xpi themes S01 (see `devdocs/slices/xpi-themes-s01-COMPLETION.md`).
**HEAD:** a26377c0996d69116b61988efc266c92856d3fef (branch `xpithemesupport`),
worktree digest over the modified tracked files:
`08b3573f594437bf883ade386c6f345787d9aa1fd1e5e6c86585365c7a546b62`.
**Date:** 2026-11-06. **Toolchain:** rustc 1.90 workspace toolchain
(`rust-toolchain.toml`), Linux.

## Scope delivered over the recorded commit

1. New Tier-2 crate `flavite-theme` (colors/manifest/model/apng/convert)
   with inline unit tests, APNG round-trips through the png crate
   encoder, conversion round-trips through in-memory zips, and
   budget/hostile-input refusals.
2. flavite-api: `ThemeStore` storage, `Capability::Themes`, typed
   `theme.install`/`theme.list`/`theme.uninstall` commands on the facade
   turn, additive `theme_package` preference, hermetic `ThemeStore`
   seam.
3. flavite-ui: resolved palette/editor/top-bar rendering, fixed-height
   top bar with the `theme_frame` stack, pump-driven APNG frame cycling
   gated on `reduced_motion`, theme-package section in Preferences with
   native-file-dialog install.
4. flavite-cli: `theme convert` (offline) and
   `theme install/list/select/uninstall` through the facade.
5. Architecture: `crates.toml` tier/responsibilities/edges + `zip`
   first-party external, artifacts regenerated in the same change.

## AC evidence

**AC-1 — codec correctness (T-01/T-02/T-03).**

~~~sh
cargo test -p flavite-theme
# test result: ok. 30 passed; 0 failed
~~~

Covers: hex/functional/named/array color forms; manifest parse with
warnings-as-drops and unknown-key preservation; gecko id in both
containers; dark variant; gradient capture; mapping chains with
first-key-wins, multi-slot reuse, inactive-pair fill, unmappable-theme
warnings; static PNG/JPEG decode; 3-frame APNG decode with per-frame
delays; translucent subframe `BlendOp::Over` composition;
`DisposeOp::Background` clearing; budget refusals (oversized bytes,
SVG); conversion with report/files/identity fallback; idempotent
reconversion; missing-manifest/non-theme/missing-frame-image typed
errors; traversal-name and size-cap refusals.

**AC-2 — store durability and typed commands (T-04).**

~~~sh
cargo test -p flavite-api themes
cargo test -p flavite-api --test theme_commands
# test result: ok. 4 passed (store unit)
# test result: ok. 5 passed (command bus)
~~~

Covers: install→list→load→uninstall round trip with a hermetic root;
reinstall replaces in place; invalid package typed failure; safe
collision-free id stems (traversal, empty, oversized); the full command
bus path over a tempdir vault with a hermetic device data directory
(install success payload, list counts, uninstall, stray-argument
rejection, missing-file `theme_source_unreadable`, unknown-id
`theme_not_found`, read-only vault availability, plugin
`CapabilityDenied` without `Themes` and success with it).

**AC-3 — preferences schema (T-04).**

~~~sh
cargo test -p flavite-api device_preferences
# (within the lib suite: 53 passed) theme_package round-trips, absent and
# invalid forms fall back to None with warnings, unknown fields preserved.
~~~

**AC-4 — conversion report (T-02/T-03, native scenario).**

~~~sh
./target/debug/flavite-cli theme convert /tmp/theme-smoke/starfield.xpi
# Converted: Purple Starfield (smoke)
# Theme id: smoke-starfield@flavite
# Mapped 11 color keys; dropped 2; animated header: true; dark variant: true
#   dropped ntp_background: new-tab page has no flavite surface
#   dropped properties.ntp_background_alignment: background layout properties have no flavite surface
#   warning: theme color "tints" is not a readable CSS color or RGB array; it was dropped
~~~

The output package was re-opened independently (python zipfile): entries
`header.png` + `manifest.json`; the reserved `flavite` key carries
format_version 1, the gecko id, `animated_frame: true`,
`supports_dark_variant: true`, the 11 mapped (key, slot) pairs, and the
dropped keys with reasons. The scenario package is a hand-assembled
4-frame APNG (fcTL/fdAT sequence numbers, CRCs) + Firefox-style
manifest, built outside the test suite.

**AC-5 — architecture and gates.**

~~~sh
cargo xtask arch gen && cargo xtask arch check
# arch check passed (drift + edges + layering)
cargo clippy -p flavite-theme -p flavite-api -p flavite-ui -p flavite-cli --all-targets
# 0 warnings
~~~

`flavite-theme` is Tier 2 (80% floor at the next `cov-check` run);
`flavite-api → flavite-theme` and `flavite-ui → flavite-theme` are the
only new edges; the CLI reaches the codec through the facade re-export
(no `flavite-cli → flavite-theme` edge; D16 preserved).

**AC-6 — real-package validation (demo themes).** Fixture: four real,
signed browser theme packages (nyan cat animated, praise the sun
animated, purple starfield animated, saturn plus) in the gitignored
`scratch-devdocs/demo_xpi_themes/`. The committed env-gated suite
`crates/flavite-api/tests/demo_theme_packages.rs` runs only when
`FLAVITE_DEMO_THEMES` names the fixture directory, so CI never depends
on the scratch content.

~~~sh
FLAVITE_DEMO_THEMES=$PWD/scratch-devdocs/demo_xpi_themes \
  cargo test -p flavite-api --test demo_theme_packages
# test result: ok. 3 passed; 0 failed
~~~

Evidence: all four packages convert, install into a hermetic store,
list, and load; every package maps to at least one semantic slot; the
decoded `theme_frame` (saturn plus) matches the report's animated flag;
every original entry — including `META-INF/` cose/mozilla signing files
— survives conversion with the manifest present exactly once; no
package produces an "unreadable" warning. Native CLI receipts:

~~~sh
./target/debug/flavite-cli theme convert scratch-devdocs/demo_xpi_themes/<pkg> --json
# nyan_cat:      Mapped 10, dropped 7  (tints + additional_backgrounds recorded, 0 warnings)
# praisethesun:  Mapped 11, dropped 16 (0 warnings)
# purple_starfield: Mapped 13, dropped 28 (0 warnings)
# saturn_plus:   Mapped 6,  dropped 2  (0 warnings; static theme_frame decodes)
~~~

Reconverting a converted demo package (`saturn_plus…flavite.xpi`)
reproduces the identical theme id and report (idempotence, T-03). Real
APNG inspection of the demos: 193-, 39-, 27-, and 16-frame headers —
all within the 256-frame budget — but all carried as
`additional_backgrounds`, which v1 drops per T-02; only saturn plus's
static `theme_frame` renders in v1.

**AC-7 — native GUI demo (Linux, Xvfb evidence).** A real demo vault
(`/tmp/flavite-demo-vault`) with two demo themes installed through the
CLI (`theme install` + `theme select`), the production binary booted
under Xvfb (1280×900, X11), notes opened through the actual quick-switch
dialog, and the window captured (`target/ui-visual/demo-shot-*.png`):

- **Nyan Cat (Animated)**: the top bar paints the package's `frame`
  color — sampled `rgb(0,49,99)`, exactly the manifest value — with
  themed bar text; the editor surface paints `toolbar_field` — sampled
  `rgb(0,0,0)` — with `toolbar_field_text`; the Preferences dialog lists
  "Nyan Cat (Animated)" as Active with Remove/Built-in-palettes actions.
  Nyan's translucent `toolbar` and dim `tab_background_text` correctly
  fall back through the contrast solver, so untouched surfaces keep the
  built-in palette (the fallback is visible, not guessed).
- **Saturn plus**: the static `theme_frame` image renders behind the
  fixed-height top bar via the stack (Saturn artwork visible across the
  strip) with the palette mapped from its 6 color keys.

Demo-driven fixes folded in before this record: `top_bar_colors` is now
wired into the shell (background + `top_bar_button`/`top_bar_dim_text`
text styles — it had been computed but never painted), and
`EditorColors` gained a `background` slot the widget paints (it painted
`palette().background` before, so themed `toolbar_field` surfaces never
reached the editor).

## Explicitly not claimed

- GUI animation/visual evidence (screenshots) — the animation path is
  unit-shaped (frame index advance gated on `reduced_motion`) but the
  native visual run is part of the next visual-review cycle.
- Graph-canvas themed colors (T-05 boundary), SVG/gradient images,
  background layering/tiling properties.
- `cargo llvm-cov` floor measurement for the new crate (runs with the
  next `just ci-gate`).
