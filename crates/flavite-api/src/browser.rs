//! Lazy, native-identity directory pages for the vault browser.
use flavite_vault::session::Session;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub path: PathBuf,
    pub directory: bool,
    pub symlink: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryCursor {
    offset: usize,
    digest: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryPage {
    pub entries: Arc<[DirectoryEntry]>,
    pub next: Option<DirectoryCursor>,
    pub total: usize,
}

pub(crate) fn list(
    session: &Session,
    path: &Path,
    cursor: Option<&DirectoryCursor>,
    limit: usize,
) -> Result<DirectoryPage, String> {
    if !(1..=500).contains(&limit) {
        return Err("Directory page limit must be 1..=500".into());
    }
    let mut entries: Vec<_> = session
        .list_directory(path)?
        .into_iter()
        .map(|entry| DirectoryEntry {
            path: entry.path,
            directory: entry.is_dir,
            symlink: entry.is_symlink,
        })
        .collect();
    entries.sort_by(|a, b| (!a.directory, &a.path).cmp(&(!b.directory, &b.path)));
    let mut hash = blake3::Hasher::new();
    for entry in &entries {
        let bytes = entry.path.as_os_str().as_encoded_bytes();
        hash.update(&(bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
        hash.update(&[u8::from(entry.directory), u8::from(entry.symlink)]);
    }
    let digest = *hash.finalize().as_bytes();
    let offset = match cursor {
        Some(cursor) if cursor.digest == digest && cursor.offset <= entries.len() => cursor.offset,
        Some(_) => return Err("Directory changed; refresh this folder".into()),
        None => 0,
    };
    let end = offset.saturating_add(limit).min(entries.len());
    Ok(DirectoryPage {
        entries: Arc::from(entries[offset..end].to_vec()),
        next: (end < entries.len()).then_some(DirectoryCursor {
            offset: end,
            digest,
        }),
        total: entries.len(),
    })
}
