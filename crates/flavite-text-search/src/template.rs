//! Literal replacement-template expansion (RFC-0006 D-05, §6.2).
//!
//! D-05: "Replacement syntax is literal when regex is off." In literal mode
//! the template is inserted **verbatim** — `$0`, `$1`, `${name}` and `$$`
//! carry no meaning and are copied through unchanged, so a replacement like
//! `$$cost: $1` produces exactly those characters. Capture-aware expansion
//! (`$0`/`$1`/`${name}`/`$$` with validation of unknown groups and dangling
//! `$`) arrives with regex mode in S03; the [`expand_literal`] signature
//! already carries the capture/source inputs that mode will use.
//!
//! This module owns template *expansion* and the checked output-length
//! arithmetic only (§4.1): no edits, no revisions, no buffer types. Callers
//! must enforce their size limits (NFR-8: replacement ≤ 64 KiB, per-note
//! output ≤ 32 MiB) **before** building output, using
//! [`expanded_output_len`].

use std::fmt;

use crate::matcher::MatchSpan;

/// Upper bound for one replacement template in UTF-8 bytes (NFR-8).
///
/// Enforced by callers before expansion; kept here so every consumer of the
/// template domain quotes the same limit.
pub const MAX_REPLACEMENT_BYTES: usize = 64 * 1024;

/// A replacement template used in literal mode.
///
/// Wraps the replacement string verbatim; [`Self::as_str`] is exactly the
/// text that [`expand_literal`] inserts for each match. The empty template
/// is a valid deletion. NFR-8 caps templates at [`MAX_REPLACEMENT_BYTES`];
/// enforcement belongs to the callers that admit plans.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct LiteralTemplate(String);

impl LiteralTemplate {
    /// Wrap a replacement template.
    #[must_use]
    pub fn new(template: impl Into<String>) -> Self {
        Self(template.into())
    }

    /// The verbatim template text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Unwrap the template into its owned string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl From<String> for LiteralTemplate {
    fn from(template: String) -> Self {
        Self(template)
    }
}

impl From<&str> for LiteralTemplate {
    fn from(template: &str) -> Self {
        Self(template.to_owned())
    }
}

impl fmt::Display for LiteralTemplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Expand one matched occurrence's replacement in literal mode.
///
/// The template is returned **verbatim**: captures are ignored (`_capture`
/// and `_source` exist so the S03 regex mode can reuse this signature for
/// `$0`/`$1`/`${name}` expansion; see [`crate::matcher::MatchMode::replacement_is_literal`]).
/// `$` sequences — including `$$` — are literal characters (D-05).
#[must_use]
pub fn expand_literal(template: &str, _capture: &MatchSpan, _source: &str) -> String {
    template.to_owned()
}

/// Compute the exact replacement output length with checked arithmetic.
///
/// The output is `source_len - match_bytes` (text outside matches) plus
/// `template.len() × match_count` (one verbatim template per match). Returns
/// `None` when the computation would under- or overflow, or when
/// `match_bytes > source_len` (matches cannot exceed the source). Callers
/// must call this **before** materializing output and refuse the operation
/// when the result exceeds their budget (§6.2: "compute exact output length
/// with checked arithmetic; refuse if output/edits/memory budget exceeded").
#[must_use]
pub fn expanded_output_len(
    source_len: u64,
    template: &str,
    match_count: u64,
    match_bytes: u64,
) -> Option<u64> {
    let remaining = source_len.checked_sub(match_bytes)?;
    let template_len = u64::try_from(template.len()).ok()?;
    let replacement_total = template_len.checked_mul(match_count)?;
    remaining.checked_add(replacement_total)
}
