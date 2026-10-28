# Linux UI visual smoke

The pre-D1 visual fixture is `flavite-ui/examples/graph_shader_probe.rs`. It
uses the existing deterministic synthetic graph generator and a fixed 1024 ×
768 iced window. The probe is a rendered-widget fixture, not the Lattice
application shell; D1's `app-demo` should replace or extend it with a fixture
vault and representative shell states.

## Run locally

Install Linux dependencies once, then capture the fixture:

```sh
just ui-visual-setup
just ui-visual-smoke
```

The PNG and application log are written under `target/ui-visual/`. The smoke
launches a real X11 window under Xvfb, waits for its title, captures the window
with ImageMagick, and checks that the output image decodes and contains more
than one color. CI uploads the PNG as the `lattice-ui-visual-smoke` artifact.

## What this proves

- The deterministic iced example launches and creates a visible Linux window.
- The graph widget produces a capturable frame through X11/Xvfb and the
  software GPU configured by `just gpu-setup`.
- A screenshot is retained for human or agent visual inspection.

This smoke is distinct from `shader_validation.rs`: that test validates WGSL
and wgpu pipeline creation without creating a window. Neither check asserts
pixel-perfect appearance, input flows, the future editor, or shell behavior.
Those checks belong to the D1+ app fixture and phase-slice UI tests.
