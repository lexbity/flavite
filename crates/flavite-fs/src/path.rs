//! Path safety utilities — minimal implementation for now (§4.8, §4.16.2).

use std::path::{Component, Path, PathBuf};

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
