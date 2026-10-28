//! Content-addressed local history with time-tiered retention (D6).
//!
//! Blob identity is the hash of the *uncompressed source bytes*. Per-note
//! records are separate from blobs so deduplication does not erase history
//! timestamps, reasons, or pins.

use flavite_fs::{Fs, FsError};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

pub const DEFAULT_HISTORY_CAP: u64 = 1024 * 1024 * 1024;
const META_FILE: &str = "history.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryReason {
    Save,
    ConflictResolution,
    Merge,
    Rename,
    RecoveryBase,
    Transaction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRef {
    pub note_key: String,
    pub hash: String,
    pub timestamp_secs: u64,
    pub reason: HistoryReason,
    #[serde(default)]
    pub pins: u32,
}

#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("history filesystem error: {0}")]
    Fs(#[from] FsError),
    #[error("history metadata is corrupt: {0}")]
    Metadata(String),
    #[error("history cap cannot be met without evicting pinned or protected history")]
    Capacity,
    #[error("history compression failed: {0}")]
    Compression(String),
    #[error("snapshot hash mismatch")]
    HashMismatch,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Metadata {
    records: Vec<SnapshotRef>,
}

/// Durable history rooted in the per-vault OS data directory.
pub struct HistoryStore {
    fs: Arc<dyn Fs + Send + Sync>,
    root: PathBuf,
    cap_bytes: u64,
    records: Vec<SnapshotRef>,
}

impl std::fmt::Debug for HistoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryStore")
            .field("root", &self.root)
            .field("cap_bytes", &self.cap_bytes)
            .field("records", &self.records.len())
            .finish()
    }
}

impl HistoryStore {
    pub fn open(fs: Arc<dyn Fs + Send + Sync>, root: PathBuf) -> Result<Self, HistoryError> {
        Self::open_with_cap(fs, root, DEFAULT_HISTORY_CAP)
    }

    pub fn open_with_cap(
        fs: Arc<dyn Fs + Send + Sync>,
        root: PathBuf,
        cap_bytes: u64,
    ) -> Result<Self, HistoryError> {
        let root_preexisted = fs.exists(&root)?;
        fs.create_dir_all(&root)?;
        if !root_preexisted {
            sync_directory_ancestors(&*fs, &root)?;
        }
        let meta_path = root.join(META_FILE);
        if fs
            .metadata(&meta_path)
            .is_ok_and(|metadata| metadata.size > 512 * 1024 * 1024)
        {
            return Err(HistoryError::Metadata(
                "history metadata exceeds the 512 MiB safety limit".into(),
            ));
        }
        let records = match fs.read_if_exists(&meta_path)? {
            Some(bytes) => {
                let metadata = serde_json::from_slice::<Metadata>(&bytes)
                    .map_err(|e| HistoryError::Metadata(e.to_string()))?;
                if metadata
                    .records
                    .iter()
                    .any(|record| !valid_hash(&record.hash))
                {
                    return Err(HistoryError::Metadata(
                        "history record contains an invalid content hash".into(),
                    ));
                }
                metadata.records
            }
            None => Vec::new(),
        };
        Ok(Self {
            fs,
            root,
            cap_bytes,
            records,
        })
    }

    pub fn records(&self) -> &[SnapshotRef] {
        &self.records
    }
    pub fn pinned_bytes(&self) -> Result<u64, HistoryError> {
        let hashes: HashSet<_> = self
            .records
            .iter()
            .filter(|r| r.pins > 0)
            .map(|r| r.hash.as_str())
            .collect();
        self.blob_bytes_for(&hashes)
    }

    /// Snapshot prior bytes. Returns the content hash; unchanged content is
    /// deduplicated at the blob layer but still receives a distinct record.
    pub fn snapshot(
        &mut self,
        note_key: &[u8],
        bytes: &[u8],
        reason: HistoryReason,
        pin: bool,
        now_secs: u64,
    ) -> Result<SnapshotRef, HistoryError> {
        self.prune_records(now_secs);
        self.persist_metadata()?;
        self.remove_unreferenced_blobs()?;
        let hash = blake3::hash(bytes).to_hex().to_string();
        let note_key = hex::encode(note_key);
        let encoded = zstd::stream::encode_all(bytes, 3)
            .map_err(|e| HistoryError::Compression(e.to_string()))?;
        if self.has_blob(&hash)? {
            self.get(&hash)?;
        } else {
            self.make_room(encoded.len() as u64, now_secs)?;
            let blob = self.blob_path(&hash);
            self.fs
                .create_dir_all(blob.parent().expect("history blob parent"))?;
            sync_directory_ancestors(&*self.fs, blob.parent().expect("history blob parent"))?;
            durable_new_file(&*self.fs, &blob, &encoded)?;
        }
        let record = SnapshotRef {
            note_key,
            hash,
            timestamp_secs: now_secs,
            reason,
            pins: u32::from(pin),
        };
        self.records.push(record.clone());
        self.prune_records(now_secs);
        self.persist_metadata()?;
        self.remove_unreferenced_blobs()?;
        Ok(record)
    }

    /// Persist and pin the current merge base exactly once per note/content.
    pub fn pin_merge_base(
        &mut self,
        note_key: &[u8],
        bytes: &[u8],
        now_secs: u64,
    ) -> Result<SnapshotRef, HistoryError> {
        let encoded_key = hex::encode(note_key);
        let hash = blake3::hash(bytes).to_hex().to_string();
        if let Some(record) = self.records.iter().find(|record| {
            record.note_key == encoded_key
                && record.hash == hash
                && record.reason == HistoryReason::RecoveryBase
                && record.pins > 0
        }) {
            self.get(&hash)?;
            return Ok(record.clone());
        }
        self.snapshot(note_key, bytes, HistoryReason::RecoveryBase, true, now_secs)
    }

    /// Release one pin on a superseded merge base for this note.
    pub fn unpin_merge_base(&mut self, note_key: &[u8], hash: &str) -> Result<(), HistoryError> {
        let encoded_key = hex::encode(note_key);
        if let Some(record) = self.records.iter_mut().find(|record| {
            record.note_key == encoded_key
                && record.hash == hash
                && record.reason == HistoryReason::RecoveryBase
                && record.pins > 0
        }) {
            record.pins -= 1;
        }
        self.persist_metadata()?;
        self.remove_unreferenced_blobs()
    }

    pub fn get(&self, hash: &str) -> Result<Vec<u8>, HistoryError> {
        if !valid_hash(hash) {
            return Err(HistoryError::Metadata(
                "requested history hash is malformed".into(),
            ));
        }
        let compressed = self
            .fs
            .read_if_exists(&self.blob_path(hash))?
            .ok_or_else(|| HistoryError::Metadata(format!("missing history blob {hash}")))?;
        let bytes = zstd::stream::decode_all(compressed.as_slice())
            .map_err(|e| HistoryError::Compression(e.to_string()))?;
        if blake3::hash(&bytes).to_hex().as_str() != hash {
            return Err(HistoryError::HashMismatch);
        }
        Ok(bytes)
    }

    pub fn pin(&mut self, hash: &str) -> Result<(), HistoryError> {
        let mut found = false;
        for record in &mut self.records {
            if record.hash == hash {
                record.pins = record.pins.saturating_add(1);
                found = true;
            }
        }
        if !found {
            return Err(HistoryError::Metadata(format!(
                "cannot pin unknown snapshot {hash}"
            )));
        }
        self.persist_metadata()
    }

    pub fn unpin(&mut self, hash: &str) -> Result<(), HistoryError> {
        for record in &mut self.records {
            if record.hash == hash {
                record.pins = record.pins.saturating_sub(1);
            }
        }
        self.persist_metadata()
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        self.root
            .join("blobs")
            .join(&hash[..2.min(hash.len())])
            .join(hash)
    }
    fn has_blob(&self, hash: &str) -> Result<bool, HistoryError> {
        Ok(self.fs.exists(&self.blob_path(hash))?)
    }

    fn make_room(&mut self, incoming: u64, now: u64) -> Result<(), HistoryError> {
        let mut sizes = self.live_blob_sizes()?;
        let mut total: u64 = sizes.values().sum();
        if incoming > self.cap_bytes {
            return Err(HistoryError::Capacity);
        }
        if total.saturating_add(incoming) <= self.cap_bytes {
            return Ok(());
        }
        self.prune_records(now);
        self.persist_metadata()?;
        self.remove_unreferenced_blobs()?;
        sizes = self.live_blob_sizes()?;
        total = sizes.values().sum();
        if total.saturating_add(incoming) <= self.cap_bytes {
            return Ok(());
        }

        // Under pressure, evict oldest unpinned records only while preserving
        // the newest 50 distinct snapshots per note. The cap takes precedence
        // over the floor, as specified.
        // Plan eviction against a private copy. A capacity error must not
        // make the in-memory index disagree with the durable metadata file.
        let mut retained = self.records.clone();
        let mut candidates = retained.clone();
        candidates.sort_by_key(|r| r.timestamp_secs);
        for candidate in candidates {
            if total.saturating_add(incoming) <= self.cap_bytes {
                break;
            }
            if candidate.pins > 0 {
                continue;
            }
            let key = candidate.note_key.clone();
            let distinct = retained
                .iter()
                .filter(|r| r.note_key == key)
                .map(|r| r.hash.as_str())
                .collect::<HashSet<_>>()
                .len();
            if distinct <= 50 {
                continue;
            }
            let Some(position) = retained.iter().position(|r| {
                r.note_key == candidate.note_key
                    && r.hash == candidate.hash
                    && r.timestamp_secs == candidate.timestamp_secs
                    && r.pins == candidate.pins
            }) else {
                continue;
            };
            let hash = retained[position].hash.clone();
            retained.remove(position);
            if !retained.iter().any(|r| r.hash == hash)
                && let Some(size) = sizes.remove(&hash)
            {
                total = total.saturating_sub(size);
            }
        }
        // The 50-version floor is subordinate to the hard vault cap. If
        // unpinned entries above the floor were insufficient, continue LRU
        // eviction below the floor; pinned entries remain protected.
        if total.saturating_add(incoming) > self.cap_bytes {
            let mut candidates = retained.clone();
            candidates.sort_by_key(|r| r.timestamp_secs);
            for candidate in candidates {
                if total.saturating_add(incoming) <= self.cap_bytes {
                    break;
                }
                if candidate.pins > 0 {
                    continue;
                }
                let Some(position) = retained.iter().position(|r| {
                    r.note_key == candidate.note_key
                        && r.hash == candidate.hash
                        && r.timestamp_secs == candidate.timestamp_secs
                        && r.pins == candidate.pins
                }) else {
                    continue;
                };
                let hash = retained[position].hash.clone();
                retained.remove(position);
                if !retained.iter().any(|r| r.hash == hash)
                    && let Some(size) = sizes.remove(&hash)
                {
                    total = total.saturating_sub(size);
                }
            }
        }
        if total.saturating_add(incoming) > self.cap_bytes {
            return Err(HistoryError::Capacity);
        }
        self.records = retained;
        self.persist_metadata()?;
        self.remove_unreferenced_blobs()?;
        Ok(())
    }

    fn prune_records(&mut self, now: u64) {
        let mut by_note: HashMap<String, Vec<SnapshotRef>> = HashMap::new();
        for r in self.records.drain(..) {
            by_note.entry(r.note_key.clone()).or_default().push(r);
        }
        for (_key, mut items) in by_note {
            items.sort_by_key(|r| r.timestamp_secs);
            let mut keep = HashSet::<(String, u64)>::new();
            let mut floor_hashes = HashSet::new();
            for record in items.iter().rev() {
                if floor_hashes.len() >= 50 {
                    break;
                }
                floor_hashes.insert(record.hash.clone());
            }
            for (idx, r) in items.iter().enumerate() {
                let age = now.saturating_sub(r.timestamp_secs);
                let retain = age <= 300
                    || (age <= 3600 && newest_in_bucket(&items, idx, 60))
                    || (age <= 30 * 86400 && newest_in_bucket(&items, idx, 3600))
                    || floor_hashes.contains(&r.hash)
                    || r.pins > 0;
                if retain {
                    keep.insert((r.hash.clone(), r.timestamp_secs));
                }
            }
            self.records.extend(
                items
                    .into_iter()
                    .filter(|r| keep.contains(&(r.hash.clone(), r.timestamp_secs))),
            );
        }
    }

    fn live_blob_sizes(&self) -> Result<HashMap<String, u64>, HistoryError> {
        let mut out = HashMap::new();
        for r in &self.records {
            if !out.contains_key(&r.hash) {
                let m = self.fs.metadata(&self.blob_path(&r.hash))?;
                out.insert(r.hash.clone(), m.size);
            }
        }
        Ok(out)
    }
    fn blob_bytes_for(&self, hashes: &HashSet<&str>) -> Result<u64, HistoryError> {
        let mut seen = HashSet::new();
        let mut total = 0;
        for h in hashes {
            if seen.insert(*h) {
                total += self.fs.metadata(&self.blob_path(h))?.size;
            }
        }
        Ok(total)
    }
    fn persist_metadata(&self) -> Result<(), HistoryError> {
        let bytes = serde_json::to_vec(&Metadata {
            records: self.records.clone(),
        })
        .map_err(|e| HistoryError::Metadata(e.to_string()))?;
        durable_replace(&*self.fs, &self.root.join(META_FILE), &bytes)?;
        Ok(())
    }
    fn remove_unreferenced_blobs(&self) -> Result<(), HistoryError> {
        let live: HashSet<_> = self.records.iter().map(|r| r.hash.as_str()).collect();
        let base = self.root.join("blobs");
        if !self.fs.exists(&base)? {
            return Ok(());
        }
        for shard in self.fs.read_dir(&base)? {
            if !shard.is_dir {
                continue;
            }
            for blob in self.fs.read_dir(&shard.path)? {
                let Some(name) = blob.path.file_name().and_then(|x| x.to_str()) else {
                    continue;
                };
                if !live.contains(name) {
                    let _ = self.fs.remove(&blob.path);
                }
            }
        }
        Ok(())
    }
}

fn newest_in_bucket(items: &[SnapshotRef], idx: usize, width: u64) -> bool {
    let bucket = items[idx].timestamp_secs / width;
    !items
        .iter()
        .enumerate()
        .any(|(j, other)| j > idx && other.timestamp_secs / width == bucket)
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Persist every newly-created directory entry leading to a history blob.
/// Syncing only the leaf shard protects the blob name inside that shard, but
/// does not make the shard's own name durable in `blobs/` (or `blobs/` in the
/// history root) after power loss.
fn sync_directory_ancestors(fs: &dyn Fs, start: &Path) -> Result<(), HistoryError> {
    let mut current = Some(start);
    while let Some(directory) = current {
        fs.fsync_dir(directory)?;
        current = directory.parent();
    }
    Ok(())
}

fn durable_new_file(fs: &dyn Fs, path: &Path, bytes: &[u8]) -> Result<(), HistoryError> {
    if let Err(error) = fs.write_new(path, bytes) {
        let _ = fs.remove(path);
        return Err(error.into());
    }
    if fs.read_if_exists(path)?.as_deref() != Some(bytes) {
        let _ = fs.remove(path);
        return Err(HistoryError::Metadata(format!(
            "short or altered write while creating {}",
            path.display()
        )));
    }
    if let Err(error) = fs.fsync_file(path) {
        let _ = fs.remove(path);
        return Err(error.into());
    }
    if let Some(parent) = path.parent()
        && let Err(error) = fs.fsync_dir(parent)
    {
        let _ = fs.remove(path);
        return Err(error.into());
    }
    Ok(())
}

fn durable_replace(fs: &dyn Fs, path: &Path, bytes: &[u8]) -> Result<(), HistoryError> {
    let parent = path
        .parent()
        .ok_or_else(|| HistoryError::Metadata("history path has no parent".into()))?;
    let temp = parent.join(format!(".history-{}.tmp", uuid::Uuid::now_v7()));
    if let Err(error) = fs.write_new(&temp, bytes) {
        let _ = fs.remove(&temp);
        return Err(error.into());
    }
    if fs.read_if_exists(&temp)?.as_deref() != Some(bytes) {
        let _ = fs.remove(&temp);
        return Err(HistoryError::Metadata(format!(
            "short or altered write while staging {}",
            path.display()
        )));
    }
    if let Err(error) = fs.fsync_file(&temp) {
        let _ = fs.remove(&temp);
        return Err(error.into());
    }
    if let Err(error) = fs.rename_replace(&temp, path) {
        let _ = fs.remove(&temp);
        return Err(error.into());
    }
    fs.fsync_dir(parent)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_fs::MemFs;

    #[test]
    fn content_addressing_deduplicates_blobs_but_keeps_records() {
        let fs = Arc::new(MemFs::new());
        let mut history = HistoryStore::open(fs.clone(), PathBuf::from("/data/history")).unwrap();
        let a = history
            .snapshot(b"note", b"same", HistoryReason::Save, false, 100)
            .unwrap();
        let b = history
            .snapshot(b"note", b"same", HistoryReason::Merge, false, 101)
            .unwrap();
        assert_eq!(a.hash, b.hash);
        assert_eq!(history.records().len(), 2);
        assert_eq!(history.get(&a.hash).unwrap(), b"same");
    }

    #[test]
    fn pin_survives_retention_and_reload() {
        let fs = Arc::new(MemFs::new());
        let root = PathBuf::from("/data/history");
        let mut history = HistoryStore::open(fs.clone(), root.clone()).unwrap();
        let record = history
            .snapshot(b"note", b"base", HistoryReason::RecoveryBase, true, 10)
            .unwrap();
        drop(history);
        let history = HistoryStore::open(fs, root).unwrap();
        assert_eq!(history.records()[0].hash, record.hash);
        assert_eq!(history.get(&record.hash).unwrap(), b"base");
    }

    #[test]
    fn merge_base_pinning_is_idempotent_and_releasable_by_note() {
        let fs = Arc::new(MemFs::new());
        let mut history = HistoryStore::open(fs, PathBuf::from("/data/history")).unwrap();
        let first = history.pin_merge_base(b"notes/a.md", b"base", 10).unwrap();
        let repeated = history.pin_merge_base(b"notes/a.md", b"base", 11).unwrap();
        assert_eq!(first.hash, repeated.hash);
        assert_eq!(history.records().len(), 1);
        assert!(history.pinned_bytes().unwrap() > 0);

        history
            .unpin_merge_base(b"notes/a.md", &first.hash)
            .unwrap();
        assert_eq!(history.pinned_bytes().unwrap(), 0);
    }

    #[test]
    fn retention_keeps_recent_versions_and_at_least_fifty_distinct_versions() {
        let fs = Arc::new(MemFs::new());
        let mut history = HistoryStore::open(fs, PathBuf::from("/data/history")).unwrap();
        for i in 0..100_u64 {
            history
                .snapshot(
                    b"note",
                    format!("revision-{i}").as_bytes(),
                    HistoryReason::Save,
                    false,
                    i * 60,
                )
                .unwrap();
        }
        history.prune_records(100 * 60);
        let hashes = history
            .records()
            .iter()
            .map(|record| record.hash.as_str())
            .collect::<HashSet<_>>();
        assert!(hashes.len() >= 50);
        assert!(hashes.len() <= 65);
    }
}
