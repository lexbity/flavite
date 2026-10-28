#!/usr/bin/env bash
# Render and interact with the real custom iced editor under X11/Xvfb.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [[ "$(uname -s)" != Linux ]]; then
    echo "editor visual smoke is Linux-only" >&2
    exit 2
fi

for tool in xdotool import identify; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "missing editor visual-smoke tool: $tool" >&2
        exit 2
    fi
done

if [[ -n "${DISPLAY:-}" ]] && ! xdotool getdisplaygeometry >/dev/null 2>&1; then
    echo "DISPLAY=${DISPLAY} is not reachable; retrying with Xvfb if available." >&2
    unset DISPLAY
fi

if [[ -z "${DISPLAY:-}" ]]; then
    if ! command -v xvfb-run >/dev/null 2>&1; then
        echo "DISPLAY is unset and xvfb-run is unavailable" >&2
        exit 2
    fi
    exec xvfb-run -a -s "-screen 0 1280x900x24" bash "$0"
fi

target_dir="${CARGO_TARGET_DIR:-target}"
visual_dir="$target_dir/ui-visual"
mkdir -p "$visual_dir"
vault_dir="$(mktemp -d "$visual_dir/editor-vault.XXXXXX")"
cat >"$vault_dir/demo.md" <<'NOTE'
# Custom editor visual fixture

Click into this note. The smoke harness replaces it with a visible input check.

- Caret and text layout
- Selection and keyboard input
- Session-owned edits and autosave
NOTE

cargo build -p flavite-editor --example editor-demo
env -u WAYLAND_DISPLAY WINIT_UNIX_BACKEND=x11 \
    "$target_dir/debug/examples/editor-demo" "$vault_dir" \
    >"$visual_dir/editor-app.log" 2>&1 &
app_pid=$!
window_id=""
cleanup() {
    if [[ -n "$window_id" ]]; then
        xdotool windowclose "$window_id" 2>/dev/null || true
    fi
    for _ in $(seq 1 20); do
        kill -0 "$app_pid" 2>/dev/null || break
        sleep 0.1
    done
    kill "$app_pid" 2>/dev/null || true
    wait "$app_pid" 2>/dev/null || true
}
trap cleanup EXIT

for _ in $(seq 1 100); do
    window_id="$(xdotool search --onlyvisible --name '^Lattice editor demo$' 2>/dev/null | head -n 1 || true)"
    if [[ -n "$window_id" ]]; then
        break
    fi
    if ! kill -0 "$app_pid" 2>/dev/null; then
        cat "$visual_dir/editor-app.log" >&2
        echo "editor demo exited before creating its window" >&2
        exit 1
    fi
    sleep 0.1
done

if [[ -z "$window_id" ]]; then
    cat "$visual_dir/editor-app.log" >&2
    echo "timed out waiting for the editor demo window" >&2
    exit 1
fi

xdotool windowactivate --sync "$window_id"
sleep 1
xdotool mousemove --sync --window "$window_id" 80 205
xdotool click --clearmodifiers 1
xdotool key --clearmodifiers ctrl+a
xdotool type --clearmodifiers --delay 100 "Visual editor input confirmed"
for _ in $(seq 1 60); do
    if grep -Fq "Visual editor input confirmed" "$vault_dir/demo.md"; then
        break
    fi
    sleep 0.1
done
if ! grep -Fq "Visual editor input confirmed" "$vault_dir/demo.md"; then
    cat "$visual_dir/editor-app.log" >&2
    cat "$vault_dir/demo.md" >&2
    import -window "$window_id" "$visual_dir/editor-failure.png" 2>/dev/null || true
    echo "editor input did not reach the Session autosave" >&2
    exit 1
fi

# Highlight the final grapheme so the captured frame exercises selection draw
# as well as text, caret placement, and the save status.
xdotool key --clearmodifiers shift+Left
sleep 0.5
import -window "$window_id" "$visual_dir/editor.png"
identify "$visual_dir/editor.png"
color_count="$(identify -format '%k' "$visual_dir/editor.png")"
if (( color_count < 16 )); then
    echo "captured editor fixture appears blank ($color_count colors)" >&2
    exit 1
fi
echo "editor screenshot: $visual_dir/editor.png"
