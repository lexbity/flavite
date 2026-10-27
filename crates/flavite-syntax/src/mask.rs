//! Exclusion masks: byte ranges whose contents are *not* Markdown prose.
//!
//! [`TextMask`] marks code spans, code blocks, raw HTML and math. Both parse
//! paths ([`crate::pulldown_glue`] and [`crate::ts_ranges`]) build the same
//! mask over the same content, which is what makes the scanner's link/tag
//! rules agree between the indexer parse and the editor parse (RFC-0001 §4.3
//! acceptance criterion AC-5).
//!
//! The mask is *exclusion*-based rather than an inclusion list of prose text
//! events: an inclusion list built from pulldown `Text` events would drop
//! emphasis-delimiter characters inside e.g. `[[a*b*]]` and make `*` part of
//! link text in one parse but not the other.

use std::ops::Range;

/// A set of excluded byte ranges over a text of `len` bytes.
///
/// Ranges are merged, sorted and non-overlapping; queries are binary searches
/// plus a prefix sum, so scanning a large note stays cheap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMask {
    /// Merged, sorted, non-overlapping excluded ranges.
    ranges: Vec<Range<usize>>,
    /// `prefix[i]` = total excluded bytes in `ranges[..i]`.
    prefix: Vec<usize>,
    /// Total length of the underlying text in bytes.
    len: usize,
}

impl TextMask {
    /// Builds a mask from arbitrary (possibly overlapping or unsorted) ranges.
    ///
    /// Ranges are clipped to `0..len`, sorted and merged. `O(n log n)` in the
    /// number of ranges.
    #[must_use]
    pub fn new(mut ranges: Vec<Range<usize>>, len: usize) -> Self {
        ranges.retain(|r| r.start < r.end && r.start < len);
        for r in &mut ranges {
            r.end = r.end.min(len);
        }
        ranges.sort_by_key(|r| (r.start, r.end));
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
        for r in ranges {
            match merged.last_mut() {
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => merged.push(r),
            }
        }
        let mut prefix = Vec::with_capacity(merged.len() + 1);
        prefix.push(0usize);
        for r in &merged {
            let total = prefix.last().copied().unwrap_or(0) + (r.end - r.start);
            prefix.push(total);
        }
        Self { ranges: merged, prefix, len }
    }

    /// A mask that excludes nothing.
    #[must_use]
    pub fn empty(len: usize) -> Self {
        Self::new(Vec::new(), len)
    }

    /// Total length of the underlying text in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` when no byte of the text is excluded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Number of merged excluded ranges (diagnostics and tests).
    #[must_use]
    pub fn range_count(&self) -> usize {
        self.ranges.len()
    }

    /// The merged excluded ranges, in ascending order.
    pub fn ranges(&self) -> &[Range<usize>] {
        &self.ranges
    }

    /// `true` when byte offset `pos` falls inside an excluded range.
    ///
    /// Positions at or after the text length are never masked: the exclusive
    /// end of the text is a valid (empty) span boundary.
    #[must_use]
    pub fn is_masked(&self, pos: usize) -> bool {
        if pos >= self.len {
            return false;
        }
        match self.ranges.binary_search_by(|r| {
            if pos < r.start {
                std::cmp::Ordering::Greater
            } else if pos >= r.end {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        }) {
            Ok(_) => true,
            Err(_) => false,
        }
    }

    /// Total number of masked bytes inside `span`.
    #[must_use]
    pub fn masked_length(&self, span: &Range<usize>) -> usize {
        if span.start >= span.end {
            return 0;
        }
        let from = self.index_of(span.start);
        let mut total = 0usize;
        for r in self.ranges.iter().skip(from) {
            if r.start >= span.end {
                break;
            }
            let lo = r.start.max(span.start);
            let hi = r.end.min(span.end);
            if lo < hi {
                total += hi - lo;
            }
        }
        total
    }

    /// The unmasked sub-ranges of `span`, in ascending order.
    ///
    /// Concatenating the text of these sub-ranges yields the prose that the
    /// heading/title extraction keeps.
    #[must_use]
    pub fn unmasked(&self, span: &Range<usize>) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        if span.start >= span.end {
            return out;
        }
        let mut cursor = span.start;
        for r in &self.ranges {
            if r.start >= span.end {
                break;
            }
            if r.end <= cursor {
                continue;
            }
            if cursor < r.start {
                out.push(cursor..r.start);
            }
            cursor = cursor.max(r.end);
        }
        if cursor < span.end {
            out.push(cursor..span.end);
        }
        out
    }

    /// Index of the first merged range whose end is after `pos`.
    fn index_of(&self, pos: usize) -> usize {
        self.ranges.partition_point(|r| r.end <= pos)
    }

    /// Total number of masked bytes before byte offset `pos`.
    #[must_use]
    pub fn masked_before(&self, pos: usize) -> usize {
        if pos == 0 || self.ranges.is_empty() {
            return 0;
        }
        let idx = self.index_of(pos);
        let mut total = self.prefix[idx.min(self.ranges.len())];
        if let Some(r) = self.ranges.get(idx) {
            if r.start < pos {
                total += pos.min(r.end) - r.start;
            }
        }
        total.min(pos).min(self.len)
    }
}

/// Finds math spans in `content`, ignoring `excluded` byte ranges.
///
/// This is a deterministic, dependency-free heuristic shared by *both* parse
/// paths — the tree-sitter Markdown grammar has no math node and pulldown's
/// math extension is not enabled, so both glues apply this masker instead of
/// each inventing their own rules. That is what keeps AC-5 parity exact.
///
/// Rules (`content` is the note body relative to which `excluded` is indexed):
/// * `$$` … `$$` is a display span; a closing `$$` must exist on a later line.
/// * `$` … `$` is an inline span when the opening `$` is followed by a
///   non-space, the closing `$` is preceded by a non-space, and the span does
///   not cross a blank line.
/// * A `$` adjacent to an alphanumeric character (e.g. `5$` or `$5`) is
///   currency text, not a delimiter.
#[must_use]
pub fn math_ranges(content: &str, excluded: &[Range<usize>]) -> Vec<Range<usize>> {
    let bytes = content.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    'outer: while i < bytes.len() {
        if bytes[i] != b'$' || is_within(excluded, i) {
            i += 1;
            continue;
        }
        // Currency guard: `$5` / `5$` are not delimiters.
        let prev = prev_char(content, i);
        let next = next_char(content, i + 1);
        if prev.map_or(false, |c| c.is_alphanumeric())
            || next.map_or(false, |c| c.is_ascii_digit())
        {
            i += 1;
            continue;
        }
        if bytes.get(i + 1) == Some(&b'$') {
            // Display span: find a closing `$$` on a later line.
            let mut j = i + 2;
            while j + 1 < bytes.len() {
                if bytes[j] == b'$' && bytes[j + 1] == b'$' && !is_within(excluded, j) {
                    out.push(i..j + 2);
                    i = j + 2;
                    continue 'outer;
                }
                j += 1;
            }
            i += 2;
            continue;
        }
        // Inline span: closing `$` with non-space neighbours, no blank line.
        let mut j = i + 1;
        while j < bytes.len() {
            match bytes[j] {
                b'\n' => {
                    if is_blank_line_end(content, j) {
                        break;
                    }
                }
                b'$' if !is_within(excluded, j) => {
                    let before = prev_char(content, j);
                    if before.is_some_and(|c| !c.is_whitespace()) {
                        out.push(i..j + 1);
                        i = j + 1;
                        continue 'outer;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        i += 1;
    }
    out
}

/// `true` when `pos` lies inside one of `ranges`.
fn is_within(ranges: &[Range<usize>], pos: usize) -> bool {
    ranges.iter().any(|r| r.start <= pos && pos < r.end)
}

/// Character immediately before byte offset `pos`, if any.
fn prev_char(content: &str, pos: usize) -> Option<char> {
    content[..pos].chars().next_back()
}

/// Character at byte offset `pos`, if any.
fn next_char(content: &str, pos: usize) -> Option<char> {
    content[pos..].chars().next()
}

/// `true` when everything between the previous `\n` (or start) and `pos` is
/// whitespace — i.e. `pos` ends a blank line.
fn is_blank_line_end(content: &str, pos: usize) -> bool {
    let line_start = content[..pos].rfind('\n').map_or(0, |i| i + 1);
    content[line_start..pos].chars().all(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_overlapping_and_unsorted_ranges() {
        let m = TextMask::new(vec![10..20, 0..5, 4..8, 30..30, 60..100], 40);
        assert_eq!(m.ranges(), &[0..8, 10..20]);
        assert!(m.is_masked(3));
        assert!(!m.is_masked(9));
        assert!(m.is_masked(15));
        assert!(!m.is_masked(39));
        assert!(!m.is_masked(40), "end of text is never masked");
    }

    #[test]
    fn masked_length_and_unmasked_split() {
        let m = TextMask::new(vec![4..8], 16);
        assert_eq!(m.masked_length(&(0..16)), 4);
        assert_eq!(m.masked_length(&(4..8)), 4);
        assert_eq!(m.masked_length(&(8..16)), 0);
        assert_eq!(m.unmasked(&(0..16)), vec![0..4, 8..16]);
        assert_eq!(m.masked_before(16), 4);
        assert_eq!(m.masked_before(2), 0);
        assert_eq!(m.masked_before(6), 2);
    }

    #[test]
    fn display_math_span() {
        let c = "a $$x^2$$ b";
        assert_eq!(math_ranges(c, &[]), vec![2..9]);
    }

    #[test]
    fn inline_math_span() {
        let c = "cost is $x+1$ today";
        assert_eq!(math_ranges(c, &[]), vec![9..14]);
    }

    #[test]
    fn currency_is_not_math() {
        assert!(math_ranges("costs $5 or 5$ here", &[]).is_empty());
    }

    #[test]
    fn math_ignores_masked_code() {
        let c = "`$x$` and $y$";
        let excluded = vec![0..5usize];
        assert_eq!(math_ranges(c, &excluded), vec![10..13]);
    }

    #[test]
    fn inline_math_does_not_cross_blank_line() {
        let c = "$a\n\nb$ and $c$";
        assert_eq!(math_ranges(c, &[]), vec![11..14]);
    }
}
