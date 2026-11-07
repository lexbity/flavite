//! Scale-aware type and chrome metrics (RFC-0003 W11, D-15, NFR-14).
//!
//! The historic constants in `style.rs` are the 100% baseline; [`Metrics`]
//! multiplies them by the validated interface scale (75–200%) and carries
//! the editor font/line-width preferences. View code reads the active
//! [`Metrics`] from the app instead of the constants so every surface
//! scales together and the geometry solver sees the same chrome growth.

use super::{
    BRAND, HEADING, PALETTE_WIDTH, SPACING, SPACING_LG, SPACING_SM, TAB_HEIGHT, TEXT, TEXT_LG,
    TEXT_SM, TOP_BAR_HEIGHT,
};

/// Minimum icon-button target before scale (NFR-15 floor: 24 px).
const ICON_TARGET: f32 = 32.0;
/// Tab-strip action controls reserve width at 100%.
const CONTROLS_WIDTH: f32 = 96.0;

/// The resolved, scale-aware metrics for one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    /// The interface scale as a multiplier (0.75–2.0).
    pub scale: f32,
    /// Secondary text.
    pub text_sm: f32,
    /// Body text.
    pub text: f32,
    /// Emphasized text and inputs.
    pub text_lg: f32,
    /// Pane headers.
    pub heading: f32,
    /// Sidebar brand wordmark.
    pub brand: f32,
    /// Tab strip height.
    pub tab_height: f32,
    /// Top toolbar strip height (theme_frame backdrop, xpi themes).
    pub top_bar_height: f32,
    /// Tab-strip action controls reserve width.
    pub controls_width: f32,
    /// Sidebar width.
    pub sidebar_width: f32,
    /// Command palette card width.
    pub palette_width: f32,
    /// Base spacing unit.
    pub spacing: f32,
    /// Tight spacing.
    pub spacing_sm: f32,
    /// Loose spacing.
    pub spacing_lg: f32,
    /// Icon-button target size (never below the NFR-15 24 px floor).
    pub icon_target: f32,
    /// Editor font size in logical px.
    pub editor_font_size: f32,
    /// Editor line height derived from the font size (rounded to 0.5 px
    /// so caret/scroll math stays stable across preference changes).
    pub editor_line_height: f32,
    /// Readable editor line width in columns (`None` = full pane width).
    pub line_width_columns: Option<u16>,
}

impl Metrics {
    /// Metrics from raw validated preference values. The sidebar width is
    /// the user's own logical-pixel choice and is not scale-multiplied.
    #[must_use]
    pub fn new(
        interface_scale_pct: u16,
        editor_font_size: u16,
        line_width_columns: Option<u16>,
        sidebar_width: u16,
    ) -> Self {
        let scale = f32::from(interface_scale_pct) / 100.0;
        let editor = f32::from(editor_font_size);
        // 1.4 ratio, rounded to the nearest half pixel: the editor widget
        // derives caret/scroll positions from this value, so it must be a
        // stable function of the font size alone.
        let editor_line_height = (editor * 1.4 * 2.0).round() / 2.0;
        let scaled = |base: f32| (base * scale * 2.0).round() / 2.0;
        Self {
            scale,
            text_sm: scaled(TEXT_SM),
            text: scaled(TEXT),
            text_lg: scaled(TEXT_LG),
            heading: scaled(HEADING),
            brand: scaled(BRAND),
            tab_height: scaled(TAB_HEIGHT),
            top_bar_height: scaled(TOP_BAR_HEIGHT),
            controls_width: scaled(CONTROLS_WIDTH),
            sidebar_width: f32::from(sidebar_width),
            palette_width: scaled(PALETTE_WIDTH),
            spacing: scaled(SPACING),
            spacing_sm: scaled(SPACING_SM),
            spacing_lg: scaled(SPACING_LG),
            icon_target: scaled(ICON_TARGET).max(24.0),
            editor_font_size: editor,
            editor_line_height,
            line_width_columns,
        }
    }

    /// The editor widget's type settings (font size + derived line height).
    #[must_use]
    pub fn editor_type(&self) -> flavite_editor::TypeMetrics {
        flavite_editor::TypeMetrics {
            font_size: self.editor_font_size,
            line_height: self.editor_line_height,
        }
    }

    /// The editor wrap width in logical pixels for the readable line
    /// width preference (`None` = full pane width).
    #[must_use]
    pub fn editor_max_line_width(&self) -> Option<f32> {
        // Monospace advance is ~0.6 em; columns × advance caps the wrap.
        self.line_width_columns
            .map(|columns| f32::from(columns) * 0.6 * self.editor_font_size)
    }
}

impl Default for Metrics {
    fn default() -> Self {
        // Registry defaults (RFC-0007 D-04): scale 100%, font 14,
        // line width 80 columns, sidebar 260 px.
        Self::new(100, 14, Some(80), 260)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_metrics_match_the_historic_constants() {
        let metrics = Metrics::default();
        assert_eq!(metrics.scale, 1.0);
        assert_eq!(metrics.text_sm, 12.0);
        assert_eq!(metrics.text, 14.0);
        assert_eq!(metrics.text_lg, 16.0);
        assert_eq!(metrics.heading, 20.0);
        assert_eq!(metrics.brand, 19.0);
        assert_eq!(metrics.tab_height, 32.0);
        assert_eq!(metrics.spacing, 8.0);
        assert_eq!(metrics.editor_font_size, 14.0);
        assert_eq!(metrics.editor_line_height, 19.5);
    }

    #[test]
    fn scale_multiplies_chrome_and_type() {
        let metrics = Metrics::new(150, 16, None, 240);
        assert_eq!(metrics.scale, 1.5);
        assert_eq!(metrics.text, 21.0);
        assert_eq!(metrics.tab_height, 48.0);
        assert_eq!(
            metrics.editor_font_size, 16.0,
            "editor size is its own preference"
        );
        let tiny = Metrics::new(75, 12, Some(40), 240);
        assert_eq!(tiny.text_sm, 9.0);
        assert_eq!(tiny.line_width_columns, Some(40));
    }

    #[test]
    fn icon_targets_never_drop_below_the_accessibility_floor() {
        let metrics = Metrics::new(75, 12, None, 240);
        assert!(metrics.icon_target >= 24.0, "NFR-15 pointer target floor");
    }
}
