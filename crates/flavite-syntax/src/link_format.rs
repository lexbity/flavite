//! Safe link serialization and prospective insertion proof (RFC-0003 §9,
//! W05).
//!
//! This is the single formatting authority for wikilinks. The scanner
//! (`crate::scanner`) remains the single parsing authority: serialization is
//! only valid when the scanner reads back exactly the intended occurrence,
//! and an insertion is only applied when a full parse of the prospective
//! document proves that exactly one new intended link occurrence is created
//! and no neighboring link interpretation is altered.

use crate::pulldown_glue::{Link, parse_note};
use crate::scanner::{LinkPayload, SpanKind};
use crate::text::TextSnapshot;

/// Longest serialized link text the formatter admits (bounded, NFR-12).
pub const MAX_LINK_LENGTH: usize = 1024;

/// A serialized wikilink with the payload the scanner must read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerializedLink {
    /// The exact `[[...]]` text.
    pub text: String,
    /// The scanner payload this text must produce.
    pub payload: LinkPayload,
}

/// Why a target cannot be serialized under the existing grammar (D-06: no
/// invented semantics — the app never mints IDs, renames, or percent-encodes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkFormatError {
    /// No path components.
    EmptyTarget,
    /// A component violates the existing grammar.
    InvalidComponent {
        /// The offending component.
        component: String,
        /// Human-readable reason.
        reason: &'static str,
    },
    /// The serialized form would exceed the bounded length.
    TooLong {
        /// Actual serialized length.
        length: usize,
    },
}

impl std::fmt::Display for LinkFormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyTarget => write!(f, "link target needs at least one path component"),
            Self::InvalidComponent { component, reason } => {
                write!(
                    f,
                    "link target component {component:?} is not representable: {reason}"
                )
            }
            Self::TooLong { length } => {
                write!(
                    f,
                    "serialized link is {length} bytes, over the {MAX_LINK_LENGTH} byte bound"
                )
            }
        }
    }
}

impl std::error::Error for LinkFormatError {}

/// Serialize one wikilink: a vault-relative extensionless path in `[[...]]`
/// (D-06 default). An alias is included only when the caller explicitly
/// provides one — the semantic insertion flow never auto-aliases (the RFC's
/// canonical prepare result serializes `[[Systems/Emergence]]` with a
/// separate `target_display`), while the editor autocomplete path controls
/// its own aliasing through [`format_candidate_link`]. The `.md` suffix is
/// stripped from the final component.
///
/// # Errors
/// Returns [`LinkFormatError`] when any component (or the alias) is not
/// representable by the existing grammar.
pub fn serialize_link(
    components: &[String],
    alias: Option<&str>,
) -> Result<SerializedLink, LinkFormatError> {
    if components.is_empty() {
        return Err(LinkFormatError::EmptyTarget);
    }
    let mut serialized_target = String::new();
    for (index, component) in components.iter().enumerate() {
        check_component(component)?;
        let mut component = component.as_str();
        if index + 1 == components.len()
            && let Some(stem) = component.strip_suffix(".md")
        {
            component = stem;
        }
        if component.is_empty() {
            return Err(LinkFormatError::InvalidComponent {
                component: component.to_owned(),
                reason: "stripping the extension left an empty final component",
            });
        }
        if !serialized_target.is_empty() {
            serialized_target.push('/');
        }
        serialized_target.push_str(component);
    }
    let alias = match alias {
        Some(alias) if !alias.is_empty() => {
            check_component(alias)?;
            Some(alias.to_owned())
        }
        _ => None,
    };
    let text = match &alias {
        Some(alias) => format!("[[{serialized_target}|{alias}]]"),
        None => format!("[[{serialized_target}]]"),
    };
    if text.len() > MAX_LINK_LENGTH {
        return Err(LinkFormatError::TooLong { length: text.len() });
    }
    // Scanner round-trip: the text is only valid when the single parsing
    // authority reads back exactly the intended occurrence.
    let snapshot = TextSnapshot::new(text.as_bytes())
        .map_err(|_| LinkFormatError::TooLong { length: text.len() })?;
    let spans = crate::scanner::scan_inline(&snapshot, 0..snapshot.text().len());
    let payload = LinkPayload {
        target_raw: serialized_target.clone(),
        alias,
        anchor: None,
        by_id: false,
    };
    let matches = spans
        .iter()
        .filter(|span| span.kind == SpanKind::Link && span.payload.as_ref() == Some(&payload))
        .count();
    if matches != 1 {
        return Err(LinkFormatError::InvalidComponent {
            component: serialized_target.clone(),
            reason: "the scanner does not read back exactly one intended link",
        });
    }
    Ok(SerializedLink { text, payload })
}

/// Serialize the stable-ID form `[[id:ID|display]]` (D-06: only when the
/// user enabled stable-link insertion and the ID is unconflicted).
///
/// # Errors
/// Returns [`LinkFormatError`] when the ID or alias is not representable.
pub fn serialize_stable_link(id: &str, display: &str) -> Result<SerializedLink, LinkFormatError> {
    if id.is_empty() {
        return Err(LinkFormatError::InvalidComponent {
            component: id.to_owned(),
            reason: "stable ID is empty",
        });
    }
    let target_raw = format!("id:{id}");
    let alias = (!display.is_empty()).then(|| display.to_owned());
    let text = match &alias {
        Some(alias) => format!("[[{target_raw}|{alias}]]"),
        None => format!("[[{target_raw}]]"),
    };
    if text.len() > MAX_LINK_LENGTH {
        return Err(LinkFormatError::TooLong { length: text.len() });
    }
    let snapshot = TextSnapshot::new(text.as_bytes())
        .map_err(|_| LinkFormatError::TooLong { length: text.len() })?;
    let spans = crate::scanner::scan_inline(&snapshot, 0..snapshot.text().len());
    let payload = LinkPayload {
        target_raw,
        alias,
        anchor: None,
        by_id: true,
    };
    let matches = spans
        .iter()
        .filter(|span| span.kind == SpanKind::Link && span.payload.as_ref() == Some(&payload))
        .count();
    if matches != 1 {
        return Err(LinkFormatError::InvalidComponent {
            component: payload.target_raw.clone(),
            reason: "the scanner does not read back exactly one intended stable link",
        });
    }
    Ok(SerializedLink { text, payload })
}

/// Port of the editor autocomplete formatter (behavior-preserving): the
/// stable-ID form only when stable links are enabled and the candidate has
/// an unconflicted ID, an alias when the display title differs from the
/// target path, otherwise the bare target.
#[must_use]
pub fn format_candidate_link(
    target: &str,
    title: &str,
    stable_id: Option<&str>,
    stable_links: bool,
) -> String {
    if stable_links && let Some(id) = stable_id {
        return format!("[[id:{id}|{title}]]");
    }
    if target == title {
        format!("[[{target}]]")
    } else {
        format!("[[{target}|{title}]]")
    }
}

fn check_component(component: &str) -> Result<(), LinkFormatError> {
    if component.is_empty() {
        return Err(LinkFormatError::InvalidComponent {
            component: component.to_owned(),
            reason: "empty",
        });
    }
    if component == "." || component == ".." {
        return Err(LinkFormatError::InvalidComponent {
            component: component.to_owned(),
            reason: "path traversal is not representable",
        });
    }
    for character in component.chars() {
        if matches!(
            character,
            '\0' | '\n' | '\r' | '[' | ']' | '|' | '#' | '^' | '!'
        ) {
            return Err(LinkFormatError::InvalidComponent {
                component: component.to_owned(),
                reason: "contains a character reserved by the link grammar",
            });
        }
    }
    Ok(())
}

/// The exact one-op buffer edit a proven insertion authorizes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertionProof {
    /// Start of the replaced range in normalized text coordinates.
    pub start: usize,
    /// End of the replaced range in normalized text coordinates.
    pub end: usize,
    /// The serialized link text to insert.
    pub replacement: String,
    /// The target the new occurrence must carry after the edit.
    pub target_raw: String,
}

/// Why a prospective insertion cannot be proven (D-06/FR-09: every case is
/// a zero-mutation refusal with a repair action).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProofRejection {
    /// The caret is not a UTF-8 boundary or is past the text.
    CaretOutOfBounds,
    /// The insertion point cannot host a link: masked range (code, math,
    /// raw HTML, frontmatter), inside an existing link, an immediately
    /// preceding `!`, bracket adjacency that changes interpretation, or a
    /// parse that does not produce exactly one new intended occurrence.
    CaretNotLinkable,
    /// The source document itself does not parse; no interference claim is
    /// possible, so nothing is inserted.
    SourceUnparsable(String),
}

impl ProofRejection {
    /// The user-facing repair action for this rejection (FR-09).
    #[must_use]
    pub fn repair_action(&self) -> &'static str {
        match self {
            Self::CaretOutOfBounds => "place the caret inside the note text",
            Self::CaretNotLinkable => {
                "move the caret to a plain-text position outside code, math, and existing links"
            }
            Self::SourceUnparsable(_) => "resolve the document parse error before inserting links",
        }
    }
}

/// Prove that inserting `serialized.text` at the collapsed caret `caret`
/// creates exactly one new intended link occurrence and alters no other
/// link interpretation. Returns the authorized buffer edit.
///
/// The proof parses the original and the prospective document with the
/// shared CommonMark mask + scanner, so code fences, inline code, math,
/// raw HTML, frontmatter, embeds (`!`), and bracket adjacency are all
/// refused mechanically rather than by pattern matching.
///
/// # Errors
/// Returns [`ProofRejection`] without authorizing any edit.
pub fn prove_insertion(
    source: &str,
    caret: usize,
    serialized: &SerializedLink,
) -> Result<InsertionProof, ProofRejection> {
    if caret > source.len() || !source.is_char_boundary(caret) {
        return Err(ProofRejection::CaretOutOfBounds);
    }
    let original = parse_note(source.as_bytes()).map_err(ProofRejection::SourceUnparsable)?;
    let mut prospective = String::with_capacity(source.len() + serialized.text.len());
    prospective.push_str(&source[..caret]);
    prospective.push_str(&serialized.text);
    prospective.push_str(&source[caret..]);
    let parsed =
        parse_note(prospective.as_bytes()).map_err(|_| ProofRejection::CaretNotLinkable)?;
    let delta = serialized.text.len();

    // Every original occurrence must survive unchanged: identical payload at
    // its own span (before the caret) or its shifted span (after the caret).
    // An original span straddling the caret would change interpretation.
    for link in &original.links {
        let (start, end) = (link.span.start.0, link.span.end.0);
        let (mapped_start, mapped_end) = if end <= caret {
            (start, end)
        } else if start >= caret {
            (start + delta, end + delta)
        } else {
            // Insertion inside an existing occurrence: its interpretation
            // would change, which the contract refuses outright.
            return Err(ProofRejection::CaretNotLinkable);
        };
        let surviving = parsed.links.iter().any(|candidate| {
            candidate.span.start.0 == mapped_start
                && candidate.span.end.0 == mapped_end
                && same_occurrence(candidate, link)
        });
        if !surviving {
            return Err(ProofRejection::CaretNotLinkable);
        }
    }

    // Exactly one new occurrence, spanning exactly the inserted bytes, with
    // the intended payload (not an embed, no accidental anchor).
    let expected_range = (caret, caret + delta);
    let new_occurrences: Vec<&Link> = parsed
        .links
        .iter()
        .filter(|candidate| {
            !original.links.iter().any(|link| {
                let shifted_start = if link.span.start.0 >= caret {
                    link.span.start.0 + delta
                } else {
                    link.span.start.0
                };
                candidate.span.start.0 == shifted_start && same_occurrence(candidate, link)
            })
        })
        .collect();
    if new_occurrences.len() != 1 {
        return Err(ProofRejection::CaretNotLinkable);
    }
    let new_link = new_occurrences[0];
    if (new_link.span.start.0, new_link.span.end.0) != expected_range
        || new_link.is_embed
        || new_link.target_raw != serialized.payload.target_raw
        || new_link.alias != serialized.payload.alias
        || new_link.anchor.is_some()
        || new_link.by_id
    {
        return Err(ProofRejection::CaretNotLinkable);
    }
    Ok(InsertionProof {
        start: caret,
        end: caret,
        replacement: serialized.text.clone(),
        target_raw: serialized.payload.target_raw.clone(),
    })
}

fn same_occurrence(left: &Link, right: &Link) -> bool {
    left.target_raw == right.target_raw
        && left.alias == right.alias
        && left.anchor == right.anchor
        && left.is_embed == right.is_embed
        && left.by_id == right.by_id
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(components: &[&str]) -> Vec<String> {
        components.iter().map(|c| (*c).to_owned()).collect()
    }

    #[test]
    fn serializes_paths_and_strips_extension() {
        let link = serialize_link(&target(&["Systems", "Emergence.md"]), None).expect("serializes");
        assert_eq!(link.text, "[[Systems/Emergence]]");
        assert_eq!(link.payload.target_raw, "Systems/Emergence");
        assert!(link.payload.alias.is_none());
    }

    #[test]
    fn alias_serializes_only_when_explicitly_provided() {
        // The semantic flow serializes the bare path; the display travels
        // separately (RFC prepare example).
        let link = serialize_link(&target(&["Systems", "Emergence"]), None).unwrap();
        assert_eq!(link.text, "[[Systems/Emergence]]");
        // An explicit alias serializes and round-trips.
        let link = serialize_link(&target(&["index"]), Some("Home")).unwrap();
        assert_eq!(link.text, "[[index|Home]]");
        assert_eq!(link.payload.alias.as_deref(), Some("Home"));
        assert!(serialize_link(&target(&["index"]), Some("a|b")).is_err());
        // An empty alias is treated as no alias rather than an error.
        let empty = serialize_link(&target(&["index"]), Some("")).expect("empty alias is none");
        assert_eq!(empty.text, "[[index]]");
    }

    #[test]
    fn unrepresentable_targets_are_refused_without_invention() {
        assert!(serialize_link(&target(&["a", "..", "b"]), None).is_err());
        assert!(serialize_link(&target(&["a]b"]), None).is_err());
        assert!(serialize_link(&target(&["a|b"]), None).is_err());
        assert!(serialize_link(&target(&["a#b"]), None).is_err());
        assert!(serialize_link(&target(&["a!"]), None).is_err());
        assert!(serialize_link(&target(&["a\0b"]), None).is_err());
        assert!(serialize_link(&[], None).is_err());
        let long = "x".repeat(MAX_LINK_LENGTH);
        assert!(serialize_link(&target(&[&long]), None).is_err());
    }

    #[test]
    fn plain_insertion_is_proven_and_authorized() {
        let serialized = serialize_link(&target(&["beta"]), None).unwrap();
        let proof = prove_insertion("alpha body\n", 10, &serialized).expect("proven");
        assert_eq!(
            proof.replacement, "[[beta]]",
            "no alias without an explicit display"
        );
        let edited = format!("alpha body\n{}", proof.replacement);
        assert_eq!(edited, "alpha body\n[[beta]]");
    }

    #[test]
    fn masked_ranges_embeds_and_straddles_are_refused() {
        let serialized = serialize_link(&target(&["beta"]), None).unwrap();
        // Inside inline code.
        assert_eq!(
            prove_insertion("see `code` now", 7, &serialized),
            Err(ProofRejection::CaretNotLinkable)
        );
        // Inside a code fence.
        assert_eq!(
            prove_insertion("```\nplain\n```\n", 7, &serialized),
            Err(ProofRejection::CaretNotLinkable)
        );
        // Inside an existing link.
        assert_eq!(
            prove_insertion("see [[other]] here", 9, &serialized),
            Err(ProofRejection::CaretNotLinkable)
        );
        // Directly after a `!` (would become an embed).
        assert_eq!(
            prove_insertion("bang !", 6, &serialized),
            Err(ProofRejection::CaretNotLinkable)
        );
        // Bracket adjacency changes interpretation.
        assert_eq!(
            prove_insertion("xx[[", 4, &serialized),
            Err(ProofRejection::CaretNotLinkable)
        );
        // Out of bounds / split char.
        assert_eq!(
            prove_insertion("héllo", 2, &serialized),
            Err(ProofRejection::CaretOutOfBounds)
        );
    }

    #[test]
    fn adjacent_occurrences_are_allowed_and_survivors_are_verified() {
        let serialized = serialize_link(&target(&["b"]), None).unwrap();
        // Directly after an existing link: both occurrences coexist.
        let proof = prove_insertion("[[a]] here", 5, &serialized).expect("proven");
        let edited = format!("[[a]]{} here", proof.replacement);
        let parsed = parse_note(edited.as_bytes()).unwrap();
        assert_eq!(parsed.links.len(), 2);
        // Directly before an existing link.
        let proof = prove_insertion("here [[a]]", 5, &serialized).expect("proven");
        let edited = format!("here {}[[a]]", proof.replacement);
        let parsed = parse_note(edited.as_bytes()).unwrap();
        assert_eq!(parsed.links.len(), 2);
    }
}
