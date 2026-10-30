# Pinned iced accessibility patch

This directory contains a narrow local patch to the iced 0.14.0 runtime used
by Flavite. The three source packages are Apache-2.0 OR MIT, as recorded in
their Cargo manifests. Copyright remains with the iced contributors. The
patched sources add the AccessKit event-loop adapter and two `Program` hooks;
they contain no Flavite-specific behavior.

| Crate | Version | Upstream source revision | SHA-256 of registry archive |
|---|---:|---|---|
| `iced_program` | 0.14.0 | `3997291f318a8bc06fa522f5579836fb3feb94df` (`program`) | `6dfafec2947cda688d8eb00dac337ba11aa60f9ef6335aed343e189d26e4a673` |
| `iced_runtime` | 0.14.0 | `3997291f318a8bc06fa522f5579836fb3feb94df` (`runtime`) | `d1889b819ce4c06674183242e336c8d49465665441396914dc07cc86f44fa8d4` |
| `iced_winit` | 0.14.1 | `38237dd294da3256c2ac4d3f8288fc73956c91e0` (`winit`) | `7589888c8e951899cc688247a69933bb7a0511f0b4b2e122ac3fcd5dacb37f72` |

The original archives came from crates.io and can be re-fetched by the pinned
versions above. Patch dependencies are pinned in `Cargo.lock`; AccessKit uses
`accesskit` 0.25.2 and `accesskit_winit` 0.34.2. The license identifiers and
source revisions are retained in each vendored `Cargo.toml` and
`.cargo_vcs_info.json`.

## Local delta

- `iced_program`: `Program` and `Instance` expose an optional semantic tree and
  an optional action-to-message callback.
- `iced_runtime`: forwards AccessKit winit events as runtime actions.
- `iced_winit`: creates one AccessKit adapter per native window, processes
  platform accessibility events on the winit loop, publishes the program tree
  after redraw, and returns native actions to the usual runtime message path.
- Existing deprecated `try_next` calls in the touched winit loop were migrated
  to `try_recv` to keep strict warning gates clean.

Upstream references: <https://github.com/iced-rs/iced> and
<https://github.com/AccessKit/accesskit>.
