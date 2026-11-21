//! Coverage for the private OS data-directory primitives (§6.9): atomic
//! create-replace with fsync, including the not-found and failure paths.

use flavite_vault::private_state::{read_if_exists, write_atomic};

#[test]
fn read_if_exists_reports_absent_and_present_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.bin");
    assert!(read_if_exists(&path).unwrap().is_none());
    std::fs::write(&path, b"abc").unwrap();
    assert_eq!(read_if_exists(&path).unwrap().as_deref(), Some(&b"abc"[..]));
}

#[test]
fn read_if_exists_surfaces_non_notfound_errors() {
    // Reading a directory is an error (EISDIR), not a "missing file".
    let dir = tempfile::tempdir().unwrap();
    let err = read_if_exists(dir.path()).expect_err("directory read fails");
    assert!(!err.is_empty());
}

#[test]
fn write_atomic_creates_parents_and_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a/b/c/state.bin");
    write_atomic(&path, b"hello").expect("write");
    assert_eq!(std::fs::read(&path).unwrap(), b"hello");

    // Overwrite replaces atomically.
    write_atomic(&path, b"world").expect("rewrite");
    assert_eq!(std::fs::read(&path).unwrap(), b"world");
}

#[test]
fn write_atomic_failure_cleans_the_temp_file() {
    let dir = tempfile::tempdir().unwrap();
    // The destination is an existing non-empty directory: the rename fails
    // and the temporary file must not linger.
    let dest = dir.path().join("occupied");
    std::fs::create_dir(&dest).unwrap();
    std::fs::write(dest.join("inner"), b"x").unwrap();
    let err = write_atomic(&dest, b"data").expect_err("rename into a directory fails");
    assert!(!err.is_empty());
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains("tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "temp file is removed on failure");
}
