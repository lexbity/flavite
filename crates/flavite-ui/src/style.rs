//! Shell visual language: theme choice, spacing/type scale, and reusable
//! component styles. Every surface derives from the active theme palette so
//! the shell stays coherent without hand-picked colors per widget.
//! [`tokens`] owns the semantic light/dark palettes (W11), [`contrast`]
//! the WCAG arithmetic, and [`type_scale`] the scale-aware metrics.

pub mod contrast;
pub mod tokens;
pub mod type_scale;

pub use tokens::Scheme;
pub use tokens::theme_resolved;
pub use type_scale::Metrics;

use iced::widget::{button, container, text};
use iced::{Background, Border, Color, Shadow, Theme, Vector, border};

/// The iced theme for one semantic scheme (W11: light/dark/System).
#[must_use]
pub fn theme(scheme: Scheme) -> Theme {
    tokens::theme(scheme)
}

/// Base spacing unit.
pub const SPACING: f32 = 8.0;
/// Tight spacing (list rows, chip gaps).
pub const SPACING_SM: f32 = 4.0;
/// Loose spacing (section separation).
pub const SPACING_LG: f32 = 12.0;
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
/// Tab strip height (§13.1: tab height 32 px).
pub const TAB_HEIGHT: f32 = 32.0;
/// Top toolbar strip height. The bar is fixed-height so a theme package's
/// `theme_frame` image can layer exactly behind it (xpi themes); the row
/// content is vertically centered within it.
pub const TOP_BAR_HEIGHT: f32 = 44.0;
/// Splitter thickness at 100% scale (§7.5). Single source: the workspace
/// model owns the value; the layout solver divides by exactly this width.
pub const SPLITTER_WIDTH: f32 = flavite_workspace::SPLITTER_WIDTH;
/// Focus ring width (§7.5/NFR-15: focus ring ≥ 2 px).
pub const FOCUS_WIDTH: f32 = 2.0;

/// Corner radius shared by cards, rows, and action buttons.
const RADIUS: f32 = 3.0;

/// A continuous toolbar surface, shared by tab strips and view headers.
pub fn toolbar(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(theme.extended_palette().background.weaker.color.into()),
        ..container::Style::default()
    }
}

/// Flat tabs distinguish the active document from toolbar buttons.
pub fn tab(theme: &Theme, active: bool) -> container::Style {
    let ext = theme.extended_palette();
    container::Style {
        background: Some(
            if active {
                ext.background.weak.color
            } else {
                ext.background.weaker.color
            }
            .into(),
        ),
        border: Border {
            color: if active {
                ext.primary.base.color
            } else {
                Color::TRANSPARENT
            },
            width: 1.0,
            ..Border::default()
        },
        ..container::Style::default()
    }
}

/// A compact icon button with a 32px target and visible keyboard focus.
pub fn toolbar_button(theme: &Theme, status: button::Status) -> button::Style {
    let mut result = button::text(theme, status);
    result.border.radius = 2.0.into();
    result
}

pub fn menu_backdrop(_theme: &Theme, _status: button::Status) -> button::Style {
    button::Style {
        background: None,
        ..button::Style::default()
    }
}

pub fn drop_target(theme: &Theme, allowed: bool) -> container::Style {
    let mut color = if allowed {
        theme.extended_palette().primary.base.color
    } else {
        theme.palette().danger
    };
    color.a = 0.28;
    container::Style {
        background: Some(color.into()),
        border: Border {
            color: theme.palette().primary,
            width: 2.0,
            ..Border::default()
        },
        ..container::Style::default()
    }
}

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
    // NFR-15: chip text must meet AA against the painted fill; the
    // toolkit-generated pair can fall short for mid-tone accents. The
    // best of the candidate text colors wins, and the fill is blended
    // toward the shell background just enough to reach 4.5:1.
    let candidates = [pair.text, theme.palette().text, theme.palette().background];
    let mut text = candidates
        .into_iter()
        .max_by(|a, b| {
            contrast::ratio(*a, pair.color)
                .partial_cmp(&contrast::ratio(*b, pair.color))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .expect("non-empty candidates");
    let mut fill = pair.color;
    let mut steps = 0;
    while contrast::ratio(text, fill) < 4.5 && steps < 4 {
        fill = contrast::blend(fill, theme.palette().background, 0.25);
        text = candidates
            .into_iter()
            .max_by(|a, b| {
                contrast::ratio(*a, fill)
                    .partial_cmp(&contrast::ratio(*b, fill))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .expect("non-empty candidates");
        steps += 1;
    }
    container::Style {
        text_color: Some(text),
        background: Some(Background::Color(fill)),
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
        let (background, text_color) = if status == button::Status::Disabled {
            (
                None,
                Color {
                    a: 0.45,
                    ..theme.palette().text
                },
            )
        } else if selected {
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

/// Text style: secondary/de-emphasized text. The alpha is the smallest
/// of the fixed dimming steps that still meets the 4.5:1 contract over
/// the surface it paints (NFR-15), so dimming never costs readability.
#[must_use]
pub fn dim_text(theme: &Theme) -> text::Style {
    let palette = theme.palette();
    let surface = theme.extended_palette().background.weaker.color;
    let alpha = [0.65f32, 0.7, 0.75, 0.8, 0.85, 0.9, 1.0]
        .into_iter()
        .find(|alpha| {
            contrast::ratio(
                contrast::over(
                    Color {
                        a: *alpha,
                        ..palette.text
                    },
                    surface,
                ),
                surface,
            ) >= 4.5
        })
        .unwrap_or(1.0);
    text::Style {
        color: Some(Color {
            a: alpha,
            ..palette.text
        }),
    }
}

/// Text style: accent-colored text (brand, hints).
#[must_use]
pub fn accent_text(theme: &Theme) -> text::Style {
    // The toolkit's derived `primary.strong` is tuned for the opposite
    // surface family and can fall below AA; the accent is whichever of
    // the base/strong pair contrasts better on the actual background.
    let palette = theme.palette();
    let strong = theme.extended_palette().primary.strong.color;
    let accent = if contrast::ratio(palette.primary, palette.background)
        >= contrast::ratio(strong, palette.background)
    {
        palette.primary
    } else {
        strong
    };
    text::Style {
        color: Some(accent),
    }
}

/// Container style: one tab-group pane; the focused group carries a
/// visible non-color border.
#[must_use]
pub fn pane(theme: &Theme, focused: bool) -> container::Style {
    let ext = theme.extended_palette();
    let border = if focused {
        Border {
            color: ext.primary.base.color,
            width: FOCUS_WIDTH,
            radius: border::Radius::from(0.0),
        }
    } else {
        Border {
            color: ext.background.strong.color,
            width: 1.0,
            radius: border::Radius::from(0.0),
        }
    };
    container::Style {
        text_color: None,
        background: Some(Background::Color(theme.palette().background)),
        border,
        shadow: Shadow::default(),
        snap: false,
    }
}

/// Title-bar style for the pane-grid drag handle above each group.
#[must_use]
pub fn pane_title_bar(theme: &Theme, focused: bool) -> container::Style {
    let ext = theme.extended_palette();
    container::Style {
        text_color: None,
        background: Some(Background::Color(if focused {
            ext.primary.weak.color
        } else {
            ext.background.weaker.color
        })),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: border::Radius::from(0.0),
        },
        shadow: Shadow::default(),
        snap: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_styles_derive_from_the_theme_palette() {
        let theme = theme(crate::style::tokens::Scheme::Dark);
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
