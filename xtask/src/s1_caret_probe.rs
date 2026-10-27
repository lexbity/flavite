//! S1 caret geometry and hit-test precision probe — shared code for binary and tests.
//!
//! This module contains the measurement logic and types used by both the
//! `s1_caret_probe` binary and the headless tests in `xtask/tests/s1_caret_probe.rs`.

use cosmic_text::{Attrs, Buffer, Cursor, FontSystem, Metrics, Shaping, Wrap};

/// Scale factors to test (matching the S1 checklist).
pub const SCALE_FACTORS: &[f32] = &[1.0, 1.25, 1.5, 2.0];

/// Font size in logical pixels.
pub const FONT_SIZE: f32 = 14.0;

/// Sample text for measurement: mixes ASCII to exercise shaping.
pub const SAMPLE_TEXT: &str = "Hello, world! This is a test.";

/// Maximum acceptable round-trip error in pixels.
pub const MAX_ROUNDTRIP_ERROR: f32 = 0.5;

/// Results for one scale factor.
#[derive(Debug, Clone)]
pub struct ScaleResults {
    /// Scale factor.
    pub scale: f32,
    /// Font metrics at this scale.
    pub font_size: f32,
    /// Line height at this scale.
    pub line_height: f32,
    /// Maximum round-trip error across all sampled positions.
    pub max_error: f32,
    /// Whether the max error is within the acceptable bound.
    pub passes: bool,
}

impl ScaleResults {
    /// Creates a new result for the given scale and metrics.
    pub fn new(scale: f32, metrics: Metrics) -> Self {
        Self {
            scale,
            font_size: metrics.font_size,
            line_height: metrics.line_height,
            max_error: 0.0,
            passes: false,
        }
    }
}

/// Run the full caret/hit-test measurement at a given scale factor.
pub fn measure_at_scale(scale: f32) -> ScaleResults {
    let mut font_system = FontSystem::new();
    // Load system fonts
    font_system.db_mut().load_system_fonts();

    let metrics = Metrics::new(FONT_SIZE * scale, FONT_SIZE * scale * 1.2);
    let mut buffer = Buffer::new(&mut font_system, metrics);

    // Configure the buffer
    buffer.set_wrap(&mut font_system, Wrap::None);

    let attrs = Attrs::new();
    let shaping = Shaping::Advanced;

    // Set the text
    buffer.set_text(&mut font_system, SAMPLE_TEXT, &attrs, shaping, None);

    // Perform shaping
    buffer.shape_until_scroll(&mut font_system, true);

    let mut results = ScaleResults::new(scale, metrics);

    // Sample at each character position
    let mut max_error = 0.0f32;

    // Sample at each character boundary
    for char_idx in 0..=SAMPLE_TEXT.len() {
        // Create a cursor at this character (line 0, index char_idx)
        let cursor = Cursor::new(0, char_idx);

        // Get the layout cursor
        let layout_cursor = buffer.layout_cursor(&mut font_system, cursor);

        if let Some(lc) = layout_cursor {
            // Get the x position from the layout by computing it
            // We need to sum up the advances of glyphs before this cursor
            let mut x_pos = 0.0f32;
            let mut found = false;

            for run in buffer.layout_runs() {
                if run.line_i == lc.line {
                    // This is the right line
                    for (glyph_idx, glyph) in run.glyphs.iter().enumerate() {
                        if glyph_idx == lc.glyph {
                            // At this glyph, the cursor is here
                            // For simplicity, we use the accumulated x
                            found = true;
                            break;
                        }
                        x_pos += glyph.w;
                    }
                    break;
                }
            }

            if found {
                // Now hit-test at this x position
                let y = metrics.line_height / 2.0;
                let hit_cursor = buffer.hit(x_pos, y);

                if let Some(hit_cursor) = hit_cursor {
                    // Get the x position of the hit cursor
                    let hit_layout_cursor = buffer.layout_cursor(&mut font_system, hit_cursor);

                    if let Some(hit_lc) = hit_layout_cursor {
                        let mut hit_x = 0.0f32;
                        let mut hit_found = false;

                        for run in buffer.layout_runs() {
                            if run.line_i == hit_lc.line {
                                for (glyph_idx, glyph) in run.glyphs.iter().enumerate() {
                                    if glyph_idx == hit_lc.glyph {
                                        hit_found = true;
                                        break;
                                    }
                                    hit_x += glyph.w;
                                }
                                break;
                            }
                        }

                        if hit_found {
                            let error = (x_pos - hit_x).abs();
                            max_error = max_error.max(error);
                        }
                    }
                }
            }
        }
    }

    // Store results
    results.max_error = max_error;
    results.passes = max_error <= MAX_ROUNDTRIP_ERROR;

    results
}

/// Run all measurements and return results.
pub fn run_measurements() -> Vec<ScaleResults> {
    SCALE_FACTORS
        .iter()
        .map(|&scale| measure_at_scale(scale))
        .collect()
}
