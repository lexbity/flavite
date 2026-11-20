//! Glob matching for `vault.ignore_globs` (RFC-0007 D-12).
//!
//! Patterns are matched against vault-relative, slash-separated paths with
//! a deliberately small, documented grammar:
//!
//! - A pattern without `/` matches **any single path component** (the
//!   gitignore basename rule): `*.tmp` matches `a.tmp` and `x/y.tmp`, and
//!   `.DS_Store` matches `.DS_Store` at any depth.
//! - `*` inside a component matches any run of characters except `/`.
//! - `**` is a component that matches zero or more components.
//! - Every other character matches literally.
//!
//! The matcher is pure and total: a malformed or empty pattern simply
//! never matches. The settings layer bounds patterns (≤128, each ≤256
//! bytes) before they reach the scan pipeline.

use std::path::{Component, Path};

/// Whether `relative` matches any of the given glob patterns.
///
/// `relative` must be a vault-relative path (possibly `"."` for the
/// root). The hardcoded `.flavite` plumbing exclusion is deliberately NOT
/// part of this matcher: it is applied after user globs by the scan
/// pipeline and cannot be overridden.
#[must_use]
pub fn matches_any(patterns: &[String], relative: &Path) -> bool {
    patterns.iter().any(|pattern| matches(pattern, relative))
}

/// Whether a single pattern matches a vault-relative path.
#[must_use]
pub fn matches(pattern: &str, relative: &Path) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let components: Vec<&std::ffi::OsStr> = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            // The root, parent, and prefix components never appear in a
            // vault-relative path produced by `strip_prefix`.
            _ => None,
        })
        .collect();
    let normalized = pattern.trim_matches('/');
    if normalized.is_empty() {
        return false;
    }
    if !normalized.contains('/') {
        // Basename rule: a single-component pattern matches any single
        // component of the path.
        return components.iter().any(|component| {
            component_matches(normalized, component)
        });
    }
    let pattern_components: Vec<&str> = normalized.split('/').filter(|part| !part.is_empty()).collect();
    if pattern_components.is_empty() {
        return false;
    }
    path_matches(&pattern_components, &components)
}

/// Full path match with `**` support (only for patterns containing `/`).
fn path_matches(pattern: &[&str], path: &[&std::ffi::OsStr]) -> bool {
    match (pattern.first(), path.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(&"**"), _) => {
            // `**` consumes zero or more components; try every split.
            for consumed in 0..=path.len() {
                if path_matches(&pattern[1..], &path[consumed..]) {
                    return true;
                }
            }
            false
        }
        (Some(segment), Some(component)) => {
            component_matches(segment, component) && path_matches(&pattern[1..], &path[1..])
        }
        (Some(_), None) => false,
    }
}

/// Component-level match with `*` wildcards (never crosses `/`).
fn component_matches(pattern: &str, component: &std::ffi::OsStr) -> bool {
    let text = match component.to_str() {
        Some(text) => text,
        None => return pattern.is_empty(),
    };
    wildcard_match(pattern.as_bytes(), text.as_bytes())
}

/// Byte-level wildcard match: `*` matches any run of bytes.
fn wildcard_match(pattern: &[u8], text: &[u8]) -> bool {
    match (pattern.first(), text.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(b'*'), _) => {
            for index in 0..=text.len() {
                if wildcard_match(&pattern[1..], &text[index..]) {
                    return true;
                }
            }
            false
        }
        (Some(p), Some(t)) => p == t && wildcard_match(&pattern[1..], &text[1..]),
        (Some(_), None) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn m(pattern: &str, relative: &str) -> bool {
        matches(pattern, Path::new(relative))
    }

    #[test]
    fn basename_patterns_match_at_any_depth() {
        assert!(m("*.tmp", "a.tmp"));
        assert!(m("*.tmp", "x/y/z.tmp"));
        assert!(!m("*.tmp", "a.tmpx"));
        assert!(!m("*.tmp", "dir/a.md"));
        assert!(m(".DS_Store", ".DS_Store"));
        assert!(m(".DS_Store", "a/b/.DS_Store"));
        assert!(!m(".DS_Store", "a/b/.DS_Storex"));
    }

    #[test]
    fn literal_exact_patterns() {
        assert!(m("scratch", "scratch"));
        assert!(m("scratch", "a/scratch"));
        // A basename pattern matches the component anywhere, so it covers a
        // directory of that name (pruning the whole subtree).
        assert!(m("scratch", "scratch/b.md"));
        assert!(!m("scratch", "scratchy"));
        assert!(!m("scratch", "other/scratchy/b.md"));
    }

    #[test]
    fn double_star_crosses_directories() {
        assert!(m("scratch/**", "scratch"));
        assert!(m("scratch/**", "scratch/b.md"));
        assert!(m("scratch/**", "scratch/sub/deep.md"));
        assert!(!m("scratch/**", "scratchy/b.md"));
        assert!(!m("scratch/**", "other/scratch/b.md"));
        assert!(m("**/notes/*.md", "a/notes/x.md"));
        assert!(m("**/notes/*.md", "notes/x.md"));
        assert!(!m("**/notes/*.md", "a/notes/x.txt"));
        assert!(m("a/**/c", "a/c"));
        assert!(m("a/**/c", "a/b/c"));
        assert!(m("a/**/c", "a/b/d/c"));
        assert!(!m("a/**/c", "a/b/d"));
    }

    #[test]
    fn star_within_component() {
        assert!(m("*.md", "note.md"));
        assert!(m("*.md", "x/note.md"));
        assert!(!m("*.md", "x/note.txt"));
        assert!(m("note-*.md", "note-0001.md"));
        assert!(!m("note-*.md", "notes-0001.md"));
        assert!(m("a/*/c", "a/b/c"));
        assert!(!m("a/*/c", "a/b/d/c"));
    }

    #[test]
    fn flavite_rule_never_matches_through_user_globs_alone() {
        // A `.flavite*` user glob matches `.flavite` itself — the scan
        // pipeline's hardcoded exclusion applies after and still wins, so
        // matching here is fine; exclusion is enforced by the caller.
        assert!(m(".flavite*", ".flavite"));
        assert!(m(".flavite*", "a/.flavite-something"));
        assert!(!m(".flavite*", "a/other"));
    }

    #[test]
    fn empty_and_malformed_patterns_never_match() {
        assert!(!m("", "anything"));
        assert!(!m("/", "anything"));
        assert!(!m("//", "a/b"));
        assert!(!m("///", "a/b/c"));
    }

    #[test]
    fn root_and_trailing_slashes_are_tolerated() {
        assert!(m("scratch/**", "scratch/"));
        assert!(m("scratch//**", "scratch/b.md"));
        assert!(m("scratch/**/", "scratch/b.md"));
        assert!(m("a//b", "a/b"));
    }

    #[test]
    fn non_utf8_components_never_match_nonempty_patterns() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let binding = OsString::from_vec(vec![0xff, 0xfe]);
        let raw = Path::new(&binding);
        assert!(!matches("*.tmp", raw));
        assert!(!matches("*.tmp", &Path::new("a").join(raw)));
        assert!(!matches_any(&[], raw));
    }

    #[test]
    fn matches_any_requires_any_pattern() {
        assert!(!matches_any(&[], Path::new("a.tmp")));
        assert!(matches_any(&["*.tmp".to_owned()], Path::new("a.tmp")));
        assert!(matches_any(
            &["*.md".to_owned(), "*.tmp".to_owned()],
            Path::new("a.tmp")
        ));
    }
}
