#!/usr/bin/env bash
# RFC-0006 §11 workbench visual runner: `--scenario NAME --backend gpu|canvas`.
#
# Introduced with the first native workbench scenario (S04
# `graph-editor-loop`). Reuses the fixture/window/harness conventions of
# the RFC-0003 runner through `scripts/tests/workbench/common.py`, keeps
# vault/data roots isolated per run, honors the renderer backend choice,
# and requires both semantic readiness and paint for every check. Missing
# prerequisite tools fail with a nonzero, incomplete status — never a
# silent pass (AC-20).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

scenario=""
backend="gpu"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --scenario) scenario="$2"; shift 2 ;;
        --backend) backend="$2"; shift 2 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done
case "$backend" in
    gpu|canvas) ;;
    *) echo "backend must be gpu or canvas (got: $backend)" >&2; exit 2 ;;
esac
case "$scenario" in
    graph-editor-loop) ;;
    "") echo "missing --scenario (implemented: graph-editor-loop)" >&2; exit 2 ;;
    *) echo "unsupported scenario: $scenario (implemented: graph-editor-loop)" >&2; exit 2 ;;
esac
if [[ "$(uname -s)" != Linux ]]; then
    echo "workbench visual runner is Linux-only" >&2
    exit 2
fi

for tool in xdotool import identify python3; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "missing workbench-visual tool: $tool" >&2
        exit 2
    fi
done
python3 - <<'PY' || exit 2
try:
    import PIL  # noqa: F401
except ImportError:
    print("missing python module: Pillow (PIL) is required for pixel checks", file=__import__("sys").stderr)
    raise SystemExit(2)
PY

# Keep test pointer/keyboard ownership separate from the user's desktop.
if [[ "${FLAVITE_XVFB_ACTIVE:-0}" != 1 ]] && command -v xvfb-run >/dev/null 2>&1; then
    exec xvfb-run -a -s "-screen 0 ${FLAVITE_VISUAL_SCREEN:-2560x1600x24}" \
        env FLAVITE_XVFB_ACTIVE=1 bash "$0" --scenario "$scenario" --backend "$backend"
fi
if [[ -n "${DISPLAY:-}" ]] && ! xdotool getdisplaygeometry >/dev/null 2>&1; then
    echo "DISPLAY=${DISPLAY} is not reachable; retrying with Xvfb if available." >&2
    if [[ "${FLAVITE_XVFB_ACTIVE:-0}" == 1 ]]; then
        echo "Xvfb started by this runner is not reachable; refusing recursive relaunch." >&2
        exit 2
    fi
    unset DISPLAY
fi
if [[ -z "${DISPLAY:-}" ]]; then
    if ! command -v xvfb-run >/dev/null 2>&1; then
        echo "DISPLAY is unset and xvfb-run is unavailable" >&2
        exit 2
    fi
    exec xvfb-run -a -s "-screen 0 ${FLAVITE_VISUAL_SCREEN:-2560x1600x24}" \
        env FLAVITE_XVFB_ACTIVE=1 bash "$0" --scenario "$scenario" --backend "$backend"
fi

target_dir="${CARGO_TARGET_DIR:-target}"
evidence_dir="$target_dir/workbench-evidence"
mkdir -p "$evidence_dir"
run_id="${scenario}-${backend}-$(date +%s)-$$"
run_dir="$evidence_dir/$run_id"
mkdir -p "$run_dir"

# Disposable fixture vault and fixture-only OS-data root (hermetic per run).
vault_dir="$(mktemp -d "$run_dir/vault.XXXXXX")"
mkdir -p "$vault_dir/Systems"
printf '# Alpha\n\nalpha body\n' >"$vault_dir/alpha.md"
printf '# Beta\n\nbeta body\n' >"$vault_dir/beta.md"
printf '# Gamma\n\ngamma body\n' >"$vault_dir/Systems/gamma.md"

python3 - "$vault_dir" "$run_dir/expected-fixture.json" <<'PY'
import hashlib,json,pathlib,sys
root=pathlib.Path(sys.argv[1])
files={str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(root.rglob('*.md'))}
pathlib.Path(sys.argv[2]).write_text(json.dumps({'schema':1,'fixture_seed':6006,'scenario':sys.argv[3] if len(sys.argv)>3 else '', 'files_sha256':files},indent=2)+'\n')
PY
python3 - "$run_dir/expected-fixture.json" "$scenario" <<'PY'
import json,sys,pathlib
# Record the scenario name into the fixture manifest for provenance.
p=pathlib.Path(sys.argv[1]); doc=json.loads(p.read_text()); doc["scenario"]=sys.argv[2]; p.write_text(json.dumps(doc,indent=2)+"\n")
PY

cargo build -p flavite-ui --example app-demo --features ui-test-support

harness_log="$run_dir/harness.jsonl"
: >"$harness_log"
app_log="$run_dir/app.log"
window_id=""
app_pid=""

cleanup() {
    if [[ -n "$window_id" ]]; then
        xdotool windowclose "$window_id" 2>/dev/null || true
    fi
    if [[ -n "$app_pid" ]]; then
        for _ in $(seq 1 30); do
            kill -0 "$app_pid" 2>/dev/null || break
            sleep 0.1
        done
        kill "$app_pid" 2>/dev/null || true
        wait "$app_pid" 2>/dev/null || true
    fi
    rm -rf "$device_dir" 2>/dev/null || true
}
trap cleanup EXIT

# Short absolute data dir (instance socket SUN_LEN); hermetic per run.
device_dir="$(mktemp -d "${TMPDIR:-/tmp}/flavite-workbench-device.XXXXXXXX")"
env -u WAYLAND_DISPLAY WINIT_UNIX_BACKEND=x11 \
    FLAVITE_VAULT="$vault_dir" \
    FLAVITE_DATA_DIR="$device_dir" \
    FLAVITE_GRAPH_BACKEND="$backend" \
    FLAVITE_HARNESS_OUT_FD=3 \
    FLAVITE_HARNESS_AUTOSAVE_IDLE_SECS=60 \
    "$target_dir/debug/examples/app-demo" "$vault_dir" \
    3>"$harness_log" >"$app_log" 2>&1 &
app_pid=$!

for _ in $(seq 1 100); do
    window_id="$(xdotool search --onlyvisible --name '^Flavite$' 2>/dev/null | head -n 1 || true)"
    if [[ -n "$window_id" ]]; then break; fi
    if ! kill -0 "$app_pid" 2>/dev/null; then
        cat "$app_log" >&2
        echo "application exited before creating its window" >&2
        exit 1
    fi
    sleep 0.1
done
if [[ -z "$window_id" ]]; then
    cat "$app_log" >&2
    echo "timed out waiting for the application window" >&2
    exit 1
fi

# Boot readiness: shell mounted and the fixture catalog readable.
boot_deadline=$((SECONDS + 30))
until HARNESS_FILE="$harness_log" python3 -c "
import json, sys
lines=[l for l in open(sys.argv[1]) if l.strip()]
if lines:
    s=json.loads(lines[-1])
    sys.exit(0 if s.get('groups') and s.get('catalog',0)>=3 else 1)
sys.exit(1)
" "$harness_log" 2>/dev/null; do
    if (( SECONDS >= boot_deadline )); then
        cat "$app_log" >&2
        echo "timed out waiting for workbench boot readiness" >&2
        exit 1
    fi
    if ! kill -0 "$app_pid" 2>/dev/null; then
        cat "$app_log" >&2
        echo "application exited during boot" >&2
        exit 1
    fi
    sleep 0.1
done

# Scenario modules share the runner's argv contract:
# (window, harness, output, vault, backend).
python3 "scripts/tests/workbench/${scenario//-/_}.py" \
    "$window_id" "$harness_log" "$run_dir" "$vault_dir" "$backend"

echo "workbench scenario $scenario ($backend) passed: $run_dir"
