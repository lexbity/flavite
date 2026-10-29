//! Scanner-backed link semantics and in-memory autocomplete for the editor.

use flavite_syntax::{LinkPayload, ScannedSpan, TextMask, TextSnapshot, scanner::scan_inline};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use std::ops::Range;

/// A note the host has already obtained from the vault's indexed title list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteCandidate {
    /// Link target, normally a vault-relative note path without `.md`.
    pub target: String,
    /// Indexed title.
    pub title: String,
    /// Indexed aliases.
    pub aliases: Vec<String>,
    /// Unconflicted stable ID, when available.
    pub stable_id: Option<String>,
}

impl NoteCandidate {
    /// Construct a candidate with no aliases or stable ID.
    #[must_use]
    pub fn new(target: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            title: title.into(),
            aliases: Vec::new(),
            stable_id: None,
        }
    }

    fn score(
        &self,
        pattern: &Pattern,
        matcher: &mut Matcher,
        utf32: &mut Vec<char>,
    ) -> Option<u32> {
        [self.title.as_str(), self.target.as_str()]
            .into_iter()
            .chain(self.aliases.iter().map(String::as_str))
            .filter_map(|candidate| pattern.score(Utf32Str::new(candidate, utf32), matcher))
            .max()
    }
}

/// Link scan output tied to one immutable editor view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSpan {
    /// Complete scanner span, including delimiters.
    pub span: Range<usize>,
    /// Link or embed payload.
    pub payload: LinkPayload,
    /// Whether this is an embed (`![[...]]`).
    pub embed: bool,
    /// Best-effort note resolution from the indexed candidate snapshot. Embed
    /// targets remain unclassified here because they resolve against attachments.
    pub resolution: LinkResolution,
}

/// Resolution status used for link decoration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkResolution {
    /// The target has one indexed note candidate.
    Resolved,
    /// More than one indexed note candidate matches.
    Ambiguous,
    /// No indexed note candidate matches.
    #[default]
    Unresolved,
    /// The span is an embed and must be resolved against attachments.
    NotApplicable,
}

/// Scan all non-code, non-HTML, non-math regions with the shared grammar.
/// This follows the indexer's `parse_note` mask and therefore cannot invent an
/// editor-only interpretation of link syntax.
#[must_use]
pub fn scan_links(text: &str) -> Vec<LinkSpan> {
    let Ok(parsed) = flavite_syntax::pulldown_glue::parse_note(text.as_bytes()) else {
        return Vec::new();
    };
    parsed
        .links
        .into_iter()
        .map(|link| LinkSpan {
            span: link.span.start.0..link.span.end.0,
            payload: LinkPayload {
                target_raw: link.target_raw,
                alias: link.alias,
                anchor: link.anchor,
                by_id: link.by_id,
            },
            embed: link.is_embed,
            resolution: if link.is_embed {
                LinkResolution::NotApplicable
            } else {
                LinkResolution::Unresolved
            },
        })
        .collect()
}

/// Use the index's verdict for a scanned link. Candidate ranking belongs to
/// `flavite-index`; the editor only maps a supplied snapshot onto decorations.
#[must_use]
pub fn resolve_link(span: &LinkSpan, verdict: Option<LinkResolution>) -> LinkResolution {
    if span.embed {
        return LinkResolution::NotApplicable;
    }
    verdict.unwrap_or(LinkResolution::Unresolved)
}

/// Shared-scanner spans for editor decorations; code/math/HTML are excluded.
#[must_use]
pub fn scan_decorations(text: &str) -> Vec<ScannedSpan> {
    let Ok(parsed) = flavite_syntax::pulldown_glue::parse_note(text.as_bytes()) else {
        return Vec::new();
    };
    scan_masked(&parsed.snapshot, &parsed.mask)
}

fn scan_masked(snapshot: &TextSnapshot, mask: &TextMask) -> Vec<ScannedSpan> {
    mask.unmasked(&(0..snapshot.text().len()))
        .into_iter()
        .flat_map(|range| scan_inline(snapshot, range))
        .collect()
}

/// Rank a bounded in-memory candidate list by subsequence match. The host
/// supplies the indexed title/alias list once; this function performs no I/O.
#[must_use]
pub fn match_candidates<'a>(
    candidates: &'a [NoteCandidate],
    query: &str,
    limit: usize,
) -> Vec<&'a NoteCandidate> {
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut utf32 = Vec::new();
    let mut ranked = candidates
        .iter()
        .filter_map(|candidate| {
            let score = if query.is_empty() {
                Some(0)
            } else {
                candidate.score(&pattern, &mut matcher, &mut utf32)
            }?;
            Some((score, candidate))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|(score_a, a), (score_b, b)| {
        score_b
            .cmp(score_a)
            .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
            .then_with(|| a.target.cmp(&b.target))
    });
    ranked
        .into_iter()
        .take(limit)
        .map(|(_, candidate)| candidate)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_syntax::SpanKind;

    #[test]
    fn links_and_decorations_use_shared_scanner_and_exclusions() {
        let text = "[[Ideas]] `[[code]]`\n\n```md\n[[fence]]\n```\n\n$[[math]]$ ![[diagram.png]] #topic ^block";
        let links = scan_links(text);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].payload.target_raw, "Ideas");
        assert!(!links[0].embed);
        assert_eq!(links[1].payload.target_raw, "diagram.png");
        assert!(links[1].embed);
        let decorations = scan_decorations(text);
        assert_eq!(
            decorations
                .iter()
                .filter(|span| span.kind == SpanKind::Tag)
                .count(),
            1
        );
        assert_eq!(
            decorations
                .iter()
                .filter(|span| span.kind == SpanKind::BlockId)
                .count(),
            1
        );
    }

    #[test]
    fn candidates_match_titles_targets_and_aliases_deterministically() {
        let mut first = NoteCandidate::new("notes/ideas", "Ideas");
        first.aliases.push("Brainstorm".into());
        let second = NoteCandidate::new("notes/inbox", "Inbox");
        let candidates = [second, first];
        let found = match_candidates(&candidates, "bra", 8);
        assert_eq!(found[0].title, "Ideas");
        assert!(match_candidates(&candidates, "zzz", 8).is_empty());
    }

    #[test]
    fn note_link_resolution_uses_the_index_verdict() {
        let make_span = |target: &str, by_id: bool| LinkSpan {
            span: 0..target.len() + 4,
            payload: LinkPayload {
                target_raw: target.into(),
                alias: None,
                anchor: None,
                by_id,
            },
            embed: false,
            resolution: LinkResolution::Unresolved,
        };

        assert_eq!(
            resolve_link(&make_span("ideas", false), Some(LinkResolution::Ambiguous)),
            LinkResolution::Ambiguous
        );
        assert_eq!(
            resolve_link(
                &make_span("id:01J9ZK", true),
                Some(LinkResolution::Resolved)
            ),
            LinkResolution::Resolved
        );
        assert_eq!(
            resolve_link(&make_span("missing", false), None),
            LinkResolution::Unresolved
        );
    }
}
