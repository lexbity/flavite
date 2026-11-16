//! Indentation detection and manipulation (FEUX-0005 E-2, E-3, E-5).
//!
//! This module provides pure, deterministic detection of indentation units
//! and the normative algorithms for indent/dedent and tabs<->spaces conversion.
//!
//! # Detection
//!
//! The `detect_unit` function implements the normative algorithm from FEUX-0005 §5.1:
//! - Style vote: first whitespace char of each non-blank line determines vote
//! - Width candidates: all-space leading runs of 1..=8 spaces contribute
//! - Width = smallest mode of candidates; ties → smallest; none → 4
//! - Fallback: `(Space, 4)` when no usable signal
//!
//! # Algorithms
//!
//! - **Indent**: prepend one unit to each non-empty/non-whitespace-only line
//! - **Dedent**: per line, remove to previous tab stop (tab-width aligned)
//! - **Conversions**: leading-whitespace only, byte-lossless in visible columns

use std::collections::HashMap;

/// The detected indentation style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Style {
    /// Tab-based indentation.
    Tab,
    /// Space-based indentation.
    Space,
}

impl Style {
    /// Returns the string representation of one unit with the given width.
    /// For Tab, width is ignored (always "\t").
    #[must_use]
    pub fn unit_text(self, width: u8) -> String {
        match self {
            Style::Tab => "\t".to_string(),
            Style::Space => " ".repeat(width as usize),
        }
    }
}

/// A detected indentation unit: style and width.
/// Width is always 1..=8 for detected units, or 4 for the fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Unit {
    /// The indentation style.
    pub style: Style,
    /// The indentation width (1..=8 for detected, 4 for fallback).
    pub width: u8,
}

impl Unit {
    /// Returns the string representation of one unit.
    #[must_use]
    pub fn unit_text(&self) -> String {
        self.style.unit_text(self.width)
    }

    /// Returns the string representation of `amount` units.
    #[must_use]
    pub fn text(&self, amount: u32) -> String {
        self.unit_text().repeat(amount as usize)
    }

    /// Returns the fallback unit: Space with width 4.
    #[must_use]
    pub fn fallback() -> Self {
        Unit {
            style: Style::Space,
            width: 4,
        }
    }

    /// Format the unit for display in completion messages.
    #[must_use]
    pub fn display(&self) -> String {
        match self.style {
            Style::Tab => "tabs".to_string(),
            Style::Space => format!("{} spaces", self.width),
        }
    }
}

/// Detect the indentation unit from the given text.
///
/// Normative algorithm from FEUX-0005 §5.1 / E-2:
/// 1. For each non-blank, non-whitespace-only line:
///    - Extract leading whitespace (longest prefix of [\t ])
///    - If first char is '\t': increment tab_votes
///    - If first char is ' ' and run is all-spaces with 1 ≤ n ≤ 8:
///      - increment space_votes
///      - add n to width_candidates
///    - Mixed runs (spaces then tab): vote by first char, NO width candidate
/// 2. style = tab_votes > space_votes ? Tab : Space (ties and zero → Space)
/// 3. width = mode(width_candidates); ties → smallest; none → 4
///
/// Runs of 9..=16 spaces are treated as alignment padding: they vote on style
/// (via first char = ' ') but contribute NO width candidate.
///
/// The fallback (Space, 4) applies when there are no usable lines.
#[must_use]
pub fn detect_unit(text: &str) -> Unit {
    let mut tab_votes: u32 = 0;
    let mut space_votes: u32 = 0;
    let mut width_candidates: HashMap<u8, u32> = HashMap::new();
    let mut has_usable_line = false;

    for line in text.lines() {
        // Skip empty lines and whitespace-only lines (noise for both vote and width)
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        has_usable_line = true;

        // Extract leading whitespace
        let leading = line
            .chars()
            .take_while(|c| *c == '\t' || *c == ' ')
            .collect::<String>();

        if leading.is_empty() {
            // No leading whitespace - vote for space (flush-left line)
            // But doesn't contribute to width candidates
            space_votes += 1;
            continue;
        }

        let first_char = leading.chars().next().unwrap();

        if first_char == '\t' {
            tab_votes += 1;
        } else if first_char == ' ' {
            // All-spaces run: check length
            let len = leading.len() as u8;
            if len <= 8 {
                space_votes += 1;
                // Width candidates are 1..=8
                *width_candidates.entry(len).or_insert(0) += 1;
            } else {
                // 9..=16 spaces: vote on style only (space), no width candidate
                space_votes += 1;
            }
        } else {
            // Mixed run (spaces then tab): first char decides
            // This should not happen since we only have \t and ' ' in leading
            // But if it does, treat as the first char
            if first_char == '\t' {
                tab_votes += 1;
            } else {
                space_votes += 1;
            }
        }
    }

    // Determine style
    let style = if tab_votes > space_votes {
        Style::Tab
    } else {
        Style::Space
    };

    // Determine width
    let width = if !width_candidates.is_empty() {
        // Find mode: highest count, ties → smallest width
        let mut best_width = 0;
        let mut best_count = 0u32;

        for (width, count) in &width_candidates {
            if *count > best_count || (*count == best_count && *width < best_width) {
                best_width = *width;
                best_count = *count;
            }
        }

        best_width
    } else {
        // No width candidates: fallback to 4
        // This happens when:
        // - No lines with 1..=8 space runs
        // - Only tab lines
        // - Only 9..=16 space lines (alignment padding)
        // - No usable lines at all
        4
    };

    // If no usable lines at all, return fallback
    if !has_usable_line {
        return Unit::fallback();
    }

    // For Tab style with no space candidates, width is still 4 (fallback for tabs)
    // This is correct per spec: tabs always use width from detection or fallback
    Unit { style, width }
}

/// Compute the previous tab stop for dedenting.
///
/// Normative algorithm from FEUX-0005 §5.2:
/// For a line with `n` leading spaces at width `w`:
/// - Remove `n - floor((n - 1) / w) * w` spaces
///
/// Examples at width 4:
/// - n=4: 4 - floor(3/4)*4 = 4 - 0 = 4 → remove 4 (to column 0, previous tab stop)
/// - n=5: 5 - floor(4/4)*4 = 5 - 4 = 1 → remove 1 (to column 4)
/// - n=2: 2 - floor(1/4)*4 = 2 - 0 = 2 → remove 2 (to column 0)
/// - n=7: 7 - floor(6/4)*4 = 7 - 4 = 3 → remove 3 (to column 4)
///
/// For tab characters: remove the first tab (counts as width columns visually).
///
/// For mixed runs (spaces then tab): remove the first tab, keep preceding spaces.
///
/// Returns the new leading whitespace string after removing one unit of indentation.
#[must_use]
pub fn dedent_step(leading_ws: &str, width: u8) -> String {
    if leading_ws.is_empty() {
        return String::new();
    }

    let chars: Vec<char> = leading_ws.chars().collect();

    // Find first tab
    if let Some(_first_tab_idx) = chars.iter().position(|&c| c == '\t') {
        // Remove the first tab
        let mut new_chars = chars;
        new_chars.remove(_first_tab_idx);
        return new_chars.into_iter().collect();
    }

    // All spaces
    let n = chars.len();
    if n == 0 {
        return String::new();
    }

    let w = width as usize;
    // Number of spaces to remove: n - floor((n - 1) / w) * w
    let removal = n - ((n - 1) / w) * w;
    // Remove from the end (right side)
    if removal >= n {
        String::new()
    } else {
        chars.into_iter().skip(removal).collect()
    }
}

/// Compute the number of characters to remove for dedent (for testing/legacy).
/// This is the amount that would be removed from the END of the whitespace.
#[must_use]
pub fn dedent_amount(leading_ws: &str, width: u8) -> usize {
    if leading_ws.is_empty() {
        return 0;
    }

    let chars: Vec<char> = leading_ws.chars().collect();

    // Find first tab
    if let Some(_first_tab_idx) = chars.iter().position(|&c| c == '\t') {
        // Remove the first tab only
        return 1;
    }

    // All spaces
    let n = chars.len();
    if n == 0 {
        return 0;
    }

    let w = width as usize;
    // n - floor((n - 1) / w) * w
    n - ((n - 1) / w) * w
}

/// Check if a line is empty or whitespace-only.
#[must_use]
pub fn is_blank_or_ws(line: &str) -> bool {
    line.trim().is_empty()
}

/// Check if a line is whitespace-only (may contain only whitespace).
#[must_use]
pub fn is_whitespace_only(line: &str) -> bool {
    line.chars().all(|c| c == ' ' || c == '\t')
}

/// Extract leading whitespace from a line.
#[must_use]
pub fn leading_whitespace(line: &str) -> String {
    line.chars()
        .take_while(|c| *c == '\t' || *c == ' ')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_unit_pure_tab() {
        let text = "\tfn main() {\n\t\tprintln!(\"hello\");\n\t}\n";
        let unit = detect_unit(text);
        assert_eq!(unit.style, Style::Tab);
        assert_eq!(unit.width, 4); // fallback width for pure tabs
    }

    #[test]
    fn test_detect_unit_4_space() {
        let text = "    fn main() {\n        println!(\"hello\");\n    }\n";
        let unit = detect_unit(text);
        assert_eq!(unit.style, Style::Space);
        assert_eq!(unit.width, 4);
    }

    #[test]
    fn test_detect_unit_2_space() {
        let text = "  fn main() {\n    println!(\"hello\");\n  }\n";
        let unit = detect_unit(text);
        assert_eq!(unit.style, Style::Space);
        assert_eq!(unit.width, 2);
    }

    #[test]
    fn test_detect_unit_mixed_tab_space() {
        // More tab votes than space votes
        let text = "\tfn a() {}\n\tfn b() {}\n  fn c() {}\n";
        let unit = detect_unit(text);
        assert_eq!(unit.style, Style::Tab);
        // width is determined from space lines' width candidates
        // Here we have one space line with 2 spaces, so width = 2
        assert_eq!(unit.width, 2);
    }

    #[test]
    fn test_detect_unit_mixed_space_tab() {
        // More space votes than tab votes
        let text = "  fn a() {}\n  fn b() {}\n\tfn c() {}\n";
        let unit = detect_unit(text);
        assert_eq!(unit.style, Style::Space);
        assert_eq!(unit.width, 2);
    }

    #[test]
    fn test_detect_unit_fallback() {
        // No indentation at all
        let text = "fn main() {\nprintln!(\"hello\");\n}\n";
        let unit = detect_unit(text);
        assert_eq!(unit.style, Style::Space);
        assert_eq!(unit.width, 4);
    }

    #[test]
    fn test_detect_unit_alignment_padding_excluded() {
        // 12 spaces should not contribute to width candidates
        // The line "    let x:    i32 = 1;" has 4 leading spaces
        // The spaces after : are not leading, so only 4 spaces count
        let text = "    fn main() {\n    let x:    i32 = 1;\n}\n";
        let unit = detect_unit(text);
        assert_eq!(unit.style, Style::Space);
        assert_eq!(unit.width, 4);
    }

    #[test]
    fn test_detect_unit_mode_smallest_tie() {
        // Equal counts for width 2 and 4, should pick smallest (2)
        let text = "  a\n  b\n    c\n    d\n";
        let unit = detect_unit(text);
        assert_eq!(unit.style, Style::Space);
        // 2 appears twice, 4 appears twice → smallest wins
        assert_eq!(unit.width, 2);
    }

    #[test]
    fn test_dedent_amount_spaces() {
        // width = 4
        // Per spec E-3: dedent to previous tab stop
        // n=4 at width=4: previous tab stop is 0, so remove 4
        assert_eq!(dedent_amount("    ", 4), 4); // at tab stop 4, dedent to 0
        assert_eq!(dedent_amount("     ", 4), 1); // 5 spaces → remove 1 to get to 4
        assert_eq!(dedent_amount("  ", 4), 2); // 2 spaces → remove 2 to get to 0
        assert_eq!(dedent_amount("   ", 4), 3); // 3 spaces → remove 3 to get to 0
        // n=7, w=4: 7 - floor(6/4)*4 = 7 - 1*4 = 3, previous tab stop is 4
        assert_eq!(dedent_amount("       ", 4), 3); // 7 spaces → remove 3 to get to 4
    }

    #[test]
    fn test_dedent_amount_tabs() {
        assert_eq!(dedent_amount("\t", 4), 1); // remove the tab
        assert_eq!(dedent_amount("\t\t", 4), 1); // remove first tab only
        assert_eq!(dedent_amount("  \t", 4), 1); // mixed: remove the tab
    }

    #[test]
    fn test_dedent_amount_empty() {
        assert_eq!(dedent_amount("", 4), 0);
    }

    #[test]
    fn test_unit_text() {
        let tab_unit = Unit {
            style: Style::Tab,
            width: 4,
        };
        assert_eq!(tab_unit.unit_text(), "\t");
        assert_eq!(tab_unit.text(3), "\t\t\t");

        let space_unit = Unit {
            style: Style::Space,
            width: 2,
        };
        assert_eq!(space_unit.unit_text(), "  ");
        assert_eq!(space_unit.text(3), "      ");
    }

    #[test]
    fn test_unit_display() {
        let tab_unit = Unit {
            style: Style::Tab,
            width: 4,
        };
        assert_eq!(tab_unit.display(), "tabs");

        let space_unit = Unit {
            style: Style::Space,
            width: 4,
        };
        assert_eq!(space_unit.display(), "4 spaces");
    }

    #[test]
    fn test_detect_unit_8_cap() {
        // 9 spaces should vote for Space but not contribute to width
        let text = "         x\n"; // 9 spaces
        let unit = detect_unit(text);
        assert_eq!(unit.style, Style::Space);
        assert_eq!(unit.width, 4); // fallback because no 1..=8 candidates
    }

    #[test]
    fn test_detect_unit_only_alignment_lines() {
        // Only lines with 9+ spaces (alignment)
        let text = "         x\n          y\n"; // 9 and 10 spaces
        let unit = detect_unit(text);
        assert_eq!(unit.style, Style::Space);
        assert_eq!(unit.width, 4); // fallback
    }
}
