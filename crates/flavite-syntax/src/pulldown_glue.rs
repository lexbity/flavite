//! Pulldown-cmark parse glue (§4.3, D10, AC-5).
//!
//! This module provides the indexer-side parse path:
//! 1. Parse markdown with pulldown-cmark (spec-correct CommonMark).
//! 2. Build a [`TextMask`] of excluded ranges (code spans/blocks, raw HTML, math).
//! 3. Run the [`crate::scanner`] on the unmasked text to extract link/tag/block-ID spans.
//!
//! The scanner is the **single semantics source** for what counts as a link/tag;
//! tree-sitter never decides this (D10). Both parse paths use the same scanner,
//! which is what makes AC-5 parity possible.

use crate::mask::{TextMask, math_ranges};
use crate::scanner::{ScannedSpan, SpanKind, scan_inline};
use crate::text::{Span, TextByte, TextSnapshot};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::ops::Range;

/// A parsed note with all extracted structure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedNote {
    /// The text snapshot (normalized bytes + source mapping).
    pub snapshot: TextSnapshot,
    /// All link spans found (wikilinks and embeds).
    pub links: Vec<Link>,
    /// All tag spans found.
    pub tags: Vec<Span<TextByte>>,
    /// All block ID spans found.
    pub block_ids: Vec<Span<TextByte>>,
    /// Headings with their level and text content (for title derivation).
    pub headings: Vec<(HeadingLevel, String)>,
    /// The text mask excluding code/HTML/math regions.
    pub mask: TextMask,
}

/// A resolved link from scanning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Span in normalized text coordinates.
    pub span: Span<TextByte>,
    /// The raw target text (including `id:` prefix if present).
    pub target_raw: String,
    /// Alias text if present (from `[[target|alias]]`).
    pub alias: Option<String>,
    /// Anchor text if present (from `[[target#anchor]]`).
    pub anchor: Option<String>,
    /// `true` if this is an embed (`![[...]]`).
    pub is_embed: bool,
    /// `true` if the target is a stable ID link (`id:...`).
    pub by_id: bool,
}

impl Link {
    /// Convert from a [`ScannedSpan`] with a [`SpanKind::Link`] or [`SpanKind::Embed`].
    pub(super) fn from_scanned(span: ScannedSpan) -> Option<Self> {
        let payload = span.payload?;
        Some(Self {
            span: span.span,
            target_raw: payload.target_raw,
            alias: payload.alias,
            anchor: payload.anchor,
            is_embed: span.kind == SpanKind::Embed,
            by_id: payload.by_id,
        })
    }
}

/// Parse a note's bytes and extract all Lattice-specific syntax.
///
/// This is the indexer parse path: pulldown-cmark for structure, our scanner
/// for inline extensions (D10).
///
/// # Arguments
///
/// * `bytes` — the raw note bytes.
///
/// # Returns
///
/// A [`ParsedNote`] containing the snapshot, all extracted spans, headings, and
/// the exclusion mask.
pub fn parse_note(bytes: &[u8]) -> Result<ParsedNote, String> {
    let snapshot = TextSnapshot::new(bytes).map_err(|e| e.to_string())?;
    let text = snapshot.text();

    // Build the exclusion mask from code spans/blocks, raw HTML, and math.
    let (mask, _body_start) = build_mask(text);

    // Collect headings and extract inline spans from text events.
    let mut links = Vec::new();
    let mut tags = Vec::new();
    let mut block_ids = Vec::new();
    let mut headings = Vec::new();

    // Parse with pulldown-cmark.
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);

    let parser = Parser::new_ext(text, opts);

    let mut current_heading_level: Option<HeadingLevel> = None;
    let mut current_heading_text = String::new();
    let mut in_code_block = false;
    let mut text_buffer = String::new();
    let mut text_start = 0usize;

    for (event, range) in parser.into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                // Flush any buffered text before the heading
                if !text_buffer.is_empty() {
                    scan_buffer(
                        &snapshot,
                        &mask,
                        &text_buffer,
                        text_start,
                        &mut links,
                        &mut tags,
                        &mut block_ids,
                    );
                    text_buffer.clear();
                }
                current_heading_level = Some(level);
                current_heading_text.clear();
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(level) = current_heading_level.take()
                    && !current_heading_text.is_empty()
                {
                    headings.push((level, current_heading_text.clone()));
                }
                // Reset text buffer after heading
                text_buffer.clear();
                text_start = range.end;
            }
            Event::Start(Tag::CodeBlock(_)) => {
                // Flush any buffered text before the code block
                if !text_buffer.is_empty() {
                    scan_buffer(
                        &snapshot,
                        &mask,
                        &text_buffer,
                        text_start,
                        &mut links,
                        &mut tags,
                        &mut block_ids,
                    );
                    text_buffer.clear();
                }
                in_code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                text_buffer.clear();
                text_start = range.end;
            }
            Event::Text(_) => {
                // Skip text in code blocks
                if in_code_block {
                    text_buffer.clear();
                    continue;
                }

                // Get the text from the snapshot
                let text_content = &text[range.start..range.end];

                // Collect heading text
                if current_heading_level.is_some() {
                    current_heading_text.push_str(text_content);
                    continue;
                }

                // Buffer text for scanning (to handle split wikilinks)
                if text_buffer.is_empty() {
                    text_start = range.start;
                }
                text_buffer.push_str(text_content);
            }
            Event::Code(_) => {
                // Inline code span - flush buffer and mark as excluded
                if !text_buffer.is_empty() {
                    scan_buffer(
                        &snapshot,
                        &mask,
                        &text_buffer,
                        text_start,
                        &mut links,
                        &mut tags,
                        &mut block_ids,
                    );
                    text_buffer.clear();
                }
                text_buffer.clear();
                text_start = range.end;
            }
            Event::Html(_) => {
                // HTML - flush buffer
                if !text_buffer.is_empty() {
                    scan_buffer(
                        &snapshot,
                        &mask,
                        &text_buffer,
                        text_start,
                        &mut links,
                        &mut tags,
                        &mut block_ids,
                    );
                    text_buffer.clear();
                }
                text_buffer.clear();
                text_start = range.end;
            }
            _ => {}
        }
    }

    // Flush any remaining buffered text
    if !text_buffer.is_empty() {
        scan_buffer(
            &snapshot,
            &mask,
            &text_buffer,
            text_start,
            &mut links,
            &mut tags,
            &mut block_ids,
        );
    }

    Ok(ParsedNote {
        snapshot,
        links,
        tags,
        block_ids,
        headings,
        mask,
    })
}

/// Scan a buffered text region and extract spans.
fn scan_buffer(
    snapshot: &TextSnapshot,
    mask: &TextMask,
    buffer: &str,
    buffer_start: usize,
    links: &mut Vec<Link>,
    tags: &mut Vec<Span<TextByte>>,
    block_ids: &mut Vec<Span<TextByte>>,
) {
    let buffer_end = buffer_start + buffer.len();
    let buffer_range = buffer_start..buffer_end;

    // A masked prefix must not discard valid prose later in the same parser
    // text buffer (for example, a wikilink after inline math). Scan each
    // unmasked interval with the one shared inline grammar.
    for range in mask.unmasked(&buffer_range) {
        for span in scan_inline(snapshot, range) {
            match span.kind {
                SpanKind::Link | SpanKind::Embed => {
                    if let Some(link) = Link::from_scanned(span) {
                        links.push(link);
                    }
                }
                SpanKind::Tag => tags.push(span.span),
                SpanKind::BlockId => block_ids.push(span.span),
            }
        }
    }
}

/// Build the exclusion mask from code spans/blocks, raw HTML, and math.
///
/// Returns the mask and the byte offset where the body (prose) content starts
/// (after frontmatter if present).
fn build_mask(text: &str) -> (TextMask, usize) {
    let mut excluded: Vec<Range<usize>> = Vec::new();
    let mut body_start = 0usize;

    // Check for frontmatter delimiter.
    if text.starts_with("---") {
        // Find the closing --- or ...
        if let Some(close) = find_frontmatter_close(text) {
            excluded.push(0..close);
            body_start = close;
        }
    }

    // Parse to collect code/HTML regions in a single pass.
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);

    let parser = Parser::new_ext(text, opts);
    let mut code_block_start: Option<usize> = None;

    for (event, range) in parser.into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                code_block_start = Some(range.start);
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(start) = code_block_start.take() {
                    // Include everything from start to end of the code block
                    excluded.push(start..range.end);
                }
            }
            Event::Code(_) => {
                // Inline code span - exclude this range
                excluded.push(range.clone());
            }
            Event::Html(_) => {
                // HTML block or inline - exclude this range
                excluded.push(range.clone());
            }
            Event::InlineMath(_) => {
                // Inline math (if enabled by pulldown)
                excluded.push(range.clone());
            }
            Event::DisplayMath(_) => {
                // Display math (if enabled by pulldown)
                excluded.push(range.clone());
            }
            _ => {}
        }
    }

    // Add math ranges (they're not natively supported by pulldown-cmark without extensions).
    // We scan the full text (excluding already-masked regions).
    let math_ranges = math_ranges(text, &excluded);
    excluded.extend(math_ranges);

    // Merge and sort.
    excluded.sort_by_key(|a| a.start);

    (TextMask::new(excluded, text.len()), body_start)
}

/// Find the closing `---` or `...` delimiter for frontmatter.
fn find_frontmatter_close(text: &str) -> Option<usize> {
    let after_open = 3; // Skip initial "---"
    let rest = &text[after_open..];

    let mut line_start = 0usize;
    for line in rest.lines() {
        let trimmed = line.trim();
        if trimmed == "---" || trimmed == "..." {
            return Some(after_open + line_start + line.len());
        }
        line_start += line.len() + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_note() {
        let src = "# Hello\n\nThis is a [[link]].\n";
        let result = parse_note(src.as_bytes()).expect("parse");
        assert!(!result.headings.is_empty(), "Expected at least one heading");
        if !result.headings.is_empty() {
            assert_eq!(result.headings[0].0, HeadingLevel::H1);
        }
        assert_eq!(result.links.len(), 1);
        assert_eq!(result.links[0].target_raw, "link");
        assert!(!result.links[0].is_embed);
    }

    #[test]
    fn parses_wikilink_with_alias() {
        let src = "[[target|My Link]]";
        let result = parse_note(src.as_bytes()).expect("parse");
        assert_eq!(result.links.len(), 1);
        assert_eq!(result.links[0].target_raw, "target");
        assert_eq!(result.links[0].alias, Some("My Link".to_string()));
    }

    #[test]
    fn parses_embed() {
        let src = "![[diagram.png]]";
        let result = parse_note(src.as_bytes()).expect("parse");
        assert_eq!(result.links.len(), 1);
        assert!(result.links[0].is_embed);
        assert_eq!(result.links[0].target_raw, "diagram.png");
    }

    #[test]
    fn parses_tag() {
        let src = "#tag";
        let result = parse_note(src.as_bytes()).expect("parse");
        assert_eq!(result.tags.len(), 1);
    }

    #[test]
    fn parses_block_id() {
        let src = "^block123";
        let result = parse_note(src.as_bytes()).expect("parse");
        assert_eq!(result.block_ids.len(), 1);
    }

    #[test]
    fn excludes_code_spans() {
        let src = "`[[not a link]]`";
        let result = parse_note(src.as_bytes()).expect("parse");
        assert_eq!(result.links.len(), 0);
    }

    #[test]
    fn excludes_code_blocks() {
        let src = "\n\n```\n[[not a link]]\n```\n";
        let result = parse_note(src.as_bytes()).expect("parse");
        assert_eq!(result.links.len(), 0);
    }

    #[test]
    fn frontmatter_does_not_interfere() {
        let src = "---\ntitle: Test\n---\n\n[[link]]";
        let result = parse_note(src.as_bytes()).expect("parse");
        assert_eq!(result.links.len(), 1);
        assert_eq!(result.links[0].target_raw, "link");
    }

    #[test]
    fn parses_headings() {
        let src = "# H1\n## H2\n### H3";
        let result = parse_note(src.as_bytes()).expect("parse");
        assert_eq!(result.headings.len(), 3);
        assert_eq!(result.headings[0].0, HeadingLevel::H1);
        assert_eq!(result.headings[1].0, HeadingLevel::H2);
        assert_eq!(result.headings[2].0, HeadingLevel::H3);
    }
}
