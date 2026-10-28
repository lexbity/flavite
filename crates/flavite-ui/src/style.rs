//! Shell visual language: theme choice, spacing/type scale, and reusable
//! component styles. Every surface derives from the active theme palette so
//! the shell stays coherent without hand-picked colors per widget.

use iced::widget::{button, container, text};
use iced::{Background, Border, Color, Shadow, Theme, Vector, border};

/// The shell's theme. Catppuccin Macchiato: a calm dark base with readable
/// accents; all component styles read from its palette instead of hard-coding.
pub const THEME: Theme = Theme::CatppuccinMacchiato;

/// Base spacing unit.
pub const SPACING: f32 = 12.0;
/// Tight spacing (list rows, chip gaps).
pub const SPACING_SM: f32 = 6.0;
/// Loose spacing (section separation).
pub const SPACING_LG: f32 = 18.0;
/// Sidebar width.
pub const SIDEBAR_WIDTH: f32 = 232.0;
/// Command palette card width.
pub const PALETTE_WIDTH: f32 = 640.0;
/// Secondary text.
pub const TEXT_SM: f32 = 12.0;
/// Body text.
pub const TEXT: f32 = 14.0;
/// Emphasized text and inputs.
pub const TEXT_LG: f32 = 16.0;
/// Pane headers.
pub const HEADING: f32 = 20.0;
/// Sidebar brand wordmark.
pub const BRAND: f32 = 19.0;

/// Corner radius shared by cards, rows, and action buttons.
const RADIUS: f32 = 8.0;

/// A surface tone used by [`card`]-like containers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Neutral surface (cards, panels).
    Neutral,
    /// Primary accent surface (chips, highlights).
    Primary,
    /// Success surface (ready indicators).
    Success,
    /// Warning surface (in-flight work).
    Warning,
    /// Danger surface (conflicts, failures).
    Danger,
}

/// Container style: a rounded card surface with a hairline border.
#[must_use]
pub fn card(theme: &Theme) -> container::Style {
    let ext = theme.extended_palette();
    container::Style {
        text_color: None,
        background: Some(Background::Color(ext.background.weak.color)),
        border: Border {
            color: ext.background.strong.color,
            width: 1.0,
            radius: border::Radius::from(RADIUS),
        },
        shadow: Shadow::default(),
        snap: false,
    }
}

/// Container style: the sidebar panel.
#[must_use]
pub fn sidebar(theme: &Theme) -> container::Style {
    let ext = theme.extended_palette();
    container::Style {
        text_color: None,
        background: Some(Background::Color(ext.background.weaker.color)),
        border: Border {
            color: ext.background.strong.color,
            width: 1.0,
            radius: border::Radius::from(0.0),
        },
        shadow: Shadow::default(),
        snap: false,
    }
}

/// Container style: the persistent bottom status bar.
#[must_use]
pub fn status_bar(theme: &Theme) -> container::Style {
    let ext = theme.extended_palette();
    container::Style {
        text_color: None,
        background: Some(Background::Color(ext.background.weaker.color)),
        border: Border {
            color: ext.background.strong.color,
            width: 1.0,
            radius: border::Radius::from(0.0),
        },
        shadow: Shadow::default(),
        snap: false,
    }
}

/// Container style: the pane content region.
#[must_use]
pub fn content(theme: &Theme) -> container::Style {
    container::Style {
        text_color: None,
        background: Some(Background::Color(theme.palette().background)),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: border::Radius::from(0.0),
        },
        shadow: Shadow::default(),
        snap: false,
    }
}

/// Container style: a small rounded pill (state chips).
#[must_use]
pub fn chip(theme: &Theme, tone: Tone) -> container::Style {
    let ext = theme.extended_palette();
    let pair = match tone {
        Tone::Neutral => ext.background.neutral,
        Tone::Primary => ext.primary.weak,
        Tone::Success => ext.success.weak,
        Tone::Warning => ext.warning.weak,
        Tone::Danger => ext.danger.weak,
    };
    container::Style {
        text_color: Some(pair.text),
        background: Some(Background::Color(pair.color)),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: border::Radius::from(999.0),
        },
        shadow: Shadow::default(),
        snap: false,
    }
}

/// Container style: a small circular status dot.
#[must_use]
pub fn dot(theme: &Theme, tone: Tone) -> container::Style {
    let ext = theme.extended_palette();
    let pair = match tone {
        Tone::Neutral => ext.background.strongest,
        Tone::Primary => ext.primary.base,
        Tone::Success => ext.success.base,
        Tone::Warning => ext.warning.base,
        Tone::Danger => ext.danger.base,
    };
    container::Style {
        text_color: None,
        background: Some(Background::Color(pair.color)),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: border::Radius::from(999.0),
        },
        shadow: Shadow::default(),
        snap: false,
    }
}

/// Button style: a selectable row (sidebar navigation, palette results).
pub fn list_row(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style + Copy {
    move |theme, status| {
        let ext = theme.extended_palette();
        let (background, text_color) = if selected {
            (
                Some(Background::Color(ext.primary.weak.color)),
                ext.primary.strong.text,
            )
        } else {
            match status {
                button::Status::Hovered | button::Status::Pressed => (
                    Some(Background::Color(ext.background.strong.color)),
                    theme.palette().text,
                ),
                _ => (None, theme.palette().text),
            }
        };
        button::Style {
            background,
            text_color,
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: border::Radius::from(RADIUS),
            },
            shadow: Shadow::default(),
            snap: false,
        }
    }
}

/// Button style: the primary action of a pane header or settings pane.
pub fn action() -> impl Fn(&Theme, button::Status) -> button::Style + Copy {
    move |theme, status| {
        let ext = theme.extended_palette();
        let pair = match status {
            button::Status::Active | button::Status::Hovered => ext.primary.base,
            button::Status::Pressed => ext.primary.strong,
            button::Status::Disabled => ext.background.neutral,
        };
        button::Style {
            background: Some(Background::Color(pair.color)),
            text_color: pair.text,
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: border::Radius::from(RADIUS),
            },
            shadow: Shadow::default(),
            snap: false,
        }
    }
}

/// Button style: a quiet secondary action.
pub fn ghost() -> impl Fn(&Theme, button::Status) -> button::Style + Copy {
    move |theme, status| {
        let ext = theme.extended_palette();
        let background = match status {
            button::Status::Hovered | button::Status::Pressed => {
                Some(Background::Color(ext.background.strong.color))
            }
            _ => None,
        };
        button::Style {
            background,
            text_color: theme.palette().text,
            border: Border {
                color: ext.background.strong.color,
                width: 1.0,
                radius: border::Radius::from(RADIUS),
            },
            shadow: Shadow::default(),
            snap: false,
        }
    }
}

/// Button style: the invisible full-area backdrop behind the palette overlay.
#[must_use]
pub fn backdrop(_theme: &Theme, _status: button::Status) -> button::Style {
    button::Style {
        background: Some(Background::Color(Color {
            a: 0.45,
            ..Color::BLACK
        })),
        text_color: Color::TRANSPARENT,
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: border::Radius::from(0.0),
        },
        shadow: Shadow::default(),
        snap: false,
    }
}

/// Container style: the floating command palette card.
#[must_use]
pub fn palette_card(theme: &Theme) -> container::Style {
    let mut card_style = card(theme);
    card_style.shadow = Shadow {
        offset: Vector::new(0.0, 12.0),
        blur_radius: 32.0,
        color: Color {
            a: 0.45,
            ..Color::BLACK
        },
    };
    card_style
}

/// Text style: secondary/de-emphasized text.
#[must_use]
pub fn dim_text(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(Color {
            a: 0.65,
            ..theme.palette().text
        }),
    }
}

/// Text style: accent-colored text (brand, hints).
#[must_use]
pub fn accent_text(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(theme.extended_palette().primary.strong.color),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_styles_derive_from_the_theme_palette() {
        let theme = THEME;
        let palette = theme.palette();
        let card_style = card(&theme);
        let Some(Background::Color(card_background)) = card_style.background else {
            panic!("card style must set a solid background");
        };
        assert_ne!(card_background, palette.background);
        assert!(card_style.border.width > 0.0);
        assert!(palette_card(&theme).shadow.blur_radius > 0.0);
        let selected = list_row(true)(&theme, button::Status::Active);
        let unselected = list_row(false)(&theme, button::Status::Active);
        assert_ne!(selected.background, unselected.background);
    }
}
