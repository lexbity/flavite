//! CSS color parsing for theme manifests.
//!
//! Firefox theme colors are CSS color strings (`#rgb`, `#rrggbb`,
//! `rgb()`, `rgba()`, named colors); Chrome theme colors are RGB arrays.
//! Both land here as a premultiplied-nothing RGBA8 tuple with the original
//! alpha preserved, so the contrast solver above this boundary can see the
//! theme author's intent.

/// An sRGB color with a straight (non-premultiplied) alpha channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba {
    /// Red channel (0–255).
    pub r: u8,
    /// Green channel (0–255).
    pub g: u8,
    /// Blue channel (0–255).
    pub b: u8,
    /// Alpha channel (0 = transparent, 255 = opaque).
    pub a: u8,
}

impl Rgba {
    /// Build an opaque color.
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// Build a color with explicit alpha.
    #[must_use]
    pub const fn with_alpha(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Composite this color over an opaque backdrop, returning the opaque
    /// result. Fully transparent inputs return `None` so callers can fall
    /// back to their own default instead of painting a hole.
    #[must_use]
    pub fn over(self, backdrop: Self) -> Option<Self> {
        if self.a == 0 {
            return None;
        }
        if self.a == 255 {
            return Some(self);
        }
        let alpha = u16::from(self.a);
        let blend = |front: u8, back: u8| -> u8 {
            ((u16::from(front) * alpha + u16::from(back) * (255 - alpha)) / 255) as u8
        };
        Some(Self {
            r: blend(self.r, backdrop.r),
            g: blend(self.g, backdrop.g),
            b: blend(self.b, backdrop.b),
            a: 255,
        })
    }
}

/// A subset of the CSS named colors that actually appear in published
/// themes. Values are the CSS specification's, not a design choice.
const NAMED_COLORS: &[(&str, Rgba)] = &[
    ("black", Rgba::rgb(0, 0, 0)),
    ("white", Rgba::rgb(255, 255, 255)),
    ("red", Rgba::rgb(255, 0, 0)),
    ("green", Rgba::rgb(0, 128, 0)),
    ("blue", Rgba::rgb(0, 0, 255)),
    ("yellow", Rgba::rgb(255, 255, 0)),
    ("cyan", Rgba::rgb(0, 255, 255)),
    ("aqua", Rgba::rgb(0, 255, 255)),
    ("magenta", Rgba::rgb(255, 0, 255)),
    ("fuchsia", Rgba::rgb(255, 0, 255)),
    ("gray", Rgba::rgb(128, 128, 128)),
    ("grey", Rgba::rgb(128, 128, 128)),
    ("silver", Rgba::rgb(192, 192, 192)),
    ("maroon", Rgba::rgb(128, 0, 0)),
    ("olive", Rgba::rgb(128, 128, 0)),
    ("purple", Rgba::rgb(128, 0, 128)),
    ("navy", Rgba::rgb(0, 0, 128)),
    ("teal", Rgba::rgb(0, 128, 128)),
    ("orange", Rgba::rgb(255, 165, 0)),
    ("transparent", Rgba::with_alpha(0, 0, 0, 0)),
];

/// Parse a CSS color string: hex (`#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`),
/// functional (`rgb()`, `rgba()` with comma or space syntax, percentages
/// allowed), or a named color. Whitespace and case are normalized.
#[must_use]
pub fn parse_color(text: &str) -> Option<Rgba> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix('#') {
        return parse_hex(rest);
    }
    let lower = text.to_ascii_lowercase();
    if let Some(&(_, color)) = NAMED_COLORS.iter().find(|(name, _)| *name == lower) {
        return Some(color);
    }
    // Functional notation. Legacy comma syntax carries channels at 0–255
    // and an optional fourth alpha value at 0–1 (`rgba(1, 2, 3, 0.5)` is
    // the historical oddity: two scales in one call). Modern space syntax
    // separates the alpha with a slash and reads it at 0–1 or a percentage.
    let (function, inner) = lower.split_once('(')?;
    let inner = inner.strip_suffix(')')?;
    if !matches!(function, "rgb" | "rgba") {
        return None;
    }
    let (channel_tokens, alpha_token) = match inner.split_once('/') {
        Some((channels, alpha)) => (
            channels.split(' ').collect::<Vec<_>>(),
            Some(alpha.trim().to_owned()),
        ),
        None => {
            let tokens: Vec<&str> = if inner.contains(',') {
                inner.split(',').map(str::trim).collect()
            } else {
                inner.split_whitespace().collect()
            };
            match tokens.len() {
                3 => (tokens, None),
                // Legacy alpha: unitless 0–1 while channels stay 0–255.
                4 => (tokens[..3].to_vec(), Some(tokens[3].to_owned())),
                _ => return None,
            }
        }
    };
    let mut channels = [0u8; 3];
    let mut count = 0usize;
    for token in channel_tokens {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if count >= 3 {
            return None;
        }
        channels[count] = parse_channel(token)?;
        count += 1;
    }
    if count != 3 {
        return None;
    }
    let alpha = match alpha_token.as_deref() {
        None => 255,
        Some(token) => {
            if let Some(percent) = token.strip_suffix('%') {
                let value: f32 = percent.parse().ok()?;
                if !(0.0..=100.0).contains(&value) {
                    return None;
                }
                (value / 100.0 * 255.0).round() as u8
            } else {
                // Alpha (both syntaxes) is unitless 0–1.
                let value: f32 = token.parse().ok()?;
                if !(0.0..=1.0).contains(&value) {
                    return None;
                }
                (value * 255.0).round() as u8
            }
        }
    };
    Some(Rgba::with_alpha(
        channels[0],
        channels[1],
        channels[2],
        alpha,
    ))
}

fn parse_channel(token: &str) -> Option<u8> {
    if let Some(percent) = token.strip_suffix('%') {
        let value: f32 = percent.parse().ok()?;
        if !(0.0..=100.0).contains(&value) {
            return None;
        }
        return Some((value / 100.0 * 255.0).round() as u8);
    }
    let value: f32 = token.parse().ok()?;
    if !(0.0..=255.0).contains(&value) {
        return None;
    }
    Some(value.round() as u8)
}

fn parse_hex(rest: &str) -> Option<Rgba> {
    if rest.is_empty() || rest.len() > 8 || !rest.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match rest.len() {
        3 => {
            let channels: Vec<u8> = (0..3)
                .map(|i| u8::from_str_radix(&rest[i..i + 1], 16).ok().map(|v| v * 17))
                .collect::<Option<_>>()?;
            Some(Rgba::rgb(channels[0], channels[1], channels[2]))
        }
        4 => {
            let channels: Vec<u8> = (0..4)
                .map(|i| u8::from_str_radix(&rest[i..i + 1], 16).ok().map(|v| v * 17))
                .collect::<Option<_>>()?;
            Some(Rgba::with_alpha(
                channels[0],
                channels[1],
                channels[2],
                channels[3],
            ))
        }
        6 => {
            let channels: Vec<u8> = (0..3)
                .map(|i| u8::from_str_radix(&rest[i * 2..i * 2 + 2], 16).ok())
                .collect::<Option<_>>()?;
            Some(Rgba::rgb(channels[0], channels[1], channels[2]))
        }
        8 => {
            let channels: Vec<u8> = (0..4)
                .map(|i| u8::from_str_radix(&rest[i * 2..i * 2 + 2], 16).ok())
                .collect::<Option<_>>()?;
            Some(Rgba::with_alpha(
                channels[0],
                channels[1],
                channels[2],
                channels[3],
            ))
        }
        _ => None,
    }
}

/// Parse one manifest color value: a CSS string, or an array whose entries
/// are either CSS strings (the Firefox active/inactive form) or a single
/// Chrome-style RGB(A) integer triplet. Every parseable entry is returned;
/// the caller decides how many it consumes.
#[must_use]
pub fn parse_color_value(value: &serde_json::Value) -> Option<Vec<Rgba>> {
    match value {
        serde_json::Value::String(text) => parse_color(text).map(|color| vec![color]),
        serde_json::Value::Array(entries) => {
            if entries.first().is_some_and(serde_json::Value::is_number) {
                return parse_rgb_array(entries).map(|color| vec![color]);
            }
            let colors: Vec<Rgba> = entries
                .iter()
                .filter_map(|entry| entry.as_str().and_then(parse_color))
                .collect();
            (!colors.is_empty()).then_some(colors)
        }
        _ => None,
    }
}

fn parse_rgb_array(entries: &[serde_json::Value]) -> Option<Rgba> {
    if entries.is_empty() || entries.len() > 4 {
        return None;
    }
    let mut channels = [255u8; 4];
    for (index, entry) in entries.iter().enumerate() {
        let number = entry.as_f64()?;
        if !(0.0..=255.0).contains(&number) {
            return None;
        }
        channels[index] = number.round() as u8;
    }
    Some(Rgba::with_alpha(
        channels[0],
        channels[1],
        channels[2],
        channels[3],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_forms_parse_with_scaling() {
        assert_eq!(parse_color("#000"), Some(Rgba::rgb(0, 0, 0)));
        assert_eq!(parse_color("#FFF"), Some(Rgba::rgb(255, 255, 255)));
        assert_eq!(parse_color("#1a0f38"), Some(Rgba::rgb(0x1a, 0x0f, 0x38)));
        assert_eq!(
            parse_color("#1a0f38ff"),
            Some(Rgba::with_alpha(0x1a, 0x0f, 0x38, 0xff))
        );
        assert_eq!(parse_color("#abc"), Some(Rgba::rgb(0xaa, 0xbb, 0xcc)));
        assert_eq!(
            parse_color("#abc8"),
            Some(Rgba::with_alpha(0xaa, 0xbb, 0xcc, 0x88))
        );
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("#12g"), None);
    }

    #[test]
    fn functional_forms_parse_in_both_syntaxes() {
        assert_eq!(
            parse_color("rgb(71, 105, 91)"),
            Some(Rgba::rgb(71, 105, 91))
        );
        assert_eq!(
            parse_color("rgba(71, 105, 91, 0.5)"),
            Some(Rgba::with_alpha(71, 105, 91, 128))
        );
        assert_eq!(parse_color("rgb(71 105 91)"), Some(Rgba::rgb(71, 105, 91)));
        assert_eq!(
            parse_color("rgb(71 105 91 / 0.5)"),
            Some(Rgba::with_alpha(71, 105, 91, 128))
        );
        assert_eq!(
            parse_color("rgb(71 105 91 / 100%)"),
            Some(Rgba::with_alpha(71, 105, 91, 255))
        );
        assert_eq!(
            parse_color("rgb(50%, 0%, 50%)"),
            Some(Rgba::rgb(128, 0, 128))
        );
        assert_eq!(parse_color("rgb(300, 0, 0)"), None);
        assert_eq!(parse_color("hsl(120, 50%, 50%)"), None);
    }

    #[test]
    fn named_colors_and_transparency() {
        assert_eq!(parse_color("White"), Some(Rgba::rgb(255, 255, 255)));
        assert_eq!(parse_color(" navy "), Some(Rgba::rgb(0, 0, 128)));
        assert_eq!(
            parse_color("transparent"),
            Some(Rgba::with_alpha(0, 0, 0, 0))
        );
        assert_eq!(parse_color("chartreuse"), None);
    }

    #[test]
    fn alpha_compositing_over_backdrops() {
        assert_eq!(
            Rgba::with_alpha(10, 20, 30, 0).over(Rgba::rgb(1, 2, 3)),
            None
        );
        assert_eq!(
            Rgba::with_alpha(10, 20, 30, 255).over(Rgba::rgb(1, 2, 3)),
            Some(Rgba::rgb(10, 20, 30))
        );
        let half = Rgba::with_alpha(255, 255, 255, 128).over(Rgba::rgb(0, 0, 0));
        assert_eq!(half, Some(Rgba::rgb(128, 128, 128)));
    }

    #[test]
    fn manifest_color_values_accept_strings_and_arrays() {
        assert_eq!(
            parse_color_value(&serde_json::json!("#1a0f38")),
            Some(vec![Rgba::rgb(0x1a, 0x0f, 0x38)])
        );
        assert_eq!(
            parse_color_value(&serde_json::json!([71, 105, 91])),
            Some(vec![Rgba::rgb(71, 105, 91)])
        );
        assert_eq!(
            parse_color_value(&serde_json::json!([71, 105, 91, 128])),
            Some(vec![Rgba::with_alpha(71, 105, 91, 128)])
        );
        // Firefox active/inactive pair.
        assert_eq!(
            parse_color_value(&serde_json::json!(["#101010", "#202020"])).map(|c| c.len()),
            Some(2)
        );
        assert_eq!(parse_color_value(&serde_json::json!(42)), None);
    }
}
