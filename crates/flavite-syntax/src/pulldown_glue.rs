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

/// Parse a note's bytes and extract all Flavite-specific syntax.
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
    parse_snapshot(snapshot)
}

/// Canonical parsing over exact Session TextByte coordinates.
pub fn parse_normalized(text: &str) -> Result<ParsedNote, String> {
    parse_snapshot(TextSnapshot::from_normalized(text))
}

/// Parse an already-owned coordinate snapshot through the same canonical
/// scanner/mask path, without applying file normalization a second time.
pub fn parse_snapshot(snapshot: TextSnapshot) -> Result<ParsedNote, String> {
    let text = snapshot.text();
    if text.contains('\0') {
        return Err("markdown contains an embedded NUL byte".into());
    }

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
                // Heading text participates in the same canonical extension scan.
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
            Event::Start(Tag::Paragraph) => {
                // A paragraph boundary's structural newlines appear in no
                // text event. Flush before the block so the buffer never
                // spans one: a buffer that skips the separator bytes no
                // longer matches `snapshot[buffer_start..]`, and the
                // truncated scan range silently hid every link after the
                // first paragraph (the same failure mode the SoftBreak arm
                // below fixes for line breaks).
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
                text_start = range.start;
            }
            Event::End(TagEnd::Paragraph) => {
                // Flush at the block end and re-anchor to it: the next
                // paragraph's first text event re-anchors from its own
                // start, so each paragraph scans aligned bytes.
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
                }

                // Buffer text for scanning (to handle split wikilinks).
                // The buffer must stay byte-identical to
                // `snapshot[text_start..]`: structural bytes the event
                // stream drops (paragraph separators, emphasis markers,
                // soft/hard breaks) are filled in from the snapshot, so
                // every scan range covers exactly the bytes it names. A
                // buffer that skipped them desynchronized the scan window
                // and silently hid links after the first dropped byte.
                let expected_start = text_start + text_buffer.len();
                if range.start > expected_start {
                    text_buffer.push_str(&text[expected_start..range.start]);
                }
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
            Event::Html(_) | Event::InlineHtml(_) => {
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
    if text.starts_with("---\n") {
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
            Event::Html(_) | Event::InlineHtml(_) => {
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
        // Regression (W12 crash-recovery evidence): a wikilink after a
        // second paragraph — with inline markup before it — was silently
        // lost because the scan buffer skipped structural bytes and its
        // window desynchronized from the snapshot.
        {
            let multi =
                parse_note(b"# H\n\nfirst paragraph with **bold**\n\nSee [[target]].\n").unwrap();
            let targets: Vec<&str> = multi
                .links
                .iter()
                .map(|link| link.target_raw.as_str())
                .collect();
            assert_eq!(
                targets,
                ["target"],
                "links after later paragraphs are indexed"
            );
            let last = multi.links.last().unwrap();
            let text = multi.snapshot.text();
            assert_eq!(
                &text[last.span.start.0..last.span.end.0],
                "[[target]]",
                "the extracted span names the real snapshot bytes"
            );
        }
        assert!(!result.links[0].is_embed);
    }

    #[test]
    fn rejects_embedded_nul_before_markdown_parsing() {
        assert!(parse_note(b"[[m[]]\0").is_err());
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
