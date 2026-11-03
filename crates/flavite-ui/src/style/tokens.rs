//! Semantic light/dark token sets (RFC-0003 W11, D-15, TD-15).
//!
//! The palette is the single origin of shell color: every component style
//! in `style.rs` derives from the active [`Scheme`]'s palette, and both
//! palettes must pass the contrast contract in `visual_contract.rs`.
//! Graph data colors map onto these token roles instead of hand-picking
//! literals in the graph modules.

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
                selection: iced::Color {
                    a: 0.45,
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
