#![forbid(unsafe_code)]

//! Tier 1 — source matching (RFC-0006 §4.1, D-05, §6.2).
//!
//! This crate owns **exactly** the source-matching domain from the §4.1
//! extraction contract: literal (and, from S03, regex) source matches, match
//! options, capture data and replacement-template expansion, plus the checked
//! output-length arithmetic used before any replacement is built. It owns no
//! editor or buffer authority: no revisions, no Undo, no position maps, no
//! mutation grants, no caret/selection model and no UI. Callers convert match
//! spans into edits through the existing buffer/API boundaries
//! (`flavite-vault`/`flavite-api` adapt them to `EditOp`; this crate never
//! does).
//!
//! The matcher **borrows** already-normalized UTF-8 (`&str`) from an existing
//! [`flavite_syntax::text::TextSnapshot`] (typically
//! [`flavite_syntax::text::TextSnapshot::from_normalized`] or
//! `.text()`). It never normalizes input again, never copies the text into a
//! second container and never allocates an owned snapshot of its own.
//! Matches are returned as [`flavite_syntax::Span`] ranges of
//! [`flavite_syntax::text::TextByte`] — the canonical normalized-text
//! coordinates — so callers map them back to source bytes with the same
//! snapshot they borrowed (`TextSnapshot::span_to_source`). There is no
//! second offset model in this crate.
//!
//! S01 scope is **literal only** ([`matcher::MatchMode::Literal`]): case
//! folding approximates Rust-regex Unicode *simple* folding, whole-word
//! filtering uses UAX #29 word segments (pinned `unicode-segmentation`), and
//! replacement templates are taken verbatim (`$` is literal, D-05). The
//! regex mode arrives with the S03 helper process; nothing in this crate
//! registers regex capability today.
//!
//! Per the S01 source gates, this crate spells `flavite_syntax` paths out
//! fully instead of `use flavite_*` imports, and its dependency list is
//! limited to `flavite-syntax` plus the pinned segmentation crate.

pub mod matcher;
pub mod template;
