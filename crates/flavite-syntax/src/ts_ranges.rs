//! Tree-sitter parse glue (§4.3, D10, AC-5).
//!
//! This module provides the editor-side parse path:
//! 1. Parse markdown with tree-sitter (using the tree-sitter-md grammar) —
//!    **once** per call (RFC-0008 S6 stage A: the block tree is shared by
//!    the mask builder and the inline-window walk; no duplicate block parse).
//! 2. Build a [`TextMask`] of excluded ranges (code spans/blocks, HTML,
//!    frontmatter, math) — block exclusions from the shared tree, inline
//!    exclusions from one inline-grammar parse ([`crate::ts_inline`]).
//! 3. Run the [`crate::scanner`] on the unmasked prose windows to extract
//!    link/tag/block-ID spans.
//!
//! The scanner is the **single semantics source** for what counts as a link/tag;
//! tree-sitter never decides this (D10). Both parse paths use the same scanner,
//! which is what makes AC-5 parity possible.
//!
//! Note: tree-sitter's markdown grammar is not fully CommonMark-compliant,
//! so we use it primarily for block structure and code fence detection.

use crate::mask::{TextMask, find_frontmatter_close, math_ranges};
use crate::scanner::{SpanKind, scan_inline};
use crate::text::{Span, TextByte, TextSnapshot};
use std::ops::Range;
use tree_sitter::{Language, Node, Parser, Tree};

/// Get the tree-sitter language for markdown.
pub(crate) fn markdown_language() -> Language {
    tree_sitter_md::LANGUAGE.into()
}

// Number of block-grammar `parse` calls made on the current thread since the
// last reset (`#[cfg(test)]` instrumentation for S6-T2). Thread-local so
// parallel tests never see each other's parses.
#[cfg(test)]
thread_local! {
    pub static BLOCK_PARSE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A parsed note with all extracted structure from tree-sitter.
#[derive(Debug, Clone, PartialEq)]
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
    // tree-sitter's C parser treats embedded NUL as an input terminator and
    // can crash on malformed trailing bytes. Reject it before crossing FFI.
    if text.contains('\0') {
        return Err("markdown contains an embedded NUL byte".into());
    }

    // ONE block-grammar parse. The same tree feeds both the mask builder and
    // the scan-window walk (S6 stage A: parse once, walk twice).
    let tree = parse_block(text).ok_or("failed to parse with tree-sitter")?;
    let inline = crate::ts_inline::parse_inline(text);

    let (mask, windows) = mask_and_windows_from_trees(text, &tree, inline.as_ref());

    // Collect inline spans from unmasked prose windows.
    let mut links = Vec::new();
    let mut tags = Vec::new();
    let mut block_ids = Vec::new();
    for window in windows {
        for range in mask.unmasked(&window) {
            scan_window(&snapshot, range, &mut links, &mut tags, &mut block_ids);
        }
    }

    Ok(TsParsedNote {
        snapshot,
        links,
        tags,
        block_ids,
        mask,
    })
}

/// Parse `text` with the block grammar. Counted under `#[cfg(test)]` so
/// S6-T2 can assert exactly one block parse per `parse_note_ts` call.
fn parse_block(text: &str) -> Option<Tree> {
    #[cfg(test)]
    BLOCK_PARSE_COUNT.with(|c| c.set(c.get() + 1));
    let mut parser = Parser::new();
    parser.set_language(&markdown_language()).ok()?;
    parser.parse(text, None)
}

/// Build the exclusion mask and the prose scan windows from already-parsed
/// block + inline trees.
///
/// One walk over the block tree produces both:
/// * **exclusions** — code blocks, HTML blocks and YAML frontmatter
///   (`minus_metadata`), plus the shared frontmatter heuristic
///   ([`find_frontmatter_close`]) and the inline-grammar exclusions;
/// * **windows** — the ranges whose unmasked content is prose: paragraphs,
///   heading content (without their `#`/underline markers) and table cells
///   (without their `|` separators). This mirrors pulldown's scan windows so
///   AC-5 parity holds (RFC-0008 S6).
pub(crate) fn mask_and_windows_from_trees(
    text: &str,
    tree: &Tree,
    inline: Option<&Tree>,
) -> (TextMask, Vec<Range<usize>>) {
    let mut excluded: Vec<Range<usize>> = Vec::new();
    let mut windows: Vec<Range<usize>> = Vec::new();

    // Shared frontmatter rule with the pulldown glue. The YAML-style
    // `minus_metadata` node covers the `---` … `---` form; the shared
    // heuristic additionally covers the `...` closer so both glues agree.
    if text.starts_with("---\n")
        && let Some(close) = find_frontmatter_close(text)
    {
        excluded.push(0..close);
    }

    // Footnote-definition labels (`[^label]:` at line starts) are part of the
    // `FootnoteDefinition` tag, which pulldown never emits as text — so they
    // must be masked (the *references* `[^1]` in prose stay scanned, matching
    // pulldown's window gap arithmetic).
    excluded.extend(footnote_definition_label_ranges(text));

    walk_block_structure(tree.root_node(), &mut excluded, &mut windows);

    if let Some(inline) = inline {
        crate::ts_inline::extend_inline_exclusions_from_tree(inline, &mut excluded);
    }
    excluded.extend(math_ranges(text, &excluded));

    windows.sort_by_key(|r| (r.start, r.end));
    windows.dedup();

    (TextMask::new(excluded, text.len()), windows)
}

/// Node kinds whose entire range is excluded content (not prose).
fn is_block_excluded_kind(kind: &str) -> bool {
    matches!(
        kind,
        "fenced_code_block" | "indented_code_block" | "html_block" | "minus_metadata"
    )
}

/// Node kinds whose *content* (sans markers) is prose: the scan window is the
/// node's `inline` child when present, else the node itself.
const PROSE_BLOCK_KINDS: &[&str] = &["atx_heading", "setext_heading"];

/// Recursively walk the block tree, collecting excluded ranges and prose scan
/// windows in one pass.
fn walk_block_structure(
    node: Node<'_>,
    excluded: &mut Vec<Range<usize>>,
    windows: &mut Vec<Range<usize>>,
) {
    let kind = node.kind();

    if is_block_excluded_kind(kind) {
        excluded.push(node.byte_range());
        return;
    }

    if kind == "paragraph" {
        windows.push(node.byte_range());
        return;
    }

    if kind == "pipe_table" {
        // Pulldown's event stream does not flush between cells/rows: a table
        // scans as ONE window across all cells (pipes and the delimiter row
        // are gap bytes). Mirror that with the whole table node.
        windows.push(node.byte_range());
        return;
    }

    if PROSE_BLOCK_KINDS.contains(&kind) {
        let window = first_child_of_kind(&node, "inline")
            .map_or_else(|| node.byte_range(), |inline| inline.byte_range());
        windows.push(window);
        return;
    }

    if kind == "inline" {
        // Safety net: a bare inline section directly under the document or a
        // section (loose grammar output). Children of paragraphs/headings are
        // already covered by the parent's window and must not be added here.
        if matches!(
            node.parent().map(|p| p.kind()),
            Some("section") | Some("document")
        ) {
            windows.push(node.byte_range());
        }
        return;
    }

    // Descend into children.
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            walk_block_structure(cursor.node(), excluded, windows);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// First child of `node` with kind `want`, if any.
fn first_child_of_kind<'a>(node: &Node<'a>, want: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.node().kind() == want {
                return Some(cursor.node());
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

/// Byte ranges of footnote-definition labels: `[^label]:` at the start of a
/// line (after optional indentation).
///
/// Pulldown models a footnote definition as a `FootnoteDefinition` tag whose
/// label is never emitted as text, so neither the label nor its brackets can
/// appear in a scan window. Inline footnote *references* (`[^1]` in prose)
/// are not masked: pulldown's window gap arithmetic surfaces them as text
/// bytes, and the parity corpus asserts that behavior (footnotes.md).
fn footnote_definition_label_ranges(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut line_start = 0usize;
    for line in text.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        let indent = content.len() - content.trim_start_matches([' ', '\t']).len();
        if let Some(rest) = content[indent..].strip_prefix("[^")
            && let Some(close) = rest.find(']')
            && close > 0
            && rest.as_bytes().get(close + 1) == Some(&b':')
        {
            // "[^" (2) + label (close) + "]" (1) + ":" (1).
            let start = line_start + indent;
            out.push(start..start + close + 4);
        }
        line_start += line.len();
    }
    out
}

/// Run the scanner over one unmasked prose window, mapping spans into the full
/// text and routing them by kind.
#[allow(clippy::too_many_arguments)]
fn scan_window(
    snapshot: &TextSnapshot,
    range: Range<usize>,
    links: &mut Vec<super::pulldown_glue::Link>,
    tags: &mut Vec<Span<TextByte>>,
    block_ids: &mut Vec<Span<TextByte>>,
) {
    for span in scan_inline(snapshot, range) {
        push_scanned_span(span, links, tags, block_ids);
    }
}

/// Route one scanned span by kind. Shared by the full-parse scan and the
/// incremental structure engine's damaged-region rescans.
pub(crate) fn push_scanned_span(
    span: crate::scanner::ScannedSpan,
    links: &mut Vec<super::pulldown_glue::Link>,
    tags: &mut Vec<Span<TextByte>>,
    block_ids: &mut Vec<Span<TextByte>>,
) {
    match span.kind {
        SpanKind::Link | SpanKind::Embed => {
            if let Some(link) = super::pulldown_glue::Link::from_scanned(span) {
                links.push(link);
            }
        }
        SpanKind::Tag => tags.push(span.span),
        SpanKind::BlockId => block_ids.push(span.span),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset_parse_counters() {
        BLOCK_PARSE_COUNT.with(|c| c.set(0));
        crate::ts_inline::INLINE_PARSE_COUNT.with(|c| c.set(0));
    }

    fn block_parses() -> usize {
        BLOCK_PARSE_COUNT.with(|c| c.get())
    }

    fn inline_parses() -> usize {
        crate::ts_inline::INLINE_PARSE_COUNT.with(|c| c.get())
    }

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
        let tree = parse_block(src).expect("parse");
        let inline = crate::ts_inline::parse_inline(src);
        let (mask, _windows) = mask_and_windows_from_trees(src, &tree, inline.as_ref());
        assert!(mask.is_masked(2));
        let result = parse_note_ts(src.as_bytes()).expect("parse");
        assert!(result.links.is_empty());
    }

    #[test]
    fn parse_rejects_invalid_utf8_and_collects_all_inline_span_kinds() {
        assert!(parse_note_ts(&[0xff]).is_err());
        assert!(parse_note_ts(b"[[m[]]\0").is_err());

        let result = parse_note_ts(b"See ![[image.png]] and [[note]] #topic ^block").unwrap();
        assert_eq!(result.links.len(), 2);
        assert_eq!(result.links[0].target_raw, "image.png");
        assert_eq!(result.tags.len(), 1);
        assert_eq!(result.block_ids.len(), 1);
    }

    /// S6-T2: stage A must parse the block grammar exactly once per
    /// `parse_note_ts` call — and the inline grammar exactly once.
    #[test]
    fn stage_a_parses_block_grammar_once_per_call() {
        reset_parse_counters();
        let src = "# H [[link]]\n\n`[[code]]` and $x$ and [[tail]]\n";
        let result = parse_note_ts(src.as_bytes()).expect("parse");
        assert_eq!(
            block_parses(),
            1,
            "block grammar must be parsed exactly once"
        );
        assert_eq!(
            inline_parses(),
            1,
            "inline grammar must be parsed exactly once"
        );
        assert_eq!(
            result.links.len(),
            2,
            "heading and paragraph links both found"
        );
    }

    /// Windows must cover heading content (markers stripped) and the whole
    /// table (pulldown scans tables as one merged window) — the
    /// pulldown-visible prose.
    #[test]
    fn windows_strip_heading_markers_and_cover_whole_tables() {
        let src = "# H [[link]]\n\n| a | b |\n|---|---|\n| [[x]] | y |\n";
        let tree = parse_block(src).expect("parse");
        let inline = crate::ts_inline::parse_inline(src);
        let (_mask, windows) = mask_and_windows_from_trees(src, &tree, inline.as_ref());
        let table_start = src.find("| a |").expect("table start");
        let table_end = src.find("\n| [[x]]").map(|i| i + 1).expect("table end");
        let table_window = windows
            .iter()
            .find(|w| w.start == table_start)
            .expect("table window");
        // One window covers the entire table (header through last body row).
        assert!(
            table_window.end > table_end,
            "table window must cover body rows: {table_window:?}"
        );
        // No window may include a heading marker.
        for w in &windows {
            let slice = &src[w.clone()];
            assert!(
                !slice.starts_with("#"),
                "window must not include heading markers: {slice:?}"
            );
        }
        let result = parse_note_ts(src.as_bytes()).expect("parse");
        let targets: Vec<&str> = result.links.iter().map(|l| l.target_raw.as_str()).collect();
        assert_eq!(targets, ["link", "x"]);
    }

    /// Frontmatter (both `---` and `...` closers) is excluded via the shared
    /// heuristic even before the block grammar sees it.
    #[test]
    fn frontmatter_is_masked_with_shared_heuristic() {
        for src in [
            "---\ntitle: #tag\n---\n\n[[link]]\n",
            "---\ntitle: #tag\n...\n\n[[link]]\n",
        ] {
            let result = parse_note_ts(src.as_bytes()).expect("parse");
            assert_eq!(
                result.tags.len(),
                0,
                "frontmatter tag must be masked: {src:?}"
            );
            assert_eq!(result.links.len(), 1, "body link must survive: {src:?}");
        }
    }
}
