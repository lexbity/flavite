//! Scanner-backed link semantics and in-memory autocomplete for the editor.

use flavite_syntax::{LinkPayload, ScannedSpan, TextMask, TextSnapshot, scanner::scan_inline};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use std::ops::Range;

use flavite_api::CompletionCandidate;

fn candidate_score(
    candidate: &CompletionCandidate,
    pattern: &Pattern,
    matcher: &mut Matcher,
    utf32: &mut Vec<char>,
) -> Option<u32> {
    [candidate.title.as_str(), candidate.target.as_str()]
        .into_iter()
        .chain(candidate.aliases.iter().map(String::as_str))
        .chain(candidate.stable_id.iter().map(String::as_str))
        .filter_map(|text| pattern.score(Utf32Str::new(text, utf32), matcher))
        .max()
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
    /// The resolver returned more than one indexed candidate.
    MultipleCandidates,
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
    scan_note(text).0
}

/// Scan links and decorations in one pass. The two views share the same
/// canonical parse; running [`scan_links`] and [`scan_decorations`]
/// separately parsed every keystroke's revision twice on the UI thread.
#[must_use]
pub fn scan_note(text: &str) -> (Vec<LinkSpan>, Vec<ScannedSpan>) {
    let Ok(parsed) = flavite_syntax::pulldown_glue::parse_normalized(text) else {
        return (Vec::new(), Vec::new());
    };
    let links = parsed
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
        .collect();
    let decorations = scan_masked(&parsed.snapshot, &parsed.mask);
    (links, decorations)
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
    scan_note(text).1
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
    candidates: &'a [CompletionCandidate],
    query: &str,
    limit: usize,
) -> Vec<&'a CompletionCandidate> {
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut utf32 = Vec::new();
    let limit = limit.min(8);
    if limit == 0 {
        return Vec::new();
    }
    let compare = |(score_a, a): &(u32, &CompletionCandidate),
                   (score_b, b): &(u32, &CompletionCandidate)| {
        score_b
            .cmp(score_a)
            .then_with(|| {
                a.title
                    .chars()
                    .flat_map(char::to_lowercase)
                    .cmp(b.title.chars().flat_map(char::to_lowercase))
            })
            .then_with(|| a.target.cmp(&b.target))
    };
    // Keep only visible suggestions. Scanning 50k candidates must not
    // allocate/sort a 50k-row result vector on every widget rebuild.
    let mut ranked = Vec::with_capacity(limit);
    for candidate in candidates {
        let score = if query.is_empty() {
            Some(0)
        } else {
            candidate_score(candidate, &pattern, &mut matcher, &mut utf32)
        };
        let Some(score) = score else { continue };
        let item = (score, candidate);
        let at = ranked.partition_point(|existing| compare(existing, &item).is_le());
        if at < limit {
            if ranked.len() == limit {
                ranked.pop();
            }
            ranked.insert(at, item);
        }
    }
    ranked.into_iter().map(|(_, candidate)| candidate).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_syntax::SpanKind;

    #[test]
    fn suggestions_remain_bounded_and_ordered_at_catalog_limit() {
        let candidates = (0..50_000)
            .rev()
            .map(|n| CompletionCandidate::new(format!("notes/{n:05}"), format!("Title {n:05}")))
            .collect::<Vec<_>>();
        let results = match_candidates(&candidates, "", usize::MAX);
        assert_eq!(results.len(), 8);
        assert_eq!(
            results
                .iter()
                .map(|c| c.target.as_str())
                .collect::<Vec<_>>(),
            (0..8).map(|n| format!("notes/{n:05}")).collect::<Vec<_>>()
        );
        assert!(match_candidates(&candidates, "", 0).is_empty());
    }

    #[test]
    fn scan_note_matches_the_individual_scans() {
        // The combined pass exists so a keystroke parses the document once;
        // it must return exactly what the two single-purpose scans return.
        let text = "[[Ideas]] `[[code]]`\n\n```md\n[[fence]]\n```\n\n$[[math]]$ ![[diagram.png]] #topic ^block\n\n*emphasis* and **strong** with [[Another]]".repeat(20);
        let (links, decorations) = scan_note(&text);
        assert_eq!(links, scan_links(&text));
        assert_eq!(decorations, scan_decorations(&text));
        assert!(!links.is_empty());
        assert!(!decorations.is_empty());
    }

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
        let mut first = CompletionCandidate::new("notes/ideas", "Ideas");
        first.aliases.push("Brainstorm".into());
        let second = CompletionCandidate::new("notes/inbox", "Inbox");
        let candidates = [second, first];
        let found = match_candidates(&candidates, "bra", 8);
        assert_eq!(found[0].title, "Ideas");
        assert!(match_candidates(&candidates, "zzz", 8).is_empty());
    }

    #[test]
    fn completion_can_match_unconflicted_stable_ids() {
        let mut candidate = CompletionCandidate::new("notes/target", "Target note");
        candidate.stable_id = Some("01JABCDEF".into());
        assert_eq!(match_candidates(&[candidate], "01JABC", 8).len(), 1);
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
            resolve_link(
                &make_span("ideas", false),
                Some(LinkResolution::MultipleCandidates)
            ),
            LinkResolution::MultipleCandidates
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
