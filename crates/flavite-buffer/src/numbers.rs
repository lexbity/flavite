//! Number scanning and operations for FEUX-0004 X02.
//!
//! This module provides the number scanner and increment/decrement/sequence
//! operations as specified in E-9.

use crate::selection::Selection;
use flavite_syntax::text::word_boundaries;

/// Typed planning refusal for number operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NumberError {
    /// No number found in the selection.
    NoNumber,
    /// Number overflow/underflow when incrementing/decrementing.
    Overflow { value: String, delta: i64 },
    /// For NumberSequence: one or more selections contain no number.
    SequenceNoNumber,
}

/// Result of scanning for a number in a selection.
#[derive(Debug, Clone)]
pub struct NumberScanResult {
    /// Byte start of the number (including optional sign).
    pub start: usize,
    /// Byte end of the number (exclusive).
    pub end: usize,
    /// The parsed numeric value.
    pub value: i64,
    /// Whether the number has leading zeros.
    pub has_leading_zeros: bool,
    /// The original width of the number (digits only, not including sign).
    pub digit_width: usize,
    /// Whether the number has a sign.
    pub has_sign: bool,
}

/// Scan for the first integer in a selection's range.
///
/// For non-empty members: leftmost maximal run `[+-]?[0-9]+` inside the range.
/// For carets: scan outward from caret within UAX #29 word boundaries.
///
/// Returns `None` if no number is found.
#[must_use]
pub fn scan_number(text: &str, member: &Selection) -> Option<NumberScanResult> {
    if member.is_caret() {
        scan_number_at_caret(text, member.head())
    } else {
        scan_number_in_range(text, member.start(), member.end())
    }
}

/// Scan for a number at a caret position using UAX #29 word boundaries.
fn scan_number_at_caret(text: &str, caret: usize) -> Option<NumberScanResult> {
    if caret >= text.len() {
        return None;
    }

    // Get all word boundaries
    let boundaries = word_boundaries(text);

    // Find the word segment containing the caret
    let mut segment_start = 0usize;
    for &boundary in boundaries.iter() {
        if boundary > caret {
            // The caret is in the segment [segment_start, boundary)
            let segment = &text[segment_start..boundary];
            if let Some(result) = scan_number_in_slice(segment) {
                return Some(NumberScanResult {
                    start: segment_start + result.start,
                    end: segment_start + result.end,
                    ..result
                });
            }
            return None;
        }
        segment_start = boundary;
    }

    None
}

/// Scan for the first integer in a byte range.
fn scan_number_in_range(text: &str, start: usize, end: usize) -> Option<NumberScanResult> {
    let bytes = text.as_bytes();
    let mut pos = start;

    while pos < end {
        let c = bytes[pos];
        if c == b'+' || c == b'-' {
            // Potential sign
            if pos + 1 < end && bytes[pos + 1].is_ascii_digit() {
                // Found sign + digit, scan the full number
                return scan_number_from(text, pos, end);
            }
            pos += 1;
        } else if c.is_ascii_digit() {
            // Found digit, scan the full number
            return scan_number_from(text, pos, end);
        } else {
            pos += 1;
        }
    }

    None
}

/// Scan a number starting from a given position within bounds.
fn scan_number_from(text: &str, start: usize, bound: usize) -> Option<NumberScanResult> {
    let bytes = text.as_bytes();
    let pos = start;

    // Check for sign
    let has_sign = if pos < bound {
        let c = bytes[pos];
        (c == b'+' || c == b'-') && (pos + 1 < bound && bytes[pos + 1].is_ascii_digit())
    } else {
        false
    };

    let digit_start = if has_sign { pos + 1 } else { pos };

    // Scan digits
    let mut digit_end = digit_start;
    while digit_end < bound && bytes[digit_end].is_ascii_digit() {
        digit_end += 1;
    }

    if digit_end == digit_start {
        // No digits found
        return None;
    }

    // Parse the number
    let num_str = &text[start..digit_end];
    let value: i64 = num_str.parse().ok()?;

    // Check for leading zeros
    let digit_slice = &text[digit_start..digit_end];
    let has_leading_zeros = digit_slice.starts_with('0') && digit_slice.len() > 1;
    let digit_width = digit_slice.len();

    Some(NumberScanResult {
        start,
        end: digit_end,
        value,
        has_leading_zeros,
        digit_width,
        has_sign,
    })
}

/// Scan a number in a string slice (used for word boundary segments).
fn scan_number_in_slice(text: &str) -> Option<NumberScanResult> {
    let bytes = text.as_bytes();
    let mut pos = 0;

    while pos < bytes.len() {
        let c = bytes[pos];
        if c == b'+' || c == b'-' {
            // Potential sign
            if pos + 1 < bytes.len() && bytes[pos + 1].is_ascii_digit() {
                return scan_number_from(text, pos, bytes.len());
            }
            pos += 1;
        } else if c.is_ascii_digit() {
            return scan_number_from(text, pos, bytes.len());
        } else {
            pos += 1;
        }
    }

    None
}

/// Increment or decrement a number, preserving leading zeros.
///
/// Returns the replacement string and its byte range, or an error if overflow.
pub fn increment_number(
    text: &str,
    scan_result: &NumberScanResult,
    delta: i64,
) -> Result<String, NumberError> {
    // Check for overflow
    let new_value = scan_result
        .value
        .checked_add(delta)
        .ok_or_else(|| NumberError::Overflow {
            value: text[scan_result.start..scan_result.end].to_string(),
            delta,
        })?;

    // Format with leading zero preservation
    let sign_char = if scan_result.has_sign {
        if new_value < 0 {
            "-"
        } else if new_value > 0 {
            "+"
        } else {
            "" // Don't show sign for zero
        }
    } else {
        ""
    };

    let new_str = new_value.to_string();

    if scan_result.has_sign {
        let new_abs = new_value.abs();
        let new_abs_str = new_abs.to_string();

        // Check if we should preserve width
        if scan_result.has_leading_zeros && scan_result.digit_width > new_abs_str.len() {
            let width = scan_result.digit_width;
            let padded = format!("{:0>width$}", new_abs, width = width);
            Ok(format!("{}{}", sign_char, padded))
        } else {
            Ok(format!("{}{}", sign_char, new_abs_str))
        }
    } else {
        // Preserve leading zeros: if original had leading zeros and new value fits
        if scan_result.has_leading_zeros && scan_result.digit_width > new_str.len() {
            let width = scan_result.digit_width;
            Ok(format!("{:0>width$}", new_value, width = width))
        } else {
            Ok(new_str)
        }
    }
}

/// Extract the number range from a scan result.
#[must_use]
pub fn number_range(scan_result: &NumberScanResult) -> (usize, usize) {
    (scan_result.start, scan_result.end)
}

/// Get the string representation of a number from text and scan result.
#[must_use]
pub fn number_str<'a>(text: &'a str, scan_result: &NumberScanResult) -> &'a str {
    &text[scan_result.start..scan_result.end]
}

/// Extract the first integer from a string (E-8 numeric sort key).
/// The first integer is the leftmost maximal run `[+-]?[0-9]+` anywhere in
/// the line (sign adjacent only), matching the E-9 scanner's sign rules.
/// Lines without an integer yield `None`.
#[must_use]
pub fn extract_first_number(line: &str) -> Option<i64> {
    let bytes = line.as_bytes();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let c = bytes[pos];
        if c == b'+' || c == b'-' {
            if pos + 1 < bytes.len() && bytes[pos + 1].is_ascii_digit() {
                let sign = if c == b'-' { -1i64 } else { 1i64 };
                let mut digits = String::new();
                for ch in line[pos + 1..].chars() {
                    if ch.is_ascii_digit() {
                        digits.push(ch);
                    } else {
                        break;
                    }
                }
                let num: i64 = digits.parse().unwrap_or(0);
                return Some(sign * num);
            }
            pos += 1;
        } else if c.is_ascii_digit() {
            let mut digits = String::new();
            for ch in line[pos..].chars() {
                if ch.is_ascii_digit() {
                    digits.push(ch);
                } else {
                    break;
                }
            }
            let num: i64 = digits.parse().unwrap_or(0);
            return Some(num);
        } else {
            pos += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::RawSelection;
    use crate::selection::SelectionSet;

    fn set_of(text: &str, range: std::ops::Range<usize>) -> SelectionSet {
        SelectionSet::normalize(
            text,
            vec![RawSelection {
                id: None,
                anchor: range.start,
                head: range.end,
            }],
            None,
        )
        .unwrap()
        .0
    }

    fn set_caret(text: &str, pos: usize) -> SelectionSet {
        SelectionSet::normalize(
            text,
            vec![RawSelection {
                id: None,
                anchor: pos,
                head: pos,
            }],
            None,
        )
        .unwrap()
        .0
    }

    // E-9 spec tests
    #[test]
    fn scan_increments_single_number() {
        let text = "The answer is 42";
        let set = set_of(text, 14..16);
        let member = set.iter().next().unwrap();
        let scan_result = scan_number(text, member).unwrap();
        assert_eq!(scan_result.value, 42);
        assert_eq!(scan_result.start, 14);
        assert_eq!(scan_result.end, 16);
    }

    #[test]
    fn scan_decrements_single_number() {
        let text = "Count: 10";
        let set = set_of(text, 7..9);
        let member = set.iter().next().unwrap();
        let scan_result = scan_number(text, member).unwrap();
        assert_eq!(scan_result.value, 10);
    }

    #[test]
    fn scan_preserves_leading_zeros() {
        let text = "Count: 007";
        let set = set_of(text, 7..10);
        let member = set.iter().next().unwrap();
        let scan_result = scan_number(text, member).unwrap();
        assert_eq!(scan_result.value, 7);
        assert!(scan_result.has_leading_zeros);
        assert_eq!(scan_result.digit_width, 3);

        // Test increment preserves width
        let replacement = increment_number(text, &scan_result, 1).unwrap();
        assert_eq!(replacement, "008");
    }

    #[test]
    fn scan_width_grows_past_capacity() {
        let text = "099";
        let set = set_of(text, 0..3);
        let member = set.iter().next().unwrap();
        let scan_result = scan_number(text, member).unwrap();
        assert_eq!(scan_result.value, 99);

        let replacement = increment_number(text, &scan_result, 1).unwrap();
        assert_eq!(replacement, "100");
    }

    #[test]
    fn scan_mid_word_digits() {
        let text = "abc123def";
        let set = set_of(text, 0..text.len());
        let member = set.iter().next().unwrap();
        let scan_result = scan_number(text, member).unwrap();
        // Leftmost number in range is 123
        assert_eq!(scan_result.value, 123);
        assert_eq!(scan_result.start, 3);
        assert_eq!(scan_result.end, 6);
    }

    #[test]
    fn scan_caret_forward() {
        let text = "abc123def";
        let set = set_caret(text, 3);
        let member = set.iter().next().unwrap();
        let scan_result = scan_number(text, member).unwrap();
        assert_eq!(scan_result.value, 123);
    }

    #[test]
    fn scan_caret_backward() {
        let text = "123abc";
        let set = set_caret(text, 3);
        let member = set.iter().next().unwrap();
        let scan_result = scan_number(text, member).unwrap();
        assert_eq!(scan_result.value, 123);
    }

    #[test]
    fn scan_sign_must_be_adjacent() {
        let text = "a -5";
        let set = set_of(text, 0..4);
        let member = set.iter().next().unwrap();
        let scan_result = scan_number(text, member).unwrap();
        // The "-" is followed by "5" with no gap, so -5 is found
        assert_eq!(scan_result.value, -5);
    }

    #[test]
    fn scan_sign_not_adjacent_no_number() {
        let text = "a- 5";
        let set = set_of(text, 0..text.len());
        let member = set.iter().next().unwrap();
        let scan_result = scan_number(text, member);
        // Sign not adjacent to digits - should find "5" as a number
        assert!(scan_result.is_some());
        assert_eq!(scan_result.unwrap().value, 5);
    }

    #[test]
    fn scan_dec_minus_zero() {
        let text = "-0";
        let set = set_of(text, 0..2);
        let member = set.iter().next().unwrap();
        let scan_result = scan_number(text, member).unwrap();
        assert_eq!(scan_result.value, 0);

        let replacement = increment_number(text, &scan_result, -1).unwrap();
        assert_eq!(replacement, "-1");
    }

    #[test]
    fn scan_overflow_refuses() {
        let text = "9223372036854775807";
        let set = set_of(text, 0..text.len());
        let member = set.iter().next().unwrap();
        let scan_result = scan_number(text, member).unwrap();
        assert_eq!(scan_result.value, i64::MAX);

        // i64::MAX + 1 should overflow
        let result = increment_number(text, &scan_result, 1);
        assert!(matches!(result, Err(NumberError::Overflow { .. })));
    }

    #[test]
    fn scan_underflow_refuses() {
        let text = "-9223372036854775808";
        let set = set_of(text, 0..text.len());
        let member = set.iter().next().unwrap();
        let scan_result = scan_number(text, member).unwrap();
        assert_eq!(scan_result.value, i64::MIN);

        // i64::MIN - 1 should underflow
        let result = increment_number(text, &scan_result, -1);
        assert!(matches!(result, Err(NumberError::Overflow { .. })));
    }
}
