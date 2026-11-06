//! Literal and regex source matching over borrowed normalized UTF-8
//! (RFC-0006 §4.1, §6.2, D-05).

use std::sync::Arc;

use flavite_syntax::{Span, TextByte};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

/// Pattern size bound (NFR-8: patterns ≤ 8 KiB UTF-8).
pub const PATTERN_MAX_BYTES: usize = 8 * 1024;

/// Compiled-regex program size bound (NFR-8: compiled regex ≤ 8 MiB).
pub const REGEX_SIZE_LIMIT_BYTES: usize = 8 * 1024 * 1024;

/// Maximum capture groups per pattern (S03: checked compile group count).
pub const REGEX_MAX_GROUPS: usize = 64;

/// Match-count bound per operation (NFR-8: ≤ 1m matches/job). Exceeding it
/// is a typed error, never a silent truncation.
pub const DEFAULT_MAX_MATCHES: usize = 1_000_000;

/// Semantics version folded into option digests. Bump when the pinned
/// Unicode tables, word segmentation or engine behavior change: old plans
/// are never reinterpreted after an upgrade (§6.2).
pub const MATCH_SEMANTICS_VERSION: u32 = 1;

/// Matching mode (D-05). The two modes are deliberately separate: literal
/// and regex share the matcher but never share semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    /// Literal text matching (default).
    #[default]
    Literal,
    /// Rust `regex` language matching.
    Regex,
}

/// Match options (D-05 defaults: case-insensitive, whole word off).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MatchOptions {
    /// Literal or regex mode.
    pub mode: MatchMode,
    /// Literal default is case-insensitive (Unicode simple folding). In
    /// regex mode the standard inline flags override this setting.
    pub case_sensitive: bool,
    /// Both match endpoints must be UAX #29 word-segment boundaries.
    pub whole_word: bool,
}

impl Default for MatchOptions {
    fn default() -> Self {
        Self {
            mode: MatchMode::Literal,
            case_sensitive: false,
            whole_word: false,
        }
    }
}

impl MatchOptions {
    /// Digest of the options alone (word segmentation version included).
    #[must_use]
    pub fn digest(&self) -> u64 {
        let mut hash = fnv1a_start();
        hash = fnv1a(hash, &MATCH_SEMANTICS_VERSION.to_le_bytes());
        hash = fnv1a(hash, &[u8::from(self.mode == MatchMode::Regex)]);
        hash = fnv1a(hash, &[u8::from(self.case_sensitive)]);
        hash = fnv1a(hash, &[u8::from(self.whole_word)]);
        hash
    }
}

/// Per-operation match-count bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchLimits {
    /// Refuse (typed error, not truncation) beyond this many matches.
    pub max_matches: usize,
}

impl Default for MatchLimits {
    fn default() -> Self {
        Self {
            max_matches: DEFAULT_MAX_MATCHES,
        }
    }
}

/// One match with capture metadata tied to the searched text's lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// Whole-match span (capture group 0) in `TextByte` coordinates.
    pub span: Span<TextByte>,
    groups: Arc<[Option<Span<TextByte>>]>,
}

impl Match {
    /// Construct one match from its whole span and capture-group spans
    /// (group 0 included). Cross-crate oracle tests build matches without
    /// going through the engine, proving the consumer-facing shape is not
    /// engine-private.
    #[must_use]
    pub fn new(span: Span<TextByte>, groups: Vec<Option<Span<TextByte>>>) -> Self {
        Self {
            span,
            groups: groups.into(),
        }
    }

    /// Number of capture groups including group 0.
    #[must_use]
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// Span of capture group `index`; `None` when the group did not
    /// participate in the match.
    #[must_use]
    pub fn group_span(&self, index: usize) -> Option<Span<TextByte>> {
        self.groups.get(index).copied().flatten()
    }

    /// Text of capture group `index` borrowed from the searched source.
    #[must_use]
    pub fn group_text<'t>(&self, text: &'t str, index: usize) -> Option<&'t str> {
        let span = self.group_span(index)?;
        text.get(span.start.0..span.end.0)
    }
}

/// Matching errors: typed, never a partial result.
#[derive(Debug, Clone)]
pub enum MatchError {
    /// A literal empty query is Idle, not "match everywhere" (D-05).
    EmptyQuery,
    /// Pattern exceeded [`PATTERN_MAX_BYTES`].
    PatternTooLong {
        /// Observed pattern size.
        observed: usize,
        /// The configured limit.
        limit: usize,
    },
    /// Invalid or oversize regex program.
    Regex(regex::Error),
    /// More capture groups than [`REGEX_MAX_GROUPS`].
    RegexTooManyGroups {
        /// Observed group count (including group 0).
        observed: usize,
        /// The configured limit.
        limit: usize,
    },
    /// A scope endpoint outside the text or mid-scalar.
    InvalidScope {
        /// The offending scope endpoint.
        offset: usize,
    },
    /// The operation would exceed its match-count budget.
    LimitExceeded {
        /// Observed count at refusal (limit + 1).
        observed: usize,
        /// The configured limit.
        limit: usize,
    },
    /// A requested ordinal beyond the match list.
    OrdinalOutOfRange {
        /// The rejected ordinal.
        ordinal: usize,
        /// Total match count.
        total: usize,
    },
}

impl std::fmt::Display for MatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::EmptyQuery => write!(
                f,
                "empty literal query is Idle, not a match-everywhere search"
            ),
            Self::PatternTooLong { observed, limit } => {
                write!(f, "pattern is {observed} bytes, limit is {limit}")
            }
            Self::Regex(ref source) => write!(f, "regex pattern is invalid: {source}"),
            Self::RegexTooManyGroups { observed, limit } => {
                write!(f, "regex has {observed} capture groups, limit is {limit}")
            }
            Self::InvalidScope { offset } => write!(
                f,
                "scope offset {offset} is not a char boundary within the searched text"
            ),
            Self::LimitExceeded { observed, limit } => {
                write!(f, "match count {observed} exceeds limit {limit}")
            }
            Self::OrdinalOutOfRange { ordinal, total } => write!(
                f,
                "match ordinal {ordinal} is out of range for {total} matches"
            ),
        }
    }
}

impl std::error::Error for MatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Regex(source) => Some(source),
            _ => None,
        }
    }
}

impl From<regex::Error> for MatchError {
    fn from(source: regex::Error) -> Self {
        Self::Regex(source)
    }
}

#[derive(Debug, Clone)]
enum Engine {
    /// Case-sensitive literal scan (no regex involved).
    LiteralBytes { pattern: Box<str> },
    /// Case-insensitive literal: the escaped pattern under case folding,
    /// giving exactly Rust regex Unicode simple folding (D-05).
    LiteralFolded(Regex),
    /// Explicit regex mode.
    RegexMode(Regex),
}

/// A compiled, reusable matcher: build once, search many sources.
#[derive(Debug, Clone)]
pub struct Matcher {
    options: MatchOptions,
    limits: MatchLimits,
    engine: Engine,
    digest: u64,
    group_names: Arc<[Option<String>]>,
}

impl Matcher {
    /// Compile `pattern` under `options`/`limits`.
    ///
    /// # Errors
    ///
    /// [`MatchError::EmptyQuery`] for a literal empty query,
    /// [`MatchError::PatternTooLong`], [`MatchError::Regex`] for invalid or
    /// oversize regex programs and [`MatchError::RegexTooManyGroups`].
    pub fn new(
        pattern: &str,
        options: MatchOptions,
        limits: MatchLimits,
    ) -> Result<Self, MatchError> {
        if pattern.len() > PATTERN_MAX_BYTES {
            return Err(MatchError::PatternTooLong {
                observed: pattern.len(),
                limit: PATTERN_MAX_BYTES,
            });
        }
        let (engine, group_names) = match options.mode {
            MatchMode::Literal if pattern.is_empty() => return Err(MatchError::EmptyQuery),
            MatchMode::Literal if options.case_sensitive => (
                Engine::LiteralBytes {
                    pattern: pattern.into(),
                },
                Arc::from([None]),
            ),
            MatchMode::Literal => {
                let folded = RegexBuilder::new(&regex::escape(pattern))
                    .case_insensitive(true)
                    .size_limit(REGEX_SIZE_LIMIT_BYTES)
                    .build()?;
                (Engine::LiteralFolded(folded), Arc::from([None]))
            }
            MatchMode::Regex => {
                let compiled = RegexBuilder::new(pattern)
                    .case_insensitive(!options.case_sensitive)
                    .multi_line(true)
                    .dot_matches_new_line(false)
                    .size_limit(REGEX_SIZE_LIMIT_BYTES)
                    .build()?;
                if compiled.captures_len() > REGEX_MAX_GROUPS {
                    return Err(MatchError::RegexTooManyGroups {
                        observed: compiled.captures_len(),
                        limit: REGEX_MAX_GROUPS,
                    });
                }
                let names: Vec<Option<String>> = compiled
                    .capture_names()
                    .map(|name| name.map(str::to_owned))
                    .collect();
                (Engine::RegexMode(compiled), Arc::from(names))
            }
        };
        let mut digest = options.digest();
        digest = fnv1a(digest, &limits.max_matches.to_le_bytes());
        digest = fnv1a(digest, pattern.as_bytes());
        Ok(Self {
            options,
            limits,
            engine,
            digest,
            group_names,
        })
    }

    /// The compiled options.
    #[must_use]
    pub const fn options(&self) -> &MatchOptions {
        &self.options
    }

    /// Digest binding semantics version, options, limits and pattern.
    #[must_use]
    pub const fn digest(&self) -> u64 {
        self.digest
    }

    /// Capture group count including group 0.
    #[must_use]
    pub fn capture_count(&self) -> usize {
        self.group_names.len()
    }

    /// Name of capture group `index`, if named.
    #[must_use]
    pub fn capture_name(&self, index: usize) -> Option<&str> {
        self.group_names.get(index).and_then(Option::as_deref)
    }

    /// Find all matches over the whole text, ascending and non-overlapping.
    ///
    /// # Errors
    ///
    /// [`MatchError::LimitExceeded`] and [`MatchError::InvalidScope`].
    pub fn find(&self, text: &str) -> Result<Vec<Match>, MatchError> {
        self.find_in(text, None)
    }

    /// Find all matches fully contained in `scope`, ascending and
    /// non-overlapping. A match straddling a scope edge is excluded, never
    /// clipped (D-05: the selected-text scope is frozen to exact ranges).
    ///
    /// Whole-word filtering is evaluated against the FULL text's UAX #29
    /// boundaries, never the scope slice's — the scope restricts which
    /// matches are reported, not what a match means.
    ///
    /// # Errors
    ///
    /// [`MatchError::LimitExceeded`] and [`MatchError::InvalidScope`].
    pub fn find_in(
        &self,
        text: &str,
        scope: Option<Span<TextByte>>,
    ) -> Result<Vec<Match>, MatchError> {
        self.iter_matches(text, scope)?.collect()
    }

    /// Streaming match iterator with a monotonic source cursor (§6.2).
    /// The helper and the collect-all [`Matcher::find_in`] consume this one
    /// semantic path: the literal walk, simple-fold case matching, UAX #29
    /// whole-word endpoints and the checked match budget live exactly here,
    /// never re-implemented elsewhere.
    ///
    /// The iterator yields ascending, non-overlapping matches. Exceeding
    /// [`MatchLimits::max_matches`] is a yielded
    /// [`MatchError::LimitExceeded`] item — the consumer decides how to
    /// report it — and the iterator terminates after that item.
    ///
    /// # Errors
    ///
    /// [`MatchError::InvalidScope`] if a scope endpoint is out of bounds,
    /// reversed, or mid-scalar.
    pub fn iter_matches<'t>(
        &'t self,
        text: &'t str,
        scope: Option<Span<TextByte>>,
    ) -> Result<MatchIter<'t>, MatchError> {
        let scope = validate_scope(text, scope)?;
        let boundaries = if self.options.whole_word {
            Some(flavite_syntax::text::WordBoundarySet::compute(text))
        } else {
            None
        };
        let inner = match &self.engine {
            Engine::LiteralBytes { pattern } => InnerIter::Literal {
                haystack: &text[scope.start.0..scope.end.0],
                pattern: pattern.clone(),
                cursor: 0,
                base: scope.start.0,
            },
            Engine::LiteralFolded(compiled) | Engine::RegexMode(compiled) => {
                // Keep our own scalar cursor rather than `captures_iter`:
                // Rust's iterator suppresses an empty match immediately
                // after a non-empty match. D-05 requires the empty match at
                // that same boundary, then scalar progress after the empty
                // match (including exactly one end-of-document match).
                InnerIter::Regex {
                    regex: compiled,
                    haystack: &text[scope.start.0..scope.end.0],
                    cursor: 0,
                    base: scope.start.0,
                }
            }
        };
        Ok(MatchIter {
            limits: self.limits,
            text,
            boundaries,
            inner,
            emitted: 0,
            done: false,
        })
    }
}

enum InnerIter<'t> {
    /// Case-sensitive literal walk: leftmost, non-overlapping, ascending.
    Literal {
        haystack: &'t str,
        pattern: Box<str>,
        cursor: usize,
        base: usize,
    },
    /// Regex/folded-literal captures over the scope slice, offsets rebased
    /// to the full text.
    Regex {
        regex: &'t Regex,
        haystack: &'t str,
        cursor: usize,
        base: usize,
    },
}

/// Borrowed streaming match iterator (see [`Matcher::iter_matches`]).
pub struct MatchIter<'t> {
    limits: MatchLimits,
    text: &'t str,
    boundaries: Option<flavite_syntax::text::WordBoundarySet>,
    inner: InnerIter<'t>,
    emitted: usize,
    done: bool,
}

impl Iterator for MatchIter<'_> {
    type Item = Result<Match, MatchError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            let candidate = match &mut self.inner {
                InnerIter::Literal {
                    haystack,
                    pattern,
                    cursor,
                    base,
                } => {
                    if *cursor > haystack.len() {
                        self.done = true;
                        return None;
                    }
                    let tail = &haystack[*cursor..];
                    let relative = if pattern.is_empty() {
                        0
                    } else if let Some(at) = tail.find(&**pattern) {
                        at
                    } else {
                        self.done = true;
                        return None;
                    };
                    let start = *base + *cursor + relative;
                    let end = start + pattern.len();
                    // Non-overlap: advance past the whole match. Zero-length
                    // literal patterns still move the cursor (the EmptyQuery
                    // guard makes this unreachable for the UI, but the walk
                    // must never loop on one position).
                    *cursor = (end - *base).max(*cursor + 1);
                    let span = Span::new(TextByte(start), TextByte(end));
                    Ok(Match::new(span, vec![Some(span)]))
                }
                InnerIter::Regex {
                    regex,
                    haystack,
                    cursor,
                    base,
                } => {
                    if let Some(candidate) = next_regex_candidate(regex, haystack, cursor, *base) {
                        candidate
                    } else {
                        self.done = true;
                        return None;
                    }
                }
            };
            let Ok(hit) = candidate else {
                self.done = true;
                return Some(candidate);
            };
            if let Some(boundaries) = &self.boundaries {
                // Whole word requires BOTH endpoints to be UAX #29 word
                // boundaries of the whole text (§6.2).
                if !boundaries.contains(hit.span.start.0) || !boundaries.contains(hit.span.end.0) {
                    continue;
                }
            }
            if self.emitted >= self.limits.max_matches {
                self.done = true;
                return Some(Err(MatchError::LimitExceeded {
                    observed: self.emitted + 1,
                    limit: self.limits.max_matches,
                }));
            }
            self.emitted += 1;
            return Some(Ok(hit));
        }
    }
}

fn next_regex_candidate(
    regex: &Regex,
    haystack: &str,
    cursor: &mut usize,
    base: usize,
) -> Option<Result<Match, MatchError>> {
    if *cursor > haystack.len() {
        return None;
    }
    let caps = regex.captures_at(haystack, *cursor)?;
    let whole = caps.get(0)?;
    if whole.start() == whole.end() {
        // The sentinel len + 1 prevents a second EOF match. Otherwise
        // advance one Unicode scalar so a location appears only once.
        *cursor = if whole.end() == haystack.len() {
            haystack.len().saturating_add(1)
        } else {
            whole.end()
                + haystack[whole.end()..]
                    .chars()
                    .next()
                    .map_or(0, char::len_utf8)
        };
    } else {
        // Re-search at a non-EOF endpoint so a zero-width match at that
        // boundary is observable, as D-05 requires.
        *cursor = if whole.end() == haystack.len() {
            haystack.len().saturating_add(1)
        } else {
            whole.end()
        };
    }
    let groups = caps
        .iter()
        .map(|group| group.map(|m| Span::new(TextByte(base + m.start()), TextByte(base + m.end()))))
        .collect();
    Some(Ok(Match::new(
        Span::new(TextByte(base + whole.start()), TextByte(base + whole.end())),
        groups,
    )))
}

/// Borrowed text the iterator searches (exposed for consumer-side
/// capture-text checks without cloning).
impl<'t> MatchIter<'t> {
    #[must_use]
    pub const fn text(&self) -> &'t str {
        self.text
    }
}

fn validate_scope(text: &str, scope: Option<Span<TextByte>>) -> Result<Span<TextByte>, MatchError> {
    match scope {
        None => Ok(Span::new(TextByte(0), TextByte(text.len()))),
        Some(scope) => {
            for offset in [scope.start.0, scope.end.0] {
                if offset > text.len() || !text.is_char_boundary(offset) {
                    return Err(MatchError::InvalidScope { offset });
                }
            }
            if scope.end < scope.start {
                return Err(MatchError::InvalidScope {
                    offset: scope.end.0,
                });
            }
            Ok(scope)
        }
    }
}

/// Select matches by requested ordinals (§6.2: "select next/all or
/// requested match ordinals without overlap"). Ordinals are deduplicated
/// and sorted, so the result is ascending and non-overlapping by
/// construction.
///
/// # Errors
///
/// [`MatchError::OrdinalOutOfRange`] for any ordinal beyond the matches.
pub fn select_ordinals(matches: &[Match], ordinals: &[usize]) -> Result<Vec<Match>, MatchError> {
    let mut picked: Vec<usize> = ordinals.to_vec();
    picked.sort_unstable();
    picked.dedup();
    for ordinal in &picked {
        if *ordinal >= matches.len() {
            return Err(MatchError::OrdinalOutOfRange {
                ordinal: *ordinal,
                total: matches.len(),
            });
        }
    }
    Ok(picked
        .into_iter()
        .map(|ordinal| matches[ordinal].clone())
        .collect())
}

/// One page of a match list (§6.2: the crate owns `MatchPage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchPage {
    /// Zero-based index of the first match on this page.
    pub start: usize,
    /// Maximum number of matches on this page.
    pub len: usize,
}

impl MatchPage {
    /// Default page length (D-07: pages default to 50).
    pub const DEFAULT_LEN: usize = 50;

    /// A page starting at `start` with up to `len` matches.
    #[must_use]
    pub const fn new(start: usize, len: usize) -> Self {
        Self { start, len }
    }

    /// Slice a match list; pages past the end are empty, never an error
    /// (page replay is idempotent).
    #[must_use]
    pub fn slice<'m>(&self, matches: &'m [Match]) -> &'m [Match] {
        let start = self.start.min(matches.len());
        let end = start.saturating_add(self.len).min(matches.len());
        &matches[start..end]
    }

    /// Whether more matches exist after this page.
    #[must_use]
    pub const fn has_more(&self, total: usize) -> bool {
        self.start + self.len < total
    }
}

/// FNV-1a 64-bit digest start offset.
pub(crate) const fn fnv1a_start() -> u64 {
    0xcbf2_9ce4_8422_2325
}

/// FNV-1a 64-bit fold of `bytes` into `hash` (deterministic digests only).
pub(crate) const fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    let mut index = 0;
    while index < bytes.len() {
        hash ^= u64::from_ne_bytes([bytes[index], 0, 0, 0, 0, 0, 0, 0]);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        index += 1;
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal(pattern: &str) -> Matcher {
        Matcher::new(pattern, MatchOptions::default(), MatchLimits::default()).expect("compiles")
    }

    #[test]
    fn case_sensitive_literal_scans_ascending() {
        let matcher = Matcher::new(
            "aa",
            MatchOptions {
                case_sensitive: true,
                ..MatchOptions::default()
            },
            MatchLimits::default(),
        )
        .unwrap();
        let found = matcher.find("aaaaaa").unwrap();
        assert_eq!(found.len(), 3, "non-overlapping, ascending");
        for (index, m) in found.iter().enumerate() {
            assert_eq!(
                m.span,
                Span::new(TextByte(2 * index), TextByte(4 + 2 * index - 2))
            );
        }
        assert_eq!(found[0].group_count(), 1);
    }

    #[test]
    fn case_sensitive_literal_handles_multi_byte() {
        // "é" is two bytes; spans are measured in the searched text, never
        // derived from pattern length.
        let matcher = Matcher::new(
            "éé",
            MatchOptions {
                case_sensitive: true,
                ..MatchOptions::default()
            },
            MatchLimits::default(),
        )
        .unwrap();
        let found = matcher.find("xéééé").unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].span, Span::new(TextByte(1), TextByte(5)));
        assert_eq!(found[1].span, Span::new(TextByte(5), TextByte(9)));
    }

    #[test]
    fn empty_literal_query_is_idle() {
        let err = Matcher::new("", MatchOptions::default(), MatchLimits::default())
            .expect_err("empty literal query");
        assert!(matches!(err, MatchError::EmptyQuery));
    }

    #[test]
    fn scoped_find_requires_full_containment() {
        let matcher = literal("b");
        let scope = Span::new(TextByte(1), TextByte(4));
        let found = matcher.find_in("ababa", Some(scope)).unwrap();
        assert_eq!(found.len(), 2, "straddling match at 4 is excluded");
        assert_eq!(found[0].span, Span::new(TextByte(1), TextByte(2)));
        assert_eq!(found[1].span, Span::new(TextByte(3), TextByte(4)));

        // Byte 1 is inside the two-byte é scalar.
        let err = matcher
            .find_in("éa", Some(Span::new(TextByte(1), TextByte(3))))
            .expect_err("mid-scalar scope start");
        assert!(matches!(err, MatchError::InvalidScope { offset: 1 }));
    }

    #[test]
    fn scoped_regex_whole_word_uses_whole_text_boundaries() {
        // The scope slice must not redefine word boundaries: "world"
        // starting inside "helloworld" is never a whole word, even though
        // the scope slice starts at its own offset zero.
        let options = MatchOptions {
            mode: MatchMode::Regex,
            whole_word: true,
            ..MatchOptions::default()
        };
        let matcher = Matcher::new("world", options, MatchLimits::default()).unwrap();
        let found = matcher
            .find_in("helloworld", Some(Span::new(TextByte(5), TextByte(10))))
            .unwrap();
        assert!(found.is_empty(), "edge-of-word inside a larger word");
        let found = matcher
            .find_in("hello world", Some(Span::new(TextByte(6), TextByte(11))))
            .unwrap();
        assert_eq!(found.len(), 1, "same slice content, real boundary");
    }

    #[test]
    fn streaming_iterator_and_collect_agree() {
        let matcher = Matcher::new(
            "a*",
            MatchOptions {
                mode: MatchMode::Regex,
                ..MatchOptions::default()
            },
            MatchLimits::default(),
        )
        .unwrap();
        let text = "ab aca a";
        let streamed: Vec<(usize, usize)> = matcher
            .iter_matches(text, None)
            .unwrap()
            .map(|item| item.expect("within budget"))
            .map(|m| (m.span.start.0, m.span.end.0))
            .collect();
        let collected: Vec<(usize, usize)> = matcher
            .find(text)
            .unwrap()
            .into_iter()
            .map(|m| (m.span.start.0, m.span.end.0))
            .collect();
        assert_eq!(streamed, collected);
        assert!(!streamed.is_empty(), "a* always produces empty matches");
    }

    #[test]
    fn iterator_budget_is_a_typed_item_then_terminates() {
        let matcher = Matcher::new(
            "aa",
            MatchOptions {
                case_sensitive: true,
                ..MatchOptions::default()
            },
            MatchLimits { max_matches: 2 },
        )
        .unwrap();
        let items: Vec<_> = matcher.iter_matches("aaaaaa", None).unwrap().collect();
        assert_eq!(items.len(), 3);
        assert!(items[0].is_ok() && items[1].is_ok());
        assert!(matches!(
            items[2],
            Err(MatchError::LimitExceeded {
                observed: 3,
                limit: 2,
            })
        ));
        // Terminated: no further items after the budget failure.
        let mut iter = matcher.iter_matches("aaaaaa", None).unwrap();
        for _ in 0..3 {
            assert!(iter.next().is_some());
        }
        assert!(iter.next().is_none());
    }

    #[test]
    fn match_count_limit_is_typed_not_truncated() {
        let matcher =
            Matcher::new("a", MatchOptions::default(), MatchLimits { max_matches: 2 }).unwrap();
        let err = matcher.find("aaaa").expect_err("over limit");
        assert!(matches!(
            err,
            MatchError::LimitExceeded {
                observed: 3,
                limit: 2,
            }
        ));
    }

    #[test]
    fn pages_slice_idempotently() {
        let matcher = literal("a");
        let found = matcher.find("ababab").unwrap();
        assert_eq!(found.len(), 3);
        let page = MatchPage::new(2, MatchPage::DEFAULT_LEN);
        assert_eq!(page.slice(&found).len(), 1);
        assert!(!page.has_more(found.len()));
        let past = MatchPage::new(100, 50);
        assert_eq!(past.slice(&found), &[]);
        assert!(!past.has_more(found.len()));
        let first = MatchPage::new(0, 2);
        assert_eq!(first.slice(&found).len(), 2);
        assert!(first.has_more(found.len()));
    }

    #[test]
    fn ordinal_selection_sorts_dedups_and_validates() {
        let matcher = literal("a");
        let found = matcher.find("aaaa").unwrap();
        let picked = select_ordinals(&found, &[3, 1, 1]).unwrap();
        assert_eq!(picked.len(), 2);
        assert_eq!(picked[0].span, Span::new(TextByte(1), TextByte(2)));
        assert_eq!(picked[1].span, Span::new(TextByte(3), TextByte(4)));
        let err = select_ordinals(&found, &[4]).expect_err("ordinal out of range");
        assert!(matches!(
            err,
            MatchError::OrdinalOutOfRange {
                ordinal: 4,
                total: 4,
            }
        ));
        assert!(select_ordinals(&found, &[]).unwrap().is_empty());
    }

    #[test]
    fn digest_binds_options_semantics_and_pattern() {
        let base = literal("abc").digest();
        assert_eq!(base, literal("abc").digest(), "digest is deterministic");
        assert_ne!(base, literal("abd").digest());
        assert_ne!(
            base,
            Matcher::new(
                "abc",
                MatchOptions {
                    case_sensitive: true,
                    ..MatchOptions::default()
                },
                MatchLimits::default(),
            )
            .unwrap()
            .digest()
        );
        assert_ne!(
            base,
            Matcher::new(
                "abc",
                MatchOptions {
                    whole_word: true,
                    ..MatchOptions::default()
                },
                MatchLimits::default(),
            )
            .unwrap()
            .digest()
        );
        // Limits are part of the identity: old plans never change bound.
        assert_ne!(
            base,
            Matcher::new(
                "abc",
                MatchOptions::default(),
                MatchLimits { max_matches: 9 }
            )
            .unwrap()
            .digest()
        );
    }

    #[test]
    fn pattern_length_is_bounded() {
        let long = "a".repeat(PATTERN_MAX_BYTES + 1);
        let err = Matcher::new(&long, MatchOptions::default(), MatchLimits::default())
            .expect_err("pattern too long");
        assert!(matches!(err, MatchError::PatternTooLong { .. }));
    }
}
