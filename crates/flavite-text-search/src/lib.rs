#![forbid(unsafe_code)]
//! Pure source matching for note and vault Find/Replace (RFC-0006 §4.1,
//! §6.2, D-05).
//!
//! Ownership boundary: this crate owns [`matcher::MatchOptions`],
//! [`template::ReplacementTemplate`], match pages, capture metadata and
//! replacement-template expansion **only**. It does not own edits, cursors,
//! selection models, position maps, Markdown commands, revisions or any
//! mutation authority, and it must never gain a dependency on editor,
//! buffer, API, vault, index, Fs, workspace or iced (§4 architecture
//! configuration is authoritative).
//!
//! The matcher borrows immutable normalized UTF-8 (an existing
//! `TextSnapshot`'s text); it never creates a competing text container.
//! Runtime matches use `flavite-syntax::Span<TextByte>`, and capture values
//! are slices of the same source lifetime.
//!
//! Semantics pinned by D-05:
//!
//! * Literal/regex operate on serialized Markdown source; a literal empty
//!   query is Idle (no matches), while zero-length regex matches are
//!   allowed at UTF-8 scalar boundaries and emitted once per location in
//!   ascending order.
//! * Case-insensitive literal matching is Rust regex Unicode **simple**
//!   folding — no accent stripping and no canonical normalization (`ß`
//!   does not equal `ss`; composed/decomposed accents remain distinct).
//!   Regex mode is the Rust `regex` language with Unicode on, multiline
//!   anchors on and dot-newline off unless inline flags request it; inline
//!   flags override the case option.
//! * Whole word requires both match endpoints to be UAX #29 word-segment
//!   boundaries (shared primitives in `flavite-syntax::text`).
//! * Replacement syntax is literal when regex is off; with the dollar
//!   grammar, `$0`/`$1`/`${name}`/`$$` are recognized and unknown groups,
//!   dangling `$` and malformed braces are validation errors before any
//!   edit.

pub mod matcher;
pub mod template;

pub use matcher::{
    DEFAULT_MAX_MATCHES, MATCH_SEMANTICS_VERSION, Match, MatchError, MatchLimits, MatchMode,
    MatchOptions, MatchPage, Matcher, PATTERN_MAX_BYTES, REGEX_MAX_GROUPS, REGEX_SIZE_LIMIT_BYTES,
    select_ordinals,
};
pub use template::{ReplacementTemplate, TEMPLATE_MAX_BYTES, TemplateError, TemplateGrammar};
