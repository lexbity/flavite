# Lattice justfile — single entry point for every gate recipe (§10.3).
# A1: bootstrap gate (fmt + clippy + tests). A3: fuzz-smoke, setup, gc/clean.
# A4: promotes gate to full G0 (arch check) + establishes G1.

# Run rustfmt in check mode.
fmt:
    cargo fmt --all -- --check

# Apply rustfmt to all files.
fmt-fix:
    cargo fmt --all

# Run clippy with warnings escalated to errors.
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Run all workspace tests.
test:
    cargo test --workspace

# Type-check the workspace.
check:
    cargo check --workspace

# Bootstrap gate (Phase 0.0 A1–A3): fmt + clippy + tests.
# NOT full G0 — full G0 (with `arch check`) lands in A4 (§10.3).
gate: fmt clippy test
    @echo "Bootstrap gate passed (fmt + clippy + test). Full G0 arrives in A4."

# Run every fuzz target over checked-in seeds + generated input (§10.6).
# Discovers targets dynamically from fuzz/fuzz_targets/*.rs.
# 30 s per target locally; G1 overrides to 60 s.
fuzz-smoke:
    #!/usr/bin/env bash
    set -euo pipefail
    cd fuzz
    targets=$(ls fuzz_targets/*.rs | xargs -n1 basename | sed 's/\.rs$//')
    if [ -z "$targets" ]; then
        echo "No fuzz targets found in fuzz/fuzz_targets/"
        exit 0
    fi
    failed=0
    for target in $targets; do
        echo "=== fuzz-smoke: $target (30s) ==="
        if ! cargo +nightly fuzz run "$target" -- -max_total_time=30 -rss_limit_mb=4096; then
            echo "FUZZ SMOKE FAILED: $target"
            failed=1
        fi
    done
    exit $failed

# G1 variant: 60 s per target (§10.3).
fuzz-smoke-ci:
    #!/usr/bin/env bash
    set -euo pipefail
    cd fuzz
    targets=$(ls fuzz_targets/*.rs | xargs -n1 basename | sed 's/\.rs$//')
    if [ -z "$targets" ]; then
        echo "No fuzz targets found in fuzz/fuzz_targets/"
        exit 0
    fi
    failed=0
    for target in $targets; do
        echo "=== fuzz-smoke-ci: $target (60s) ==="
        if ! cargo +nightly fuzz run "$target" -- -max_total_time=60 -rss_limit_mb=4096; then
            echo "FUZZ SMOKE FAILED: $target"
            failed=1
        fi
    done
    exit $failed

# Developer onboarding (§10.8): install tools + nightly + pre-push hook. Idempotent.
setup:
    cargo xtask setup

# Full cleanup (§10.10): removes target/, fuzz/target/, fuzz/artifacts/,
# profiling intermediates. Pass --dry-run to preview without removing.
clean *args:
    cargo xtask clean {{args}}

# Post-gate garbage collection (§10.10): threshold-based, runs after every gate.
gc:
    cargo xtask gc
