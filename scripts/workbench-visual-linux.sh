#!/usr/bin/env bash
# RFC-0006 §11 workbench visual runner: `--scenario NAME [--scenario NAME2 …]
# --backend gpu|canvas [--jobs N]`.
#
# Introduced with the first native workbench scenario (S04
# `graph-editor-loop`). Reuses the fixture/window/harness conventions of
# the RFC-0003 runner through `scripts/tests/workbench/common.py`, keeps
# vault/data roots isolated per run, honors the renderer backend choice,
# and requires both semantic readiness and paint for every check. Missing
# prerequisite tools fail with a nonzero, incomplete status — never a
# silent pass (AC-20).
#
# `--scenario` may be repeated (or given a comma-separated list): the
# scenarios then run CONCURRENTLY, each in its own background subshell with
# its own Xvfb window, vault, data dir, evidence dir and run log, bounded by
# `--jobs` (default 4). The binary is built ONCE up front and every scenario
# reuses it, so a batch pays the rebuild once instead of once per scenario.
# The runner exits nonzero if ANY scenario failed.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

scenarios=()
backend="gpu"
jobs=4
worker_scenario=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --worker)
            worker_scenario="$2"
            shift 2
            ;;
        --scenario)
            IFS=',' read -ra parts <<< "$2"
            for part in "${parts[@]}"; do
                [[ -n "$part" ]] && scenarios+=("$part")
            done
            shift 2
            ;;
        --backend) backend="$2"; shift 2 ;;
        --jobs) jobs="$2"; shift 2 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done
if [[ -n "$worker_scenario" ]]; then
    scenarios=("$worker_scenario")
fi
if (( ${#scenarios[@]} == 0 )); then
    echo "missing --scenario (implemented: graph-editor-loop note-regex-replace link-completion reference-workspace context-mode context-mode-history context-mode-feedback context-mode-budget context-mode-multiselect context-mode-setops context-mode-matches context-mode-failures context-workbench-acceptance probe-find probe-menu-open probe-menu-kb)" >&2
    exit 2
fi
for scenario in "${scenarios[@]}"; do
    case "$scenario" in
        graph-editor-loop|note-regex-replace|link-completion|reference-workspace|context-mode|context-mode-history|context-mode-feedback|context-mode-budget|context-mode-multiselect|context-mode-setops|context-mode-matches|context-mode-failures|context-workbench-acceptance|probe-find|probe-menu-open|probe-menu-kb) ;;
        *) echo "unsupported scenario: $scenario (implemented: graph-editor-loop note-regex-replace link-completion reference-workspace context-mode context-mode-history context-mode-feedback context-mode-budget context-mode-multiselect context-mode-setops context-mode-matches context-mode-failures context-workbench-acceptance probe-find probe-menu-open probe-menu-kb)" >&2; exit 2 ;;
    esac
done
case "$backend" in
    gpu|canvas) ;;
    *) echo "backend must be gpu or canvas (got: $backend)" >&2; exit 2 ;;
esac
if (( jobs < 1 )); then
    echo "--jobs must be at least 1 (got: $jobs)" >&2
    exit 2
fi
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
needs_ocr=0
for scenario in "${scenarios[@]}"; do
    if [[ "$scenario" == context-mode* ]]; then
        needs_ocr=1
    fi
done
if (( needs_ocr == 1 )) && ! command -v tesseract >/dev/null 2>&1; then
    echo "Context Mode paint verification requires tesseract" >&2
    exit 2
fi
python3 - <<'PY' || exit 2
try:
    import PIL  # noqa: F401
except ImportError:
    print("missing python module: Pillow (PIL) is required for pixel checks", file=__import__("sys").stderr)
    raise SystemExit(2)
PY

# Keep test pointer/keyboard ownership separate from the user's desktop —
# and each scenario fully isolated from every other: EVERY scenario runs
# under its own Xvfb display (xvfb-run -a in the dispatch below). Sharing
# one display across concurrent scenarios makes their windowfocus/typing
# calls fight over the X focus and lose keystrokes.
if ! command -v xvfb-run >/dev/null 2>&1; then
    echo "xvfb-run is required (each scenario runs under its own Xvfb display)" >&2
    exit 2
fi

target_dir="${CARGO_TARGET_DIR:-target}"
evidence_dir="$target_dir/workbench-evidence"
mkdir -p "$evidence_dir"

# The binary is built ONCE for the whole batch (and a no-op incremental
# build in worker mode): every scenario reuses it, so a parallel sweep pays
# the rebuild once instead of once per scenario, and N concurrent
# `cargo build`s never contend on the cargo lock.
cargo build -p flavite-ui --example app-demo --features ui-test-support
for scenario in "${scenarios[@]}"; do
    if [[ "$scenario" == "note-regex-replace" ]]; then
        cargo build -p flavite-text-search-worker
        cp "$target_dir/debug/flavite-text-search-worker" \
           "$target_dir/debug/examples/flavite-text-search-worker"
        break
    fi
done

# One scenario end to end. Runs as a background subshell: its own Xvfb
# window, vault, data dir, evidence dir and run log; the EXIT trap tears the
# app down and records the exit status for the parent's summary.
run_scenario() {
    local scenario="$1" backend="$2"
    local run_id="${scenario}-${backend}-$(date +%s)-$BASHPID"
    local run_dir="$evidence_dir/$run_id"
    mkdir -p "$run_dir"

    # Disposable fixture vault and fixture-only OS-data root (hermetic per
    # run).
    local vault_dir
    vault_dir="$(mktemp -d "$run_dir/vault.XXXXXX")"
    mkdir -p "$vault_dir/Systems"
    printf '# Alpha\n\nalpha body\n' >"$vault_dir/alpha.md"
    printf '# Beta\n\nbeta body\n' >"$vault_dir/beta.md"
    printf '# Gamma\n\ngamma body\n' >"$vault_dir/Systems/gamma.md"

    if [[ "$scenario" == "note-regex-replace" ]]; then
        printf '# Alpha\n\ncat scatter cat\n\ncatapult CAT\n' >"$vault_dir/alpha.md"
    elif [[ "$scenario" == "link-completion" ]]; then
        printf '%s\n' '---' 'title: Beta' 'aliases: [Rocket]' '---' '# Beta' '' 'beta body' >"$vault_dir/beta.md"
    fi

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

    local harness_log="$run_dir/harness.jsonl"
    : >"$harness_log"
    local app_log="$run_dir/app.log"
    # Everything the EXIT trap touches is a GLOBAL (defaulted here): the
    # trap fires after run_scenario returns, when function locals are out
    # of scope. In parent-parallel mode each background subshell gets its
    # own copy, so concurrent scenarios stay isolated.
    window_id=""
    app_pid=""
    scenario_status=0
    device_dir=""

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
        echo "$scenario_status" >"$evidence_dir/status-$scenario"
    }
    trap cleanup EXIT

    # Short absolute data dir (instance socket SUN_LEN); hermetic per run.
    device_dir="$(mktemp -d "${TMPDIR:-/tmp}/flavite-workbench-device.XXXXXXXX")"
    local launch_env=(
        -u WAYLAND_DISPLAY
        WINIT_UNIX_BACKEND=x11
        FLAVITE_VAULT="$vault_dir"
        FLAVITE_DATA_DIR="$device_dir"
        FLAVITE_GRAPH_BACKEND="$backend"
        FLAVITE_HARNESS_OUT_FD=3
        FLAVITE_HARNESS_AUTOSAVE_IDLE_SECS=60
    )
    # FEUX-0002 X01: the budget scenario runs under a shrunk retained-history
    # budget so eviction/truncation is reachable in a real session. The
    # scenario fails (as its seeded negative) if this override is absent.
    if [[ "$scenario" == "context-mode-budget" ]]; then
        launch_env+=(FLAVITE_HARNESS_HISTORY_BUDGET_BYTES=35000)
    fi
    env "${launch_env[@]}" "$target_dir/debug/examples/app-demo" "$vault_dir" \
        3>"$harness_log" >"$app_log" 2>&1 &
    app_pid=$!

    # Window discovery must be pid-exact: concurrent scenarios all present a
    # window titled "Flavite", so a name-only search could adopt another
    # run's window (and its keystrokes).
    for _ in $(seq 1 100); do
        window_id=""
        local win
        for win in $(xdotool search --onlyvisible --name '^Flavite$' 2>/dev/null || true); do
            if [[ "$(xdotool getwindowpid "$win" 2>/dev/null || true)" == "$app_pid" ]]; then
                window_id="$win"
                break
            fi
        done
        if [[ -n "$window_id" ]]; then break; fi
        if ! kill -0 "$app_pid" 2>/dev/null; then
            cat "$app_log" >&2
            echo "application exited before creating its window" >&2
            return 1
        fi
        sleep 0.1
    done
    if [[ -z "$window_id" ]]; then
        cat "$app_log" >&2
        echo "timed out waiting for the application window" >&2
        return 1
    fi

    # Boot readiness: shell mounted and the fixture catalog readable.
    local boot_deadline=$((SECONDS + 30))
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
            return 1
        fi
        if ! kill -0 "$app_pid" 2>/dev/null; then
            cat "$app_log" >&2
            echo "application exited during boot" >&2
            return 1
        fi
        sleep 0.1
    done

    # Scenario modules share the runner's argv contract:
    # (window, harness, output, vault, backend). The scenario's own output
    # is kept in the run dir (concurrent children must not interleave).
    python3 "scripts/tests/workbench/${scenario//-/_}.py" \
        "$window_id" "$harness_log" "$run_dir" "$vault_dir" "$backend" \
        2>&1 | tee "$run_dir/run.log" || scenario_status=$?

    if (( scenario_status != 0 )); then
        echo "workbench scenario $scenario ($backend) FAILED: $run_dir (log: $run_dir/run.log)" >&2
        return "$scenario_status"
    fi

    # Evidence manifest (§11): HEAD, lock hash and the dirty-source digest
    # using the existing visual-runner algorithm, stamped on pass only.
    python3 - "$run_dir" "$scenario" "$backend" <<'PY'
import hashlib, json, pathlib, subprocess, sys
run_dir, scenario, backend = sys.argv[1:4]
h = hashlib.sha256()
paths = set(subprocess.check_output(["git", "ls-files", "-co", "--exclude-standard", "-z"]).split(b"\0"))
for raw in sorted(p for p in paths if p):
    path = pathlib.Path(raw.decode())
    if not path.is_file() or not (
        str(path).startswith(("crates/", "vendor/", "scripts/", ".github/"))
        or str(path) in ("Cargo.lock", "Cargo.toml", "justfile")
    ):
        continue
    data = path.read_bytes()
    h.update(raw + b"\0" + str(len(data)).encode() + b"\0" + data)
build = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
lock = subprocess.check_output(["cargo", "xtask", "blake3", "Cargo.lock"], text=True)
lock = [line for line in lock.split() if len(line.strip()) == 64][-1].strip()
pathlib.Path(run_dir, "provenance.json").write_text(
    json.dumps(
        {
            "schema": 1,
            "build": build,
            "lock_blake3": lock,
            "worktree_sha256": h.hexdigest(),
            "scenario": scenario,
            "platform": "linux",
            "render_backend": backend,
            "input_evidence": "rendered_os_input",
            "fixture_seed": 6006,
        },
        indent=2,
    )
    + "\n"
)
PY

    echo "workbench scenario $scenario ($backend) passed: $run_dir"
}

# Worker mode (internal): one scenario, already built by the parent, under
# the Xvfb display xvfb-run allocated for this process.
if [[ -n "$worker_scenario" ]]; then
    run_scenario "$worker_scenario" "$backend"
    exit "$?"
fi

# Dispatch every scenario concurrently, bounded by --jobs. Each child runs
# under its OWN Xvfb display (xvfb-run -a): concurrent scenarios sharing one
# display fight over the X focus and lose typed keystrokes. Statuses land in
# per-scenario status files (a `wait -n` limiter would reap a job whose
# status the summary then cannot read).
first=1
for scenario in "${scenarios[@]}"; do
    while (( $(jobs -rp | wc -l) >= jobs )); do
        sleep 0.2
    done
    # Stagger the starts: each worker boots its own Xvfb and initializes
    # software rendering; simultaneous boots dropped the first typed
    # keystrokes under the CPU burst.
    if (( first )); then
        first=0
    else
        sleep 3
    fi
    echo "[dispatch] $scenario"
    xvfb-run -a -s "-screen 0 ${FLAVITE_VISUAL_SCREEN:-2560x1600x24}" \
        bash "$0" --worker "$scenario" --backend "$backend" &
done
wait

overall=0
for scenario in "${scenarios[@]}"; do
    st=1
    if [[ -f "$evidence_dir/status-$scenario" ]]; then
        st="$(<"$evidence_dir/status-$scenario")"
    fi
    if [[ "$st" != "0" ]]; then
        echo "[summary] FAIL $scenario" >&2
        overall=1
    fi
done
if (( overall == 0 )); then
    echo "[summary] all ${#scenarios[@]} scenario(s) passed"
fi
exit "$overall"
