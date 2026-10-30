#!/usr/bin/env bash
# Native X11/AT-SPI round trip against the actual Flavite shell.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [[ "$(uname -s)" != Linux ]]; then
    echo "native AT-SPI smoke is Linux-only" >&2
    exit 2
fi
if [[ -z "${DISPLAY:-}" ]] || ! xdotool getdisplaygeometry >/dev/null 2>&1; then
    echo "a reachable X11 DISPLAY is required for this native AT-SPI smoke" >&2
    exit 2
fi
python3 -c 'import gi; gi.require_version("Atspi", "2.0"); from gi.repository import Atspi'
if command -v gdbus >/dev/null 2>&1; then
    atspi_enabled="$(gdbus call --session \
        --dest org.a11y.Bus \
        --object-path /org/a11y/bus \
        --method org.freedesktop.DBus.Properties.Get \
        org.a11y.Status IsEnabled 2>/dev/null || true)"
    if [[ "$atspi_enabled" != *true* ]]; then
        echo "AT-SPI IsEnabled is false; start a screen reader before this native test" >&2
        exit 2
    fi
fi

target_dir="${CARGO_TARGET_DIR:-target}"
run_root="$(mktemp -d "$target_dir/workspace-native-atspi.XXXXXX")"
vault_root="$run_root/vault"
mkdir -p "$vault_root" "$run_root/data" "$run_root/config" "$run_root/cache"
cp crates/flavite-ui/tests/fixtures/native-accessibility.md "$vault_root/demo.md"
cargo build -p flavite-ui --example app-demo

env -u WAYLAND_DISPLAY \
    WINIT_UNIX_BACKEND=x11 \
    FLAVITE_VAULT="$vault_root" \
    XDG_DATA_HOME="$run_root/data" \
    XDG_CONFIG_HOME="$run_root/config" \
    XDG_CACHE_HOME="$run_root/cache" \
    "$target_dir/debug/examples/app-demo" "$vault_root" \
    >"$run_root/app.log" 2>&1 &
app_pid=$!
cleanup() {
    kill "$app_pid" 2>/dev/null || true
    wait "$app_pid" 2>/dev/null || true
}
trap cleanup EXIT

window_id=""
for _ in $(seq 1 100); do
    if ! kill -0 "$app_pid" 2>/dev/null; then
        echo "Flavite exited before creating its window; log: $run_root/app.log" >&2
        cat "$run_root/app.log" >&2
        exit 1
    fi
    window_id="$(xdotool search --onlyvisible --name '^Flavite$' 2>/dev/null | head -n 1 || true)"
    [[ -n "$window_id" ]] && break
    sleep 0.1
done
if [[ -z "$window_id" ]]; then
    echo "Flavite window did not become visible; log: $run_root/app.log" >&2
    cat "$run_root/app.log" >&2
    exit 1
fi

if ! python3 scripts/workspace-native-accessibility-linux.py "$vault_root"; then
    echo "native AT-SPI runner failed; app log: $run_root/app.log" >&2
    cat "$run_root/app.log" >&2
    exit 1
fi
