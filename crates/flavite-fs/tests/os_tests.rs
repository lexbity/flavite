//! Tests for OsFs - the real filesystem implementation

use flavite_fs::os::OsFs;
use flavite_fs::fs::{Fs, FsError};
use std::path::Path;
use tempfile::tempdir;

#[test]
fn os_fs_new() {
    let fs = OsFs::new();
    // Just verify it creates without panic
}

#[test]
fn os_fs_exists() {
    let fs = OsFs::new();
    
    // Test with a file that exists
    let temp_dir = tempdir().unwrap();
    let test_file = temp_dir.path().join("test.txt");
    std::fs::write(&test_file, b"content").unwrap();
    
    assert!(fs.exists(&test_file).unwrap());
    
    // Test with a file that doesn't exist
    let nonexistent = temp_dir.path().join("nonexistent.txt");
    assert!(!fs.exists(&nonexistent).unwrap());
}

#[test]
fn os_fs_read_write() {
    let fs = OsFs::new();
    let temp_dir = tempdir().unwrap();
    let test_file = temp_dir.path().join("test.txt");
    
    // Write and read
    fs.write_new(&test_file, b"hello").unwrap();
    let content = fs.read_if_exists(&test_file).unwrap();
    assert_eq!(content, Some(b"hello".to_vec()));
}

#[test]
fn os_fs_metadata() {
    let fs = OsFs::new();
    let temp_dir = tempdir().unwrap();
    let test_file = temp_dir.path().join("test.txt");
    
    std::fs::write(&test_file, b"hello").unwrap();
    
    let meta = fs.metadata(&test_file).unwrap();
    assert_eq!(meta.size, 5);
    assert!(!meta.is_dir);
    assert!(!meta.is_symlink);
}

#[test]
fn os_fs_create_dir_all() {
    let fs = OsFs::new();
    let temp_dir = tempdir().unwrap();
    let nested = temp_dir.path().join("a/b/c");
    
    fs.create_dir_all(&nested).unwrap();
    assert!(nested.exists());
}

#[test]
fn os_fs_remove() {
    let fs = OsFs::new();
    let temp_dir = tempdir().unwrap();
    let test_file = temp_dir.path().join("test.txt");
    
    std::fs::write(&test_file, b"content").unwrap();
    assert!(test_file.exists());
    
    fs.remove(&test_file).unwrap();
    assert!(!test_file.exists());
}

#[test]
fn os_fs_remove_not_found() {
    let fs = OsFs::new();
    let temp_dir = tempdir().unwrap();
    let nonexistent = temp_dir.path().join("nonexistent.txt");
    
    let result = fs.remove(&nonexistent);
    assert!(matches!(result, Err(FsError::NotFound(_))));
}

#[test]
fn os_fs_rename_replace() {
    let fs = OsFs::new();
    let temp_dir = tempdir().unwrap();
    let from = temp_dir.path().join("from.txt");
    let to = temp_dir.path().join("to.txt");
    
    std::fs::write(&from, b"from content").unwrap();
    
    fs.rename_replace(&from, &to).unwrap();
    
    assert!(!from.exists());
    assert!(to.exists());
    
    let content = std::fs::read(&to).unwrap();
    assert_eq!(content, b"from content");
}

#[test]
fn os_fs_read_dir() {
    let fs = OsFs::new();
    let temp_dir = tempdir().unwrap();
    
    // Create some files
    std::fs::write(temp_dir.path().join("file1.txt"), b"1").unwrap();
    std::fs::write(temp_dir.path().join("file2.txt"), b"2").unwrap();
    std::fs::create_dir(temp_dir.path().join("subdir")).unwrap();
    
    let entries = fs.read_dir(temp_dir.path()).unwrap();
    assert_eq!(entries.len(), 3); // file1.txt, file2.txt, subdir
    
    let file_names: Vec<_> = entries.iter().map(|e| e.name.to_string_lossy()).collect();
    assert!(file_names.contains(&"file1.txt".into()));
    assert!(file_names.contains(&"file2.txt".into()));
    assert!(file_names.contains(&"subdir".into()));
}

#[test]
fn os_fs_validate_in_root() {
    let fs = OsFs::new();
    let temp_dir = tempdir().unwrap();
    
    let root = temp_dir.path();
    
    // Create a file to validate
    let valid = root.join("file.txt");
    std::fs::write(&valid, b"content").unwrap();
    
    // Valid path within root
    fs.validate_in_root(root, &valid).unwrap();
    
    // Invalid path escaping root
    let invalid = Path::new("/etc/passwd");
    let result = fs.validate_in_root(root, invalid);
    assert!(matches!(result, Err(FsError::PathEscape(_))));
}

#[test]
fn os_fs_canonicalize() {
    let fs = OsFs::new();
    let temp_dir = tempdir().unwrap();
    
    // Create a symlink
    let target = temp_dir.path().join("target.txt");
    let link = temp_dir.path().join("link.txt");
    
    std::fs::write(&target, b"content").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, &link).unwrap();
        
        let canonical = fs.canonicalize(&link).unwrap();
        assert!(canonical.exists());
    }
}

#[test]
fn os_fs_resolve_symlink() {
    let fs = OsFs::new();
    let temp_dir = tempdir().unwrap();
    
    let target = temp_dir.path().join("target.txt");
    let link = temp_dir.path().join("link.txt");
    
    std::fs::write(&target, b"content").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, &link).unwrap();
        
        let resolved = fs.resolve_symlink(&link).unwrap();
        assert!(resolved.exists());
    }
}

#[test]
fn os_fs_write_new_creates_file() {
    let fs = OsFs::new();
    let temp_dir = tempdir().unwrap();
    let test_file = temp_dir.path().join("test.txt");
    
    // Write a new file
    fs.write_new(&test_file, b"content").unwrap();
    
    // Verify it was created
    assert!(test_file.exists());
    let content = std::fs::read(&test_file).unwrap();
    assert_eq!(content, b"content");
}

#[test]
fn os_fs_read_capped() {
    let fs = OsFs::new();
    let temp_dir = tempdir().unwrap();
    let test_file = temp_dir.path().join("test.txt");
    
    std::fs::write(&test_file, b"hello world").unwrap();
    
    // Read with cap larger than file
    let content = fs.read_capped(&test_file, 100).unwrap();
    assert_eq!(content, b"hello world");
    
    // Read with exact cap
    let content = fs.read_capped(&test_file, 11).unwrap();
    assert_eq!(content, b"hello world");
}

#[test]
fn os_fs_read_capped_too_large() {
    let fs = OsFs::new();
    let temp_dir = tempdir().unwrap();
    let test_file = temp_dir.path().join("test.txt");
    
    std::fs::write(&test_file, b"hello world").unwrap();
    
    // Try to read with cap smaller than file
    let result = fs.read_capped(&test_file, 5);
    assert!(matches!(result, Err(FsError::ReadTooLarge { .. })));
}

#[test]
fn os_fs_encode_decode_path_key() {
    let fs = OsFs::new();
    let path = Path::new("test/file.txt");
    
    let key = fs.encode_path_key(path).unwrap();
    let decoded = fs.decode_path_key(&key).unwrap();
    
    assert_eq!(decoded, path);
}

#[test]
fn os_fs_encode_path_key_invalid() {
    let fs = OsFs::new();
    
    // Try to encode an absolute path
    let result = fs.encode_path_key(Path::new("/absolute/path"));
    assert!(matches!(result, Err(FsError::InvalidPath(_))));
    
    // Try to encode a path with parent traversal
    let result = fs.encode_path_key(Path::new("../escape"));
    assert!(matches!(result, Err(FsError::InvalidPath(_))));
}
