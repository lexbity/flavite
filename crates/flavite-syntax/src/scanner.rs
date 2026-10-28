//! Inline scanner for Lattice syntax extensions (§4.3, D10).
//!
//! The scanner is **pure and IO-free**. It is the single source of link/tag
//! semantics for both the indexer (pulldown-cmark parse path) and the editor
//! (tree-sitter parse path). Tree-sitter never decides what a link is (D10).
//!
//! # Syntax covered
//!
//! - Wiki links: `[[target]]`, `[[target|alias]]`, `[[target#anchor]]`,
//!   `[[target|alias#anchor]]`, `[[id:01J9ZK...|alias]]`
//! - Embeds: `![[target]]`, `![[target|alias]]`
//! - Tags: `#tag`, `#nested/tag`, `#tag-with-dashes`
//! - Block IDs: `^block-id`
//!
//! # Output
//!
//! The scanner produces [`ScannedSpan`]s with [`SpanKind`] and payloads.

use crate::text::{Span, TextByte, TextSnapshot};
use std::ops::Range;

/// The kind of a scanned span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpanKind {
    /// A wikilink (`[[...]]`).
    Link,
    /// An embed (`![[...]]`).
    Embed,
    /// A tag (`#...`).
    Tag,
    /// A block ID (`^...`).
    BlockId,
}

/// Payload for a [`SpanKind::Link`] or [`SpanKind::Embed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkPayload {
    /// The raw text between the delimiters, **including** `id:` prefix when
    /// present, but **excluding** the `|alias` portion.
    pub target_raw: String,
    /// The alias part, if the link used the `|` syntax.
    pub alias: Option<String>,
    /// The anchor part, if the link used the `#` syntax.
    pub anchor: Option<String>,
    /// `true` if the target begins with `id:` (stable-form link).
    pub by_id: bool,
}

/// A single scanned span: kind, byte range, and payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedSpan {
    /// Span in normalized [`TextByte`] coordinates.
    pub span: Span<TextByte>,
    /// What this span represents.
    pub kind: SpanKind,
    /// Link/embed payload; `None` for tags and block IDs.
    pub payload: Option<LinkPayload>,
}

/// Run the inline scanner over `text` within `range`, returning all spans found.
///
/// This is the **single semantics source** for what counts as a link/tag in
/// Lattice. Tree-sitter never decides this (D10).
///
/// # Arguments
///
/// * `snapshot` — the [`TextSnapshot`] providing the normalized text and
///   source-byte mapping.
/// * `range` — a byte range in **normalized** coordinates over which to scan.
///
/// # Returns
///
/// A vector of [`ScannedSpan`]s in ascending byte order, non-overlapping.
pub fn scan_inline(snapshot: &TextSnapshot, range: Range<usize>) -> Vec<ScannedSpan> {
    let text = snapshot.text();
    let bytes = text.as_bytes();

    let start = range.start.min(text.len());
    let end = range.end.min(text.len());

    if start >= end {
        return Vec::new();
    }

    let mut spans = Vec::new();
    let mut i = start;

    while i < end {
        match bytes.get(i) {
            Some(b'[') => {
                // Potential wikilink or embed: try `[[` or `![[`
                if i + 1 < end
                    && bytes[i + 1] == b'['
                    && let Some(span) = scan_wikilink(bytes, &mut i, end, SpanKind::Link)
                {
                    spans.push(span);
                    continue;
                }
                i += 1;
            }
            Some(b'!') => {
                // Potential embed: try `![[`
                if i + 2 < end && bytes[i + 1] == b'[' && bytes[i + 2] == b'[' {
                    i += 1; // Skip `!` so scan_wikilink sees `[[`
                    if let Some(span) = scan_wikilink(bytes, &mut i, end, SpanKind::Embed) {
                        spans.push(span);
                        continue;
                    }
                } else {
                    i += 1;
                }
            }
            Some(b'#') => {
                // Potential tag: must be at a word boundary
                if is_word_boundary(text, i)
                    && let Some(span) = scan_tag(bytes, &mut i, end)
                {
                    spans.push(span);
                    continue;
                }
                i += 1;
            }
            Some(b'^') => {
                // Potential block ID.
                if let Some(span) = scan_block_id(bytes, &mut i, end) {
                    spans.push(span);
                    continue;
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    spans
}

/// Scan a wikilink or embed starting at `[[` at position `i`.
///
/// Advances `i` to just past the closing `]]`.
/// Returns `None` if the link is malformed (no closing `]]`).
fn scan_wikilink(bytes: &[u8], i: &mut usize, end: usize, kind: SpanKind) -> Option<ScannedSpan> {
    let start = *i;
    *i += 2; // Skip `[[`

    // Collect the raw link content up to `]]` or end.
    let content_start = *i;
    let mut alias_start: Option<usize> = None;
    let mut anchor_start: Option<usize> = None;

    // Find the closing `]]`.
    while *i + 1 < end {
        if bytes[*i] == b']' && bytes[*i + 1] == b']' {
            let content_end = *i;
            *i += 2; // Skip `]]`

            // Both common orders are accepted: target#anchor|alias and
            // target|alias#anchor. Each field is bounded by the other
            // separator only when that separator follows it. Compute ranges
            // from ordered positions so malformed/arbitrary input can never
            // create a reversed slice.
            let target_end = [alias_start, anchor_start]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(content_end);
            let target_raw = String::from_utf8_lossy(&bytes[content_start..target_end]).to_string();
            let alias = alias_start.map(|separator| {
                let alias_end = anchor_start
                    .filter(|anchor| *anchor > separator)
                    .unwrap_or(content_end);
                String::from_utf8_lossy(&bytes[separator + 1..alias_end]).to_string()
            });
            let anchor = anchor_start.map(|separator| {
                let anchor_end = alias_start
                    .filter(|alias| *alias > separator)
                    .unwrap_or(content_end);
                String::from_utf8_lossy(&bytes[separator + 1..anchor_end]).to_string()
            });

            let by_id = target_raw.starts_with("id:");

            return Some(ScannedSpan {
                span: Span::new(TextByte(start), TextByte(*i)),
                kind,
                payload: Some(LinkPayload {
                    target_raw,
                    alias,
                    anchor,
                    by_id,
                }),
            });
        }

        // Record the first occurrence of each separator. Their order is
        // handled when the closed link's field ranges are constructed.
        match bytes[*i] {
            b'|' if alias_start.is_none() => {
                alias_start = Some(*i);
            }
            b'#' if anchor_start.is_none() => {
                anchor_start = Some(*i);
            }
            _ => {}
        }

        *i += 1;
    }

    // Malformed: no closing `]]`. Don't emit a span.
    None
}

/// Scan a tag starting with `#` at position `i`.
///
/// A tag is a `#` followed by word characters, hyphens, underscores, and
/// forward slashes (for hierarchical tags). It must be at a word boundary.
///
/// Advances `i` to just past the tag.
fn scan_tag(bytes: &[u8], i: &mut usize, end: usize) -> Option<ScannedSpan> {
    let start = *i;
    *i += 1; // Skip `#`

    let tag_start = *i;
    while *i < end {
        let b = bytes[*i];
        if is_tag_char(b) {
            *i += 1;
        } else {
            break;
        }
    }

    let tag_end = *i;

    // A tag must have at least one character after `#`.
    if tag_end > tag_start {
        return Some(ScannedSpan {
            span: Span::new(TextByte(start), TextByte(tag_end)),
            kind: SpanKind::Tag,
            payload: None,
        });
    }

    None
}

/// Scan a block ID starting with `^` at position `i`.
///
/// A block ID is `^` followed by alphanumeric characters, hyphens, and
/// underscores.
///
/// Advances `i` to just past the ID.
fn scan_block_id(bytes: &[u8], i: &mut usize, end: usize) -> Option<ScannedSpan> {
    let start = *i;
    *i += 1; // Skip `^`

    let id_start = *i;
    while *i < end {
        let b = bytes[*i];
        if is_block_id_char(b) {
            *i += 1;
        } else {
            break;
        }
    }

    let id_end = *i;

    if id_end > id_start {
        return Some(ScannedSpan {
            span: Span::new(TextByte(start), TextByte(id_end)),
            kind: SpanKind::BlockId,
            payload: None,
        });
    }

    None
}

/// Returns `true` if position `i` is at a word boundary in `text`.
///
/// A word boundary means either:
/// - `i` is at the start of the text, or
/// - The character before `i` is not a word character (alphanumeric or underscore).
fn is_word_boundary(text: &str, i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let prev = text[..i].chars().next_back();
    !prev.is_some_and(|c| c.is_alphanumeric() || c == '_')
}

/// Returns `true` if `b` is a valid character in a tag.
fn is_tag_char(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'/' | b'.')
}

/// Returns `true` if `b` is a valid character in a block ID.
fn is_block_id_char(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::TextSnapshot;

    fn snap(s: &str) -> TextSnapshot {
        TextSnapshot::new(s.as_bytes()).expect("utf-8")
    }

    fn scan(s: &str) -> Vec<ScannedSpan> {
        let s = snap(s);
        scan_inline(&s, 0..s.text().len())
    }

    #[test]
    fn empty_text() {
        assert!(scan("").is_empty());
    }

    #[test]
    fn simple_wikilink() {
        let spans = scan("[[target]]");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].kind, SpanKind::Link);
        let p = spans[0].payload.as_ref().unwrap();
        assert_eq!(p.target_raw, "target");
        assert_eq!(p.alias, None);
        assert_eq!(p.anchor, None);
        assert!(!p.by_id);
    }

    #[test]
    fn wikilink_with_alias() {
        let spans = scan("[[target|My Alias]]");
        assert_eq!(spans.len(), 1);
        let p = spans[0].payload.as_ref().unwrap();
        assert_eq!(p.target_raw, "target");
        assert_eq!(p.alias, Some("My Alias".to_string()));
        assert_eq!(p.anchor, None);
    }

    #[test]
    fn wikilink_with_anchor() {
        let spans = scan("[[target#heading]]");
        assert_eq!(spans.len(), 1);
        let p = spans[0].payload.as_ref().unwrap();
        assert_eq!(p.target_raw, "target");
        assert_eq!(p.anchor, Some("heading".to_string()));
    }

    #[test]
    fn wikilink_alias_and_anchor_are_order_independent_and_bounded() {
        for input in ["[[target#heading|alias]]", "[[target|alias#heading]]"] {
            let spans = scan(input);
            assert_eq!(spans.len(), 1, "input: {input}");
            let payload = spans[0].payload.as_ref().unwrap();
            assert_eq!(payload.target_raw, "target", "input: {input}");
            assert_eq!(payload.alias.as_deref(), Some("alias"), "input: {input}");
            assert_eq!(payload.anchor.as_deref(), Some("heading"), "input: {input}");
        }
    }

    #[test]
    fn stable_form_link() {
        let spans = scan("[[id:01J9ZK|My Note]]");
        assert_eq!(spans.len(), 1);
        let p = spans[0].payload.as_ref().unwrap();
        assert!(p.by_id);
        assert_eq!(p.target_raw, "id:01J9ZK");
        assert_eq!(p.alias, Some("My Note".to_string()));
    }

    #[test]
    fn embed() {
        let spans = scan("![[diagram.png]]");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].kind, SpanKind::Embed);
        let p = spans[0].payload.as_ref().unwrap();
        assert_eq!(p.target_raw, "diagram.png");
    }

    #[test]
    fn simple_tag() {
        let spans = scan("#tag");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].kind, SpanKind::Tag);
    }

    #[test]
    fn hierarchical_tag() {
        let spans = scan("#project/urgent");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].kind, SpanKind::Tag);
    }

    #[test]
    fn tag_with_dashes() {
        let spans = scan("#my-tag-name");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].kind, SpanKind::Tag);
    }

    #[test]
    fn tag_not_at_word_boundary() {
        // `a#tag` — the `#` is not at a word boundary, so no tag.
        assert!(scan("a#tag").is_empty());
        // `a #tag` — space before `#`, so tag is found.
        let spans = scan("a #tag");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].kind, SpanKind::Tag);
    }

    #[test]
    fn block_id() {
        let spans = scan("^block123");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].kind, SpanKind::BlockId);
    }

    #[test]
    fn block_id_with_hyphens() {
        let spans = scan("^my-block-id");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].kind, SpanKind::BlockId);
    }

    #[test]
    fn multiple_spans() {
        let spans = scan("#tag [[link]] ^block");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].kind, SpanKind::Tag);
        assert_eq!(spans[1].kind, SpanKind::Link);
        assert_eq!(spans[2].kind, SpanKind::BlockId);
    }

    #[test]
    fn malformed_wikilink_no_close() {
        // `[[target` with no `]]` — no span emitted.
        assert!(scan("[[target").is_empty());
    }

    #[test]
    fn tag_at_start() {
        let spans = scan("#start");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].kind, SpanKind::Tag);
    }

    #[test]
    fn tag_not_in_word() {
        // `hello#world` — not a tag because `#` is not at a word boundary.
        assert!(scan("hello#world").is_empty());
    }
}
