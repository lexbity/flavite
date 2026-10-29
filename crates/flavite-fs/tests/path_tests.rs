//! Tests for path safety utilities

use flavite_fs::path::{has_non_normal_components, lexical_canonicalize, validate_vault_relative};
use std::path::Path;

#[test]
fn has_non_normal_components_root_dir() {
    assert!(has_non_normal_components(Path::new("/")));
    assert!(has_non_normal_components(Path::new("/root")));
}

#[test]
fn has_non_normal_components_current_dir() {
    assert!(has_non_normal_components(Path::new(".")));
    assert!(has_non_normal_components(Path::new("./file.txt")));
}

#[test]
fn has_non_normal_components_parent_dir() {
    assert!(has_non_normal_components(Path::new("..")));
    assert!(has_non_normal_components(Path::new("../file.txt")));
    assert!(has_non_normal_components(Path::new("../../dir/file.txt")));
}

#[test]
fn has_non_normal_components_normal() {
    assert!(!has_non_normal_components(Path::new("file.txt")));
    assert!(!has_non_normal_components(Path::new("dir/file.txt")));
    assert!(!has_non_normal_components(Path::new("a/b/c/file.txt")));
}

#[test]
fn lexical_canonicalize_simple() {
    assert_eq!(lexical_canonicalize(Path::new("a/b/c")), Path::new("a/b/c"));
}

#[test]
fn lexical_canonicalize_with_current_dir() {
    assert_eq!(lexical_canonicalize(Path::new("a/./b")), Path::new("a/b"));
    assert_eq!(
        lexical_canonicalize(Path::new("./a/./b/.")),
        Path::new("a/b")
    );
}

#[test]
fn lexical_canonicalize_with_parent_dir() {
    assert_eq!(
        lexical_canonicalize(Path::new("a/b/../c")),
        Path::new("a/c")
    );
    assert_eq!(lexical_canonicalize(Path::new("a/../b")), Path::new("b"));
}

#[test]
fn lexical_canonicalize_multiple_parents() {
    // lexical canonicalization removes .. by popping from the accumulated path
    assert_eq!(
        lexical_canonicalize(Path::new("a/b/c/../../d")),
        Path::new("a/d")
    );
    // This pops beyond the start, resulting in just "a"
    assert_eq!(lexical_canonicalize(Path::new("../../a")), Path::new("a"));
}

#[test]
fn lexical_canonicalize_root() {
    assert_eq!(
        lexical_canonicalize(Path::new("/a/b/../c")),
        Path::new("/a/c")
    );
    assert_eq!(lexical_canonicalize(Path::new("/")), Path::new("/"));
}

#[test]
fn lexical_canonicalize_complex() {
    assert_eq!(
        lexical_canonicalize(Path::new("a/./b/../c/./d")),
        Path::new("a/c/d")
    );
}

#[test]
fn lexical_canonicalize_empty() {
    assert_eq!(lexical_canonicalize(Path::new("")), Path::new(""));
}

#[test]
fn vault_relative_path_validation_rejects_portable_hazards() {
    for hazardous in [
        "CON",
        "a:b",
        "x~1.md",
        "trailing.",
        "trailing ",
        "..",
        "/absolute",
    ] {
        assert!(
            validate_vault_relative(Path::new(hazardous)).is_err(),
            "hazardous path was accepted: {hazardous:?}"
        );
    }
}
