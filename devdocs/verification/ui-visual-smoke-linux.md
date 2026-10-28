# Linux UI visual smoke

The Linux visual checks cover two actual iced windows:
`flavite-ui/examples/graph_shader_probe.rs` renders the deterministic graph
fixture, while `flavite-editor/examples/editor-demo.rs` opens a fixture vault
through the Flavite facade and exercises the custom editor.

## Run locally

Install Linux dependencies once, then capture the fixture:

```sh
just ui-visual-setup
just ui-visual-smoke
just ui-editor-visual-smoke
```

The PNG and application logs are written under `target/ui-visual/`. Both smokes
launch real X11 windows under Xvfb and capture them with ImageMagick. The graph
smoke checks that its image decodes and contains multiple colors. The editor
smoke types into a fixture note, accepts `[[Concepts]]` autocomplete, ctrl-clicks
the link, types into the opened target, and verifies both files were saved. CI
uploads the screenshots as visual-smoke artifacts.

## What this proves

- The deterministic graph fixture launches and creates a visible Linux window.
- The graph widget produces a capturable frame through X11/Xvfb and the
  software GPU configured by `just gpu-setup`.
- The custom editor accepts text and link completion through real X11 input,
  routes ctrl-click through the public command facade, preserves focus on the
  opened buffer, and reaches the Session save path for both notes.
- A screenshot is retained for human or agent visual inspection.

These checks are distinct from `shader_validation.rs`: that test validates WGSL
and wgpu pipeline creation without creating a window. The screenshots are
smoke evidence, not pixel-perfect assertions or cross-platform IME/a11y tests.
