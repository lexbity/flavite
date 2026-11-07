//! Private OS data-directory file primitives (RFC-0006 §6.9): atomic
//! create-replace with fsync for device-local state (operation keys,
//! high-water heads). These paths live OUTSIDE the vault and are never
//! part of note content, scanning or watcher scope.

use std::path::Path;

/// Read a private file; `None` when absent.
pub fn read_if_exists(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// Atomically write a private file (temp + rename + best-effort sync).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let temp = path.with_extension(format!("tmp-{}", uuid::Uuid::now_v7()));
    let result = (|| -> Result<(), String> {
        std::fs::write(&temp, bytes).map_err(|e| e.to_string())?;
        if let Ok(file) = std::fs::File::open(&temp) {
            let _ = file.sync_all();
        }
        std::fs::rename(&temp, path).map_err(|e| e.to_string())?;
        // The rename itself must be durable: this is the high-water head's
        // durability boundary (§6.9) — surviving records with a lost head
        // rename would quarantine a valid job as HeadMismatch.
        if let Some(parent) = path.parent()
            && let Ok(dir) = std::fs::File::open(parent)
        {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}
