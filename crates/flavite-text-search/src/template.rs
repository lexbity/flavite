//! Replacement-template validation and expansion (RFC-0006 §4.1, §6.2,
//! D-05).
//!
//! Replacement syntax is **literal** when regex is off: `$` is treated
//! literally and no capture expansion happens. With the dollar grammar,
//! `$0`, `$1`, `${name}` and `$$` are recognized; unknown groups, dangling
//! `$` and malformed braces are validation errors before any edit, and
//! optional unmatched groups substitute empty. Capture expansion is
//! evaluated per original match and never rescans inserted text.

use flavite_syntax::{Span, TextByte};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::matcher::{MATCH_SEMANTICS_VERSION, Match, Matcher, fnv1a, fnv1a_start};

/// Template size bound (NFR-8: replacement ≤ 64 KiB).
pub const TEMPLATE_MAX_BYTES: usize = 64 * 1024;

/// Which replacement grammar the template uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TemplateGrammar {
    /// `$` is literal text; expansion is the template verbatim (D-05 when
    /// regex is off).
    #[default]
    Literal,
    /// `$0`/`$1`/`${name}`/`$$` capture grammar (D-05 when regex is on).
    Dollar,
}

/// One parsed template token.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TemplateToken {
    /// Verbatim text (including a literal `$` from `$$`).
    Text(String),
    /// `$N` — a decimal capture-group reference.
    GroupIndex(u64),
    /// `${name}` — a named capture-group reference.
    GroupName(String),
}

/// A validated replacement template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplacementTemplate {
    raw: String,
    grammar: TemplateGrammar,
    tokens: Vec<TemplateToken>,
}

impl ReplacementTemplate {
    /// Validate and parse `raw` under `grammar`.
    ///
    /// # Errors
    ///
    /// [`TemplateError::TooLong`] over the byte bound; for the dollar
    /// grammar [`TemplateError::DanglingDollar`] and
    /// [`TemplateError::MalformedBraces`] at parse time (unknown groups are
    /// validated against the matcher at expansion, still before any edit).
    pub fn new(raw: &str, grammar: TemplateGrammar) -> Result<Self, TemplateError> {
        if raw.len() > TEMPLATE_MAX_BYTES {
            return Err(TemplateError::TooLong {
                observed: raw.len(),
                limit: TEMPLATE_MAX_BYTES,
            });
        }
        let tokens = match grammar {
            TemplateGrammar::Literal => vec![TemplateToken::Text(raw.to_owned())],
            TemplateGrammar::Dollar => parse_dollar(raw)?,
        };
        Ok(Self {
            raw: raw.to_owned(),
            grammar,
            tokens,
        })
    }

    /// A literal template (`$` stays literal).
    ///
    /// # Errors
    ///
    /// [`TemplateError::TooLong`].
    pub fn literal(raw: &str) -> Result<Self, TemplateError> {
        Self::new(raw, TemplateGrammar::Literal)
    }

    /// The raw template text.
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// The grammar in use.
    #[must_use]
    pub const fn grammar(&self) -> TemplateGrammar {
        self.grammar
    }

    /// Digest binding semantics version, grammar and raw text.
    #[must_use]
    pub fn digest(&self) -> u64 {
        let hash = fnv1a(fnv1a_start(), &MATCH_SEMANTICS_VERSION.to_le_bytes());
        let hash = fnv1a(hash, &[u8::from(self.grammar == TemplateGrammar::Dollar)]);
        fnv1a(hash, self.raw.as_bytes())
    }

    /// Expand this template for one original match.
    ///
    /// # Errors
    ///
    /// [`TemplateError::UnknownGroup`] and [`TemplateError::LengthOverflow`].
    pub fn expand(
        &self,
        matcher: &Matcher,
        text: &str,
        hit: &Match,
    ) -> Result<String, TemplateError> {
        let mut out = String::with_capacity(self.expansion_len(matcher, hit)?);
        for token in &self.tokens {
            match token {
                TemplateToken::Text(segment) => out.push_str(segment),
                TemplateToken::GroupIndex(index) => {
                    out.push_str(group_text_by_index(matcher, text, hit, *index)?);
                }
                TemplateToken::GroupName(name) => {
                    out.push_str(group_text_by_name(matcher, text, hit, name)?);
                }
            }
        }
        Ok(out)
    }

    /// Exact expansion length for one match, with checked arithmetic.
    ///
    /// # Errors
    ///
    /// [`TemplateError::UnknownGroup`] and [`TemplateError::LengthOverflow`].
    pub fn expansion_len(&self, matcher: &Matcher, hit: &Match) -> Result<usize, TemplateError> {
        let mut total = 0usize;
        for token in &self.tokens {
            // Unknown groups fail here exactly as in `expand`, before any
            // edit can be computed (D-05).
            let len = match token {
                TemplateToken::Text(segment) => segment.len(),
                TemplateToken::GroupIndex(index) => {
                    let span = require_group_by_index(matcher, hit, *index)?;
                    span.map_or(0, |value| value.len())
                }
                TemplateToken::GroupName(name) => {
                    let span = require_group_by_name(matcher, hit, name)?;
                    span.map_or(0, |value| value.len())
                }
            };
            total = total
                .checked_add(len)
                .ok_or(TemplateError::LengthOverflow)?;
        }
        Ok(total)
    }

    /// Exact total output length after replacing every match, checked
    /// against `budget` before any buffer reservation (§6.2).
    ///
    /// # Errors
    ///
    /// [`TemplateError::LengthOverflow`] on checked-arithmetic overflow and
    /// [`TemplateError::OutputTooLarge`] when the result exceeds `budget`.
    pub fn total_output_len(
        &self,
        original_len: usize,
        matcher: &Matcher,
        hits: &[Match],
        budget: usize,
    ) -> Result<usize, TemplateError> {
        let mut total = original_len;
        for found in hits {
            let expansion = self.expansion_len(matcher, found)?;
            let span_len = found.span.len();
            // Matches are non-overlapping and inside the text.
            total = total
                .checked_sub(span_len)
                .ok_or(TemplateError::LengthOverflow)?;
            total = total
                .checked_add(expansion)
                .ok_or(TemplateError::LengthOverflow)?;
        }
        if total > budget {
            return Err(TemplateError::OutputTooLarge {
                observed: total,
                limit: budget,
            });
        }
        Ok(total)
    }
}

impl Serialize for ReplacementTemplate {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (self.grammar, self.raw.as_str()).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ReplacementTemplate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (grammar, raw) = <(TemplateGrammar, String)>::deserialize(deserializer)?;
        Self::new(&raw, grammar).map_err(serde::de::Error::custom)
    }
}

/// Parse the dollar grammar: `$$` → literal `$`, `$N` → decimal group,
/// `${name}` → named group. Any `$` not forming one of these is a dangling
/// dollar; `${` without a well-formed name and closing brace is malformed
/// (D-05).
fn parse_dollar(raw: &str) -> Result<Vec<TemplateToken>, TemplateError> {
    let bytes = raw.as_bytes();
    let mut tokens = Vec::new();
    let mut text_start = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != b'$' {
            index += 1;
            continue;
        }
        let dollar_at = index;
        let Some(next) = bytes.get(index + 1).copied() else {
            return Err(TemplateError::DanglingDollar { offset: dollar_at });
        };
        match next {
            b'$' => {
                flush_text(&mut tokens, raw, text_start, dollar_at);
                tokens.push(TemplateToken::Text("$".to_owned()));
                index += 2;
                text_start = index;
            }
            b'{' => {
                let Some(relative_close) = raw[dollar_at + 2..].find('}') else {
                    return Err(TemplateError::MalformedBraces { offset: dollar_at });
                };
                let close = dollar_at + 2 + relative_close;
                let name = &raw[dollar_at + 2..close];
                if name.is_empty() || !name.chars().all(is_name_char) {
                    return Err(TemplateError::MalformedBraces { offset: dollar_at });
                }
                flush_text(&mut tokens, raw, text_start, dollar_at);
                tokens.push(TemplateToken::GroupName(name.to_owned()));
                index = close + 1;
                text_start = index;
            }
            digit if digit.is_ascii_digit() => {
                let start = index + 1;
                let mut end = start;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                // Greedy decimal read; digits that overflow u64 can never
                // name an existing group and fail at expansion.
                let digits = &raw[start..end];
                let parsed = digits.parse::<u64>().unwrap_or(u64::MAX);
                flush_text(&mut tokens, raw, text_start, dollar_at);
                tokens.push(TemplateToken::GroupIndex(parsed));
                index = end;
                text_start = index;
            }
            _ => return Err(TemplateError::DanglingDollar { offset: dollar_at }),
        }
    }
    flush_text(&mut tokens, raw, text_start, raw.len());
    Ok(tokens)
}

fn flush_text(tokens: &mut Vec<TemplateToken>, raw: &str, start: usize, end: usize) {
    if end > start {
        tokens.push(TemplateToken::Text(raw[start..end].to_owned()));
    }
}

const fn is_name_char(byte: char) -> bool {
    byte.is_ascii_alphanumeric() || byte == '_'
}

fn group_index_by_name(matcher: &Matcher, name: &str) -> Option<usize> {
    (0..matcher.capture_count()).find(|&index| matcher.capture_name(index) == Some(name))
}

/// Validate `$N` against the matcher, returning the group span (`None`
/// when the optional group did not participate).
fn require_group_by_index(
    matcher: &Matcher,
    hit: &Match,
    index: u64,
) -> Result<Option<Span<TextByte>>, TemplateError> {
    let index = usize::try_from(index).unwrap_or(usize::MAX);
    if index >= matcher.capture_count() || index >= hit.group_count() {
        return Err(TemplateError::UnknownGroup {
            group: index.to_string(),
        });
    }
    Ok(hit.group_span(index))
}

/// Validate `${name}` against the matcher.
fn require_group_by_name(
    matcher: &Matcher,
    hit: &Match,
    name: &str,
) -> Result<Option<Span<TextByte>>, TemplateError> {
    match group_index_by_name(matcher, name) {
        Some(index) if index < hit.group_count() => Ok(hit.group_span(index)),
        _ => Err(TemplateError::UnknownGroup {
            group: name.to_owned(),
        }),
    }
}

fn group_text_by_index<'t>(
    matcher: &Matcher,
    text: &'t str,
    hit: &Match,
    index: u64,
) -> Result<&'t str, TemplateError> {
    // Unmatched optional groups substitute empty (D-05).
    Ok(require_group_by_index(matcher, hit, index)?
        .and_then(|_| hit.group_text(text, usize::try_from(index).unwrap_or(usize::MAX)))
        .unwrap_or(""))
}

fn group_text_by_name<'t>(
    matcher: &Matcher,
    text: &'t str,
    hit: &Match,
    name: &str,
) -> Result<&'t str, TemplateError> {
    Ok(require_group_by_name(matcher, hit, name)?
        .and_then(|_| {
            let index = group_index_by_name(matcher, name).unwrap_or(usize::MAX);
            hit.group_text(text, index)
        })
        .unwrap_or(""))
}

/// Template validation and expansion errors (D-05).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateError {
    /// Template exceeded [`TEMPLATE_MAX_BYTES`].
    TooLong {
        /// Observed template size.
        observed: usize,
        /// The configured limit.
        limit: usize,
    },
    /// A `$` that does not form `$$`, `$N` or `${name}`.
    DanglingDollar {
        /// Offset of the unpaired `$`.
        offset: usize,
    },
    /// `${` without a well-formed name and closing brace.
    MalformedBraces {
        /// Offset of the malformed `${`.
        offset: usize,
    },
    /// A group index or name the matcher does not declare.
    UnknownGroup {
        /// The group index or name that does not exist.
        group: String,
    },
    /// Checked expansion arithmetic overflowed.
    LengthOverflow,
    /// The computed output length exceeded the caller's budget.
    OutputTooLarge {
        /// The computed total output length.
        observed: usize,
        /// The caller's budget.
        limit: usize,
    },
}

impl std::fmt::Display for TemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLong { observed, limit } => {
                write!(f, "template is {observed} bytes, limit is {limit}")
            }
            Self::DanglingDollar { offset } => {
                write!(f, "dangling '$' at template offset {offset}")
            }
            Self::MalformedBraces { offset } => {
                write!(f, "malformed braces at template offset {offset}")
            }
            Self::UnknownGroup { group } => write!(f, "unknown replacement group {group}"),
            Self::LengthOverflow => write!(f, "expansion length overflow"),
            Self::OutputTooLarge { observed, limit } => {
                write!(f, "output length {observed} exceeds budget {limit}")
            }
        }
    }
}

impl std::error::Error for TemplateError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::{MatchLimits, MatchMode, MatchOptions};

    fn matcher(pattern: &str) -> Matcher {
        Matcher::new(pattern, MatchOptions::default(), MatchLimits::default()).expect("compiles")
    }

    fn regex_matcher(pattern: &str) -> Matcher {
        Matcher::new(
            pattern,
            MatchOptions {
                mode: MatchMode::Regex,
                ..MatchOptions::default()
            },
            MatchLimits::default(),
        )
        .expect("compiles")
    }

    fn literal_template(raw: &str) -> ReplacementTemplate {
        ReplacementTemplate::literal(raw).expect("valid")
    }

    fn dollar_template(raw: &str) -> ReplacementTemplate {
        ReplacementTemplate::new(raw, TemplateGrammar::Dollar).expect("valid")
    }

    #[test]
    fn literal_grammar_treats_dollar_literally() {
        let template = literal_template("$0 $$ ${x} $1");
        let matcher = matcher("abc");
        let found = matcher.find("xxabc").unwrap();
        let expansion = template.expand(&matcher, "xxabc", &found[0]).unwrap();
        assert_eq!(expansion, "$0 $$ ${x} $1");
        assert_eq!(
            template.expansion_len(&matcher, &found[0]).unwrap(),
            expansion.len()
        );
    }

    #[test]
    fn dollar_grammar_expands_groups_per_original_match() {
        let template = dollar_template("[$1|$2|$0]");
        let matcher = regex_matcher("(a+)(b+)");
        let found = matcher.find("xaabbyabb").unwrap();
        assert_eq!(found.len(), 2);
        let first = template.expand(&matcher, "xaabbyabb", &found[0]).unwrap();
        let second = template.expand(&matcher, "xaabbyabb", &found[1]).unwrap();
        assert_eq!(first, "[aa|bb|aabb]");
        assert_eq!(second, "[a|bb|abb]");
    }

    #[test]
    fn dollar_grammar_validates_before_any_edit() {
        assert!(matches!(
            ReplacementTemplate::new("end $", TemplateGrammar::Dollar),
            Err(TemplateError::DanglingDollar { offset: 4 })
        ));
        assert!(matches!(
            ReplacementTemplate::new("a $x b", TemplateGrammar::Dollar),
            Err(TemplateError::DanglingDollar { offset: 2 })
        ));
        assert!(matches!(
            ReplacementTemplate::new("${open", TemplateGrammar::Dollar),
            Err(TemplateError::MalformedBraces { offset: 0 })
        ));
        assert!(matches!(
            ReplacementTemplate::new("${}", TemplateGrammar::Dollar),
            Err(TemplateError::MalformedBraces { offset: 0 })
        ));
        assert!(matches!(
            ReplacementTemplate::new("${bad-name}", TemplateGrammar::Dollar),
            Err(TemplateError::MalformedBraces { offset: 0 })
        ));
        // Unknown groups validate against the matcher at expansion.
        let matcher = regex_matcher("(a)");
        let found = matcher.find("a").unwrap();
        assert!(matches!(
            dollar_template("$4").expand(&matcher, "a", &found[0]),
            Err(TemplateError::UnknownGroup { .. })
        ));
        assert!(matches!(
            dollar_template("${nope}").expand(&matcher, "a", &found[0]),
            Err(TemplateError::UnknownGroup { .. })
        ));
    }

    #[test]
    fn dollar_escapes_and_greedy_indices() {
        let matcher = regex_matcher("(b)");
        let found = matcher.find("xbx").unwrap();
        // `$$` yields a literal `$`; `$1` expands the group.
        let simple = dollar_template("$$1$1")
            .expand(&matcher, "xbx", &found[0])
            .unwrap();
        assert_eq!(simple, "$1b");
        // Multi-digit indices read greedily: `$10` names group 10.
        let groups10 = "(a)".repeat(10);
        let matcher10 = regex_matcher(&groups10);
        let found10 = matcher10.find("aaaaaaaaaaa").unwrap();
        let tenth = dollar_template("$10")
            .expand(&matcher10, "aaaaaaaaaaa", &found10[0])
            .unwrap();
        assert_eq!(tenth, "a");
    }

    #[test]
    fn total_output_len_is_checked_against_the_budget() {
        let matcher = regex_matcher("(a)");
        let found = matcher.find("aaaa").unwrap();
        let template = dollar_template("XX");
        // 4 bytes − 4 matched + 4×2 expansion = 8
        assert_eq!(
            template.total_output_len(4, &matcher, &found, 8).unwrap(),
            8
        );
        assert_eq!(
            template
                .total_output_len(4, &matcher, &found, 7)
                .expect_err("over budget"),
            TemplateError::OutputTooLarge {
                observed: 8,
                limit: 7,
            }
        );
    }

    #[test]
    fn template_length_is_bounded() {
        let long = "$1".repeat(TEMPLATE_MAX_BYTES / 2 + 1);
        assert!(matches!(
            ReplacementTemplate::new(&long, TemplateGrammar::Dollar),
            Err(TemplateError::TooLong { .. })
        ));
    }

    #[test]
    fn digest_binds_grammar_and_raw() {
        assert_eq!(
            literal_template("x").digest(),
            literal_template("x").digest()
        );
        assert_ne!(
            literal_template("x").digest(),
            dollar_template("x").digest()
        );
        assert_ne!(
            literal_template("x").digest(),
            literal_template("y").digest()
        );
    }

    #[test]
    fn serde_round_trip_revalidates() {
        let template = dollar_template("[$1]");
        let serialized = serde_json::to_string(&template).unwrap();
        let restored: ReplacementTemplate = serde_json::from_str(&serialized).unwrap();
        assert_eq!(restored, template);
        // An invalid stored template is refused at deserialization.
        let broken = serde_json::to_string(&(TemplateGrammar::Dollar, "bad $")).unwrap();
        assert!(serde_json::from_str::<ReplacementTemplate>(&broken).is_err());
    }
}
