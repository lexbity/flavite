//! S5-T3 (A7): mask exclusion-scan complexity evidence (RFC-0011 S5).
//!
//! `math_ranges` queries the excluded-range list by binary search instead of
//! a linear scan, and bounds the unclosed `$$` forward scan. Evidence:
//! scaling a math-dense document (K unclosed `$$` openers) grows near-
//! linearly in K, not quadratically, and an adversarial 8 MiB doc stays
//! far below the old O(K·n) regime.
//!
//! Ignored release bench (advisory prints; relative regression guard only —
//! R10: no absolute gates in CI).

use flavite_syntax::mask::math_ranges;
use std::hint::black_box;
use std::time::Instant;

fn ms(duration: std::time::Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// A doc with `k` unclosed `$$` prefixes (the pathological case: each used
/// to scan to EOF).
fn math_dense(k: usize) -> String {
    let mut out = String::with_capacity(k * 64);
    for i in 0..k {
        out.push_str("$$ $$ mid ");
        if i % 7 == 0 {
            out.push_str("[[link]] #tag\n");
        } else {
            out.push_str("text\n");
        }
    }
    out.push_str("tail");
    out
}

#[test]
#[ignore = "perf budget; runs under just bench-edit (release, --ignored)"]
fn s5_t3_mask_excluded_scan_scales_with_k_not_n() {
    // Warm up.
    let small = math_dense(200);
    black_box(math_ranges(&small, &[]));

    let measure = |k: usize| -> f64 {
        let text = math_dense(k);
        let mut samples = Vec::with_capacity(30);
        for _ in 0..30 {
            let start = Instant::now();
            let ranges = math_ranges(&text, &[]);
            samples.push(start.elapsed());
            assert!(!ranges.is_empty() || k == 0);
        }
        samples.sort_unstable();
        ms(samples[samples.len() - 1])
    };

    let t100 = measure(100);
    let t2000 = measure(2000);
    let ratio = t2000 / t100;
    println!(
        "S5-T3 mask: t(100 opens)={t100:.4} ms, t(2000 opens)={t2000:.4} ms, ratio={ratio:.2}"
    );

    // 20× the openers must not cost 20×+ (the old O(K·n) scan would blow
    // past this by orders of magnitude since n grows with K too).
    assert!(
        ratio < 30.0,
        "mask scan grows super-linearly in K: {t100:.4} → {t2000:.4} ms (ratio {ratio:.2})"
    );

    // An 8 MiB adversarial doc with 2 000 unclosed openers must complete
    // (old worst case ≈ 2000 × 8 MiB = 16 GB of byte scanning).
    let mut huge = String::with_capacity(8 * 1024 * 1024 + 2048 * 8);
    for _ in 0..2048 {
        huge.push_str("$$ $$$$ $$ ");
    }
    while huge.len() < 8 * 1024 * 1024 {
        huge.push('x');
    }
    let start = Instant::now();
    let ranges = math_ranges(&huge, &[]);
    let elapsed = ms(start.elapsed());
    println!(
        "S5-T3 mask: 8 MiB / 2048 unclosed $$ openers = {elapsed:.4} ms, {r} ranges",
        r = ranges.len()
    );
    assert!(
        elapsed < 200.0,
        "8 MiB mask scan took {elapsed:.4} ms (bounded forward scan expected <200 ms)"
    );
}
