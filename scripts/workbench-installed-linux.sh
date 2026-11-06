#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
for tool in xvfb-run xdotool import identify python3; do
    command -v "$tool" >/dev/null || { echo "incomplete: missing $tool" >&2; exit 2; }
done
cargo build -p flavite-app -p flavite-cli -p flavite-text-search-worker
output="target/workbench-installed/$(date +%s)-$$"
mkdir -p "$output"
xvfb-run -a -s '-screen 0 1440x900x24' python3 scripts/tests/workbench/installed_artifacts.py "$output"
echo "installed production workbench passed: $output"
