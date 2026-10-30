# Linux UI visual smoke

The repository has a graph renderer probe, a full-shell smoke, and the W02
workspace interaction runner. The standalone editor widget demo was retired
when the editor became part of the Flavite shell; it is not a product window.

## Run locally

Install Linux dependencies once, then capture the fixture:

```sh
just ui-visual-setup
just ui-visual-smoke
bash scripts/ui-app-visual-smoke-linux.sh
just workspace-visual layout
```

The PNG and application logs are written under `target/ui-visual/`; workspace
evidence is written under `target/workspace-evidence/`. The runners launch real
X11 windows under Xvfb and capture them with ImageMagick. The workspace runner
also sends OS keyboard input and checks semantic state transitions.

## What this proves

- The deterministic graph fixture launches and creates a visible Linux window.
- The graph widget produces a capturable frame through X11/Xvfb and the
  software GPU configured by `just gpu-setup`.
- The shell smoke opens the full Flavite window and exercises its command
  palette. The W02 workspace scenario checks note opening and tab/group
  operations in that shell; it does not claim editor typing coverage.
- A screenshot is retained for human or agent visual inspection.

These checks are distinct from `shader_validation.rs`: that test validates WGSL
and wgpu pipeline creation without creating a window. The screenshots are
smoke evidence, not pixel-perfect assertions or cross-platform IME/a11y tests.
