#!/usr/bin/env bash
# Visual workspace runner (RFC-0003 §31.1, W02 `--scenario layout`).
#
# Starts the actual shell under a real native window, injects real OS
# keyboard input, waits for semantic readiness of the workspace harness
# (never sleeps), asserts structure, and captures screenshots. The fixture
# binary is built only with `ui-test-support`; the production build cannot
# enable the harness (RFC-0003 §16.2).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

scenario="layout"
backend="x11"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --scenario) scenario="$2"; shift 2 ;;
        --backend) backend="$2"; shift 2 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done
if [[ "$scenario" != "layout" && "$scenario" != "shared-editors" ]]; then
    echo "unsupported scenario: $scenario (implemented: layout, shared-editors)" >&2
    exit 2
fi
if [[ "$(uname -s)" != Linux ]]; then
    echo "workspace visual runner is Linux-only" >&2
    exit 2
fi

for tool in xdotool import identify python3; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "missing workspace-visual tool: $tool" >&2
        exit 2
    fi
done
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
    exec xvfb-run -a -s "-screen 0 ${FLAVITE_VISUAL_SCREEN:-1440x900x24}" \
        env FLAVITE_XVFB_ACTIVE=1 bash "$0" --scenario "$scenario" --backend "$backend"
fi

target_dir="${CARGO_TARGET_DIR:-target}"
evidence_dir="$target_dir/workspace-evidence"
mkdir -p "$evidence_dir"
run_id="${scenario}-$(date +%s)-$$"
run_dir="$evidence_dir/$run_id"
mkdir -p "$run_dir"

# Disposable fixture vault and fixture-only OS-data root.
vault_dir="$(mktemp -d "$run_dir/vault.XXXXXX")"
mkdir -p "$vault_dir/Systems"
printf '# Alpha\n\nalpha body\n' >"$vault_dir/alpha.md"
printf '# Beta\n\nbeta body\n' >"$vault_dir/beta.md"
printf '# Gamma\n\ngamma body\n' >"$vault_dir/Systems/gamma.md"

cargo build -p flavite-ui --example app-demo --features ui-test-support

harness_log="$run_dir/harness.jsonl"
: >"$harness_log"
app_log="$run_dir/app.log"
window_id=""

cleanup() {
    if [[ -n "$window_id" ]]; then
        xdotool windowclose "$window_id" 2>/dev/null || true
    fi
    for _ in $(seq 1 30); do
        kill -0 "$app_pid" 2>/dev/null || break
        sleep 0.1
    done
    kill "$app_pid" 2>/dev/null || true
    wait "$app_pid" 2>/dev/null || true
}
trap cleanup EXIT

env -u WAYLAND_DISPLAY WINIT_UNIX_BACKEND=x11 \
    FLAVITE_VAULT="$vault_dir" \
    FLAVITE_HARNESS_OUT_FD=3 \
    "$target_dir/debug/examples/app-demo" \
    3>"$harness_log" >"$app_log" 2>&1 &
app_pid=$!

# Semantic harness reader: evaluate a python predicate over the last
# harness snapshot. Never sleeps on its own; the poll loop bounds it.
harness_wait() {
    local description="$1" predicate="$2"
    local deadline=$((SECONDS + 30))
    while (( SECONDS < deadline )); do
        if [[ -n "$window_id" ]] && ! kill -0 "$app_pid" 2>/dev/null; then
            break
        fi
        if HARNESS_FILE="$harness_log" python3 -c "
import json, sys
lines = [line for line in open(sys.argv[1]) if line.strip()]
if lines:
    snapshot = json.loads(lines[-1])
    sys.exit(0 if eval(sys.argv[2]) else 1)
else:
    sys.exit(1)
" "$harness_log" "$predicate" 2>/dev/null; then
            return 0
        fi
        if ! kill -0 "$app_pid" 2>/dev/null; then
            cat "$app_log" >&2
            echo "application exited before $description" >&2
            return 1
        fi
        sleep 0.1
    done
    cat "$app_log" >&2
    tail -5 "$harness_log" >&2 || true
    echo "timed out waiting for $description" >&2
    return 1
}

xdotool_key() {
    # Xvfb may run without a window manager; activate via EWMH then fails
    # even though the mapped app window is focusable.
    xdotool windowfocus --sync "$window_id"
    xdotool key --clearmodifiers "$1"
    sleep 0.3
}

capture() {
    for _ in $(seq 1 50); do
        import -window "$window_id" "$1"
        identify "$1" >/dev/null
        local colors
        colors="$(identify -format '%k' "$1")"
        if (( colors >= 16 )); then
            return 0
        fi
        sleep 0.1
    done
    echo "capture appears blank after waiting for paint: $1 ($colors colors)" >&2
    return 1
}

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

checks=()
record() {
    checks+=("{\"id\": \"$1\", \"outcome\": \"pass\", \"artifact\": \"$2\"}")
    echo "check passed: $1"
}

# 1. Boot: shell ready, one group, catalog loaded from the fixture vault.
harness_wait "boot with the catalog loaded" \
    'snapshot["groups"] and snapshot["catalog"] >= 3'
capture "$run_dir/01-boot.png"
record "boot-single-group-catalog" "01-boot.png"

if [[ "$scenario" == "shared-editors" ]]; then
    xdotool_key "ctrl+p"
    xdotool type --delay 25 "alpha"
    xdotool_key "Return"
    harness_wait "alpha editor is ready" \
        'any(t["load"] == "ready" and t["title"] == "alpha" for g in snapshot["groups"] for t in g["tabs"]) and snapshot["focused_text_chars"] > 0'
    for _ in $(seq 1 8); do
        if python3 -c '
import json, sys
lines=[line for line in open(sys.argv[1]) if line.strip()]
s=json.loads(lines[-1]) if lines else {}
active=[t for g in s.get("groups", []) for t in g["tabs"] if t["active"]]
sys.exit(0 if len(active)==1 and active[0]["title"]=="alpha" else 1)
' "$harness_log"; then
            break
        fi
        xdotool_key "ctrl+Tab"
    done
    harness_wait "alpha is the active editor tab" \
        'any(t["active"] and t["title"] == "alpha" for g in snapshot["groups"] for t in g["tabs"])'
    xdotool windowfocus --sync "$window_id"
    xdotool mousemove --window "$window_id" 700 300 click 1
    harness_wait "OS pointer focuses the actual editor widget" \
        'snapshot["writing_target"] is not None and snapshot["writing_target"]["tab"] == snapshot["focused_tab"]'
    capture "$run_dir/02-editor-focused.png"
    # One committed edit at a time keeps the real UI event loop's revision
    # acknowledgements between OS key events.
    xdotool type --clearmodifiers --delay 100 "w03-marker"
    harness_wait "OS typing changes the real note buffer" \
        '"w03-marker" in snapshot["focused_text"]'
    capture "$run_dir/03-typed-note.png"
    record "os-typed-note-buffer" "03-typed-note.png"

    xdotool_key "ctrl+alt+Right"
    harness_wait "split creates two views of the same note buffer" \
        'len(snapshot["groups"]) == 2 and all(any(t["title"] == "alpha" for t in g["tabs"]) for g in snapshot["groups"])'
    python3 -c '
import json, sys, time
path=sys.argv[1]
deadline=time.monotonic()+10
while time.monotonic()<deadline:
    lines=[line for line in open(path) if line.strip()]
    if lines:
        s=json.loads(lines[-1])
        tabs=[t for g in s["groups"] for t in g["tabs"] if t["title"]=="alpha"]
        if len(tabs)==2 and all(t["buffer"] is not None for t in tabs):
            assert tabs[0]["buffer"]==tabs[1]["buffer"], tabs
            assert tabs[0]["revision"]==tabs[1]["revision"], tabs
            break
    time.sleep(.05)
else:
    raise SystemExit("two editor views did not converge on one buffer snapshot")
' "$harness_log"
    capture "$run_dir/04-split-before-carets.png"
    # The original view retains the OS-typed caret while the deliberate
    # duplicate starts with an independent caret. The reducer's UI test
    # covers subsequent per-view caret updates; this OS scenario proves the
    # actual shell reparent/split preserves both acknowledged positions.
    python3 -c '
import json, sys
for line in reversed([line for line in open(sys.argv[1]) if line.strip()]):
    try:
        s=json.loads(line)
        break
    except json.JSONDecodeError:
        continue
else:
    raise SystemExit("no complete semantic snapshot")
tabs=[t for g in s["groups"] for t in g["tabs"] if t["title"]=="alpha"]
assert len(tabs)==2 and tabs[0]["selection"] != tabs[1]["selection"], tabs
assert s["writing_target"] and s["writing_target"]["tab"]==s["document_anchor"]
' "$harness_log"
    capture "$run_dir/05-independent-carets.png"
    record "shared-buffer-independent-carets" "05-independent-carets.png"
else

# 2. Quick switch opens a real note through an async open flight.
xdotool_key "ctrl+p"
xdotool type --delay 25 "alpha"
xdotool_key "Return"
harness_wait "alpha note ready with source text" \
    'snapshot["groups"][0]["tabs"] and any(t["load"] == "ready" and t["title"] == "alpha" for t in snapshot["groups"][0]["tabs"])'
harness_wait "focused note exposes source text" \
    'snapshot["focused_text_chars"] > 0'
capture "$run_dir/02-note-open.png"
record "quick-switch-opens-note" "02-note-open.png"

# 3. New empty tab in the focused group.
xdotool_key "ctrl+t"
harness_wait "new empty tab" \
    'sum(len(g["tabs"]) for g in snapshot["groups"]) >= 2'
capture "$run_dir/03-new-tab.png"
record "keyboard-new-tab" "03-new-tab.png"

# 4. Split right: two groups (splitting duplicates the focused Empty view).
xdotool_key "ctrl+alt+Right"
harness_wait "split right creates a second group" \
    'len(snapshot["groups"]) == 2 and len(snapshot["groups"][0]["tabs"]) == 3 and len(snapshot["groups"][1]["tabs"]) == 1'
capture "$run_dir/04-split.png"
record "keyboard-split-right" "04-split.png"

# 5. Move a tab from the first group to the next group using the documented
# host shortcut and real OS keyboard input.
xdotool_key "shift+F6"
xdotool_key "ctrl+alt+shift+Right"
harness_wait "keyboard move transfers a tab to the next group" \
    'len(snapshot["groups"]) == 2 and len(snapshot["groups"][0]["tabs"]) == 2 and len(snapshot["groups"][1]["tabs"]) == 2'
capture "$run_dir/05-moved-tab.png"
record "keyboard-move-tab-between-groups" "05-moved-tab.png"

# 6. Open beta, close it, and reopen it.
xdotool_key "ctrl+p"
xdotool type --delay 25 "beta"
xdotool_key "Return"
harness_wait "beta ready in some group" \
    'any(t["load"] == "ready" and t["title"] == "beta" for g in snapshot["groups"] for t in g["tabs"])'
xdotool_key "ctrl+w"
harness_wait "beta tab closed" \
    'not any(t["title"] == "beta" for g in snapshot["groups"] for t in g["tabs"])'
xdotool_key "ctrl+shift+t"
harness_wait "reopen restores beta" \
    'any(t["title"] == "beta" and t["load"] == "ready" for g in snapshot["groups"] for t in g["tabs"])'
capture "$run_dir/06-reopen.png"
record "close-reopen-tab" "06-reopen.png"

# 7. Maximize and restore the focused group (manual maximize).
xdotool_key "ctrl+shift+m"
harness_wait "manual maximize engaged" \
    'snapshot["maximized"] is not None'
capture "$run_dir/07-maximized.png"
record "manual-maximize" "07-maximized.png"
xdotool_key "ctrl+shift+m"
harness_wait "restore clears manual maximize" \
    'snapshot["maximized"] is None'
record "manual-restore" "07-maximized.png"

# 8. Final capture and non-blank assertions for every artifact.
capture "$run_dir/08-final.png"
record "final-capture" "08-final.png"
for image in "$run_dir"/*.png; do
    color_count="$(identify -format '%k' "$image")"
    if (( color_count < 16 )); then
        echo "capture appears blank: $image ($color_count colors)" >&2
        exit 1
    fi
done
fi

# Duplicate-id guard: tab identities inside the harness are unique.
python3 -c "
import json, sys
for line in reversed([line for line in open(sys.argv[1]) if line.strip()]):
    try:
        snapshot = json.loads(line)
        break
    except json.JSONDecodeError:
        continue
else:
    raise SystemExit('no complete semantic snapshot')
tabs = [t['tab'] for g in snapshot['groups'] for t in g['tabs']]
assert len(tabs) == len(set(tabs)), f'duplicate tab ids: {tabs}'
groups = [g['group'] for g in snapshot['groups']]
assert len(groups) == len(set(groups)), 'duplicate group ids'
" "$harness_log" || { echo "semantic identity check failed" >&2; exit 1; }
record "semantic-ids-unique" "harness.jsonl"

# Evidence manifest (§31.2).
build_hash="$(git rev-parse HEAD 2>/dev/null || echo unknown)"
lock_hash="$(cargo xtask blake3 Cargo.lock)"
{
    echo '{'
    echo "  \"schema\": 1,"
    echo "  \"build\": \"$build_hash\","
    echo "  \"lock_blake3\": \"$lock_hash\","
    echo "  \"scenario\": \"$scenario\","
    echo "  \"platform\": \"linux\","
    echo "  \"display_backend\": \"$backend\","
    echo "  \"render_backend\": \"gpu-or-software\","
    echo "  \"input_evidence\": \"rendered_os_input\","
    echo "  \"fixture_seed\": null,"
    echo "  \"run_id\": \"$run_id\","
    echo "  \"checks\": ["
    joined="$(IFS=,; echo "${checks[*]}")"
    echo "    ${joined//\{\"id/{\"id}"
    echo "  ],"
    echo "  \"unmet\": []"
    echo '}'
} >"$evidence_dir/manifest.json"

echo "workspace visual run complete: $run_dir"
echo "manifest: $evidence_dir/manifest.json"
