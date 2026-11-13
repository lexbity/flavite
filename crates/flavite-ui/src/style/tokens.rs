//! Semantic light/dark token sets (RFC-0003 W11, D-15, TD-15).
//!
//! The palette is the single origin of shell color: every component style
//! in `style.rs` derives from the active [`Scheme`]'s palette, and both
//! palettes must pass the contrast contract in `visual_contract.rs`.
//! Graph data colors map onto these token roles instead of hand-picking
//! literals in the graph modules.

use super::contrast;
use flavite_theme::SemanticTheme;
use iced::Theme;

/// Which semantic token set is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// The dark token set (the historical shell palette).
    Dark,
    /// The light token set.
    Light,
}

impl Scheme {
    /// The opposite scheme, for preference cycling.
    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }

    /// The user-visible scheme name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

/// The dark palette: neutral shell surfaces with a violet writing accent.
#[must_use]
pub fn dark_palette() -> iced::theme::Palette {
    iced::theme::Palette {
        background: iced::Color::from_rgb8(25, 29, 35),
        text: iced::Color::from_rgb8(226, 229, 239),
        primary: iced::Color::from_rgb8(180, 137, 240),
        success: iced::Color::from_rgb8(94, 206, 193),
        warning: iced::Color::from_rgb8(232, 187, 108),
        danger: iced::Color::from_rgb8(237, 128, 142),
    }
}

/// The light palette: the same roles, darkened accents for contrast.
#[must_use]
pub fn light_palette() -> iced::theme::Palette {
    iced::theme::Palette {
        background: iced::Color::from_rgb8(247, 248, 251),
        text: iced::Color::from_rgb8(35, 38, 45),
        primary: iced::Color::from_rgb8(108, 66, 178),
        success: iced::Color::from_rgb8(11, 119, 102),
        warning: iced::Color::from_rgb8(140, 94, 8),
        danger: iced::Color::from_rgb8(178, 30, 56),
    }
}

/// The iced theme for one scheme.
#[must_use]
pub fn theme(scheme: Scheme) -> Theme {
    match scheme {
        Scheme::Dark => Theme::custom("Flavite Dark", dark_palette()),
        Scheme::Light => Theme::custom("Flavite Light", light_palette()),
    }
}

/// The iced theme for one scheme with an optional theme-package overlay.
/// Themed slots composite over the built-in palette and pass through the
/// contrast solver, so an arbitrary package can never paint unreadable
/// text; surfaces the package never speaks to keep the built-in colors.
#[must_use]
pub fn theme_resolved(scheme: Scheme, themed: Option<&SemanticTheme>) -> Theme {
    let Some(themed) = themed else {
        return theme(scheme);
    };
    let name = match scheme {
        Scheme::Dark => "Flavite Dark (theme)",
        Scheme::Light => "Flavite Light (theme)",
    };
    Theme::custom(name, resolved_palette(scheme, Some(themed)))
}

/// The built-in palette for one scheme with themed slots overlaid.
#[must_use]
pub fn resolved_palette(scheme: Scheme, themed: Option<&SemanticTheme>) -> iced::theme::Palette {
    let mut palette = match scheme {
        Scheme::Dark => dark_palette(),
        Scheme::Light => light_palette(),
    };
    let Some(themed) = themed else {
        return palette;
    };
    let slots = themed_slots(scheme, themed);
    if let Some(background) = slots.window_background {
        palette.background = background;
    }
    if let Some(text) = slots
        .window_text
        .filter(|text| contrast::ratio(*text, palette.background) >= 4.5)
    {
        palette.text = text;
    }
    if let Some(accent) = slots
        .accent
        .filter(|accent| contrast::ratio(*accent, palette.background) >= 3.0)
    {
        palette.primary = accent;
    }
    palette
}

/// The top-bar surface colors (background, text) for one scheme: themed
/// frame colors made effective over the resolved window backdrop and
/// contrast-repaired, or `None` slots keeping the shell default.
#[must_use]
pub fn top_bar_colors(
    scheme: Scheme,
    themed: Option<&SemanticTheme>,
) -> (Option<iced::Color>, Option<iced::Color>) {
    let Some(themed) = themed else {
        return (None, None);
    };
    let slots = themed_slots(scheme, themed);
    let default = match scheme {
        Scheme::Dark => dark_palette(),
        Scheme::Light => light_palette(),
    };
    let text = match (slots.top_bar_text, slots.top_bar_background) {
        (Some(text), Some(background)) if contrast::ratio(text, background) >= 4.5 => Some(text),
        // The theme wanted a frame tint but its text is unreadable on it:
        // keep the package's tint, repair the text from the built-ins.
        (_, Some(background)) if contrast::ratio(default.text, background) >= 4.5 => {
            Some(default.text)
        }
        (Some(text), None) if contrast::ratio(text, default.background) >= 4.5 => Some(text),
        _ => None,
    };
    (slots.top_bar_background, text)
}

/// The editor chrome colors for one scheme with the theme-package
/// overlay: the editor surface follows the package's field slot (falling
/// back to its window slot), text is contrast-repaired, and the derived
/// selection/link roles stay computed from the resolved palette.
#[must_use]
pub fn editor_colors_resolved(
    scheme: Scheme,
    themed: Option<&SemanticTheme>,
) -> flavite_editor::EditorColors {
    let mut colors = editor_colors(scheme);
    let Some(themed) = themed else {
        return colors;
    };
    let slots = themed_slots(scheme, themed);
    let Some(background) = slots.editor_background.or(slots.window_background) else {
        return colors;
    };
    colors.background = background;
    let palette = resolved_palette(scheme, Some(themed));
    let text = slots
        .editor_text
        .filter(|text| contrast::ratio(*text, background) >= 4.5)
        .unwrap_or(palette.text);
    colors.text = text;
    // Autocomplete surfaces ride the editor background.
    colors.autocomplete_background = contrast::blend(
        background,
        palette.text,
        if scheme == Scheme::Dark { 0.08 } else { 0.35 },
    );
    colors.autocomplete_text = text;
    colors
}

/// The themed slots the shell paints with, resolved to their effective
/// opaque colors: translucent theme colors composite over the resolved
/// window backdrop (itself composited over the built-in palette when the
/// package left it unset), so every consumer sees exactly what paints.
#[must_use]
pub fn themed_slots(scheme: Scheme, themed: &SemanticTheme) -> ThemedSlots {
    let builtin = match scheme {
        Scheme::Dark => dark_palette(),
        Scheme::Light => light_palette(),
    };
    let window_background = themed
        .window_background
        .map(|color| contrast::over(to_color(color), builtin.background));
    let backdrop = window_background.unwrap_or(builtin.background);
    let composite = |color: Option<flavite_theme::Rgba>| {
        color.map(|color| contrast::over(to_color(color), backdrop))
    };
    ThemedSlots {
        window_background,
        window_text: composite(themed.window_text),
        accent: composite(themed.accent),
        editor_background: composite(themed.editor_background),
        editor_text: composite(themed.editor_text),
        top_bar_background: composite(themed.top_bar_background),
        top_bar_text: composite(themed.top_bar_text),
    }
}

/// The effective themed slots for one scheme.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThemedSlots {
    /// The general window background.
    pub window_background: Option<iced::Color>,
    /// The general window text.
    pub window_text: Option<iced::Color>,
    /// The primary accent.
    pub accent: Option<iced::Color>,
    /// The editor surface.
    pub editor_background: Option<iced::Color>,
    /// The editor text.
    pub editor_text: Option<iced::Color>,
    /// The top-bar frame background.
    pub top_bar_background: Option<iced::Color>,
    /// The top-bar frame text.
    pub top_bar_text: Option<iced::Color>,
}

fn to_color(color: flavite_theme::Rgba) -> iced::Color {
    iced::Color::from_rgba8(color.r, color.g, color.b, f32::from(color.a) / 255.0)
}

/// Graph data colors mapped onto token roles (D-01: data colors are
/// semantic roles, not literals). `note` uses the primary accent family,
/// `attachment` the warning family; unresolved is the danger family.
#[must_use]
pub fn graph_note_color(scheme: Scheme) -> [f32; 4] {
    let color = match scheme {
        Scheme::Dark => iced::Color::from_rgb8(120, 170, 255),
        Scheme::Light => iced::Color::from_rgb8(38, 84, 168),
    };
    [color.r, color.g, color.b, 1.0]
}

/// Graph attachment node color from the token roles.
#[must_use]
pub fn graph_attachment_color(scheme: Scheme) -> [f32; 4] {
    let color = match scheme {
        Scheme::Dark => iced::Color::from_rgb8(232, 187, 108),
        Scheme::Light => iced::Color::from_rgb8(140, 94, 8),
    };
    [color.r, color.g, color.b, 1.0]
}

/// Graph unresolved-node color from the token roles.
#[must_use]
pub fn graph_unresolved_color(scheme: Scheme) -> [f32; 4] {
    let color = match scheme {
        Scheme::Dark => iced::Color::from_rgb8(232, 80, 105),
        Scheme::Light => iced::Color::from_rgb8(140, 20, 40),
    };
    [color.r, color.g, color.b, 1.0]
}

/// Graph edge color from the token roles (a mid-contrast line color).
#[must_use]
pub fn graph_edge_color(scheme: Scheme) -> [f32; 3] {
    let color = match scheme {
        Scheme::Dark => iced::Color::from_rgb8(158, 168, 189),
        Scheme::Light => iced::Color::from_rgb8(96, 106, 126),
    };
    [color.r, color.g, color.b]
}

/// Graph canvas label color from the token roles.
#[must_use]
pub fn graph_label_color(scheme: Scheme) -> iced::Color {
    match scheme {
        Scheme::Dark => iced::Color::from_rgb8(224, 230, 240),
        Scheme::Light => iced::Color::from_rgb8(35, 38, 45),
    }
}

/// Graph selection-box stroke color from the token roles.
#[must_use]
pub fn graph_selection_color(scheme: Scheme) -> iced::Color {
    match scheme {
        Scheme::Dark => iced::Color::from_rgb8(41, 168, 158),
        Scheme::Light => iced::Color::from_rgb8(11, 119, 102),
    }
}

/// Editor chrome colors derived from the active scheme (W11 editor
/// adapter). Selection/link/preedit/autocomplete all follow the palette;
/// no per-widget literals remain in the editor widget.
#[must_use]
pub fn editor_colors(scheme: Scheme) -> flavite_editor::EditorColors {
    let defaults = flavite_editor::EditorColors::default();
    match scheme {
        Scheme::Dark => defaults,
        Scheme::Light => {
            let palette = light_palette();
            let text = palette.text;
            flavite_editor::EditorColors {
                background: palette.background,
                selection: iced::Color {
                    a: 0.45,
                    ..palette.primary
                },
                selection_muted: iced::Color {
                    a: 0.18,
                    ..palette.primary
                },
                link: palette.primary,
                link_unresolved: palette.danger,
                link_multiple: palette.warning,
                text,
                preedit_underline: palette.warning,
                preedit_text: text,
                autocomplete_background: iced::Color::from_rgb8(255, 255, 255),
                autocomplete_selected: palette.primary,
                autocomplete_text: text,
                autocomplete_accent: palette.primary,
            }
        }
    }
}
