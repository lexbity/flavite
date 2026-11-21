//! Pair scanning for FEUX-0004 X04 / E-7: surround operations.
//!
//! This module provides the pair-scanner for finding matching delimiter pairs
//! around selections. It supports both nesting pairs (parentheses, braces, brackets,
//! angle brackets) and self-pairs (quotes).

use crate::selection::{Selection, SelectionSet};

/// Pair characters: nesting pairs and self-pairs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pair {
    /// The opening character.
    pub open: char,
    /// The closing character.
    pub close: char,
    /// Whether this is a nesting pair (vs self-pair).
    pub is_nesting: bool,
}

impl Pair {
    /// Returns true if this is a self-pair (open == close).
    #[must_use]
    pub const fn is_self_pair(&self) -> bool {
        self.open == self.close
    }

    /// Get a pair by its opening character.
    #[must_use]
    pub fn from_open(c: char) -> Option<Self> {
        match c {
            '(' => Some(Self {
                open: '(',
                close: ')',
                is_nesting: true,
            }),
            '[' => Some(Self {
                open: '[',
                close: ']',
                is_nesting: true,
            }),
            '{' => Some(Self {
                open: '{',
                close: '}',
                is_nesting: true,
            }),
            '<' => Some(Self {
                open: '<',
                close: '>',
                is_nesting: true,
            }),
            '"' | '\'' | '`' => Some(Self {
                open: c,
                close: c,
                is_nesting: false,
            }),
            _ => None,
        }
    }

    /// Get a pair by its closing character (for self-pairs, same as open).
    #[must_use]
    pub fn from_close(c: char) -> Option<Self> {
        match c {
            ')' => Some(Self {
                open: '(',
                close: ')',
                is_nesting: true,
            }),
            ']' => Some(Self {
                open: '[',
                close: ']',
                is_nesting: true,
            }),
            '}' => Some(Self {
                open: '{',
                close: '}',
                is_nesting: true,
            }),
            '>' => Some(Self {
                open: '<',
                close: '>',
                is_nesting: true,
            }),
            '"' | '\'' | '`' => Some(Self {
                open: c,
                close: c,
                is_nesting: false,
            }),
            _ => None,
        }
    }
}

/// Result of scanning for an enclosing pair around a selection member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairMatch {
    /// Byte start of the opening delimiter.
    pub open_start: usize,
    /// Byte end of the opening delimiter (exclusive).
    pub open_end: usize,
    /// Byte start of the closing delimiter.
    pub close_start: usize,
    /// Byte end of the closing delimiter (exclusive).
    pub close_end: usize,
}

/// Scan for an enclosing pair around a selection member.
///
/// For nesting pairs: finds the innermost matching pair at depth 0 that
/// encloses the member's range.
/// For self-pairs: finds the nearest enclosing quote pair.
///
/// Returns `Some(PairMatch)` if a matching pair is found, `None` otherwise.
#[must_use]
pub fn find_enclosing_pair(text: &str, member: &Selection, pair: &Pair) -> Option<PairMatch> {
    if pair.is_self_pair() {
        find_enclosing_self_pair(text, member, pair)
    } else {
        find_enclosing_nesting_pair(text, member, pair)
    }
}

/// Find enclosing self-pair (quotes).
fn find_enclosing_self_pair(text: &str, member: &Selection, pair: &Pair) -> Option<PairMatch> {
    let quote_char = pair.open;
    let bytes = text.as_bytes();
    let member_start = member.start();
    let member_end = member.end();

    // Find the nearest quote before the member
    let mut open_pos: Option<usize> = None;
    for pos in (0..member_start).rev() {
        if bytes[pos] == quote_char as u8 {
            // Check if this quote is not escaped (simple check: odd number of backslashes before it)
            if !is_escaped(text, pos) {
                open_pos = Some(pos);
                break;
            }
        }
    }

    let open_pos = open_pos?;

    // Find the matching close after the member
    let mut depth = 0;
    let mut found_open = false;
    let mut close_pos: Option<usize> = None;

    for (pos, &byte) in bytes
        .iter()
        .enumerate()
        .skip(open_pos)
        .take(text.len() - open_pos)
    {
        if byte == quote_char as u8 && !is_escaped(text, pos) {
            if !found_open && pos == open_pos {
                found_open = true;
                depth = 1;
            } else if found_open {
                depth += 1;
                if depth % 2 == 0 {
                    // Found matching close
                    if pos >= member_end {
                        close_pos = Some(pos);
                        break;
                    }
                }
            }
        }
    }

    let close_pos = close_pos?;

    // Verify the pair encloses the member
    if open_pos < member_start && close_pos >= member_end {
        Some(PairMatch {
            open_start: open_pos,
            open_end: open_pos + quote_char.len_utf8(),
            close_start: close_pos,
            close_end: close_pos + quote_char.len_utf8(),
        })
    } else {
        None
    }
}

/// Check if a character at position is escaped (preceded by odd number of backslashes).
fn is_escaped(text: &str, pos: usize) -> bool {
    if pos == 0 {
        return false;
    }
    let bytes = text.as_bytes();
    let mut backslash_count = 0;
    let mut i = pos;
    while i > 0 && bytes[i - 1] == b'\\' {
        backslash_count += 1;
        i -= 1;
    }
    backslash_count % 2 == 1
}

/// Find enclosing nesting pair (parentheses, braces, brackets, angle brackets).
///
/// §5.5: walk left from `member.start` collecting open delimiters at depth 0
/// (closes count +1), and right from `member.end` collecting closes at depth
/// 0 (opens count +1). The innermost valid pair is the nearest open/close
/// candidate pair whose interval is balanced; a pair whose delimiters are
/// inside the member (or that crosses sibling pairs) fails the interval
/// check and is not reported — mismatch is a refusal, never a silent match.
fn find_enclosing_nesting_pair(text: &str, member: &Selection, pair: &Pair) -> Option<PairMatch> {
    let open_char = pair.open;
    let close_char = pair.close;
    let bytes = text.as_bytes();
    let member_start = member.start();
    let member_end = member.end();

    // Candidate opens: nearest-first (ascending by position) at depth 0 when
    // walking left strictly below member.start.
    let mut open_candidates: Vec<usize> = Vec::new();
    let mut balance = 0i32;
    let mut pos = member_start;
    while pos > 0 {
        pos -= 1;
        if bytes[pos] as char == close_char {
            balance += 1;
        } else if bytes[pos] as char == open_char {
            if balance == 0 {
                open_candidates.push(pos);
            } else {
                balance -= 1;
            }
        }
    }

    // Candidate closes: nearest-first at depth 0 when walking right from
    // member.end (inclusive of the boundary position).
    let mut close_candidates: Vec<usize> = Vec::new();
    let mut balance = 0i32;
    let mut pos = member_end;
    while pos < bytes.len() {
        if bytes[pos] as char == open_char {
            balance += 1;
        } else if bytes[pos] as char == close_char {
            if balance == 0 {
                close_candidates.push(pos);
            } else {
                balance -= 1;
            }
        }
        pos += 1;
    }

    // A candidate pair is valid iff its interior [open+1, close) is balanced:
    // net zero opens/closes with a non-negative prefix (no unclosed opens and
    // no closes that would close an earlier sibling).
    for &open_pos in &open_candidates {
        for &close_pos in &close_candidates {
            if close_pos <= open_pos {
                continue;
            }
            let mut net = 0i32;
            let mut valid = true;
            for &b in &bytes[open_pos + 1..close_pos] {
                if b as char == open_char {
                    net += 1;
                } else if b as char == close_char {
                    net -= 1;
                    if net < 0 {
                        valid = false;
                        break;
                    }
                }
            }
            if valid && net == 0 {
                return Some(PairMatch {
                    open_start: open_pos,
                    open_end: open_pos + open_char.len_utf8(),
                    close_start: close_pos,
                    close_end: close_pos + close_char.len_utf8(),
                });
            }
        }
    }

    None
}

/// Validation result for surround operations across multiple selections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurroundValidation {
    /// All selections have valid enclosing pairs.
    Valid,
    /// Some selections have no enclosing pair.
    MissingPairs { count: usize, total: usize },
}

/// Validate that all selections have enclosing pairs for a given pair character.
///
/// Returns `Valid` if all have pairs, or `MissingPairs` with counts if any don't.
#[must_use]
pub fn validate_enclosing_pairs(text: &str, set: &SelectionSet, pair: &Pair) -> SurroundValidation {
    let mut missing_count = 0;
    for member in set.iter() {
        if find_enclosing_pair(text, member, pair).is_none() {
            missing_count += 1;
        }
    }
    if missing_count == 0 {
        SurroundValidation::Valid
    } else {
        SurroundValidation::MissingPairs {
            count: missing_count,
            total: set.len(),
        }
    }
}

/// Get the pair for a character (used when user provides a single char).
#[must_use]
pub fn pair_for_char(c: char) -> Pair {
    Pair::from_open(c).unwrap_or(Pair {
        open: c,
        close: c,
        is_nesting: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::{RawSelection, SelectionSet};

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

    #[allow(dead_code)]
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

    #[test]
    fn pair_from_open_returns_nesting_pairs() {
        assert!(Pair::from_open('(').is_some());
        assert!(Pair::from_open('[').is_some());
        assert!(Pair::from_open('{').is_some());
        assert!(Pair::from_open('<').is_some());
        assert!(Pair::from_open('"').is_some());
        assert!(Pair::from_open('\'').is_some());
        assert!(Pair::from_open('`').is_some());
        assert!(Pair::from_open('x').is_none());
    }

    #[test]
    fn nesting_pair_finds_innermost() {
        let text = "(outer (inner) outer)";
        let set = set_of(text, 8..13); // "inner" without its delimiters
        let member = set.iter().next().unwrap();
        let pair = Pair::from_open('(').unwrap();
        let result = find_enclosing_pair(text, member, &pair);
        assert!(result.is_some());
        let match_ = result.unwrap();
        assert_eq!(match_.open_start, 7);
        assert_eq!(match_.close_start, 13);
    }

    #[test]
    fn whole_text_member_has_no_enclosing_pair() {
        // §5.5 scans strictly outward from the member's range; a member
        // covering the whole buffer has nothing outside it, so the pairing
        // is NotFound (the delimiters are inside the member, not enclosing).
        let text = "(outer (inner) outer)";
        let set = set_of(text, 0..text.len()); // whole text
        let member = set.iter().next().unwrap();
        let pair = Pair::from_open('(').unwrap();
        let result = find_enclosing_pair(text, member, &pair);
        assert!(result.is_none());
    }

    #[test]
    fn member_including_delimiters_has_no_enclosing_pair() {
        // A member that already contains its delimiters is not enclosed; the
        // scan is outward-only (E-7), so "(first)" selected whole refuses.
        let text = "(first)";
        let set = set_of(text, 0..7);
        let member = set.iter().next().unwrap();
        let pair = Pair::from_open('(').unwrap();
        let result = find_enclosing_pair(text, member, &pair);
        assert!(result.is_none());
    }

    #[test]
    fn self_pair_finds_quotes() {
        let text = "\"quoted text\"";
        let set = set_of(text, 1..11); // "quoted text" without quotes
        let member = set.iter().next().unwrap();
        let pair = Pair::from_open('"').unwrap();
        let result = find_enclosing_pair(text, member, &pair);
        assert!(result.is_some());
        let match_ = result.unwrap();
        assert_eq!(match_.open_start, 0);
        assert_eq!(match_.close_start, 12);
    }

    #[test]
    fn no_pair_returns_none() {
        let text = "no delimiters here";
        let set = set_of(text, 0..text.len());
        let member = set.iter().next().unwrap();
        let pair = Pair::from_open('(').unwrap();
        let result = find_enclosing_pair(text, member, &pair);
        assert!(result.is_none());
    }

    #[test]
    fn mismatched_pair_returns_none() {
        let text = "(unclosed";
        let set = set_of(text, 0..text.len());
        let member = set.iter().next().unwrap();
        let pair = Pair::from_open('(').unwrap();
        let result = find_enclosing_pair(text, member, &pair);
        assert!(result.is_none());
    }

    #[test]
    fn angle_brackets_work() {
        // Properly nested angle brackets: the innermost enclosing pair around
        // "b" is the (4, 8) pair.
        let text = "< a < b > c >";
        let set = set_of(text, 6..7); // "b"
        let member = set.iter().next().unwrap();
        let pair = Pair::from_open('<').unwrap();
        let result = find_enclosing_pair(text, member, &pair);
        assert!(result.is_some());
        let match_ = result.unwrap();
        assert_eq!(match_.open_start, 4);
        assert_eq!(match_.close_start, 8);
    }

    #[test]
    fn sibling_angle_brackets_are_not_nested() {
        // "<tag>content</tag>" is two sibling pairs, not nesting; a member
        // between them has no enclosing bracket pair (E-7 refusal, never a
        // silent cross-pair match).
        let text = "<tag>content</tag>";
        let set = set_of(text, 5..12); // "content"
        let member = set.iter().next().unwrap();
        let pair = Pair::from_open('<').unwrap();
        let result = find_enclosing_pair(text, member, &pair);
        assert!(result.is_none());
    }

    #[test]
    fn mixed_pairs_independently() {
        // Each pair type resolves its own innermost enclosing pair around the
        // same member, independently of the other bracket types nearby.
        let text = "{ [ ( ) ] }";
        let set = set_of(text, 5..6); // the space between ( and )
        let member = set.iter().next().unwrap();
        for (open, close, open_at, close_at) in [
            ('(', ')', 4usize, 6usize),
            ('[', ']', 2, 8),
            ('{', '}', 0, 10),
        ] {
            let pair = Pair::from_open(open).unwrap();
            let result = find_enclosing_pair(text, member, &pair);
            assert!(
                result.is_some(),
                "Should find pair for {open}{close} around the member"
            );
            let match_ = result.unwrap();
            assert_eq!(match_.open_start, open_at, "open for {open}");
            assert_eq!(match_.close_start, close_at, "close for {close}");
        }
    }

    #[test]
    fn validate_all_have_pairs() {
        let text = "(first) (second)";
        let set = SelectionSet::normalize(
            text,
            vec![
                RawSelection {
                    id: None,
                    anchor: 1,
                    head: 6,
                },
                RawSelection {
                    id: None,
                    anchor: 9,
                    head: 15,
                },
            ],
            None,
        )
        .unwrap()
        .0;
        let pair = Pair::from_open('(').unwrap();
        let result = validate_enclosing_pairs(text, &set, &pair);
        assert!(matches!(result, SurroundValidation::Valid));
    }

    #[test]
    fn validate_missing_pairs() {
        let text = "(first) second";
        let set = SelectionSet::normalize(
            text,
            vec![
                RawSelection {
                    id: None,
                    anchor: 1,
                    head: 6,
                },
                RawSelection {
                    id: None,
                    anchor: 8,
                    head: 14,
                },
            ],
            None,
        )
        .unwrap()
        .0;
        let pair = Pair::from_open('(').unwrap();
        let result = validate_enclosing_pairs(text, &set, &pair);
        assert!(matches!(
            result,
            SurroundValidation::MissingPairs { count: 1, total: 2 }
        ));
    }

    #[test]
    fn escaped_quotes_not_counted() {
        let text = "\"\\\"escaped\\\"\"";
        // bytes: 0 '"' 1 '\\' 2 '"' 3..10 "escaped" 10 '\\' 11 '"' 12 '"'
        let set = set_of(text, 3..10); // "escaped"
        let member = set.iter().next().unwrap();
        let pair = Pair::from_open('"').unwrap();
        let result = find_enclosing_pair(text, member, &pair);
        // The outer quotes should match; the escaped inner quotes are skipped.
        assert!(result.is_some());
        let match_ = result.unwrap();
        assert_eq!(match_.open_start, 0);
        assert_eq!(match_.close_start, 12);
    }
}
