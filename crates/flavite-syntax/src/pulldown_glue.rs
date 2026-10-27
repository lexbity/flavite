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

use crate::mask::{math_ranges, TextMask};
use crate::scanner::{scan_inline, ScannedSpan, SpanKind};
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
    fn from_scanned(span: ScannedSpan) -> Option<Self> {
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

    for (event, range) in parser.into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                current_heading_level = Some(level);
                current_heading_text.clear();
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(level) = current_heading_level.take() {
                    if !current_heading_text.is_empty() {
                        headings.push((level, current_heading_text.clone()));
                    }
                }
            }
            Event::Start(Tag::CodeBlock(_)) => {
                in_code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
            }
            Event::Text(_) => {
                // Skip text in code blocks
                if in_code_block {
                    continue;
                }

                // Get the text from the snapshot
                let text_content = &text[range.start..range.end];

                // Collect heading text
                if current_heading_level.is_some() {
                    current_heading_text.push_str(text_content);
                    continue;
                }

                // Get the absolute byte range of this text
                let absolute_range = range.start..range.end;

                // Skip if this text is in an excluded range
                if mask.is_masked(absolute_range.start) {
                    continue;
                }

                // Scan inline syntax in this text event
                let spans = scan_inline(&snapshot, absolute_range);
                for span in spans {
                    match span.kind {
                        SpanKind::Link => {
                            if let Some(link) = Link::from_scanned(span) {
                                links.push(link);
                            }
                        }
                        SpanKind::Embed => {
                            if let Some(link) = Link::from_scanned(span) {
                                links.push(link);
                            }
                        }
                        SpanKind::Tag => {
                            tags.push(span.span);
                        }
                        SpanKind::BlockId => {
                            block_ids.push(span.span);
                        }
                    }
                }
            }

            Event::Code(_) => {
                // Inline code spans are single events, not start/end pairs
                // Mark the range as excluded but don't track state
                // The range is already handled above
            }
            Event::Html(_) => {
                // HTML is already in the mask
            }
            _ => {}
        }
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

    // Parse again to collect code/HTML regions.
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);

    let parser = Parser::new_ext(text, opts);

    for (event, range) in parser.into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                // Mark the start of a code block
                // We'll find the end and add the range
            }
            Event::End(TagEnd::CodeBlock) => {
                // Code block end - range includes the content
            }
            Event::Code(_) => {
                // Inline code span
                excluded.push(range.start..range.end);
            }
            Event::Html(_) => {
                // HTML block or inline
                excluded.push(range.start..range.end);
            }
            Event::InlineMath(_) => {
                // Inline math (if enabled)
                excluded.push(range.start..range.end);
            }
            Event::DisplayMath(_) => {
                // Display math (if enabled)
                excluded.push(range.start..range.end);
            }
            _ => {}
        }
    }

    // Add code block ranges - parse again to get them properly
    let parser = Parser::new_ext(text, opts);
    let mut code_block_start: Option<usize> = None;

    for (event, range) in parser.into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                code_block_start = Some(range.start);
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(start) = code_block_start.take() {
                    excluded.push(start..range.end);
                }
            }
            _ => {}
        }
    }

    // Add math ranges (they're not natively supported by pulldown-cmark without extensions).
    // We scan the body text (excluding already-masked regions).
    let math_ranges = math_ranges(text, &excluded);
    excluded.extend(math_ranges);

    // Merge and sort.
    excluded.sort_by(|a, b| a.start.cmp(&b.start));

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
        assert!(result.headings.len() >= 1, "Expected at least one heading");
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
