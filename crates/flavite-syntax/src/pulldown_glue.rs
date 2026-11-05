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
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
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
    let mut excluded = Vec::new();
    if text.starts_with("---\n")
        && let Some(close) = find_frontmatter_close(text)
    {
        excluded.push(0..close);
    }
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS;
    let mut headings = Vec::new();
    let mut heading_level = None;
    let mut heading_text = String::new();
    let mut code_start = None;
    let mut pending = 0..0;
    let mut scan_ranges = Vec::new();

    // One CommonMark event pass supplies both exclusion ranges and the
    // canonical prose windows. Keep byte ranges instead of copying prose
    // into a String only to recover its length for the inline scanner.
    for (event, range) in Parser::new_ext(text, opts).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                flush_scan_range(&mut scan_ranges, &mut pending);
                heading_level = Some(level);
                heading_text.clear();
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(level) = heading_level.take()
                    && !heading_text.is_empty()
                {
                    headings.push((level, std::mem::take(&mut heading_text)));
                }
                flush_scan_range(&mut scan_ranges, &mut pending);
                pending = range.end..range.end;
            }
            Event::Start(Tag::CodeBlock(_)) => {
                flush_scan_range(&mut scan_ranges, &mut pending);
                code_start = Some(range.start);
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(start) = code_start.take() {
                    excluded.push(start..range.end);
                }
                pending = range.end..range.end;
            }
            Event::Start(Tag::Paragraph) | Event::End(TagEnd::Paragraph) => {
                flush_scan_range(&mut scan_ranges, &mut pending);
                let at = if matches!(event, Event::Start(_)) {
                    range.start
                } else {
                    range.end
                };
                pending = at..at;
            }
            Event::Text(_) => {
                if code_start.is_some() {
                    pending.end = pending.start;
                    continue;
                }
                if heading_level.is_some() {
                    heading_text.push_str(&text[range.clone()]);
                }
                // Preserve the original scan windows across omitted source
                // delimiters, emphasis, and soft/hard line breaks. Scanning
                // decoded event text would change offsets and link syntax.
                let buffered = pending.len() + range.start.saturating_sub(pending.end);
                if buffered == 0 {
                    pending.start = range.start;
                }
                pending.end = pending.start + buffered + range.len();
            }
            Event::Code(_) | Event::Html(_) | Event::InlineHtml(_) => {
                flush_scan_range(&mut scan_ranges, &mut pending);
                excluded.push(range.clone());
                pending = range.end..range.end;
            }
            Event::InlineMath(_) | Event::DisplayMath(_) => excluded.push(range),
            _ => {}
        }
    }
    flush_scan_range(&mut scan_ranges, &mut pending);
    excluded.extend(math_ranges(text, &excluded));
    let mask = TextMask::new(excluded, text.len());
    let mut links = Vec::new();
    let mut tags = Vec::new();
    let mut block_ids = Vec::new();
    for prose in scan_ranges {
        for range in mask.unmasked(&prose) {
            for span in scan_inline(&snapshot, range) {
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
    Ok(ParsedNote {
        snapshot,
        links,
        tags,
        block_ids,
        headings,
        mask,
    })
}

fn flush_scan_range(ranges: &mut Vec<Range<usize>>, pending: &mut Range<usize>) {
    if pending.start < pending.end {
        ranges.push(pending.clone());
        pending.end = pending.start;
    }
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
    fn many_excluded_paragraphs_keep_visible_links_and_exact_unicode_offsets() {
        let text = "# é\n\n".to_owned() + &"`[[hidden]]` **é** $x$ [[visible|é]]\n\n".repeat(256);
        let parsed = parse_normalized(&text).unwrap();
        assert_eq!(parsed.links.len(), 256);
        assert_eq!(parsed.headings, vec![(HeadingLevel::H1, "é".into())]);
        for link in parsed.links {
            assert_eq!(link.target_raw, "visible");
            assert_eq!(&text[link.span.start.0..link.span.end.0], "[[visible|é]]");
            assert!(!parsed.mask.is_masked(link.span.start.0));
        }
    }

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
