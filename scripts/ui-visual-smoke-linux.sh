#!/usr/bin/env bash
# Capture the deterministic iced graph fixture under Xvfb. This is deliberately
# separate from headless WGSL/device validation: it exercises a real window,
# compositor surface, and rendered frame.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [[ "$(uname -s)" != Linux ]]; then
    echo "ui visual smoke is Linux-only" >&2
    exit 2
fi

for tool in xvfb-run xdotool import identify; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "missing visual-smoke tool: $tool" >&2
        exit 2
    fi
done

target_dir="${CARGO_TARGET_DIR:-target}"
mkdir -p "$target_dir/ui-visual"
cargo build -p flavite-ui --example graph_shader_probe

env -u WAYLAND_DISPLAY WINIT_UNIX_BACKEND=x11 \
    "${target_dir}/debug/examples/graph_shader_probe" \
    >"$target_dir/ui-visual/app.log" 2>&1 &
app_pid=$!
cleanup() {
    kill "$app_pid" 2>/dev/null || true
    wait "$app_pid" 2>/dev/null || true
}
trap cleanup EXIT

window_id=""
for _ in $(seq 1 100); do
    window_id="$(xdotool search --onlyvisible --name '^Lattice Graph Visual Fixture$' 2>/dev/null | head -n 1 || true)"
    if [[ -n "$window_id" ]]; then
        break
    fi
    if ! kill -0 "$app_pid" 2>/dev/null; then
        cat "$target_dir/ui-visual/app.log" >&2
        echo "visual fixture exited before creating its window" >&2
        exit 1
    fi
    sleep 0.1
done

if [[ -z "$window_id" ]]; then
    cat "$target_dir/ui-visual/app.log" >&2
    echo "timed out waiting for the iced visual fixture window" >&2
    exit 1
fi

# Let the initial surface render before capturing it.
sleep 1
import -window "$window_id" "$target_dir/ui-visual/graph.png"
identify "$target_dir/ui-visual/graph.png"
color_count="$(identify -format '%k' "$target_dir/ui-visual/graph.png")"
if (( color_count < 2 )); then
    echo "captured visual fixture is blank (only $color_count color)" >&2
    exit 1
fi
echo "visual screenshot: $target_dir/ui-visual/graph.png"
