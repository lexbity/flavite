//! WCAG 2.x contrast arithmetic for the semantic token sets (W11, NFR-15).
//!
//! The shell's guarantee is measurable: text contrast ≥ 4.5:1, large text
//! and essential non-text controls ≥ 3:1. [`ratio`] is the single
//! definition of contrast; `visual_contract.rs` asserts it for every
//! token pair the shell actually paints.

use iced::Color;

/// Relative luminance per WCAG 2.x (sRGB, linearized channels).
#[must_use]
pub fn luminance(color: Color) -> f64 {
    fn channel(value: f32) -> f64 {
        let value = f64::from(value.clamp(0.0, 1.0));
        if value <= 0.039_28 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    0.212_6 * channel(color.r) + 0.715_2 * channel(color.g) + 0.072_2 * channel(color.b)
}

/// The WCAG contrast ratio between two opaque colors (1.0–21.0).
#[must_use]
pub fn ratio(a: Color, b: Color) -> f64 {
    let (light, dark) = {
        let la = luminance(a);
        let lb = luminance(b);
        if la >= lb { (la, lb) } else { (lb, la) }
    };
    (light + 0.05) / (dark + 0.05)
}

/// Linear blend of two opaque colors: `t = 0` returns `from`, `t = 1`
/// returns `to` (used to ease chip fills toward the shell background).
#[must_use]
pub fn blend(from: Color, to: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let mix = |a: f32, b: f32| a + (b - a) * t;
    Color {
        r: mix(from.r, to.r),
        g: mix(from.g, to.g),
        b: mix(from.b, to.b),
        a: 1.0,
    }
}

/// Composite a translucent color over an opaque backdrop and return the
/// effective painted color (used for dim/alpha text styles).
#[must_use]
pub fn over(foreground: Color, backdrop: Color) -> Color {
    let alpha = f64::from(foreground.a);
    let blend = |f: f32, b: f32| {
        ((f64::from(f) * alpha + f64::from(b) * (1.0 - alpha)) as f32).clamp(0.0, 1.0)
    };
    Color {
        r: blend(foreground.r, backdrop.r),
        g: blend(foreground.g, backdrop.g),
        b: blend(foreground.b, backdrop.b),
        a: 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_pairs_hit_reference_ratios() {
        let black = Color::BLACK;
        let white = Color::WHITE;
        assert!((ratio(black, white) - 21.0).abs() < 0.1);
        assert!(ratio(black, black) < 1.01);
    }

    #[test]
    fn alpha_compositing_matches_the_painted_result() {
        let text = Color {
            a: 0.65,
            ..Color::WHITE
        };
        let backdrop = Color::BLACK;
        let painted = over(text, backdrop);
        assert!(painted.a == 1.0);
        assert!((painted.r - 0.65).abs() < 1e-6);
    }
}
