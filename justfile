# Flavite justfile — single entry point for every gate recipe (§10.3).
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

# Nightly D14 latency probe (20k notes; Save p99 during VerifyContent).
reconcile-bench:
    cargo bench -p flavite-vault --bench reconcile_concurrency

# Architecture debt gates from RFC-0002 §7; zero matches are required.
lint-arch:
    #!/usr/bin/env bash
    set -euo pipefail
    check_absent() {
        local pattern="$1"
        shift
        local status=0
        rg -n --no-messages "$pattern" "$@" || status=$?
        if [ "$status" -eq 0 ]; then
            echo "lint-arch: forbidden match for $pattern" >&2
            exit 1
        elif [ "$status" -ne 1 ]; then
            echo "lint-arch: search failed for $pattern (status $status)" >&2
            exit "$status"
        fi
    }
    check_absent 'fn apply_attachment\b' crates
    check_absent 'open_buffers' crates/flavite-vault/src/session.rs
    check_absent 'not yet implemented' crates/flavite-vault/src/rename
    check_absent 'JournalRecovery\b' crates
    check_absent 'normalize_target_key' crates
    check_absent 'DirectoryMoveNotSupported' crates/flavite-vault/src/actor.rs
    check_absent 'Ambiguous' crates/flavite-editor/src/links.rs
    check_absent 'drift_found_total=' crates/flavite-api/src/doctor.rs
    check_absent '\[\.\.?[^]]*1024\]' crates/flavite-index/src
    check_absent 'nothing enforces' crates/flavite-api/src/capability.rs
    check_absent 'expected_identities' crates
    echo "lint-arch passed (RFC-0002 debt gates)."

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

# S4 search probe (PLAN-C C3, §9 S4): CJK bigram-tokenizer quality vs a plain
# pipeline, deferred commits under 1 s autosave load, `committed_generation`
# replay after a crash, and atomic checkpoint publication (D12 go/no-go on
# tantivy vs FTS5). CI re-runs the scaled-down (2k-note) self-check on Linux.
spike-s4-search *args:
    cargo run -p xtask --bin s4_search_probe -- {{args}}

# S4 DB probe (PLAN-C C4, §9 S4): WAL single-writer + read-pool behavior under
# reconcile load — read p99 during writes, write throughput, batch-apply and
# generation-bump timing, tantivy writer ops during reconcile, open-time
# generation check. Evidence for C7 (`pool.rs`, `tantivy.rs`) and C8 batch
# sizing. CI re-runs the scaled-down (2k-note) self-check on Linux.
spike-s4-db *args:
    cargo run -p xtask --bin s4_db_probe -- {{args}}

# G0 — local fast gate (§10.3): fmt + clippy + test + arch check. ≤ 5 min budget.
gate: fmt clippy test fault-tests lint-arch arch-check
    @just gc
    @echo "G0 passed (fmt + clippy + test + arch check)."

# Journal crash schedules are part of G0 so durability regressions fail locally.
fault-tests:
    cargo test -p flavite-vault --features fault journal::executor

# G1 — CI gate (§10.3): G0 + coverage floors + FaultFs/integration suites +
# doc build + deny/audit/shear + fuzz smoke 60 s + network-denied + MSRV.
ci-gate: gate
    @echo "=== G1: coverage ==="
    mkdir -p target/llvm-cov
    cargo llvm-cov --workspace --lcov --output-path target/llvm-cov/lcov.info
    cargo xtask cov-check --lcov target/llvm-cov/lcov.info
    @echo "=== G1: doc build ==="
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
    @echo "=== G1: cargo-deny ==="
    cargo deny check
    @echo "=== G1: cargo-audit ==="
    cargo audit
    @echo "=== G1: cargo-shear ==="
    cargo shear
    @echo "=== G1: fuzz smoke (60 s/target) ==="
    just fuzz-smoke-ci
    @echo "=== G1: network-denied ==="
    just network-denied
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

# Run maintained fuzz targets over generated input (§10.6).
# Rename planning remains excluded while its fuzz target is a prototype; smoke
# membership is intentionally listed here (fuzz/Cargo.toml still builds the
# prototype target). Smoke runs copy only tracked, unignored seeds into a
# fresh corpus so the large ignored corpora never load inside a smoke window.
# 30 s per target locally; G1 uses fuzz-smoke-ci (60 s).
fuzz-smoke:
    #!/usr/bin/env bash
    set -euo pipefail
    cd fuzz
    smoke_root="$(mktemp -d)"
    trap 'rm -rf "$smoke_root"' EXIT
    targets="image_limits parser path_safety scanner frontmatter_patcher parity workspace_layout link_insertion workspace_restore"
    failed=0
    for target in $targets; do
        corpus_dir="$smoke_root/$target"
        mkdir -p "$corpus_dir"
        while IFS= read -r -d '' seed; do
            cp "$seed" "$corpus_dir/$(basename "$seed")"
        done < <(git ls-files -z --cached --others --exclude-standard -- "corpus/$target")
        echo "=== fuzz-smoke: $target (30s) ==="
        if ! cargo +nightly fuzz run "$target" "$corpus_dir" -- -max_total_time=30 -rss_limit_mb=4096; then
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
    smoke_root="$(mktemp -d)"
    trap 'rm -rf "$smoke_root"' EXIT
    targets="image_limits parser path_safety scanner frontmatter_patcher parity workspace_layout link_insertion workspace_restore"
    failed=0
    for target in $targets; do
        corpus_dir="$smoke_root/$target"
        mkdir -p "$corpus_dir"
        while IFS= read -r -d '' seed; do
            cp "$seed" "$corpus_dir/$(basename "$seed")"
        done < <(git ls-files -z --cached --others --exclude-standard -- "corpus/$target")
        echo "=== fuzz-smoke-ci: $target (60s) ==="
        if ! cargo +nightly fuzz run "$target" "$corpus_dir" -- -max_total_time=60 -rss_limit_mb=4096; then
            echo "FUZZ SMOKE FAILED: $target"
            failed=1
        fi
    done
    exit $failed

# Developer onboarding (§10.8): install tools + nightly + pre-push hook. Idempotent.
setup:
    cargo xtask setup

# Headless GPU setup (PLAN-B §5, B2): install lavapipe so a CI runner with no
# graphics card can still create a wgpu device.
#
# `crates/flavite-ui/tests/shader_validation.rs` has two halves. The naga half
# runs everywhere; the *link* half needs a real driver, and with no adapter it
# can only skip. CI sets FLAVITE_REQUIRE_WGPU_ADAPTER=1 next to this recipe so
# that "skipped" becomes a hard failure — this recipe is what supplies the
# device that makes the requirement satisfiable. Idempotent, and a no-op
# where apt-get is absent (macOS/Windows runners never install it).
gpu-setup:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v apt-get >/dev/null 2>&1; then
        echo "gpu-setup: apt-get not found — nothing to install (non-Debian host)."
        exit 0
    fi
    sudo apt-get update
    sudo apt-get install -y --no-install-recommends mesa-vulkan-drivers vulkan-tools
    if command -v vulkaninfo >/dev/null 2>&1; then
        vulkaninfo --summary 2>/dev/null \
            | grep -iE "deviceName|apiVersion" \
            | sort -u \
            || echo "gpu-setup: vulkaninfo reported no devices."
    else
        echo "gpu-setup: vulkaninfo unavailable; the adapter requirement will surface the real error."
    fi

# Linux-only rendered UI smoke: real iced window under Xvfb + lavapipe, with a
# PNG emitted to target/ui-visual/ for human/agent inspection.
ui-visual-smoke:
    xvfb-run -a -s "-screen 0 1280x900x24" bash scripts/ui-visual-smoke-linux.sh

# Linux-only dependencies for the rendered UI smoke. Kept separate from
# gpu-setup so shader validation and visual window capture remain distinct.
ui-visual-setup:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v apt-get >/dev/null 2>&1; then
        echo "ui-visual-setup: apt-get not found; install Xvfb, xdotool, and ImageMagick manually."
        exit 2
    fi
    sudo apt-get update
    sudo apt-get install -y --no-install-recommends xvfb xdotool imagemagick

# Full cleanup (§10.10): removes target/, fuzz/target/, fuzz/artifacts/,
# profiling intermediates. Pass --dry-run to preview without removing.
# RFC-0003 §16.1: headless workspace gates. During partial implementation
# the profile argument limits scope to capabilities not yet shipped; it can
# never disable a completed slice's gate (W01 runs additively inside W02+).
workspace-gate *profile="W02":
    #!/usr/bin/env bash
    set -euo pipefail
    bash scripts/check-workspace-contracts.sh "{{profile}}"
    cargo test -p flavite-workspace
    cargo test -p flavite-ui --test workspace_layout
    cargo test -p flavite-ui --test runtime_accessibility
    cargo test -p flavite-api --test open_flights
    if [ "{{profile}}" = "W03" ]; then
        cargo test -p flavite-editor
        cargo test -p flavite-ui --test shared_views
    fi
    if [ "{{profile}}" = "W04" ]; then
        cargo test -p flavite-editor
        cargo test -p flavite-ui --test shared_views
        cargo test -p flavite-workspace --test context_properties
        cargo test -p flavite-ui --test context_routing
        cargo test -p flavite-api --test query_service
        cargo test -p flavite-api --test host_boundary
    fi
    if [ "{{profile}}" = "W05" ]; then
        cargo test -p flavite-editor
        cargo test -p flavite-ui --test shared_views
        cargo test -p flavite-workspace --test context_properties
        cargo test -p flavite-ui --test context_routing
        cargo test -p flavite-api --test query_service
        cargo test -p flavite-api --test host_boundary
        cargo test -p flavite-syntax --test link_insert_properties
        cargo test -p flavite-api --test semantic_links
        cargo test -p flavite-ui --test link_target
    fi
    if [ "{{profile}}" = "W06" ]; then
        cargo test -p flavite-editor
        cargo test -p flavite-ui --test shared_views
        cargo test -p flavite-workspace --test context_properties
        cargo test -p flavite-ui --test context_routing
        cargo test -p flavite-api --test query_service
        cargo test -p flavite-api --test host_boundary
        cargo test -p flavite-syntax --test link_insert_properties
        cargo test -p flavite-api --test semantic_links
        cargo test -p flavite-ui --test link_target
        cargo test -p flavite-workspace --test workspace_codec
        cargo test -p flavite-api --test workspace_storage
        cargo test -p flavite-ui --test workspace_restore
    fi

# RFC-0003 §31.1: real Linux window input, semantic checkpoints, screenshots,
# and an evidence manifest.
workspace-visual *scenario="layout":
    bash scripts/workspace-visual-linux.sh --scenario {{scenario}}

# RFC-0003 §31.1: native backend IME/a11y check runner; an absent required
# backend returns incomplete/nonzero.
workspace-native:
    bash scripts/workspace-native-accessibility-linux.sh

clean *args:
    cargo xtask clean {{args}}

# Post-gate garbage collection (§10.10): threshold-based, runs after every gate.
gc:
    cargo xtask gc
