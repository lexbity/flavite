# Linux custom editor visual smoke

Run `bash scripts/ui-editor-visual-smoke-linux.sh` on Linux. The harness
launches the actual `flavite-editor` iced demo under X11 (using Xvfb when
needed), types `[[Vis`, captures the autocomplete popup, then replaces the
buffer with a known string and waits for the Session autosave before taking the
final editor capture.

Artifacts are written under `target/ui-visual/`:

- `editor-autocomplete.png` shows the in-memory title/target candidates.
- `editor.png` shows the final selected text and saved status.
- `editor-app.log` captures the demo's standard output and errors.

The script also checks that the saved note contains the typed content and that
the final screenshot decodes and contains visible color variation. This is a
rendered interaction smoke, not a pixel-perfect visual regression test.
