#![forbid(unsafe_code)]

//! Tier 1 — durability-critical. Inline scanner (`[[links]]`, `![[embeds]]`,
//! `#tags`, `^block-id`), frontmatter byte-preserving patcher, pulldown-cmark
//! glue. Pure and IO-free (§4.1, D10, D11).
