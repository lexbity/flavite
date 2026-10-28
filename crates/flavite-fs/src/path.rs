//! Path safety utilities — minimal implementation for now (§4.8, §4.16.2).

use std::path::{Component, Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

/// Check if a path contains any non-normal components or absolute paths.
pub fn has_non_normal_components(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(
            c,
            Component::RootDir | Component::CurDir | Component::ParentDir | Component::Prefix(_)
        )
    })
}

/// Lexical canonicalization (no symlink resolution).
pub fn lexical_canonicalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => out.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => out.push(name),
            Component::Prefix(_) => {}
        }
    }
    out
}

/// Validate a vault-relative path before it is used as an encoded key or
/// passed to the operating system. This rejects Windows path spellings on all
/// platforms so a vault moved between operating systems has the same safety
/// boundary.
pub fn validate_vault_relative(path: &Path) -> Result<String, String> {
    if path.is_absolute() || has_non_normal_components(path) {
        return Err("path must be relative and contain only normal components".into());
    }
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    let mut parts = Vec::new();
    for component in path.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        #[cfg(unix)]
        let part = std::str::from_utf8(part.as_bytes())
            .map_err(|_| "path contains a non-UTF-8 component")?;
        #[cfg(not(unix))]
        let part = part
            .to_str()
            .ok_or("path contains a non-Unicode component")?;
        if part.is_empty() || part.contains(':') || part.contains('\\') {
            return Err("path contains an ADS or platform path separator".into());
        }
        if part.ends_with('.') || part.ends_with(' ') {
            return Err("path component ends in a dot or space".into());
        }
        let base = part
            .split('.')
            .next()
            .unwrap_or(part)
            .trim_end_matches([' ', '.']);
        let upper = base.to_ascii_uppercase();
        if matches!(
            upper.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
        ) || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.as_bytes()[3].is_ascii_digit()
            && upper.as_bytes()[3] != b'0')
            || (upper.len() == 4
                && (upper.starts_with("COM") || upper.starts_with("LPT"))
                && matches!(upper.as_bytes()[3], 0xb9 | 0xb2 | 0xb3))
            || base
                .strip_prefix("COM")
                .is_some_and(|suffix| matches!(suffix, "¹" | "²" | "³"))
            || base
                .strip_prefix("LPT")
                .is_some_and(|suffix| matches!(suffix, "¹" | "²" | "³"))
        {
            return Err("path contains a Windows reserved device name".into());
        }
        // Windows short-name aliases are ambiguous and can address a different
        // file from the spelling stored in the vault.
        let stem = base.split('.').next().unwrap_or(base);
        if stem.contains('~')
            && stem
                .rsplit_once('~')
                .is_some_and(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err("path contains an 8.3 short-name form".into());
        }
        parts.push(part.nfc().collect::<String>());
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_portability_and_traversal_hazards() {
        for bad in [
            "../note.md",
            "C:/note.md",
            "note.md:stream",
            "\\\\?\\C:\\note.md",
            "notes~1.md",
            "CON",
            "nul.txt",
            "LPT9.log",
            "COM¹.txt",
            "name.",
            "name ",
        ] {
            assert!(validate_vault_relative(Path::new(bad)).is_err(), "{bad:?}");
        }
        for good in ["notes/a.md", "COM0.txt", "normal~draft.md"] {
            assert!(validate_vault_relative(Path::new(good)).is_ok(), "{good:?}");
        }
    }

    #[test]
    fn normalizes_unicode_to_nfc() {
        assert_eq!(
            validate_vault_relative(Path::new("cafe\u{301}/note.md")).unwrap(),
            "café/note.md"
        );
    }
}
