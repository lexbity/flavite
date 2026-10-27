# Lattice justfile — single entry point for every gate recipe (§10.3).
# A1: bootstrap gate (fmt + clippy + tests). A3 adds gc/clean. A4 promotes to full G0 + G1.

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

# Bootstrap gate (Phase 0.0 slice A1): fmt + clippy + tests.
# NOT full G0 — full G0 (with `arch check`) lands in A4 (§10.3).
gate: fmt clippy test
    @echo "Bootstrap gate passed (fmt + clippy + test). Full G0 arrives in A4."
