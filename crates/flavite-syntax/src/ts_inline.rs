//! Inline-grammar exclusion pass (RFC-0008 S6, stage A).
//!
//! The tree-sitter-md **block** grammar leaves inline content opaque, so the
//! block tree alone cannot see inline code spans, inline HTML, math or link
//! destinations. This module parses the same text once with the **inline**
//! grammar — a separate language, hence a separate parse — and extends the
//! exclusion mask with the ranges pulldown's event stream also omits:
//!
//! * `code_span` — matches pulldown `Code` events;
//! * `html_tag` — matches pulldown `Html`/`InlineHtml` events;
//! * `latex_block` — math spans (also covered by `crate::mask::math_ranges`,
//!   kept here for grammar-level belt-and-braces);
//! * `link_destination` / `link_title` — pulldown never emits the destination
//!   or title of an inline link/image as text, so a `#anchor` in a URL must
//!   not be scanned as a tag (verified against pulldown's event stream).
//!
//! The scanner remains the single semantics source (D10); this pass only
//! decides *what is not prose*.

use std::ops::Range;
use tree_sitter::{Language, Parser, Tree, TreeCursor};

// Number of inline-grammar `parse` calls made on the current thread since the
// last reset (`#[cfg(test)]` instrumentation for S6-T2). Thread-local so
// parallel tests never see each other's parses.
#[cfg(test)]
thread_local! {
    pub(crate) static INLINE_PARSE_COUNT: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

fn inline_language() -> Language {
    tree_sitter_md::INLINE_LANGUAGE.into()
}

/// Parse `text` with the inline grammar. Counted under `#[cfg(test)]` so
/// S6-T2 can assert exactly one inline parse per `parse_note_ts` call.
pub(crate) fn parse_inline(text: &str) -> Option<Tree> {
    #[cfg(test)]
    INLINE_PARSE_COUNT.with(|c| c.set(c.get() + 1));
    let mut parser = Parser::new();
    parser.set_language(&inline_language()).ok()?;
    parser.parse(text, None)
}

/// Extend `excluded` with inline-grammar ranges that are not prose.
/// Test-only: production paths use the parsed-tree variant.
#[cfg(test)]
pub(crate) fn extend_inline_exclusions(text: &str, excluded: &mut Vec<Range<usize>>) {
    let Some(tree) = parse_inline(text) else {
        return;
    };
    extend_inline_exclusions_from_tree(&tree, excluded);
}

/// Extend `excluded` from an already-parsed inline tree. Used by the
/// incremental structure engine, which owns the inline tree and must not
/// re-parse the whole document per keystroke.
pub(crate) fn extend_inline_exclusions_from_tree(tree: &Tree, excluded: &mut Vec<Range<usize>>) {
    let mut cursor = tree.root_node().walk();
    collect_inline_excluded_ranges(&mut cursor, excluded);
}

/// `true` for inline-grammar node kinds whose content is not prose.
fn is_inline_excluded_kind(kind: &str) -> bool {
    matches!(
        kind,
        "code_span" | "html_tag" | "latex_block" | "link_destination" | "link_title"
    )
}

/// Recursively collect excluded byte ranges from inline-grammar nodes.
fn collect_inline_excluded_ranges(cursor: &mut TreeCursor<'_>, excluded: &mut Vec<Range<usize>>) {
    let node = cursor.node();
    if is_inline_excluded_kind(node.kind()) {
        excluded.push(node.byte_range());
        // Don't descend into excluded nodes.
        return;
    }
    if cursor.goto_first_child() {
        loop {
            collect_inline_excluded_ranges(cursor, excluded);
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
    fn inline_exclusions_cover_code_html_math_and_destinations() {
        let text = "`[[code]]` <a href=\"#tag\">[[html]]</a> $x$ [t](https://x#a)\n";
        let mut excluded = Vec::new();
        extend_inline_exclusions(text, &mut excluded);
        let len = text.len();
        let mask = crate::TextMask::new(excluded, len);
        // Inline code is masked (` "[[code]]` spans bytes 0..=9).
        assert!(mask.is_masked(0));
        assert!(mask.is_masked(8));
        // The HTML opening tag (including its `#tag` attribute) is masked
        // (`<a href="#tag">` spans bytes 11..=25).
        assert!(mask.is_masked(13));
        assert!(mask.is_masked(20));
        assert!(mask.is_masked(25));
        // Math is masked (`$x$` spans bytes 39..=41).
        assert!(mask.is_masked(39));
        assert!(mask.is_masked(40));
        // The link destination (including its `#a` anchor) is masked
        // (`https://x#a` spans bytes 47..=58).
        assert!(mask.is_masked(49));
        assert!(mask.is_masked(56));
        // But the prose in between stays unmasked: `[[html]]` (bytes 26..=33).
        assert!(!mask.is_masked(26));
        assert!(!mask.is_masked(28));
        assert!(!mask.is_masked(32));
        // And the link label `[t]` stays unmasked (bytes 43..=45).
        assert!(!mask.is_masked(43));
        assert!(!mask.is_masked(44));
        assert!(!mask.is_masked(45));
    }
}
