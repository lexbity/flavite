//! `OsFs` — the real filesystem implementation (§4.1, §4.4).
//!
//! Minimal implementation for now; will be expanded in subsequent work.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use crate::fs::{DirEntry, Fs, FsError, Metadata, Result};
use crate::path::validate_vault_relative;

/// The real filesystem implementation.
#[derive(Debug, Default)]
pub struct OsFs;

impl OsFs {
    pub fn new() -> Self {
        Self
    }

    fn mtime_ns(metadata: &std::fs::Metadata) -> i64 {
        match metadata.modified() {
            Ok(st) => match st.duration_since(std::time::UNIX_EPOCH) {
                Ok(d) => d.as_nanos() as i64,
                Err(e) => -(e.duration().as_nanos() as i64),
            },
            Err(_) => 0,
        }
    }

    fn get_native_id(path: &Path) -> Result<(u64, u64)> {
        let meta = std::fs::symlink_metadata(path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                FsError::NotFound(path.to_path_buf())
            } else {
                FsError::Io(format!("failed to get metadata: {}", e))
            }
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok((meta.dev(), meta.ino()))
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            Ok((meta.volume_serial_number(), meta.file_index()))
        }
    }
}

impl Fs for OsFs {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
        // Simplified: use std's canonicalize
        std::fs::canonicalize(path)
            .map_err(|e| FsError::Io(format!("failed to canonicalize {}: {}", path.display(), e)))
    }

    fn resolve_symlink(&self, path: &Path) -> Result<PathBuf> {
        match std::fs::canonicalize(path) {
            Ok(target) => Ok(target),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let parent = path.parent().unwrap_or(Path::new("."));
                let name = path.file_name().ok_or_else(|| {
                    FsError::InvalidPath(format!("path has no file name: {}", path.display()))
                })?;
                let parent = std::fs::canonicalize(parent).map_err(|e| {
                    FsError::Io(format!(
                        "failed to canonicalize parent {}: {e}",
                        parent.display()
                    ))
                })?;
                Ok(parent.join(name))
            }
            Err(e) => Err(FsError::Io(format!(
                "failed to resolve {}: {e}",
                path.display()
            ))),
        }
    }

    fn encode_path_key(&self, path: &Path) -> Result<Vec<u8>> {
        validate_vault_relative(path)
            .map(String::into_bytes)
            .map_err(FsError::InvalidPath)
    }

    fn decode_path_key(&self, key: &[u8]) -> Result<PathBuf> {
        let s = std::str::from_utf8(key)
            .map_err(|_| FsError::InvalidPath("invalid UTF-8 in path key".into()))?;
        Ok(PathBuf::from(s))
    }

    fn validate_in_root(&self, root: &Path, target: &Path) -> Result<()> {
        let canon_root = self.canonicalize(root)?;
        let canon_target = self.canonicalize(target)?;
        if canon_target.starts_with(&canon_root) {
            Ok(())
        } else {
            Err(FsError::PathEscape(target.to_path_buf()))
        }
    }

    fn read_if_exists(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(FsError::Io(format!(
                "failed to read {}: {}",
                path.display(),
                e
            ))),
        }
    }

    fn read_capped(&self, path: &Path, max: u64) -> Result<Vec<u8>> {
        let bytes = self.read_if_exists(path)?;
        match bytes {
            Some(b) if b.len() as u64 > max => Err(FsError::ReadTooLarge {
                path: path.to_path_buf(),
                max,
            }),
            Some(b) => Ok(b),
            None => Err(FsError::NotFound(path.to_path_buf())),
        }
    }

    fn metadata(&self, path: &Path) -> Result<Metadata> {
        let meta = std::fs::symlink_metadata(path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                FsError::NotFound(path.to_path_buf())
            } else {
                FsError::Io(format!("failed to get metadata: {}", e))
            }
        })?;

        let file_type = meta.file_type();
        let (volume_id, native_id) = Self::get_native_id(path)?;
        Ok(Metadata {
            size: meta.len(),
            mtime_ns: Self::mtime_ns(&meta),
            native_id,
            volume_id,
            is_symlink: file_type.is_symlink(),
            is_dir: file_type.is_dir(),
            mode: 0o644,
        })
    }

    fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>> {
        let entries = std::fs::read_dir(path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                FsError::DirNotFound(path.to_path_buf())
            } else {
                FsError::Io(format!("failed to read dir: {}", e))
            }
        })?;

        let mut result = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| FsError::Io(format!("dir entry error: {}", e)))?;
            let ft = entry
                .file_type()
                .map_err(|e| FsError::Io(format!("failed to get file type: {}", e)))?;
            result.push(DirEntry {
                name: entry.file_name(),
                path: entry.path(),
                is_dir: ft.is_dir(),
                is_symlink: ft.is_symlink(),
            });
        }
        Ok(result)
    }

    fn exists(&self, path: &Path) -> Result<bool> {
        Ok(path.exists())
    }

    fn is_placeholder(&self, _path: &Path) -> Result<bool> {
        Ok(false)
    }

    fn write_new(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent()
            && parent != Path::new("")
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| FsError::Io(format!("failed to create parent: {}", e)))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    FsError::AlreadyExists(path.to_path_buf())
                } else {
                    FsError::Io(format!("failed to create {}: {e}", path.display()))
                }
            })?;
        use std::io::Write;
        file.write_all(bytes)
            .map_err(|e| FsError::Io(format!("failed to write {}: {e}", path.display())))?;
        Ok(())
    }

    fn copy_mode(&self, from: &Path, to: &Path) -> Result<()> {
        let permissions = std::fs::metadata(from)
            .map_err(|e| FsError::Io(format!("failed to read permissions: {e}")))?
            .permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = permissions.mode() & 0o7777;
            std::fs::set_permissions(to, std::fs::Permissions::from_mode(mode))
                .map_err(|e| FsError::Io(format!("failed to copy mode: {e}")))?;
        }
        #[cfg(windows)]
        {
            let readonly = permissions.readonly();
            let mut destination = std::fs::metadata(to)
                .map_err(|e| FsError::Io(format!("failed to read destination permissions: {e}")))?
                .permissions();
            destination.set_readonly(readonly);
            std::fs::set_permissions(to, destination)
                .map_err(|e| FsError::Io(format!("failed to copy read-only flag: {e}")))?;
        }
        Ok(())
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        std::fs::create_dir_all(path)
            .map_err(|e| FsError::Io(format!("failed to create dir: {}", e)))?;
        Ok(())
    }

    fn remove(&self, path: &Path) -> Result<()> {
        let meta = std::fs::symlink_metadata(path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                FsError::NotFound(path.to_path_buf())
            } else {
                FsError::Io(format!("failed to stat: {}", e))
            }
        })?;
        if meta.file_type().is_dir() {
            return Err(FsError::Io("cannot remove directory".into()));
        }
        std::fs::remove_file(path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                FsError::NotFound(path.to_path_buf())
            } else {
                FsError::Io(format!("failed to remove: {}", e))
            }
        })?;
        Ok(())
    }

    fn rename_replace(&self, from: &Path, to: &Path) -> Result<()> {
        std::fs::rename(from, to).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                FsError::NotFound(from.to_path_buf())
            } else {
                FsError::Io(format!("failed to rename: {}", e))
            }
        })?;
        Ok(())
    }

    fn rename_no_replace(&self, from: &Path, to: &Path) -> Result<()> {
        // Creating a hard link is an atomic no-replace operation: the
        // destination is either created in full or left untouched. Both paths
        // are in the same directory for our staging callers.
        std::fs::hard_link(from, to).map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                FsError::AlreadyExists(to.to_path_buf())
            } else if e.kind() == io::ErrorKind::NotFound {
                FsError::NotFound(from.to_path_buf())
            } else {
                FsError::Io(format!("failed to link without replacement: {e}"))
            }
        })?;
        if let Err(e) = std::fs::remove_file(from) {
            let _ = std::fs::remove_file(to);
            return Err(FsError::Io(format!("failed to remove rename source: {e}")));
        }
        Ok(())
    }

    fn fsync_file(&self, path: &Path) -> Result<()> {
        let file = File::open(path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                FsError::NotFound(path.to_path_buf())
            } else {
                FsError::Io(format!("failed to open: {}", e))
            }
        })?;
        file.sync_all()
            .map_err(|e| FsError::FsyncFailed(format!("failed to fsync: {}", e)))?;
        Ok(())
    }

    fn fsync_dir(&self, path: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            let dir = File::open(path).map_err(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    FsError::NotFound(path.to_path_buf())
                } else {
                    FsError::Io(format!("failed to open dir: {}", e))
                }
            })?;
            dir.sync_all()
                .map_err(|e| FsError::FsyncUnsupported(format!("dir fsync failed: {}", e)))?;
        }
        #[cfg(windows)]
        {
            return Err(FsError::FsyncUnsupported(
                "dir fsync not supported on Windows".into(),
            ));
        }
        Ok(())
    }
}
