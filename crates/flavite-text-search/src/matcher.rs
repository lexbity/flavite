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
            Self::OrdinalOutOfRange { ordinal, total } => {
                write!(
                    f,
                    "match ordinal {ordinal} is out of range for {total} matches"
                )
            }
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
    /// # Errors
    ///
    /// [`MatchError::LimitExceeded`] and [`MatchError::InvalidScope`].
    pub fn find_in(
        &self,
        text: &str,
        scope: Option<Span<TextByte>>,
    ) -> Result<Vec<Match>, MatchError> {
        let scope = validate_scope(text, scope)?;
        let mut matches = Vec::new();
        match &self.engine {
            Engine::LiteralBytes { pattern } => {
                let haystack = &text[scope.start.0..scope.end.0];
                for (idx, segment) in haystack.match_indices(pattern.as_ref()) {
                    if matches.len() >= self.limits.max_matches {
                        return Err(limit_error(self.limits.max_matches));
                    }
                    let start = scope.start.0 + idx;
                    let span = Span::new(TextByte(start), TextByte(start + segment.len()));
                    matches.push(Match {
                        span,
                        groups: Arc::from([Some(span)]),
                    });
                }
            }
            Engine::LiteralFolded(compiled) | Engine::RegexMode(compiled) => {
                let haystack = &text[scope.start.0..scope.end.0];
                for caps in compiled.captures_iter(haystack) {
                    if matches.len() >= self.limits.max_matches {
                        return Err(limit_error(self.limits.max_matches));
                    }
                    let Some(whole) = caps.get(0) else {
                        continue;
                    };
                    let mut groups = Vec::with_capacity(caps.len());
                    for group in caps.iter() {
                        groups.push(group.map(|m| {
                            Span::new(
                                TextByte(scope.start.0 + m.start()),
                                TextByte(scope.start.0 + m.end()),
                            )
                        }));
                    }
                    matches.push(Match {
                        span: Span::new(
                            TextByte(scope.start.0 + whole.start()),
                            TextByte(scope.start.0 + whole.end()),
                        ),
                        groups: Arc::from(groups),
                    });
                }
            }
        }
        if self.options.whole_word {
            let boundaries = flavite_syntax::text::word_boundaries(text);
            matches.retain(|m| {
                flavite_syntax::text::is_word_boundary(&boundaries, m.span.start.0)
                    && flavite_syntax::text::is_word_boundary(&boundaries, m.span.end.0)
            });
        }
        Ok(matches)
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

fn limit_error(limit: usize) -> MatchError {
    MatchError::LimitExceeded {
        observed: limit + 1,
        limit,
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
                Span::new(TextByte(2 * index), TextByte(2 * index + 2))
            );
            assert_eq!(m.group_count(), 1);
        }
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
    }

    #[test]
    fn pattern_length_is_bounded() {
        let long = "a".repeat(PATTERN_MAX_BYTES + 1);
        let err = Matcher::new(&long, MatchOptions::default(), MatchLimits::default())
            .expect_err("pattern too long");
        assert!(matches!(err, MatchError::PatternTooLong { .. }));
    }
}
