//! Tests for atomic write utilities

use flavite_fs::atomic::{atomic_write, cleanup_orphan_temps, list_orphan_temps, read_prior_bytes};
use flavite_fs::fs::Fs;
use flavite_fs::mem::MemFs;
use std::path::Path;

#[test]
fn atomic_write_basic() {
    let fs = MemFs::new();
    let path = Path::new("/test/file.md");
    
    // Create parent directory
    fs.create_dir_all(Path::new("/test")).unwrap();
    
    let _hash = atomic_write(&fs, path, b"hello world").unwrap();
    
    // Verify file was written
    let content = fs.read_if_exists(path).unwrap();
    assert_eq!(content, Some(b"hello world".to_vec()));
}

#[test]
fn atomic_write_overwrites_existing() {
    let fs = MemFs::new();
    let path = Path::new("/test/file.md");
    
    // Create parent directory
    fs.create_dir_all(Path::new("/test")).unwrap();
    
    // Write initial content
    fs.write_new(path, b"initial").unwrap();
    
    // Atomic write should overwrite
    let _hash = atomic_write(&fs, path, b"updated").unwrap();
    
    let content = fs.read_if_exists(path).unwrap();
    assert_eq!(content, Some(b"updated".to_vec()));
}

#[test]
fn atomic_write_in_subdirectory() {
    let fs = MemFs::new();
    let path = Path::new("/vault/notes/file.md");
    
    // Create parent directories
    fs.create_dir_all(Path::new("/vault/notes")).unwrap();
    
    let hash = atomic_write(&fs, path, b"content").unwrap();
    
    let content = fs.read_if_exists(path).unwrap();
    assert_eq!(content, Some(b"content".to_vec()));
}

#[test]
fn read_prior_bytes_existing_file() {
    let fs = MemFs::new();
    let path = Path::new("/test/file.md");
    
    fs.create_dir_all(Path::new("/test")).unwrap();
    fs.write_new(path, b"test data").unwrap();
    
    let result = read_prior_bytes(&fs, path).unwrap();
    assert!(result.is_some());
    
    let (bytes, _hash) = result.unwrap();
    assert_eq!(bytes, b"test data");
}

#[test]
fn read_prior_bytes_nonexistent_file() {
    let fs = MemFs::new();
    let path = Path::new("/nonexistent/file.md");
    
    let result = read_prior_bytes(&fs, path).unwrap();
    assert!(result.is_none());
}

#[test]
fn list_orphan_temps_empty() {
    let fs = MemFs::new();
    let dir = Path::new("/test");
    
    fs.create_dir_all(dir).unwrap();
    
    let orphans = list_orphan_temps(&fs, dir).unwrap();
    assert!(orphans.is_empty());
}

#[test]
fn list_orphan_temps_with_orphans() {
    let fs = MemFs::new();
    let dir = Path::new("/test");
    
    fs.create_dir_all(dir).unwrap();
    
    // Create a file with the temp prefix
    let orphan_path = dir.join(".flavite-atomic-12345");
    fs.write_new(&orphan_path, b"orphan").unwrap();
    
    let orphans = list_orphan_temps(&fs, dir).unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0], orphan_path);
}

#[test]
fn test_cleanup_orphan_temps() {
    let fs = MemFs::new();
    let dir = Path::new("/test");
    
    fs.create_dir_all(dir).unwrap();
    
    // Create orphan temp files
    let orphan1 = dir.join(".flavite-atomic-111");
    let orphan2 = dir.join(".flavite-atomic-222");
    fs.write_new(&orphan1, b"orphan1").unwrap();
    fs.write_new(&orphan2, b"orphan2").unwrap();
    
    // Create a normal file
    let normal = dir.join("normal.txt");
    fs.write_new(&normal, b"normal").unwrap();
    
    let count = cleanup_orphan_temps(&fs, dir).unwrap();
    assert_eq!(count, 2);
    
    // Verify orphans are gone
    assert!(fs.read_if_exists(&orphan1).unwrap().is_none());
    assert!(fs.read_if_exists(&orphan2).unwrap().is_none());
    
    // Verify normal file still exists
    assert!(fs.read_if_exists(&normal).unwrap().is_some());
}

#[test]
fn test_cleanup_orphan_temps_no_orphans() {
    let fs = MemFs::new();
    let dir = Path::new("/test");
    
    fs.create_dir_all(dir).unwrap();
    
    let count = cleanup_orphan_temps(&fs, dir).unwrap();
    assert_eq!(count, 0);
}
