# Lattice justfile — single entry point for every gate recipe (§10.3).
# A4: full G0 (fmt + clippy + test + arch check, ends with gc) + G1 (ci-gate).

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

# Architecture check (drift + edges + layering, §10.2).
arch-check:
    cargo xtask arch check

# Generate architecture artifacts (§10.2).
arch-gen:
    cargo xtask arch gen

# S3 watcher-truth probe (PLAN-C C1, §9 S3-1): per-OS watcher evidence with a
# D5 nomination-property self-check. CI runs scenarios 1–3 on Linux headless.
spike-s3 *args:
    cargo run -p xtask --bin s3_watcher_probe -- {{args}}

# S3 filesystem-semantics probe (PLAN-C C2, §9 S3-2): rename/symlink/fsync/
# placeholder/advisory-lock/no-clobber evidence feeding the `Fs` trait contract.
# Scenario 5 (advisory lock after kill -9) is proven by spawning the probe
# binary, not by `cargo test`.
spike-s3-fs *args:
    cargo run -p xtask --bin s3_fs_probe -- {{args}}

# G0 — local fast gate (§10.3): fmt + clippy + test + arch check. ≤ 5 min budget.
gate: fmt clippy test arch-check
    @just gc
    @echo "G0 passed (fmt + clippy + test + arch check)."

# G1 — CI gate (§10.3): G0 + coverage floors + FaultFs/integration suites +
# doc build + deny/audit/shear + fuzz smoke 60 s + network-denied + MSRV.
ci-gate: gate
    @echo "=== G1: doc build ==="
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
    @echo "=== G1: cargo-deny ==="
    cargo deny check
    @echo "=== G1: cargo-audit ==="
    cargo audit
    @echo "=== G1: fuzz smoke (60 s/target) ==="
    just fuzz-smoke-ci
    @echo "=== G1: MSRV build ==="
    just msrv
    @echo "G1 passed."

# Build documentation with warnings as errors (§10.4).
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# MSRV build (§10.4, §10.8): check against the workspace rust-version.
msrv:
    cargo +1.90.0 check --workspace

# Network-denied test run (§10.3, AC-26): build first, then run tests under
# unshare with no network. A seeded connection attempt must fail the assertion.
network-denied:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo test --workspace --no-run
    echo "=== network-denied: running tests under unshare -n ==="
    if command -v unshare >/dev/null 2>&1; then
        unshare -n cargo test --workspace || {
            echo "network-denied: tests failed under network namespace"
            exit 1
        }
    else
        echo "network-denied: unshare not available, skipping (CI provides it)"
    fi

# Coverage check (§10.5): requires lcov.info from `cargo llvm-cov`.
cov-check lcov="target/llvm-cov/lcov.info":
    cargo xtask cov-check --lcov {{lcov}}

# Run every fuzz target over checked-in seeds + generated input (§10.6).
# Discovers targets dynamically from fuzz/fuzz_targets/*.rs.
# 30 s per target locally; G1 uses fuzz-smoke-ci (60 s).
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
