#![no_main]

//! Fuzz target: `image_limits` — hostile images through `image::Limits` (§10.6, §4.10).
//!
//! Invariants: no OOM, no panic (decompression-bomb regression). Limits:
//! ≤ 64 MP total pixels, ≤ 64 MB decoded allocation.

use image::{ImageReader, Limits};
use libfuzzer_sys::fuzz_target;
use std::io::Cursor;

/// Maximum total pixels (§4.10): 64 megapixels.
const MAX_PIXELS: u64 = 64_000_000;

/// Maximum decoded allocation (§4.10): 64 MB.
const MAX_ALLOC: u64 = 64 * 1024 * 1024;

/// Maximum input size for this target: 64 MB (§10.6 budget enforcement).
const MAX_INPUT: usize = 64 * 1024 * 1024;

/// Conservative per-dimension cap derived from the total-pixel budget.
/// 8192 × 8192 = 67,108,864 ≈ 64 MP.
const MAX_DIMENSION: u32 = 8192;

fuzz_target!(|data: &[u8]| {
    // Budget enforcement (§10.6): reject oversized inputs as non-findings.
    if data.len() > MAX_INPUT {
        return;
    }

    // Configure limits: total pixels + allocation cap (§4.10).
    // `Limits` is #[non_exhaustive]; start from default and override fields.
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_ALLOC);

    // Guess the format and decode with limits.
    let cursor = Cursor::new(data);
    let mut reader = match ImageReader::new(cursor).with_guessed_format() {
        Ok(r) => r,
        Err(_) => return, // Format detection failure is not a finding.
    };
    reader.limits(limits);

    // Decode with limits. Invariants: no OOM, no panic.
    // A decode error is fine — we are testing that limits prevent bombs.
    match reader.decode() {
        Ok(img) => {
            // Verify the decoded image respects the pixel budget.
            let total = img.width() as u64 * img.height() as u64;
            assert!(
                total <= MAX_PIXELS,
                "decoded image exceeds pixel budget: {}x{} = {total} pixels",
                img.width(),
                img.height()
            );
        }
        Err(_) => {
            // Decode error (including limit rejection) is not a finding.
        }
    }
});
