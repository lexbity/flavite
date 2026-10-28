//! Tree-sitter parse glue (§4.3, D10, AC-5).
//!
//! This module provides the editor-side parse path:
//! 1. Parse markdown with tree-sitter (using the tree-sitter-md grammar).
//! 2. Build a [`TextMask`] of excluded ranges (code spans/blocks, math).
//! 3. Run the [`crate::scanner`] on the unmasked text to extract link/tag/block-ID spans.
//!
//! The scanner is the **single semantics source** for what counts as a link/tag;
//! tree-sitter never decides this (D10). Both parse paths use the same scanner,
//! which is what makes AC-5 parity possible.
//!
//! Note: tree-sitter's markdown grammar is not fully CommonMark-compliant,
//! so we use it primarily for block structure and code fence detection.

use crate::mask::{TextMask, math_ranges};
use crate::scanner::{ScannedSpan, SpanKind, scan_inline};
use crate::text::{Span, TextByte, TextSnapshot};
use std::ops::Range;
use tree_sitter::{Language, Parser};

/// Get the tree-sitter language for markdown.
fn markdown_language() -> Language {
    tree_sitter_md::LANGUAGE.into()
}

/// A parsed note with all extracted structure from tree-sitter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TsParsedNote {
    /// The text snapshot (normalized bytes + source mapping).
    pub snapshot: TextSnapshot,
    /// All link spans found (wikilinks and embeds).
    pub links: Vec<super::pulldown_glue::Link>,
    /// All tag spans found.
    pub tags: Vec<Span<TextByte>>,
    /// All block ID spans found.
    pub block_ids: Vec<Span<TextByte>>,
    /// The text mask excluding code/HTML/math regions.
    pub mask: TextMask,
}

/// Parse a note's bytes with tree-sitter and extract all Flavite-specific syntax.
///
/// This is the editor parse path: tree-sitter for structure, our scanner
/// for inline extensions (D10).
///
/// # Arguments
///
/// * `bytes` — the raw note bytes.
///
/// # Returns
///
/// A [`TsParsedNote`] containing the snapshot, all extracted spans, and
/// the exclusion mask.
///
/// # Errors
///
/// Returns an error if tree-sitter parsing fails or if the bytes are not valid UTF-8.
pub fn parse_note_ts(bytes: &[u8]) -> Result<TsParsedNote, String> {
    let snapshot = TextSnapshot::new(bytes).map_err(|e| e.to_string())?;
    let text = snapshot.text();

    // Build the exclusion mask from tree-sitter nodes.
    let mask = build_mask_ts(text);

    // Collect inline spans from text nodes.
    let mut links = Vec::new();
    let mut tags = Vec::new();
    let mut block_ids = Vec::new();

    // Parse with tree-sitter.
    let mut parser = Parser::new();
    parser
        .set_language(&markdown_language())
        .map_err(|e| format!("failed to set language: {e}"))?;

    let tree = parser
        .parse(text, None)
        .ok_or("failed to parse with tree-sitter")?;

    let root = tree.root_node();

    // Recursively collect text ranges that are not in code/HTML/math contexts.
    // We track whether we're in an excluded context (code block, inline code, HTML).
    let mut cursor = root.walk();
    collect_inline_spans(
        &mut cursor,
        text,
        &mask,
        &mut links,
        &mut tags,
        &mut block_ids,
    );

    Ok(TsParsedNote {
        snapshot,
        links,
        tags,
        block_ids,
        mask,
    })
}

/// Build the exclusion mask from tree-sitter nodes.
fn build_mask_ts(text: &str) -> TextMask {
    let mut excluded: Vec<Range<usize>> = Vec::new();

    let mut parser = Parser::new();
    // SAFETY: We know markdown_language() is valid for this text.
    if parser.set_language(&markdown_language()).is_err() {
        // If we can't set the language, return an empty mask.
        return TextMask::empty(text.len());
    }

    let tree = match parser.parse(text, None) {
        Some(t) => t,
        None => return TextMask::empty(text.len()),
    };

    let root = tree.root_node();
    let mut cursor = root.walk();

    // Recursively collect excluded ranges.
    collect_excluded_ranges(&mut cursor, &mut excluded);

    // The block grammar leaves inline content opaque. Parse the same snapshot
    // with tree-sitter-md's inline grammar so code spans are excluded too.
    let mut inline_parser = Parser::new();
    let inline_language: Language = tree_sitter_md::INLINE_LANGUAGE.into();
    if inline_parser.set_language(&inline_language).is_ok()
        && let Some(inline_tree) = inline_parser.parse(text, None)
    {
        let mut inline_cursor = inline_tree.root_node().walk();
        collect_excluded_ranges(&mut inline_cursor, &mut excluded);
    }

    // Add math ranges (tree-sitter doesn't natively handle math).
    let math_ranges = math_ranges(text, &excluded);
    excluded.extend(math_ranges);

    TextMask::new(excluded, text.len())
}

/// Node kinds that represent excluded content (code, HTML, etc.).
fn is_excluded_kind(kind: &str) -> bool {
    matches!(
        kind,
        "code_block"
            | "inline_code"
            | "code_span"
            | "fenced_code_block"
            | "html_block"
            | "html_inline"
            | "raw_html"
            | "raw_text"
    )
}

/// Recursively collect excluded byte ranges from tree-sitter nodes.
fn collect_excluded_ranges(
    cursor: &mut tree_sitter::TreeCursor<'_>,
    excluded: &mut Vec<Range<usize>>,
) {
    let node = cursor.node();
    let kind = node.kind();

    if is_excluded_kind(kind) {
        let range = node.byte_range();
        excluded.push(range.start..range.end);
        // Don't descend into excluded nodes.
        return;
    }

    // Check for frontmatter (YAML frontmatter is a special case).
    if kind == "frontmatter" || kind == "yaml_front_matter" {
        let range = node.byte_range();
        excluded.push(range.start..range.end);
        return;
    }

    // Descend into children.
    if cursor.goto_first_child() {
        loop {
            collect_excluded_ranges(cursor, excluded);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

/// Collect inline spans from text nodes that are not in excluded contexts.
fn collect_inline_spans(
    cursor: &mut tree_sitter::TreeCursor<'_>,
    text: &str,
    mask: &TextMask,
    links: &mut Vec<super::pulldown_glue::Link>,
    tags: &mut Vec<Span<TextByte>>,
    block_ids: &mut Vec<Span<TextByte>>,
) {
    let node = cursor.node();
    let kind = node.kind();

    // Skip excluded nodes (code, HTML, etc.).
    if is_excluded_kind(kind) {
        return;
    }

    // Check if this is a text node that we should scan.
    if kind == "text" || kind == "inline_text" || kind == "paragraph" {
        let byte_range = node.byte_range();
        let start = byte_range.start;
        let end = byte_range.end;

        // Only scan if this range has unmasked content.
        if !mask.is_masked(start) || !mask.is_masked(end.saturating_sub(1)) {
            // Check if any part of this range is unmasked.
            let mut scan_start = start;
            while scan_start < end {
                if !mask.is_masked(scan_start) {
                    // Find the unmasked region.
                    let mut scan_end = scan_start;
                    while scan_end < end && !mask.is_masked(scan_end) {
                        scan_end += 1;
                    }
                    if scan_end > scan_start {
                        // Scan this unmasked region.
                        let spans = scan_inline(
                            &TextSnapshot::new(&text.as_bytes()[scan_start..scan_end])
                                .expect("utf-8"),
                            0..(scan_end - scan_start),
                        );
                        // Map spans back to the full text coordinates.
                        for span in spans {
                            let mapped_span = Span::new(
                                TextByte(span.span.start.0 + scan_start),
                                TextByte(span.span.end.0 + scan_start),
                            );
                            match span.kind {
                                SpanKind::Link => {
                                    if let Some(link) =
                                        super::pulldown_glue::Link::from_scanned(ScannedSpan {
                                            span: mapped_span,
                                            kind: span.kind,
                                            payload: span.payload,
                                        })
                                    {
                                        links.push(link);
                                    }
                                }
                                SpanKind::Embed => {
                                    if let Some(link) =
                                        super::pulldown_glue::Link::from_scanned(ScannedSpan {
                                            span: mapped_span,
                                            kind: span.kind,
                                            payload: span.payload,
                                        })
                                    {
                                        links.push(link);
                                    }
                                }
                                SpanKind::Tag => {
                                    tags.push(mapped_span);
                                }
                                SpanKind::BlockId => {
                                    block_ids.push(mapped_span);
                                }
                            }
                        }
                    }
                    scan_start = scan_end;
                } else {
                    scan_start += 1;
                }
            }
        }
    }

    // Descend into children.
    if cursor.goto_first_child() {
        loop {
            collect_inline_spans(cursor, text, mask, links, tags, block_ids);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_note() {
        let src = "# Hello\n\nThis is a [[link]].\n";
        let result = parse_note_ts(src.as_bytes()).expect("parse");
        assert_eq!(result.links.len(), 1);
        assert_eq!(result.links[0].target_raw, "link");
    }

    #[test]
    fn mask_excludes_code() {
        let src = "`[[not a link]]`";
        let mask = build_mask_ts(src);
        assert!(mask.is_masked(2));
        let result = parse_note_ts(src.as_bytes()).expect("parse");
        assert!(result.links.is_empty());
    }

    #[test]
    fn parse_rejects_invalid_utf8_and_collects_all_inline_span_kinds() {
        assert!(parse_note_ts(&[0xff]).is_err());

        let result = parse_note_ts(b"See ![[image.png]] and [[note]] #topic ^block").unwrap();
        assert_eq!(result.links.len(), 2);
        assert_eq!(result.links[0].target_raw, "image.png");
        assert_eq!(result.tags.len(), 1);
        assert_eq!(result.block_ids.len(), 1);
    }
}
