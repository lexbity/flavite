//! Literal source matching over borrowed normalized text (RFC-0006 D-05/§6.2).
//!
//! Ownership: this module implements match *options*, the literal *matcher*
//! and typed match *spans* — nothing else (§4.1). It receives an immutable
//! `&str` that the caller borrowed from an existing
//! `flavite_syntax::text::TextSnapshot`, never re-normalizes it and never
//! keeps a copy; every match is a [`flavite_syntax::Span`] in
//! [`flavite_syntax::text::TextByte`] coordinates.
//!
//! # Semantics pinned by D-05
//!
//! * Matching is leftmost, ascending and **non-overlapping**: after an
//!   accepted match the scan resumes after its end.
//! * The empty query is *Idle*, never "match everywhere": [`MatchOptions::validate`]
//!   rejects it and [`find_matches`] reports an empty result.
//! * Queries over [`MAX_QUERY_BYTES`] UTF-8 bytes are rejected (NFR-8).
//! * Case-insensitive matching approximates Rust-regex Unicode **simple**
//!   case folding — see [`chars_fold_eq`] for the exact relation and its
//!   documented limits. There is no accent stripping and no canonical
//!   normalization: `syntax::text::fold_key` is deliberately **not** reused
//!   here because it applies NFC and produces whole-string lookup keys,
//!   both wrong for per-scalar literal matching.
//! * Whole-word matches require **both** endpoints to be UAX #29 word
//!   boundaries (pinned `unicode-segmentation`); punctuation-only queries
//!   qualify when both endpoints are boundaries. The boundary check is a
//!   postfilter: a rejected candidate only advances the scan by one scalar,
//!   so a bounded occurrence inside a rejected one is still found.
//! * Zero-length matches do not exist in literal mode (the empty query is
//!   Idle).
//!
//! # Complexity
//!
//! The case-sensitive path uses `str::find` (std's Two-Way substring search,
//! effectively linear). The case-insensitive path is a single monotonic
//! left-to-right pass that decodes each scalar exactly once and keeps a
//! rolling window of the query's scalars; worst-case comparison cost is
//! `O(text_chars × query_chars)`, which is the accepted S01 cost for exact
//! simple-folding semantics without the regex engine. Whole-word mode adds
//! one word-segmentation pass (`O(text)`, boundaries buffered) and an
//! `O(log boundaries)` endpoint check per candidate.

use std::collections::VecDeque;
use std::fmt;

use unicode_segmentation::UnicodeSegmentation;

/// Maximum accepted query length in UTF-8 bytes (NFR-8: patterns ≤ 8 KiB).
pub const MAX_QUERY_BYTES: usize = 8 * 1024;

/// Matching mode (§6.2 `MatchOptions`).
///
/// S01 ships literal matching only. The enum is declared with one variant
/// now so option/plan digests can carry the mode from day one; the `Regex`
/// variant and its engine wiring arrive with the S03 helper process and are
/// deliberately **not** stubbed here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum MatchMode {
    /// Literal substring matching (D-05 default).
    #[default]
    Literal,
}

impl MatchMode {
    /// Whether replacement templates in this mode expand literally.
    ///
    /// Literal mode treats `$` as a plain character (D-05: "Replacement
    /// syntax is literal when regex is off"); see [`crate::template`].
    /// Regex mode (S03) recognizes `$0`/`$1`/`${name}`/`$$` and flips this
    /// to `false`.
    #[must_use]
    pub const fn replacement_is_literal(self) -> bool {
        matches!(self, Self::Literal)
    }
}

/// Match options (§6.2).
///
/// The default is the D-05 default find configuration: literal,
/// case-insensitive, whole-word off. The default query is empty, which
/// validates as Idle until the caller sets a real query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct MatchOptions {
    /// Matching mode (literal in S01).
    pub mode: MatchMode,
    /// Query text; must be non-empty and at most [`MAX_QUERY_BYTES`] bytes.
    pub query: String,
    /// Case-sensitive matching (default: off, i.e. simple case folding).
    pub case_sensitive: bool,
    /// Require both match endpoints to be UAX #29 word boundaries.
    pub whole_word: bool,
}

impl MatchOptions {
    /// Validate the options for a matching run.
    ///
    /// # Errors
    ///
    /// * [`MatchError::EmptyQuery`] when `query` is empty — the D-05 Idle
    ///   state; callers treat it as "no active query", not as "match
    ///   everywhere".
    /// * [`MatchError::QueryTooLarge`] when `query` exceeds
    ///   [`MAX_QUERY_BYTES`] UTF-8 bytes (NFR-8).
    /// * [`MatchError::InvalidOptions`] is reserved for option combinations
    ///   that later modes reject; no literal-mode combination is invalid
    ///   beyond the query checks above.
    pub fn validate(&self) -> Result<(), MatchError> {
        if self.query.is_empty() {
            return Err(MatchError::EmptyQuery);
        }
        if self.query.len() > MAX_QUERY_BYTES {
            return Err(MatchError::QueryTooLarge {
                len: self.query.len(),
                max: MAX_QUERY_BYTES,
            });
        }
        Ok(())
    }
}

/// Why a matching run was refused (§6.2: typed errors, never silent fallbacks).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MatchError {
    /// The query is empty. Literal empty queries are Idle (D-05), never a
    /// match at every position.
    EmptyQuery,
    /// The query exceeds the pattern-size limit (NFR-8).
    QueryTooLarge {
        /// Actual query length in UTF-8 bytes.
        len: usize,
        /// The [`MAX_QUERY_BYTES`] limit.
        max: usize,
    },
    /// The option combination is not valid. Reserved for option sets that
    /// later slices reject (e.g. regex flags before S03); no S01 literal
    /// combination produces this variant.
    InvalidOptions,
}

impl fmt::Display for MatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyQuery => {
                write!(
                    f,
                    "empty query: literal empty queries are Idle, not match-everywhere"
                )
            }
            Self::QueryTooLarge { len, max } => {
                write!(f, "query is {len} UTF-8 bytes, over the {max}-byte limit")
            }
            Self::InvalidOptions => write!(f, "invalid match options"),
        }
    }
}

impl std::error::Error for MatchError {}

/// One matched occurrence in normalized-text coordinates.
///
/// `range` reuses the canonical [`flavite_syntax::Span`] over
/// [`flavite_syntax::text::TextByte`] so callers map it to source bytes with
/// the same snapshot they borrowed the text from
/// (`TextSnapshot::span_to_source`); this crate introduces no second offset
/// type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MatchSpan {
    /// Half-open byte range in the normalized text that was searched.
    pub range: flavite_syntax::Span<flavite_syntax::text::TextByte>,
    /// Length of the match in `TextByte`s (UTF-8 bytes of the normalized text).
    pub length: usize,
}

/// Result of one matching run.
///
/// Matches are ascending and non-overlapping. `truncated` is `true` only
/// when scanning stopped early because `limit` matches were already
/// collected **and** at least one further match exists; callers surface
/// that state instead of presenting a capped count as complete (D-05:
/// counts must not silently stop at a cap).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MatchResult {
    /// Matches in ascending, non-overlapping order (may be empty).
    pub matches: Vec<MatchSpan>,
    /// Whether at least one match exists beyond `limit`.
    pub truncated: bool,
}

/// Find literal matches of `options.query` in `text` (§6.2).
///
/// `text` must be the already-normalized snapshot text (LF EOLs, BOM
/// removed); it is borrowed for the duration of the call and never copied or
/// re-normalized. `limit` caps how many matches are collected; when more
/// exist, `truncated` is set. `u64::MAX` means effectively unlimited.
///
/// Invalid options ([`MatchOptions::validate`]) yield the empty Idle result
/// instead of an error, matching the pinned signature; use
/// [`find_matches_checked`] when the caller must distinguish "no matches"
/// from "invalid query".
#[must_use]
pub fn find_matches(text: &str, options: &MatchOptions, limit: u64) -> MatchResult {
    find_matches_checked(text, options, limit).unwrap_or_default()
}

/// Validated variant of [`find_matches`].
///
/// # Errors
///
/// Returns the [`MatchError`] from [`MatchOptions::validate`] unchanged and
/// performs no matching in that case.
pub fn find_matches_checked(
    text: &str,
    options: &MatchOptions,
    limit: u64,
) -> Result<MatchResult, MatchError> {
    options.validate()?;
    Ok(find_matches_core(text, options, limit))
}

/// Bounded collector enforcing the match cap (checked arithmetic throughout).
struct Collector {
    cap: usize,
    spans: Vec<MatchSpan>,
    truncated: bool,
}

impl Collector {
    /// Record one match. Returns `false` when the cap was already full and
    /// scanning must stop (`truncated` is set in that case).
    fn push(&mut self, start: usize, end: usize) -> bool {
        if self.spans.len() >= self.cap {
            self.truncated = true;
            return false;
        }
        self.spans.push(MatchSpan {
            range: flavite_syntax::Span::new(
                flavite_syntax::text::TextByte(start),
                flavite_syntax::text::TextByte(end),
            ),
            length: end - start,
        });
        true
    }
}

fn find_matches_core(text: &str, options: &MatchOptions, limit: u64) -> MatchResult {
    // The caller-supplied cap is u64 per the §6.2 protocol; usize is wide
    // enough on supported targets, so only the (impossible) narrowing of a
    // >usize::MAX cap saturates.
    let cap = usize::try_from(limit).unwrap_or(usize::MAX);
    let mut out = Collector {
        cap,
        spans: Vec::new(),
        truncated: false,
    };
    let boundaries: Option<Vec<usize>> = options.whole_word.then(|| word_boundaries(text));
    if options.case_sensitive {
        scan_exact(text, &options.query, boundaries.as_deref(), &mut out);
    } else {
        scan_folded(text, &options.query, boundaries.as_deref(), &mut out);
    }
    MatchResult {
        matches: out.spans,
        truncated: out.truncated,
    }
}

/// Case-sensitive scan: std leftmost substring search, non-overlapping.
///
/// On a whole-word rejection the scan advances by exactly one scalar (not by
/// the match length) so bounded occurrences starting inside a rejected
/// candidate are still examined.
fn scan_exact(text: &str, query: &str, boundaries: Option<&[usize]>, out: &mut Collector) {
    let mut base = 0usize;
    while let Some(offset) = text[base..].find(query) {
        let start = base + offset;
        let end = start + query.len();
        if is_word_bounded(boundaries, start, end) {
            if !out.push(start, end) {
                return;
            }
            // Consume the match: the next candidate starts after its end.
            base = end;
        } else {
            base = next_char_boundary(text, start + 1);
        }
    }
}

/// Case-insensitive scan: rolling scalar window compared under simple
/// folding (see [`chars_fold_eq`]).
///
/// Single monotonic pass; each text scalar is decoded exactly once. An
/// accepted match consumes its span (window cleared), a rejected candidate
/// advances the window front by one scalar.
fn scan_folded(text: &str, query: &str, boundaries: Option<&[usize]>, out: &mut Collector) {
    let qchars: Vec<char> = query.chars().collect();
    let query_len = qchars.len();
    let mut window: VecDeque<(usize, char)> = VecDeque::with_capacity(query_len);
    let mut chars = text.char_indices();
    loop {
        // Keep the window filled with the next `query_len` scalars.
        while window.len() < query_len {
            match chars.next() {
                Some((offset, ch)) => window.push_back((offset, ch)),
                None => break,
            }
        }
        if window.len() < query_len {
            return; // Not enough text left for another full match.
        }
        let Some(&(start, _)) = window.front() else {
            return; // Unreachable: query_len >= 1 after validation.
        };
        let matched = window
            .iter()
            .zip(qchars.iter())
            .all(|(&(_, text_ch), &query_ch)| chars_fold_eq(text_ch, query_ch));
        if matched {
            // `window` is full and contiguous, so the candidate ends where
            // its last scalar ends.
            let end = window
                .back()
                .map_or(text.len(), |&(offset, ch)| offset + ch.len_utf8());
            if is_word_bounded(boundaries, start, end) {
                if !out.push(start, end) {
                    return;
                }
                window.clear(); // Non-overlapping: resume after the match.
            } else {
                window.pop_front(); // Rejected: retry from the next scalar.
            }
        } else {
            window.pop_front();
        }
    }
}

/// Simple case-folding relation for one scalar pair.
///
/// Two scalars are fold-equal iff they are equal, their
/// `char::to_lowercase` sequences are equal, or their `char::to_uppercase`
/// sequences are equal (std case mappings, no allocation).
///
/// This **approximates** Rust-regex Unicode simple case folding (D-05) using
/// only `std`; it is not a claim of full CaseFolding.txt parity. Consequences
/// (all pinned in `tests/literal_properties.rs`):
///
/// * `ß` does **not** fold to `ss`/`s` (std `to_uppercase` expands `ß` to
///   two chars, which never equals a one-char sequence) — the same outcome
///   as simple folding;
/// * composed/decomposed accents stay distinct: no canonical normalization
///   is applied;
/// * `σ`, `ς` and `Σ` fold together (`to_uppercase` maps both `σ` and `ς`
///   to `Σ`), matching simple folding;
/// * `İ` (U+0130) folds only to itself, matching non-Turkic simple folding;
/// * `K` (U+212A Kelvin) folds to `k` and `Å` (U+212B Angstrom) to `å`.
///
/// Exotic mappings where both round trips disagree with CaseFolding.txt
/// simple folding could differ; S03 regex mode uses the regex engine's own
/// folding directly instead of this relation.
fn chars_fold_eq(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase()) || a.to_uppercase().eq(b.to_uppercase())
}

/// Byte offsets at which a UAX #29 word segment starts, plus the text end.
///
/// Both match endpoints of a whole-word match must be in this set. Text end
/// is always a boundary, so final matches qualify without special cases.
fn word_boundaries(text: &str) -> Vec<usize> {
    let mut bounds: Vec<usize> = text.split_word_bound_indices().map(|(i, _)| i).collect();
    bounds.push(text.len());
    bounds
}

/// Whether both match endpoints are UAX #29 word boundaries.
///
/// `None` boundaries (whole-word off) accepts every candidate.
fn is_word_bounded(boundaries: Option<&[usize]>, start: usize, end: usize) -> bool {
    boundaries.is_none_or(|bounds| {
        bounds.binary_search(&start).is_ok() && bounds.binary_search(&end).is_ok()
    })
}

/// Smallest char boundary at or after `at` (clamped to the text end).
fn next_char_boundary(text: &str, mut at: usize) -> usize {
    while at < text.len() && !text.is_char_boundary(at) {
        at += 1;
    }
    at
}
