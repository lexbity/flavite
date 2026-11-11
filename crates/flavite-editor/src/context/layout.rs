//! Shared logical geometry for painted controls and native accessibility.
//! Theme and scale change colors/pixels, never which commands are admitted.

use flavite_edit_context::{ContextCommand as C, ContextMenu as M};

pub const GAP: f32 = 4.0;
pub const STATE_WIDTH: f32 = 70.0;
pub const SUMMARY_WIDTH: f32 = 128.0;
pub const CHIP_WIDTH: f32 = 96.0;
pub const CHIP_GAP: f32 = 6.0;
pub const PADDING: f32 = 12.0;

#[derive(Debug, Clone, Copy)]
pub struct ContextLayout {
    width: f32,
}

impl ContextLayout {
    #[must_use]
    pub fn new(width: f32) -> Self {
        Self {
            width: if width.is_finite() {
                width.max(0.0)
            } else {
                0.0
            },
        }
    }

    #[must_use]
    pub fn shows_state(self) -> bool {
        self.width >= 128.0
    }

    #[must_use]
    pub fn command_visible(self, command: C) -> bool {
        match command {
            C::Exit => true,
            C::Commands => self.width >= 320.0,
            C::Cut | C::Copy | C::Paste => self.width >= 600.0,
            C::Undo | C::Redo => self.width >= 430.0,
            C::Find => self.width >= 800.0,
            C::History => self.width >= 860.0,
            _ => false,
        }
    }

    #[must_use]
    pub fn menu_visible(self, menu: M) -> bool {
        self.width
            >= match menu {
                M::More => 320.0,
                M::Type => 390.0,
                M::Select | M::Go => 720.0,
            }
    }

    #[must_use]
    pub fn chip_count(self) -> usize {
        (((self.width - PADDING - SUMMARY_WIDTH).max(0.0) / (CHIP_WIDTH + CHIP_GAP)) as usize)
            .min(12)
    }
}

#[must_use]
pub const fn command_width(command: C) -> f32 {
    match command {
        C::History => 64.0,
        _ => 32.0,
    }
}

#[must_use]
pub const fn menu_width(menu: M) -> f32 {
    match menu {
        M::Select => 76.0,
        M::Go => 52.0,
        M::Type | M::More => 64.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_admitted_toolbar_and_history_strip_fits_its_logical_pane() {
        for width in 128..=1600 {
            let layout = ContextLayout::new(width as f32);
            let commands: Vec<_> = C::ALL
                .iter()
                .copied()
                .filter(|&c| layout.command_visible(c))
                .collect();
            let menus: Vec<_> = [M::Select, M::Go, M::Type, M::More]
                .into_iter()
                .filter(|&m| layout.menu_visible(m))
                .collect();
            let count = commands.len() + menus.len() + usize::from(layout.shows_state());
            let required = commands.into_iter().map(command_width).sum::<f32>()
                + menus.into_iter().map(menu_width).sum::<f32>()
                + if layout.shows_state() {
                    STATE_WIDTH
                } else {
                    0.0
                }
                + GAP * count.saturating_sub(1) as f32
                + PADDING;
            assert!(
                required <= width as f32,
                "toolbar {required} exceeds {width}"
            );
            let chips = layout.chip_count();
            if chips > 0 {
                assert!(
                    SUMMARY_WIDTH + chips as f32 * (CHIP_WIDTH + CHIP_GAP) + PADDING
                        <= width as f32
                );
            }
            assert!(layout.command_visible(C::Exit));
        }
        for width in [f32::NAN, f32::INFINITY, -1.0, 0.0, 31.0] {
            let layout = ContextLayout::new(width);
            assert!(layout.command_visible(C::Exit));
            assert!(!layout.command_visible(C::Commands));
            assert_eq!(layout.chip_count(), 0);
        }
    }
}
