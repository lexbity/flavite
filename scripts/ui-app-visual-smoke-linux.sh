#!/usr/bin/env bash
# Capture the real iced application shell under Xvfb and exercise its navigation.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [[ "$(uname -s)" != Linux ]]; then
    echo "application visual smoke is Linux-only" >&2
    exit 2
fi

for tool in xdotool import identify; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "missing visual-smoke tool: $tool" >&2
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
vault_dir="$(mktemp -d "$visual_dir/app-vault.XXXXXX")"
cargo build -p flavite-ui --example app-demo
env -u WAYLAND_DISPLAY WINIT_UNIX_BACKEND=x11 \
    "$target_dir/debug/examples/app-demo" "$vault_dir" \
    >"$visual_dir/app-shell.log" 2>&1 &
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
    window_id="$(xdotool search --onlyvisible --name '^Flavite$' 2>/dev/null | head -n 1 || true)"
    if [[ -n "$window_id" ]]; then break; fi
    if ! kill -0 "$app_pid" 2>/dev/null; then
        cat "$visual_dir/app-shell.log" >&2
        echo "application exited before creating its window" >&2
        exit 1
    fi
    sleep 0.1
done
if [[ -z "$window_id" ]]; then
    cat "$visual_dir/app-shell.log" >&2
    echo "timed out waiting for the application window" >&2
    exit 1
fi

xdotool windowactivate --sync "$window_id"
sleep 1
import -window "$window_id" "$visual_dir/app-shell.png"
identify "$visual_dir/app-shell.png"
color_count="$(identify -format '%k' "$visual_dir/app-shell.png")"
if (( color_count < 16 )); then
    echo "application capture appears blank ($color_count colors)" >&2
    exit 1
fi
# Exercise the registry-backed command palette: open, type a fuzzy query,
# run the selected command with Enter, and capture each stage. xdotool key
# events need window focus, which can race with activation; retry the chord.
palette_opened=false
for _ in $(seq 1 3); do
    xdotool windowactivate --sync "$window_id"
    sleep 0.5
    xdotool key --clearmodifiers ctrl+p
    sleep 0.5
    import -window "$window_id" "$visual_dir/app-palette.png"
    if ! cmp -s "$visual_dir/app-shell.png" "$visual_dir/app-palette.png"; then
        palette_opened=true
        break
    fi
done
identify "$visual_dir/app-palette.png"
if [[ "$palette_opened" != true ]]; then
    echo "Ctrl+P did not change the rendered application pane" >&2
    exit 1
fi
xdotool type --delay 25 "svnote"
sleep 0.35
import -window "$window_id" "$visual_dir/app-palette-query.png"
xdotool key --clearmodifiers Return
sleep 0.35
import -window "$window_id" "$visual_dir/app-after-run.png"
identify "$visual_dir/app-after-run.png"
if cmp -s "$visual_dir/app-palette-query.png" "$visual_dir/app-after-run.png"; then
    echo "Enter in the palette did not run the selected command" >&2
    exit 1
fi
echo "application screenshots: $visual_dir/app-shell.png $visual_dir/app-palette.png $visual_dir/app-after-run.png"
