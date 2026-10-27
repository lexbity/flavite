#![forbid(unsafe_code)]

//! Tier 1 — durability-critical. Inline scanner (`[[links]]`, `![[embeds]]`,
//! `#tags`, `^block-id`), frontmatter byte-preserving patcher, pulldown-cmark
//! glue. Pure and IO-free (§4.1, D10, D11).

pub mod frontmatter;
pub mod mask;
pub mod pulldown_glue;
pub mod scanner;
pub mod text;
pub mod wellknown;

// Re-export key types for convenience.
pub use frontmatter::{Frontmatter, FrontmatterError, PropertyValue};
pub use mask::TextMask;
pub use pulldown_glue::{Link, ParsedNote};
pub use scanner::{LinkPayload, ScannedSpan, SpanKind};
pub use text::{Eol, LineInfo, SourceByte, Span, TextByte, TextError, TextSnapshot};
pub use wellknown::{normalize_tag, WellKnownKey};
