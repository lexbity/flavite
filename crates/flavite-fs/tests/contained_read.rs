use flavite_fs::{Fs, FsError, OsFs};
#[test]
fn contained_read_is_capped_and_rejects_parent_and_absolute_paths() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("a.txt");
    std::fs::write(&path, vec![b'x'; 1_000_000]).unwrap();
    let fs = OsFs::new();
    assert!(matches!(
        fs.read_capped(&path, 32),
        Err(FsError::ReadTooLarge { .. })
    ));
    assert!(matches!(
        fs.read_capped_in_root(root.path(), std::path::Path::new("a.txt"), 32),
        Err(FsError::ReadTooLarge { .. })
    ));
    assert!(
        fs.read_capped_in_root(root.path(), std::path::Path::new("../a.txt"), 32)
            .is_err()
    );
    assert!(fs.read_capped_in_root(root.path(), &path, 32).is_err());
    std::fs::write(&path, b"small").unwrap();
    assert_eq!(
        fs.read_capped_in_root(root.path(), std::path::Path::new("a.txt"), 32)
            .unwrap(),
        b"small"
    );
}
#[cfg(unix)]
#[test]
fn directory_capability_refuses_outside_symlinks_and_symlink_swaps() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let fs = OsFs::new();
    std::fs::write(root.path().join("inside"), b"inside").unwrap();
    std::fs::write(outside.path().join("secret"), b"secret").unwrap();
    symlink(outside.path().join("secret"), root.path().join("escape")).unwrap();
    assert!(
        fs.read_capped_in_root(root.path(), std::path::Path::new("escape"), 1024)
            .is_err()
    );
    symlink("inside", root.path().join("link")).unwrap();
    // A note that IS a symlink is refused even when its target stays in
    // root (D-10: the final object must be the note itself).
    assert!(
        fs.read_capped_in_root(root.path(), std::path::Path::new("link"), 1024)
            .is_err()
    );
    assert_eq!(
        fs.read_capped_in_root(root.path(), std::path::Path::new("inside"), 1024)
            .unwrap(),
        b"inside"
    );
    let root_path = root.path().to_path_buf();
    let outside_path = outside.path().join("secret");
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flag = running.clone();
    let writer = std::thread::spawn(move || {
        let mut index = 0;
        while flag.load(std::sync::atomic::Ordering::Acquire) {
            let next = root_path.join("next-link");
            let _ = std::fs::remove_file(&next);
            symlink(
                if index % 2 == 0 {
                    std::path::Path::new("inside")
                } else {
                    &outside_path
                },
                &next,
            )
            .unwrap();
            std::fs::rename(next, root_path.join("link")).unwrap();
            index += 1;
        }
    });
    // Under the symlink swap race no read ever succeeds: every refused
    // read is a refused symlink, so no bytes ever cross the boundary.
    for _ in 0..1000 {
        assert!(
            fs.read_capped_in_root(root.path(), std::path::Path::new("link"), 1024)
                .is_err()
        );
    }
    running.store(false, std::sync::atomic::Ordering::Release);
    writer.join().unwrap();
}
