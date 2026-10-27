//! Headless synthetic tests for the S1 caret geometry and hit-test precision probe
//! (slice A6; AC-24 instrument).
//!
//! These tests exercise the caret probe's measurement logic without a GPU or
//! window: cosmic-text shaping and hit-testing are CPU-only. CI runs these on
//! Linux, macOS, and Windows via `cargo test --workspace` (G0/G1).

use xtask::s1_caret_probe::{MAX_ROUNDTRIP_ERROR, SAMPLE_TEXT, SCALE_FACTORS, run_measurements};

/// Test that all scale factors pass the round-trip error bound.
///
/// This is the primary AC-24 test: round-trip error must be ≤ 0.5 px at every
/// scale factor (1.0, 1.25, 1.5, 2.0).
#[test]
fn caret_roundtrip_error_within_bound() {
    let results = run_measurements();
    for result in &results {
        assert!(
            result.passes,
            "Scale {:.2}x: max_error={:.4}px exceeds bound {:.1}px",
            result.scale, result.max_error, MAX_ROUNDTRIP_ERROR
        );
    }
}

/// Test that every scale factor produces non-zero metrics.
///
/// Regression test: if cosmic-text metrics computation changes, this ensures
/// we still get sensible values.
#[test]
fn metrics_are_sensible() {
    let results = run_measurements();
    for result in &results {
        assert!(result.font_size > 0.0, "font_size must be positive");
        assert!(result.line_height > 0.0, "line_height must be positive");
        assert!(
            result.font_size <= result.line_height,
            "font_size should not exceed line_height"
        );
    }
}

/// Test that the sample text is non-empty.
#[test]
fn sample_text_non_empty() {
    assert!(!SAMPLE_TEXT.is_empty(), "SAMPLE_TEXT must not be empty");
}

/// Test that all scale factors are in the expected range.
#[test]
fn scale_factors_in_range() {
    for &scale in SCALE_FACTORS {
        assert!(scale >= 1.0 && scale <= 2.0, "Scale {} out of range", scale);
    }
}

/// Test that scale factors are monotonically increasing.
#[test]
fn scale_factors_monotonic() {
    for i in 1..SCALE_FACTORS.len() {
        assert!(
            SCALE_FACTORS[i] > SCALE_FACTORS[i - 1],
            "Scale factors must be strictly increasing"
        );
    }
}

/// Test that we have the expected number of scale factors.
#[test]
fn expected_scale_factor_count() {
    assert_eq!(
        SCALE_FACTORS.len(),
        4,
        "Expected 4 scale factors (1.0, 1.25, 1.5, 2.0)"
    );
}

/// Test that every result has a valid scale factor.
#[test]
fn results_have_valid_scales() {
    let results = run_measurements();
    assert_eq!(
        results.len(),
        SCALE_FACTORS.len(),
        "Result count must match scale factor count"
    );

    for (result, &expected_scale) in results.iter().zip(SCALE_FACTORS.iter()) {
        assert_eq!(result.scale, expected_scale, "Scale factor mismatch");
    }
}

/// Test that the error bound is positive and reasonable.
#[test]
fn error_bound_is_reasonable() {
    assert!(MAX_ROUNDTRIP_ERROR > 0.0, "Error bound must be positive");
    assert!(
        MAX_ROUNDTRIP_ERROR <= 1.0,
        "Error bound of 0.5 px is the accepted spec value"
    );
}
