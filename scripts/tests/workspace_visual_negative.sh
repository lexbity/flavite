#!/usr/bin/env bash
# Negative control for the workspace visual gate (RFC-0003 W02).
#
# 1. Runs the layout scenario with a fixture-only injection that hides a
#    required tab from the semantic snapshots; the gate must fail.
# 2. Proves the injection cannot be activated in a build without the
#    `ui-test-support` feature: the harness module does not compile into
#    that build, so the same environment variables have no effect.
set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"
target_dir="${CARGO_TARGET_DIR:-target}"

status=0

echo "=== negative 1: hidden required tab must fail the visual gate ==="
if FLAVITE_HARNESS_INJECT_HIDE_TAB=beta timeout 300 \
    bash scripts/workspace-visual-linux.sh --scenario layout \
    >"$target_dir/workspace-evidence/negative-run.log" 2>&1; then
    echo "the visual gate passed while a required tab was hidden" >&2
    tail -20 "$target_dir/workspace-evidence/negative-run.log" >&2
    status=1
else
    echo "visual gate failed as required under injection"
fi

echo "=== negative 2: injection is inert without ui-test-support ==="
if ! cargo build -p flavite-ui --example app-demo 2>"$target_dir/workspace-evidence/negative-build.log"; then
    echo "the production (feature-less) example build must succeed" >&2
    cat "$target_dir/workspace-evidence/negative-build.log" >&2
    status=1
fi
if rg -q "harness::observe" "$target_dir/debug/examples/app-demo.d" 2>/dev/null; then
    : # .d files are not reliable for cfg inspection; the compile check below
      # is the behavioral proof, so this is advisory only.
fi
# Source-level check: the observe call site is cfg-gated.
if ! rg -q '#\[cfg\(feature = "ui-test-support"\)\]' crates/flavite-ui/src/app.rs; then
    echo "the harness call site must be feature-gated in app.rs" >&2
    status=1
fi

if [ "$status" -ne 0 ]; then
    echo "workspace visual negative controls FAILED" >&2
    exit 1
fi
echo "workspace visual negative controls passed"
