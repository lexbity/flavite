//! Well-known frontmatter key mapping (D11, R13).
//!
//! On parse, the keys `aliases`, `tags`/`tag`, and `title` are mapped into
//! typed tables. All other keys land in `properties`.
//!
//! On write, the only conventional key we ever synthesize is `aliases`
//! (the D8 alias strategy); everything else patches user-authored text.

/// Well-known frontmatter keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WellKnownKey {
    /// `aliases` — string or list of strings.
    Aliases,
    /// `tags` — string or list of strings.
    Tags,
    /// `tag` — string or list of strings (singular form, also accepted).
    Tag,
    /// `title` — string, overrides derived title.
    Title,
}

impl WellKnownKey {
    /// Parse a string into a well-known key.
    #[must_use]
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "aliases" => Some(Self::Aliases),
            "tags" => Some(Self::Tags),
            "tag" => Some(Self::Tag),
            "title" => Some(Self::Title),
            _ => None,
        }
    }

    /// All well-known key names.
    #[must_use]
    pub fn all() -> &'static [&'static str] {
        &["aliases", "tags", "tag", "title"]
    }
}

/// Normalize a tag string for storage and lookup.
#[must_use]
pub fn normalize_tag(tag: &str) -> String {
    crate::text::fold_key(tag)
}

/// Derive the title from available sources.
#[must_use]
pub fn derive_title(frontmatter_title: Option<&str>, first_h1: Option<&str>, stem: &str) -> String {
    frontmatter_title
        .map(String::from)
        .or_else(|| first_h1.map(String::from))
        .unwrap_or_else(|| stem.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_key_from_str() {
        assert_eq!(
            WellKnownKey::from_str("aliases"),
            Some(WellKnownKey::Aliases)
        );
        assert_eq!(WellKnownKey::from_str("tags"), Some(WellKnownKey::Tags));
        assert_eq!(WellKnownKey::from_str("tag"), Some(WellKnownKey::Tag));
        assert_eq!(WellKnownKey::from_str("title"), Some(WellKnownKey::Title));
        assert_eq!(WellKnownKey::from_str("unknown"), None);
    }

    #[test]
    fn test_normalize_tag() {
        assert_eq!(normalize_tag("Hello"), "hello");
        assert_eq!(normalize_tag("hello"), "hello");
        assert_eq!(normalize_tag("Café"), "café");
    }

    #[test]
    fn derive_title_precedence() {
        assert_eq!(
            derive_title(Some("Frontmatter"), Some("H1"), "stem"),
            "Frontmatter"
        );
        assert_eq!(derive_title(None, Some("H1"), "stem"), "H1");
        assert_eq!(derive_title(None, None, "stem"), "stem");
    }
}
