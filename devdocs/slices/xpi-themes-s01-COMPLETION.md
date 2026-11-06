# XPI Themes S01 — browser theme packages on flavite's semantic surface

**Status:** complete. Command receipts are recorded in
`devdocs/verification/xpi-themes-s01.md`. This file restates the
self-contained contract (the work has no upstream RFC; it follows the
slice/verification convention of the W-series records).

## Amended contract

**T-01a — real-package report quality (demo validation amendments).**
Validated against four real signed browser packages (gitignored
`scratch-devdocs/demo_xpi_themes`; see the verification record). Two
amendments came out of that run: Firefox's `additional_backgrounds`
array parses structurally (`ImageRef::Multiple`) so the report records a
proper drop instead of an "unreadable" warning, and Chrome `tints`
inside `colors` records its chrome-only drop reason instead of a
misleading color warning. `META-INF/` signing entries survive conversion
verbatim, UUID-shaped gecko ids sanitize safely, and reconverting an
already-converted demo package reproduces the same id and report.

**T-01 — a theme is a code-less package, parsed by a pure codec.**
`flavite-theme` (new Tier-2 crate, IO-free, `#![forbid(unsafe_code)]`) is
the single authority for theme packages: manifest parsing (WebExtension
`theme`/`dark_theme`, MV2/MV3, gradient image objects captured but
unsupported), CSS/RGB color parsing, the browser-key → flavite-slot
mapping, bounded image decode (PNG/APNG/JPEG; SVG refused), and the
standard `.xpi` → `.flavite.xpi` rewrite. Every decode is budgeted
(16 MiB bytes, 8 MiP pixels, 8192 px, 256 frames, 64 MiB total decoded,
512 zip entries, 64 MiB package) — violations are typed errors, never
truncation. Zip entry names must be safe relative paths; the storage
layer writes entries to disk by name.

**T-02 — the mapping is an explicit compatibility contract.** Browser
color keys map onto named `SemanticTheme` slots through documented
preference chains (`frame`→top bar, `toolbar`→window, `toolbar_field`→
editor, `popup`→menus, `icons`→accent, `sidebar`→sidebar; the `frame`
active/inactive pair fills `frame_inactive`). Every key the chains do
not consume is dropped **with a stated reason** in the conversion report
(`tints`: chrome-only; `ntp_*`: no surface; separators/derived states;
unknown keys). A key may fill several slots when the theme speaks
sparingly. Slots the package never supplies stay `None`: the built-in
palette keeps its seat.

**T-03 — `.flavite.xpi` is idempotent and lossless.** The converted
package keeps every original entry verbatim except `manifest.json`,
which is re-emitted with a reserved `flavite` key holding the
`ConversionReport` (format_version, theme_id, mapped, dropped,
warnings, animated_frame, supports_dark_variant). Identity is the gecko
id, else an existing report's id (reconversion never mints a second
theme), else a blake3 content hash. Writing the converted files again
through `convert_xpi` reproduces the same id and report.

**T-04 — storage is device state behind the facade.** `ThemeStore`
(flavite-api `themes.rs`) owns `<data>/flavite/themes/<sanitized-id>/`
with `theme.flavite.xpi` + `theme.json` metadata, following the
user-state pattern: injected `Fs`, `atomic_write`, symlink guards, hard
size bounds, sanitized collision-resistant directory stems. Install,
list, and uninstall run as typed commands `theme.install`, `theme.list`,
`theme.uninstall` through the `CommandRegistry` with a new
`Capability::Themes` (auto-granted to Gui/Cli/Internal origins; plugins
get nothing unless granted). Handlers run on the facade turn (the
`&Session` handler signature cannot reach the store) after registry
validation and the capability check, before buffer preconditions; they
never touch buffers or the vault session, so they stay available on
read-only vaults. A missing source file is a typed
`theme_source_unreadable`; an unknown id is `theme_not_found`.
**Selection is deliberately not a command**: it is the
`theme_package` device preference (additive, schema 1 unchanged), edited
through the same compare-and-set path as every other preference — a
second writer would race the CAS.

**T-05 — rendering resolves through the shell's solver.** The resolved
palette (`tokens::theme_resolved`) composites themed slots over the
built-in palette (translucent colors composite over the effective window
backdrop) and refuses text/accent slots that fall below the WCAG floor
(4.5:1 text, 3:0 accent) — a low-contrast package keeps its flavor
without losing readability. The editor follows `toolbar_field`/
`toolbar_field_text` (`editor_colors_resolved`); the top toolbar strip
is fixed-height (`Metrics::top_bar_height`) so a package `theme_frame`
layers exactly behind it via a `stack` (iced 0.14 `Background` has no
image variant). APNG headers animate by frame-cycling pre-decoded
handles from the 50 ms pump, honoring per-frame fcTL delays and gated on
`reduced_motion`. **Explicit v1 boundary:** graph data colors keep the
built-in scheme tokens — they are pinned by the WGSL uniform parity
contract; threading a themed accent through the graph uniforms is its
own slice.

**T-06 — the CLI converts offline; the GUI installs in place.**
`flavite theme convert <in> [out]` needs no vault and prints the report;
`flavite theme install/list/select/uninstall` go through the facade
(D16: the codec reaches the CLI via the facade's re-export, no direct
crate edge). The GUI installs from the native file dialog through the
same `theme.install` command and lists/selects/removes in the
Preferences dialog with the conversion warnings inline.

## Files and debt cleared

| Area | Change | Debt |
| --- | --- | --- |
| `crates/flavite-theme` | new crate: colors, manifest, model, apng, convert | APNG frames re-decode on every boot (load-time cost bounded by the budgets); subframe APNGs composite per spec but only full-frame packages are performance-tuned |
| `crates/flavite-api/src/themes.rs` | ThemeStore + typed command payloads + facade-turn executor | uninstall leaves the empty theme directory behind (invisible to `list`) |
| `crates/flavite-api/src/capability.rs` | `Capability::Themes` | — |
| `crates/flavite-api/src/device_preferences.rs` | additive `theme_package` key | — |
| `crates/flavite-api/src/flavite.rs` | store field, registrations, turn intercept, `themes()` accessor, `ThemeStoreError` variant, hermetic test seam | — |
| `crates/flavite-ui/src/style/tokens.rs` | `theme_resolved`, `resolved_palette`, `themed_slots`, `top_bar_colors`, `editor_colors_resolved` | success/warning/danger keep built-in per-scheme values (status colors, not theme surfaces) |
| `crates/flavite-ui/src/style/{style.rs,type_scale.rs}` | `TOP_BAR_HEIGHT` metric | the bar's height is now normalized (44 px at 100%) instead of content-derived |
| `crates/flavite-ui/src/host/appearance.rs` | theme package load/resolve/animation state, new preference messages, install/uninstall reporting | — |
| `crates/flavite-ui/src/workspace_shell.rs` | fixed-height top bar + theme_frame stack | — |
| `crates/flavite-ui/src/host/dialogs/mod.rs` | theme-package preferences section, `pick_theme_file` | — |
| `crates/flavite-ui/src/app.rs` | `ThemePackagePicked`, pump-driven animation tick, attach-time reloads | — |
| `crates/flavite-cli/src/main.rs` | `theme` subcommand (convert/install/list/select/uninstall) | — |
| `Cargo.toml` / `crates.toml` | `zip` external (deflate only), flavite-theme tier/edges/externals, `cargo xtask arch gen` | — |

## Explicitly not claimed by S01

- Graph-canvas themed colors (T-05 boundary), SVG/gradient images,
  `additional_backgrounds` layering, tiling/alignment properties, and
  the `color_scheme`/`content_color_scheme` properties.
- Non-Linux GUI verification (the dialog and stack paths follow the
  existing cross-platform widget APIs; native evidence recorded on Linux
  only).
