#![forbid(unsafe_code)]

//! Tier 1 — durability-critical. Inline scanner (`[[links]]`, `![[embeds]]`,
//! `#tags`, `^block-id`), frontmatter byte-preserving patcher, pulldown-cmark
//! glue. Pure and IO-free (§4.1, D10, D11).

pub mod frontmatter;
pub mod link_format;
pub mod mask;
pub mod pulldown_glue;
pub mod scanner;
pub mod text;
pub mod ts_ranges;
pub mod wellknown;

// Re-export key types for convenience.
pub use frontmatter::{Frontmatter, PropertyValue};
pub use mask::TextMask;
pub use pulldown_glue::{Link, ParsedNote};
pub use scanner::{LinkPayload, ScannedSpan, SpanKind};
pub use text::{Eol, LineInfo, SourceByte, Span, TextByte, TextError, TextSnapshot};
pub use wellknown::{WellKnownKey, normalize_tag};
