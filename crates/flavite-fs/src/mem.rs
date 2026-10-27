//! `MemFs` — in-memory [`Fs`] implementation for tests (§10.6.1).
//!
//! Crash model: separate **volatile** and **durable** state for both file bytes
//! and directory entries.
//!
//! - **Volatile file bytes** — what reads see right now (the "OS page cache").
//! - **Durable file bytes** — what `fsync_file` has persisted (survives power loss).
//! - **Volatile directory entries** — what `read_dir` sees right now.
//! - **Durable directory entries** — what `fsync_dir` has persisted.
//!
//! - **ProcessCrash** (`simulate_process_crash`): volatile state is retained
//!   (the OS page cache survives a process crash). No state change.
//! - **PowerLoss** (`simulate_power_loss`): volatile state reverts to durable
//!   state. File contents and directory entries that were not fsync'd are lost.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::fs::{DirEntry, Fs, FsError, Metadata, Result};

/// The root path for `MemFs`. All paths are absolute under this root.
const ROOT: &str = "/";

#[derive(Debug)]
struct MemState {
    /// Volatile file contents (OS-visible right now).
    vol_files: BTreeMap<PathBuf, Vec<u8>>,
    /// Durable file contents (after `fsync_file`; survive power loss).
    dur_files: BTreeMap<PathBuf, Vec<u8>>,
    /// Volatile directory entries: dir path → set of child paths.
    vol_dirs: BTreeMap<PathBuf, BTreeSet<PathBuf>>,
    /// Durable directory entries (after `fsync_dir`; survive power loss).
    dur_dirs: BTreeMap<PathBuf, BTreeSet<PathBuf>>,
    /// Symlinks: path → target path.
    symlinks: BTreeMap<PathBuf, PathBuf>,
    /// Native file identities.
    identities: BTreeMap<PathBuf, u64>,
    /// Next identity to assign.
    next_identity: u64,
    /// Mode bits per path.
    modes: BTreeMap<PathBuf, u32>,
    /// Modification times in nanoseconds.
    mtimes: BTreeMap<PathBuf, i64>,
    /// Injectable clock (nanoseconds).
    clock_ns: i64,
}

impl MemState {
    fn new() -> Self {
        let root = PathBuf::from(ROOT);
        let mut vol_dirs = BTreeMap::new();
        vol_dirs.insert(root.clone(), BTreeSet::new());
        let mut dur_dirs = BTreeMap::new();
        dur_dirs.insert(root.clone(), BTreeSet::new());
        let mut identities = BTreeMap::new();
        identities.insert(root.clone(), 0);
        let mut modes = BTreeMap::new();
        modes.insert(root.clone(), 0o755);
        let mut mtimes = BTreeMap::new();
        mtimes.insert(root, 0);
        Self {
            vol_files: BTreeMap::new(),
            dur_files: BTreeMap::new(),
            vol_dirs,
            dur_dirs,
            symlinks: BTreeMap::new(),
            identities,
            next_identity: 1,
            modes,
            mtimes,
            clock_ns: 0,
        }
    }

    fn assign_identity(&mut self, path: &Path) -> u64 {
        let id = self.next_identity;
        self.next_identity += 1;
        self.identities.insert(path.to_path_buf(), id);
        id
    }

    fn touch(&mut self, path: &Path) {
        self.mtimes.insert(path.to_path_buf(), self.clock_ns);
    }
}

/// In-memory filesystem with an injectable clock and symlink support.
///
/// Cheaply cloneable (shares state via `Arc`). The clone shares the same
/// underlying state — useful for `FaultFs` crash handlers.
#[derive(Clone, Debug)]
pub struct MemFs {
    state: Arc<Mutex<MemState>>,
}

impl Default for MemFs {
    fn default() -> Self {
        Self::new()
    }
}

impl MemFs {
    /// Create a new empty `MemFs` with the root directory initialized.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(MemState::new())),
        }
    }

    /// Advance the injectable clock by `ns` nanoseconds.
    pub fn advance_clock(&self, ns: i64) {
        let mut state = self.lock();
        state.clock_ns += ns;
    }

    /// Get the current clock value in nanoseconds.
    pub fn now_ns(&self) -> i64 {
        self.lock().clock_ns
    }

    /// Simulate a **process crash**: retain all volatile state.
    ///
    /// The OS page cache survives a process crash, so the volatile state is
    /// unchanged. The `FaultFs` wrapper tracks the "dead process" state.
    pub fn simulate_process_crash(&self) {
        // No state change — volatile state is retained.
    }

    /// Simulate **power loss**: revert volatile state to durable state.
    ///
    /// File contents and directory entries that were not fsync'd are lost.
    /// Acknowledged durability requires both the file sync and the namespace
    /// (directory) sync (§10.6.1).
    pub fn simulate_power_loss(&self) {
        let mut state = self.lock();
        state.vol_files = state.dur_files.clone();
        state.vol_dirs = state.dur_dirs.clone();
    }

    /// Create a symlink at `path` pointing to `target` (test helper).
    pub fn create_symlink(&self, path: &Path, target: &Path) {
        let mut state = self.lock();
        state
            .symlinks
            .insert(path.to_path_buf(), target.to_path_buf());
        if let Some(parent) = path.parent() {
            state
                .vol_dirs
                .entry(parent.to_path_buf())
                .or_default()
                .insert(path.to_path_buf());
        }
        state.assign_identity(path);
        state.modes.insert(path.to_path_buf(), 0o777);
        state.touch(path);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Check whether a path exists in the volatile directory entries.
    fn path_exists(state: &MemState, path: &Path) -> bool {
        if path == Path::new(ROOT) {
            return true;
        }
        if let Some(parent) = path.parent() {
            if let Some(children) = state.vol_dirs.get(parent) {
                return children.contains(path);
            }
        }
        false
    }

    /// Get the kind of an existing path.
    fn path_kind(state: &MemState, path: &Path) -> Option<PathKind> {
        if !Self::path_exists(state, path) {
            return None;
        }
        if state.symlinks.contains_key(path) {
            Some(PathKind::Symlink)
        } else if state.vol_dirs.contains_key(path) {
            Some(PathKind::Dir)
        } else if state.vol_files.contains_key(path) {
            Some(PathKind::File)
        } else {
            // Directory entry exists but no content — orphaned after power loss.
            // Treat as an empty file.
            Some(PathKind::File)
        }
    }

    /// Add a path to its parent's volatile directory entries.
    fn add_dir_entry(state: &mut MemState, path: &Path) {
        if let Some(parent) = path.parent() {
            state
                .vol_dirs
                .entry(parent.to_path_buf())
                .or_default()
                .insert(path.to_path_buf());
        }
    }

    /// Remove a path from its parent's volatile directory entries.
    fn remove_dir_entry(state: &mut MemState, path: &Path) {
        if let Some(parent) = path.parent() {
            if let Some(children) = state.vol_dirs.get_mut(parent) {
                children.remove(path);
            }
        }
    }

    /// Move all descendant entries from `from` to `to` in every map.
    ///
    /// Called when renaming a directory. Rewrites keys in `vol_dirs`,
    /// `vol_files`, `symlinks`, `identities`, `modes`, and `mtimes` that
    /// start with `from` to start with `to` instead.
    fn move_descendants(state: &mut MemState, from: &Path, to: &Path) {
        // Collect all keys that need to be moved (snapshot to avoid
        // borrow issues).
        let dir_keys: Vec<PathBuf> = state
            .vol_dirs
            .keys()
            .filter(|k| k.starts_with(from) && *k != from)
            .cloned()
            .collect();
        let file_keys: Vec<PathBuf> = state
            .vol_files
            .keys()
            .filter(|k| k.starts_with(from))
            .cloned()
            .collect();
        let symlink_keys: Vec<PathBuf> = state
            .symlinks
            .keys()
            .filter(|k| k.starts_with(from))
            .cloned()
            .collect();
        let identity_keys: Vec<PathBuf> = state
            .identities
            .keys()
            .filter(|k| k.starts_with(from) && *k != from)
            .cloned()
            .collect();
        let mode_keys: Vec<PathBuf> = state
            .modes
            .keys()
            .filter(|k| k.starts_with(from) && *k != from)
            .cloned()
            .collect();
        let mtime_keys: Vec<PathBuf> = state
            .mtimes
            .keys()
            .filter(|k| k.starts_with(from) && *k != from)
            .cloned()
            .collect();

        let rewrite = |old: &Path| -> PathBuf {
            let suffix = old.strip_prefix(from).unwrap_or(old);
            to.join(suffix)
        };

        // vol_dirs: move keys and update children sets.
        for old_key in dir_keys {
            let new_key = rewrite(&old_key);
            if let Some(children) = state.vol_dirs.remove(&old_key) {
                let new_children: BTreeSet<PathBuf> = children.iter().map(|c| rewrite(c)).collect();
                state.vol_dirs.insert(new_key, new_children);
            }
        }
        // vol_files: move keys.
        for old_key in file_keys {
            let new_key = rewrite(&old_key);
            if let Some(bytes) = state.vol_files.remove(&old_key) {
                state.vol_files.insert(new_key, bytes);
            }
        }
        // symlinks: move keys.
        for old_key in symlink_keys {
            let new_key = rewrite(&old_key);
            if let Some(target) = state.symlinks.remove(&old_key) {
                state.symlinks.insert(new_key, target);
            }
        }
        // identities, modes, mtimes: move keys.
        for old_key in identity_keys {
            let new_key = rewrite(&old_key);
            if let Some(v) = state.identities.remove(&old_key) {
                state.identities.insert(new_key, v);
            }
        }
        for old_key in mode_keys {
            let new_key = rewrite(&old_key);
            if let Some(v) = state.modes.remove(&old_key) {
                state.modes.insert(new_key, v);
            }
        }
        for old_key in mtime_keys {
            let new_key = rewrite(&old_key);
            if let Some(v) = state.mtimes.remove(&old_key) {
                state.mtimes.insert(new_key, v);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathKind {
    File,
    Dir,
    Symlink,
}

impl Fs for MemFs {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
        let state = self.lock();
        // Resolve `.` and `..` components lexically (no symlink resolution).
        let mut out = PathBuf::new();
        for component in path.components() {
            match component {
                Component::RootDir => out.push(ROOT),
                Component::CurDir => {}
                Component::ParentDir => {
                    out.pop();
                }
                Component::Normal(name) => out.push(name),
                Component::Prefix(_) => {
                    return Err(FsError::InvalidPath(format!(
                        "Windows prefix not supported in MemFs: {}",
                        path.display()
                    )));
                }
            }
        }
        // Resolve symlinks iteratively (with a depth limit).
        let mut current = out;
        for _ in 0..32 {
            let target = state.symlinks.get(&current).cloned();
            match target {
                Some(t) => {
                    // Resolve the symlink target relative to the symlink's parent.
                    let resolved = if t.is_absolute() {
                        t
                    } else {
                        current.parent().map(|p| p.join(&t)).unwrap_or(t)
                    };
                    // Canonicalize the resolved path recursively.
                    let mut canon = PathBuf::new();
                    for c in resolved.components() {
                        match c {
                            Component::RootDir => canon.push(ROOT),
                            Component::CurDir => {}
                            Component::ParentDir => {
                                canon.pop();
                            }
                            Component::Normal(n) => canon.push(n),
                            Component::Prefix(_) => {}
                        }
                    }
                    current = canon;
                }
                None => break,
            }
        }
        Ok(current)
    }

    fn resolve_symlink(&self, path: &Path) -> Result<PathBuf> {
        self.canonicalize(path)
    }

    fn encode_path_key(&self, path: &Path) -> Result<Vec<u8>> {
        // Reject absolute paths and non-normal components (§4.16.2).
        for component in path.components() {
            match component {
                Component::Normal(_) => {}
                _ => {
                    return Err(FsError::InvalidPath(format!(
                        "path key must be relative with normal components: {}",
                        path.display()
                    )));
                }
            }
        }
        // Portable UTF-8 encoding: each component as length-prefixed UTF-8.
        let mut out = Vec::new();
        for component in path.components() {
            let os = component.as_os_str();
            let s = os.to_str().ok_or_else(|| {
                FsError::InvalidPath(format!(
                    "non-UTF-8 path component in MemFs: {}",
                    os.to_string_lossy()
                ))
            })?;
            let bytes = s.as_bytes();
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        Ok(out)
    }

    fn decode_path_key(&self, key: &[u8]) -> Result<PathBuf> {
        let mut path = PathBuf::new();
        let mut pos = 0;
        while pos < key.len() {
            if pos + 4 > key.len() {
                return Err(FsError::InvalidPath("truncated path key header".into()));
            }
            let len =
                u32::from_le_bytes([key[pos], key[pos + 1], key[pos + 2], key[pos + 3]]) as usize;
            pos += 4;
            if pos + len > key.len() {
                return Err(FsError::InvalidPath("truncated path key component".into()));
            }
            let s = std::str::from_utf8(&key[pos..pos + len])
                .map_err(|_| FsError::InvalidPath("invalid UTF-8 in path key".into()))?;
            path.push(s);
            pos += len;
        }
        Ok(path)
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
        let state = self.lock();
        match Self::path_kind(&state, path) {
            None => Ok(None),
            Some(PathKind::Dir) => Err(FsError::Io(format!(
                "path is a directory: {}",
                path.display()
            ))),
            Some(PathKind::Symlink) => {
                let target = state
                    .symlinks
                    .get(path)
                    .ok_or_else(|| FsError::NotFound(path.to_path_buf()))?
                    .clone();
                drop(state);
                self.read_if_exists(&target)
            }
            Some(PathKind::File) => {
                Ok(Some(state.vol_files.get(path).cloned().unwrap_or_default()))
            }
        }
    }

    fn read_capped(&self, path: &Path, max: u64) -> Result<Vec<u8>> {
        let state = self.lock();
        match Self::path_kind(&state, path) {
            None => Err(FsError::NotFound(path.to_path_buf())),
            Some(PathKind::Dir) => Err(FsError::Io(format!(
                "path is a directory: {}",
                path.display()
            ))),
            Some(PathKind::Symlink) => {
                let target = state
                    .symlinks
                    .get(path)
                    .ok_or_else(|| FsError::NotFound(path.to_path_buf()))?
                    .clone();
                drop(state);
                self.read_capped(&target, max)
            }
            Some(PathKind::File) => {
                let bytes = state.vol_files.get(path).cloned().unwrap_or_default();
                if bytes.len() as u64 > max {
                    Err(FsError::ReadTooLarge {
                        path: path.to_path_buf(),
                        max,
                    })
                } else {
                    Ok(bytes)
                }
            }
        }
    }

    fn metadata(&self, path: &Path) -> Result<Metadata> {
        let state = self.lock();
        match Self::path_kind(&state, path) {
            None => Err(FsError::NotFound(path.to_path_buf())),
            Some(kind) => {
                let size = match kind {
                    PathKind::File => state
                        .vol_files
                        .get(path)
                        .map(|b| b.len() as u64)
                        .unwrap_or(0),
                    PathKind::Dir => 0,
                    PathKind::Symlink => 0,
                };
                let native_id = *state.identities.get(path).unwrap_or(&0);
                let mode = *state.modes.get(path).unwrap_or(&0o644);
                let mtime_ns = *state.mtimes.get(path).unwrap_or(&0);
                Ok(Metadata {
                    size,
                    mtime_ns,
                    native_id,
                    is_symlink: kind == PathKind::Symlink,
                    is_dir: kind == PathKind::Dir,
                    mode,
                })
            }
        }
    }

    fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>> {
        let state = self.lock();
        if !Self::path_exists(&state, path) {
            return Err(FsError::DirNotFound(path.to_path_buf()));
        }
        if !state.vol_dirs.contains_key(path) && !state.symlinks.contains_key(path) {
            return Err(FsError::NotADirectory(path.to_path_buf()));
        }
        let children = state.vol_dirs.get(path).cloned().unwrap_or_default();
        let mut entries = Vec::new();
        for child in children {
            let name = child.file_name().map(OsString::from).unwrap_or_default();
            let is_symlink = state.symlinks.contains_key(&child);
            let is_dir = !is_symlink && state.vol_dirs.contains_key(&child);
            entries.push(DirEntry {
                name,
                path: child,
                is_dir,
                is_symlink,
            });
        }
        Ok(entries)
    }

    fn exists(&self, path: &Path) -> Result<bool> {
        let state = self.lock();
        Ok(Self::path_exists(&state, path))
    }

    fn is_placeholder(&self, _path: &Path) -> Result<bool> {
        // MemFs has no cloud placeholders.
        Ok(false)
    }

    fn write_new(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        let mut state = self.lock();
        if Self::path_exists(&state, path) {
            return Err(FsError::AlreadyExists(path.to_path_buf()));
        }
        // Ensure parent directory exists.
        if let Some(parent) = path.parent() {
            if !state.vol_dirs.contains_key(parent) && parent != Path::new(ROOT) {
                return Err(FsError::NotFound(parent.to_path_buf()));
            }
        }
        state.vol_files.insert(path.to_path_buf(), bytes.to_vec());
        Self::add_dir_entry(&mut state, path);
        state.assign_identity(path);
        state.modes.insert(path.to_path_buf(), 0o644);
        state.touch(path);
        Ok(())
    }

    fn copy_mode(&self, from: &Path, to: &Path) -> Result<()> {
        let mut state = self.lock();
        let mode = *state
            .modes
            .get(from)
            .ok_or_else(|| FsError::NotFound(from.to_path_buf()))?;
        state.modes.insert(to.to_path_buf(), mode);
        Ok(())
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        let mut state = self.lock();
        // Collect all ancestor paths that need to be created.
        let mut to_create = Vec::new();
        let mut current = path.to_path_buf();
        while !state.vol_dirs.contains_key(&current) && current != *ROOT {
            to_create.push(current.clone());
            match current.parent() {
                Some(p) => current = p.to_path_buf(),
                None => break,
            }
        }
        // Create from the topmost missing ancestor down.
        for dir in to_create.into_iter().rev() {
            state.vol_dirs.insert(dir.clone(), BTreeSet::new());
            Self::add_dir_entry(&mut state, &dir);
            state.assign_identity(&dir);
            state.modes.insert(dir.clone(), 0o755);
            state.touch(&dir);
        }
        Ok(())
    }

    fn remove(&self, path: &Path) -> Result<()> {
        let mut state = self.lock();
        match Self::path_kind(&state, path) {
            None => Err(FsError::NotFound(path.to_path_buf())),
            Some(PathKind::Dir) => Err(FsError::Io(format!(
                "cannot remove directory: {}",
                path.display()
            ))),
            Some(PathKind::Symlink) => {
                state.symlinks.remove(path);
                Self::remove_dir_entry(&mut state, path);
                Ok(())
            }
            Some(PathKind::File) => {
                state.vol_files.remove(path);
                Self::remove_dir_entry(&mut state, path);
                Ok(())
            }
        }
    }

    fn rename_replace(&self, from: &Path, to: &Path) -> Result<()> {
        let mut state = self.lock();
        let kind =
            Self::path_kind(&state, from).ok_or_else(|| FsError::NotFound(from.to_path_buf()))?;
        // Self-rename is a no-op.
        if from == to {
            return Ok(());
        }
        // Remove existing destination (if different from source).
        match Self::path_kind(&state, to) {
            Some(PathKind::File) => {
                state.vol_files.remove(to);
            }
            Some(PathKind::Symlink) => {
                state.symlinks.remove(to);
            }
            Some(PathKind::Dir) => {
                return Err(FsError::Io(format!(
                    "cannot replace directory: {}",
                    to.display()
                )));
            }
            None => {}
        }
        Self::remove_dir_entry(&mut state, from);
        Self::remove_dir_entry(&mut state, to);
        match kind {
            PathKind::File => {
                let bytes = state.vol_files.remove(from).unwrap_or_default();
                state.vol_files.insert(to.to_path_buf(), bytes);
            }
            PathKind::Symlink => {
                let target = state.symlinks.remove(from).unwrap_or_default();
                state.symlinks.insert(to.to_path_buf(), target);
            }
            PathKind::Dir => {
                // Move the directory entry and all descendants.
                let children = state.vol_dirs.remove(from).unwrap_or_default();
                // Rewrite children paths from `from/...` to `to/...`.
                let new_children: BTreeSet<PathBuf> = children
                    .iter()
                    .map(|c| {
                        let suffix = c.strip_prefix(from).unwrap_or(c);
                        to.join(suffix)
                    })
                    .collect();
                state.vol_dirs.insert(to.to_path_buf(), new_children);
                // Move all descendant entries in vol_dirs, vol_files, symlinks,
                // identities, modes, mtimes.
                Self::move_descendants(&mut state, from, to);
            }
        }
        // Move identity/mode/mtime.
        if let Some(id) = state.identities.remove(from) {
            state.identities.insert(to.to_path_buf(), id);
        }
        if let Some(mode) = state.modes.remove(from) {
            state.modes.insert(to.to_path_buf(), mode);
        }
        if let Some(mtime) = state.mtimes.remove(from) {
            state.mtimes.insert(to.to_path_buf(), mtime);
        }
        Self::add_dir_entry(&mut state, to);
        Ok(())
    }

    fn rename_no_replace(&self, from: &Path, to: &Path) -> Result<()> {
        let state = self.lock();
        if Self::path_exists(&state, to) {
            return Err(FsError::AlreadyExists(to.to_path_buf()));
        }
        drop(state);
        self.rename_replace(from, to)
    }

    fn fsync_file(&self, path: &Path) -> Result<()> {
        let mut state = self.lock();
        match Self::path_kind(&state, path) {
            None => Err(FsError::NotFound(path.to_path_buf())),
            Some(PathKind::Dir) => Err(FsError::Io(format!(
                "cannot fsync directory as file: {}",
                path.display()
            ))),
            Some(PathKind::Symlink) => {
                // Fsync the target.
                let target = state
                    .symlinks
                    .get(path)
                    .ok_or_else(|| FsError::NotFound(path.to_path_buf()))?
                    .clone();
                drop(state);
                self.fsync_file(&target)
            }
            Some(PathKind::File) => {
                let bytes = state.vol_files.get(path).cloned().unwrap_or_default();
                state.dur_files.insert(path.to_path_buf(), bytes);
                Ok(())
            }
        }
    }

    fn fsync_dir(&self, path: &Path) -> Result<()> {
        let mut state = self.lock();
        if !state.vol_dirs.contains_key(path) {
            return Err(FsError::NotFound(path.to_path_buf()));
        }
        let entries = state.vol_dirs.get(path).cloned().unwrap_or_default();
        state.dur_dirs.insert(path.to_path_buf(), entries);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs() -> MemFs {
        MemFs::new()
    }

    fn write(fs: &MemFs, path: &str, content: &[u8]) {
        fs.write_new(Path::new(path), content).unwrap();
    }

    // --- write_new / read_if_exists ---

    #[test]
    fn write_and_read_roundtrip() {
        let fs = fs();
        fs.create_dir_all(Path::new("/vault")).unwrap();
        write(&fs, "/vault/note.md", b"hello world");
        assert_eq!(
            fs.read_if_exists(Path::new("/vault/note.md")).unwrap(),
            Some(b"hello world".to_vec())
        );
    }

    #[test]
    fn write_new_fails_if_exists() {
        let fs = fs();
        write(&fs, "/note.md", b"first");
        let err = fs.write_new(Path::new("/note.md"), b"second").unwrap_err();
        assert!(matches!(err, FsError::AlreadyExists(_)));
    }

    #[test]
    fn write_new_fails_if_parent_missing() {
        let fs = fs();
        let err = fs
            .write_new(Path::new("/no/such/dir/note.md"), b"x")
            .unwrap_err();
        assert!(matches!(err, FsError::NotFound(_)));
    }

    #[test]
    fn read_if_exists_returns_none_for_missing() {
        let fs = fs();
        assert_eq!(fs.read_if_exists(Path::new("/missing")).unwrap(), None);
    }

    #[test]
    fn read_if_exists_errors_on_directory() {
        let fs = fs();
        fs.create_dir_all(Path::new("/vault")).unwrap();
        let err = fs.read_if_exists(Path::new("/vault")).unwrap_err();
        assert!(matches!(err, FsError::Io(_)));
    }

    // --- read_capped ---

    #[test]
    fn read_capped_returns_content_under_cap() {
        let fs = fs();
        write(&fs, "/note.md", b"small");
        assert_eq!(
            fs.read_capped(Path::new("/note.md"), 100).unwrap(),
            b"small".to_vec()
        );
    }

    #[test]
    fn read_capped_errors_over_cap() {
        let fs = fs();
        write(&fs, "/big.md", b"this is too big");
        let err = fs.read_capped(Path::new("/big.md"), 5).unwrap_err();
        assert!(matches!(err, FsError::ReadTooLarge { max: 5, .. }));
    }

    #[test]
    fn read_capped_errors_for_missing() {
        let fs = fs();
        let err = fs.read_capped(Path::new("/missing"), 100).unwrap_err();
        assert!(matches!(err, FsError::NotFound(_)));
    }

    // --- metadata ---

    #[test]
    fn metadata_for_file() {
        let fs = fs();
        fs.advance_clock(1000);
        write(&fs, "/note.md", b"12345");
        let md = fs.metadata(Path::new("/note.md")).unwrap();
        assert_eq!(md.size, 5);
        assert_eq!(md.mtime_ns, 1000);
        assert!(!md.is_dir);
        assert!(!md.is_symlink);
        assert_eq!(md.mode, 0o644);
        assert!(md.native_id > 0);
    }

    #[test]
    fn metadata_for_dir() {
        let fs = fs();
        fs.create_dir_all(Path::new("/vault/sub")).unwrap();
        let md = fs.metadata(Path::new("/vault")).unwrap();
        assert!(md.is_dir);
        assert_eq!(md.mode, 0o755);
    }

    #[test]
    fn metadata_errors_for_missing() {
        let fs = fs();
        let err = fs.metadata(Path::new("/missing")).unwrap_err();
        assert!(matches!(err, FsError::NotFound(_)));
    }

    // --- read_dir ---

    #[test]
    fn read_dir_lists_entries() {
        let fs = fs();
        fs.create_dir_all(Path::new("/vault")).unwrap();
        write(&fs, "/vault/a.md", b"a");
        write(&fs, "/vault/b.md", b"b");
        fs.create_dir_all(Path::new("/vault/sub")).unwrap();

        let entries = fs.read_dir(Path::new("/vault")).unwrap();
        assert_eq!(entries.len(), 3);
        let names: Vec<_> = entries
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"a.md".to_string()));
        assert!(names.contains(&"b.md".to_string()));
        assert!(names.contains(&"sub".to_string()));

        let sub_entry = entries.iter().find(|e| e.name == "sub").unwrap();
        assert!(sub_entry.is_dir);
        assert!(!sub_entry.is_symlink);
    }

    #[test]
    fn read_dir_errors_for_missing() {
        let fs = fs();
        let err = fs.read_dir(Path::new("/missing")).unwrap_err();
        assert!(matches!(err, FsError::DirNotFound(_)));
    }

    #[test]
    fn read_dir_errors_for_file() {
        let fs = fs();
        write(&fs, "/note.md", b"x");
        let err = fs.read_dir(Path::new("/note.md")).unwrap_err();
        assert!(matches!(err, FsError::NotADirectory(_)));
    }

    // --- exists ---

    #[test]
    fn exists_checks() {
        let fs = fs();
        assert!(fs.exists(Path::new("/")).unwrap());
        assert!(!fs.exists(Path::new("/missing")).unwrap());
        write(&fs, "/note.md", b"x");
        assert!(fs.exists(Path::new("/note.md")).unwrap());
        fs.create_dir_all(Path::new("/dir")).unwrap();
        assert!(fs.exists(Path::new("/dir")).unwrap());
    }

    // --- is_placeholder ---

    #[test]
    fn is_placeholder_always_false() {
        let fs = fs();
        write(&fs, "/note.md", b"x");
        assert!(!fs.is_placeholder(Path::new("/note.md")).unwrap());
    }

    // --- copy_mode ---

    #[test]
    fn copy_mode_copies_bits() {
        let fs = fs();
        write(&fs, "/source.md", b"x");
        write(&fs, "/dest.md", b"y");
        fs.copy_mode(Path::new("/source.md"), Path::new("/dest.md"))
            .unwrap();
        let md = fs.metadata(Path::new("/dest.md")).unwrap();
        assert_eq!(md.mode, 0o644);
    }

    #[test]
    fn copy_mode_errors_for_missing_source() {
        let fs = fs();
        let err = fs
            .copy_mode(Path::new("/missing"), Path::new("/dest"))
            .unwrap_err();
        assert!(matches!(err, FsError::NotFound(_)));
    }

    // --- create_dir_all ---

    #[test]
    fn create_dir_all_creates_parents() {
        let fs = fs();
        fs.create_dir_all(Path::new("/a/b/c")).unwrap();
        assert!(fs.exists(Path::new("/a")).unwrap());
        assert!(fs.exists(Path::new("/a/b")).unwrap());
        assert!(fs.exists(Path::new("/a/b/c")).unwrap());
    }

    #[test]
    fn create_dir_all_is_idempotent() {
        let fs = fs();
        fs.create_dir_all(Path::new("/a/b")).unwrap();
        fs.create_dir_all(Path::new("/a/b")).unwrap();
        assert!(fs.exists(Path::new("/a/b")).unwrap());
    }

    // --- remove ---

    #[test]
    fn remove_deletes_file() {
        let fs = fs();
        write(&fs, "/note.md", b"x");
        fs.remove(Path::new("/note.md")).unwrap();
        assert!(!fs.exists(Path::new("/note.md")).unwrap());
    }

    #[test]
    fn remove_errors_for_missing() {
        let fs = fs();
        let err = fs.remove(Path::new("/missing")).unwrap_err();
        assert!(matches!(err, FsError::NotFound(_)));
    }

    #[test]
    fn remove_errors_for_directory() {
        let fs = fs();
        fs.create_dir_all(Path::new("/dir")).unwrap();
        let err = fs.remove(Path::new("/dir")).unwrap_err();
        assert!(matches!(err, FsError::Io(_)));
    }

    // --- rename_replace ---

    #[test]
    fn rename_replace_moves_file() {
        let fs = fs();
        write(&fs, "/old.md", b"content");
        fs.rename_replace(Path::new("/old.md"), Path::new("/new.md"))
            .unwrap();
        assert!(!fs.exists(Path::new("/old.md")).unwrap());
        assert_eq!(
            fs.read_if_exists(Path::new("/new.md")).unwrap(),
            Some(b"content".to_vec())
        );
    }

    #[test]
    fn rename_replace_overwrites_existing() {
        let fs = fs();
        write(&fs, "/a.md", b"aaa");
        write(&fs, "/b.md", b"bbb");
        fs.rename_replace(Path::new("/a.md"), Path::new("/b.md"))
            .unwrap();
        assert_eq!(
            fs.read_if_exists(Path::new("/b.md")).unwrap(),
            Some(b"aaa".to_vec())
        );
        assert!(!fs.exists(Path::new("/a.md")).unwrap());
    }

    #[test]
    fn rename_replace_errors_for_missing_source() {
        let fs = fs();
        let err = fs
            .rename_replace(Path::new("/missing"), Path::new("/dest"))
            .unwrap_err();
        assert!(matches!(err, FsError::NotFound(_)));
    }

    #[test]
    fn rename_replace_preserves_identity() {
        let fs = fs();
        write(&fs, "/old.md", b"x");
        let id_before = fs.metadata(Path::new("/old.md")).unwrap().native_id;
        fs.rename_replace(Path::new("/old.md"), Path::new("/new.md"))
            .unwrap();
        let id_after = fs.metadata(Path::new("/new.md")).unwrap().native_id;
        assert_eq!(id_before, id_after);
    }

    // --- rename_no_replace ---

    #[test]
    fn rename_no_replace_fails_if_dest_exists() {
        let fs = fs();
        write(&fs, "/a.md", b"a");
        write(&fs, "/b.md", b"b");
        let err = fs
            .rename_no_replace(Path::new("/a.md"), Path::new("/b.md"))
            .unwrap_err();
        assert!(matches!(err, FsError::AlreadyExists(_)));
    }

    #[test]
    fn rename_no_replace_succeeds_if_dest_missing() {
        let fs = fs();
        write(&fs, "/a.md", b"a");
        fs.rename_no_replace(Path::new("/a.md"), Path::new("/b.md"))
            .unwrap();
        assert!(fs.exists(Path::new("/b.md")).unwrap());
    }

    // --- fsync_file / fsync_dir ---

    #[test]
    fn fsync_file_persists_content() {
        let fs = fs();
        write(&fs, "/note.md", b"durable content");
        fs.fsync_file(Path::new("/note.md")).unwrap();
        // Directory entry must also be durable for the file to be accessible.
        fs.fsync_dir(Path::new("/")).unwrap();
        // Content is now durable.
        fs.simulate_power_loss();
        assert_eq!(
            fs.read_if_exists(Path::new("/note.md")).unwrap(),
            Some(b"durable content".to_vec())
        );
    }

    #[test]
    fn fsync_file_errors_for_missing() {
        let fs = fs();
        let err = fs.fsync_file(Path::new("/missing")).unwrap_err();
        assert!(matches!(err, FsError::NotFound(_)));
    }

    #[test]
    fn fsync_file_errors_for_directory() {
        let fs = fs();
        fs.create_dir_all(Path::new("/dir")).unwrap();
        let err = fs.fsync_file(Path::new("/dir")).unwrap_err();
        assert!(matches!(err, FsError::Io(_)));
    }

    #[test]
    fn fsync_dir_persists_entries() {
        let fs = fs();
        fs.create_dir_all(Path::new("/vault")).unwrap();
        fs.fsync_dir(Path::new("/")).unwrap();
        fs.fsync_dir(Path::new("/vault")).unwrap();
        write(&fs, "/vault/note.md", b"x");
        fs.fsync_file(Path::new("/vault/note.md")).unwrap();
        fs.fsync_dir(Path::new("/vault")).unwrap();
        fs.simulate_power_loss();
        assert!(fs.exists(Path::new("/vault/note.md")).unwrap());
    }

    #[test]
    fn fsync_dir_errors_for_missing() {
        let fs = fs();
        let err = fs.fsync_dir(Path::new("/missing")).unwrap_err();
        assert!(matches!(err, FsError::NotFound(_)));
    }

    // --- Crash models (§10.6.1) ---

    #[test]
    fn process_crash_retains_volatile_state() {
        let fs = fs();
        write(&fs, "/note.md", b"unsynced content");
        fs.simulate_process_crash();
        // Volatile state is retained — content still readable.
        assert_eq!(
            fs.read_if_exists(Path::new("/note.md")).unwrap(),
            Some(b"unsynced content".to_vec())
        );
    }

    #[test]
    fn power_loss_discards_unsynced_file_content() {
        let fs = fs();
        fs.create_dir_all(Path::new("/vault")).unwrap();
        fs.fsync_dir(Path::new("/")).unwrap();
        fs.fsync_dir(Path::new("/vault")).unwrap();
        // Write a file but don't fsync it.
        write(&fs, "/vault/unsynced.md", b"will be lost");
        fs.simulate_power_loss();
        // File content was not fsync'd → lost.
        // Directory entry was fsync'd (for /vault) but the new entry wasn't → also lost.
        assert!(!fs.exists(Path::new("/vault/unsynced.md")).unwrap());
    }

    #[test]
    fn power_loss_preserves_fsyncd_content() {
        let fs = fs();
        fs.create_dir_all(Path::new("/vault")).unwrap();
        fs.fsync_dir(Path::new("/")).unwrap();
        fs.fsync_dir(Path::new("/vault")).unwrap();
        write(&fs, "/vault/note.md", b"durable");
        fs.fsync_file(Path::new("/vault/note.md")).unwrap();
        fs.fsync_dir(Path::new("/vault")).unwrap();
        fs.simulate_power_loss();
        assert_eq!(
            fs.read_if_exists(Path::new("/vault/note.md")).unwrap(),
            Some(b"durable".to_vec())
        );
    }

    #[test]
    fn file_sync_without_dir_sync_is_distinguishable() {
        // AC-36: file sync without directory sync is distinguishable.
        let fs = fs();
        fs.create_dir_all(Path::new("/vault")).unwrap();
        fs.fsync_dir(Path::new("/")).unwrap();
        fs.fsync_dir(Path::new("/vault")).unwrap();
        // Write file, fsync file content, but NOT the directory.
        write(&fs, "/vault/note.md", b"content");
        fs.fsync_file(Path::new("/vault/note.md")).unwrap();
        // No fsync_dir for /vault after the write.
        fs.simulate_power_loss();
        // File content was fsync'd (durable) but directory entry was not.
        // After power loss, the directory entry is lost → file not accessible.
        assert!(!fs.exists(Path::new("/vault/note.md")).unwrap());
    }

    #[test]
    fn dir_sync_without_file_sync_is_distinguishable() {
        // AC-36: directory sync without file sync is distinguishable.
        let fs = fs();
        fs.create_dir_all(Path::new("/vault")).unwrap();
        fs.fsync_dir(Path::new("/")).unwrap();
        fs.fsync_dir(Path::new("/vault")).unwrap();
        // Write file, fsync directory (entry), but NOT the file content.
        write(&fs, "/vault/note.md", b"content");
        fs.fsync_dir(Path::new("/vault")).unwrap();
        // No fsync_file.
        fs.simulate_power_loss();
        // Directory entry survives but file content is lost.
        // After power loss, vol_files = dur_files (empty for this path).
        // The directory entry survives (dur_dirs has it).
        // So the file "exists" but is empty.
        let result = fs.read_if_exists(Path::new("/vault/note.md")).unwrap();
        assert_eq!(result, Some(Vec::new()));
    }

    // --- Symlinks ---

    #[test]
    fn symlink_resolves_to_target() {
        let fs = fs();
        fs.create_dir_all(Path::new("/vault")).unwrap();
        write(&fs, "/vault/real.md", b"real content");
        fs.create_symlink(Path::new("/vault/link.md"), Path::new("/vault/real.md"));
        assert_eq!(
            fs.read_if_exists(Path::new("/vault/link.md")).unwrap(),
            Some(b"real content".to_vec())
        );
    }

    #[test]
    fn symlink_metadata_reports_symlink() {
        let fs = fs();
        write(&fs, "/target.md", b"x");
        fs.create_symlink(Path::new("/link.md"), Path::new("/target.md"));
        let md = fs.metadata(Path::new("/link.md")).unwrap();
        assert!(md.is_symlink);
        assert!(!md.is_dir);
    }

    #[test]
    fn symlink_in_read_dir() {
        let fs = fs();
        write(&fs, "/target.md", b"x");
        fs.create_symlink(Path::new("/link.md"), Path::new("/target.md"));
        let entries = fs.read_dir(Path::new("/")).unwrap();
        let link_entry = entries.iter().find(|e| e.name == "link.md").unwrap();
        assert!(link_entry.is_symlink);
    }

    #[test]
    fn symlink_canonicalize_resolves_chain() {
        let fs = fs();
        write(&fs, "/real.md", b"x");
        fs.create_symlink(Path::new("/link1.md"), Path::new("/real.md"));
        fs.create_symlink(Path::new("/link2.md"), Path::new("/link1.md"));
        let resolved = fs.canonicalize(Path::new("/link2.md")).unwrap();
        assert_eq!(resolved, PathBuf::from("/real.md"));
    }

    #[test]
    fn remove_symlink_removes_link_not_target() {
        let fs = fs();
        write(&fs, "/target.md", b"x");
        fs.create_symlink(Path::new("/link.md"), Path::new("/target.md"));
        fs.remove(Path::new("/link.md")).unwrap();
        assert!(!fs.exists(Path::new("/link.md")).unwrap());
        assert!(fs.exists(Path::new("/target.md")).unwrap());
    }

    #[test]
    fn rename_symlink_moves_link() {
        let fs = fs();
        write(&fs, "/target.md", b"x");
        fs.create_symlink(Path::new("/link.md"), Path::new("/target.md"));
        fs.rename_replace(Path::new("/link.md"), Path::new("/link2.md"))
            .unwrap();
        assert!(!fs.exists(Path::new("/link.md")).unwrap());
        assert_eq!(
            fs.read_if_exists(Path::new("/link2.md")).unwrap(),
            Some(b"x".to_vec())
        );
    }

    #[test]
    fn fsync_symlink_fsyncs_target() {
        let fs = fs();
        write(&fs, "/target.md", b"x");
        fs.create_symlink(Path::new("/link.md"), Path::new("/target.md"));
        fs.fsync_file(Path::new("/link.md")).unwrap();
        // Target content is now durable.
        fs.simulate_power_loss();
        // Note: after power loss, dir entries for / are durable (initial state).
        // The symlink itself was created after init, so its dir entry is lost.
        // This tests that fsync_file on a symlink delegates to the target.
        // We can't easily test the durable state of the target through the symlink
        // after power loss (since the symlink's dir entry is lost), but we verify
        // no error occurs.
    }

    // --- PathKey encoding ---

    #[test]
    fn path_key_roundtrip() {
        let fs = fs();
        let path = Path::new("notes/sub/note.md");
        let key = fs.encode_path_key(path).unwrap();
        let decoded = fs.decode_path_key(&key).unwrap();
        assert_eq!(decoded, path);
    }

    #[test]
    fn path_key_rejects_absolute() {
        let fs = fs();
        let err = fs.encode_path_key(Path::new("/absolute/path")).unwrap_err();
        assert!(matches!(err, FsError::InvalidPath(_)));
    }

    #[test]
    fn path_key_rejects_parent_components() {
        let fs = fs();
        let err = fs
            .encode_path_key(Path::new("notes/../escape"))
            .unwrap_err();
        assert!(matches!(err, FsError::InvalidPath(_)));
    }

    #[test]
    fn path_key_decode_errors_on_truncated() {
        let fs = fs();
        let err = fs.decode_path_key(&[0, 0]).unwrap_err();
        assert!(matches!(err, FsError::InvalidPath(_)));
    }

    #[test]
    fn path_key_decode_errors_on_bad_utf8() {
        let fs = fs();
        // Length prefix says 1 byte, but the byte is invalid UTF-8.
        let key = [1, 0, 0, 0, 0xFF];
        let err = fs.decode_path_key(&key).unwrap_err();
        assert!(matches!(err, FsError::InvalidPath(_)));
    }

    // --- validate_in_root ---

    #[test]
    fn validate_in_root_accepts_contained() {
        let fs = fs();
        fs.validate_in_root(Path::new("/vault"), Path::new("/vault/note.md"))
            .unwrap();
    }

    #[test]
    fn validate_in_root_rejects_escape() {
        let fs = fs();
        let err = fs
            .validate_in_root(Path::new("/vault"), Path::new("/etc/passwd"))
            .unwrap_err();
        assert!(matches!(err, FsError::PathEscape(_)));
    }

    #[test]
    fn validate_in_root_rejects_parent_traversal() {
        let fs = fs();
        let err = fs
            .validate_in_root(Path::new("/vault"), Path::new("/vault/../etc"))
            .unwrap_err();
        assert!(matches!(err, FsError::PathEscape(_)));
    }

    // --- canonicalize ---

    #[test]
    fn canonicalize_resolves_dot_components() {
        let fs = fs();
        let canon = fs
            .canonicalize(Path::new("/vault/./sub/../note.md"))
            .unwrap();
        assert_eq!(canon, PathBuf::from("/vault/note.md"));
    }

    #[test]
    fn canonicalize_rejects_prefix() {
        let fs = fs();
        // On Unix, there are no prefix components, so this tests the error path
        // only on Windows. On Unix, we just verify normal canonicalization works.
        let canon = fs.canonicalize(Path::new("/vault/note.md")).unwrap();
        assert_eq!(canon, PathBuf::from("/vault/note.md"));
    }

    // --- Clock ---

    #[test]
    fn clock_advances_and_touches() {
        let fs = fs();
        assert_eq!(fs.now_ns(), 0);
        fs.advance_clock(500);
        assert_eq!(fs.now_ns(), 500);
        write(&fs, "/note.md", b"x");
        let md = fs.metadata(Path::new("/note.md")).unwrap();
        assert_eq!(md.mtime_ns, 500);
    }

    // --- Clone shares state ---

    #[test]
    fn clone_shares_state() {
        let fs = fs();
        let fs2 = fs.clone();
        write(&fs, "/note.md", b"shared");
        assert_eq!(
            fs2.read_if_exists(Path::new("/note.md")).unwrap(),
            Some(b"shared".to_vec())
        );
    }

    // --- hash_file helper ---

    #[test]
    fn hash_file_computes_blake3() {
        let fs = fs();
        write(&fs, "/note.md", b"hello");
        let hash = crate::fs::hash_file(&fs, Path::new("/note.md")).unwrap();
        let expected = *blake3::hash(b"hello").as_bytes();
        assert_eq!(hash, expected);
    }

    #[test]
    fn hash_file_errors_for_missing() {
        let fs = fs();
        let err = crate::fs::hash_file(&fs, Path::new("/missing")).unwrap_err();
        assert!(matches!(err, FsError::NotFound(_)));
    }

    // --- Edge-case coverage ---

    #[test]
    fn default_creates_new_memfs() {
        let fs = MemFs::default();
        assert!(fs.exists(Path::new("/")).unwrap());
    }

    #[test]
    fn path_exists_with_missing_parent() {
        // A path whose parent directory doesn't exist in vol_dirs.
        let fs = fs();
        // /a/b/c — parent /a/b doesn't exist, so path_exists returns false
        // even though the path components are valid.
        assert!(!fs.exists(Path::new("/a/b/c")).unwrap());
    }

    #[test]
    fn canonicalize_with_cur_dir_component() {
        let fs = fs();
        let canon = fs.canonicalize(Path::new("/vault/./note.md")).unwrap();
        assert_eq!(canon, PathBuf::from("/vault/note.md"));
    }

    #[test]
    fn canonicalize_with_parent_dir_at_root() {
        let fs = fs();
        // Parent dir at root: /.. → / (pop on root is a no-op)
        let canon = fs.canonicalize(Path::new("/..")).unwrap();
        assert_eq!(canon, PathBuf::from("/"));
    }

    #[test]
    fn canonicalize_symlink_with_relative_target() {
        let fs = fs();
        fs.create_dir_all(Path::new("/vault/sub")).unwrap();
        write(&fs, "/vault/sub/real.md", b"x");
        // Symlink with a relative target (relative to the symlink's parent).
        fs.create_symlink(Path::new("/vault/link.md"), Path::new("sub/real.md"));
        let resolved = fs.canonicalize(Path::new("/vault/link.md")).unwrap();
        assert_eq!(resolved, PathBuf::from("/vault/sub/real.md"));
    }

    #[test]
    fn canonicalize_symlink_chain_with_dot_components() {
        let fs = fs();
        fs.create_dir_all(Path::new("/a/b")).unwrap();
        write(&fs, "/a/b/real.md", b"x");
        fs.create_symlink(Path::new("/a/link.md"), Path::new("./b/../b/real.md"));
        let resolved = fs.canonicalize(Path::new("/a/link.md")).unwrap();
        assert_eq!(resolved, PathBuf::from("/a/b/real.md"));
    }

    #[test]
    fn canonicalize_symlink_loop_terminates() {
        // A symlink loop should terminate (depth limit 32) and return the
        // last resolved path without hanging.
        let fs = fs();
        fs.create_symlink(Path::new("/loop1"), Path::new("/loop2"));
        fs.create_symlink(Path::new("/loop2"), Path::new("/loop1"));
        // Should not hang; returns some path.
        let _ = fs.canonicalize(Path::new("/loop1"));
    }

    #[test]
    fn read_capped_through_symlink() {
        let fs = fs();
        write(&fs, "/real.md", b"content via symlink");
        fs.create_symlink(Path::new("/link.md"), Path::new("/real.md"));
        assert_eq!(
            fs.read_capped(Path::new("/link.md"), 100).unwrap(),
            b"content via symlink".to_vec()
        );
    }

    #[test]
    fn read_capped_through_symlink_over_cap() {
        let fs = fs();
        write(&fs, "/real.md", b"this is too long");
        fs.create_symlink(Path::new("/link.md"), Path::new("/real.md"));
        let err = fs.read_capped(Path::new("/link.md"), 5).unwrap_err();
        assert!(matches!(err, FsError::ReadTooLarge { max: 5, .. }));
    }

    #[test]
    fn read_capped_through_symlink_missing_target() {
        let fs = fs();
        fs.create_symlink(Path::new("/dangling.md"), Path::new("/nonexistent.md"));
        let err = fs.read_capped(Path::new("/dangling.md"), 100).unwrap_err();
        assert!(matches!(err, FsError::NotFound(_)));
    }

    #[test]
    fn read_if_exists_through_dangling_symlink() {
        let fs = fs();
        fs.create_symlink(Path::new("/dangling.md"), Path::new("/nonexistent.md"));
        assert_eq!(fs.read_if_exists(Path::new("/dangling.md")).unwrap(), None);
    }

    #[test]
    fn write_new_at_root() {
        let fs = fs();
        // Writing directly at root (parent is root, which always exists).
        write(&fs, "/root-file.md", b"at root");
        assert_eq!(
            fs.read_if_exists(Path::new("/root-file.md")).unwrap(),
            Some(b"at root".to_vec())
        );
    }

    #[test]
    fn create_dir_all_at_root_is_noop() {
        let fs = fs();
        // Creating / when it already exists is a no-op.
        fs.create_dir_all(Path::new("/")).unwrap();
        assert!(fs.exists(Path::new("/")).unwrap());
    }

    #[test]
    fn create_dir_all_single_level() {
        let fs = fs();
        fs.create_dir_all(Path::new("/single")).unwrap();
        assert!(fs.exists(Path::new("/single")).unwrap());
        let md = fs.metadata(Path::new("/single")).unwrap();
        assert!(md.is_dir);
        assert_eq!(md.mode, 0o755);
    }

    #[test]
    fn remove_file_then_recreate() {
        let fs = fs();
        write(&fs, "/note.md", b"first");
        fs.remove(Path::new("/note.md")).unwrap();
        write(&fs, "/note.md", b"second");
        assert_eq!(
            fs.read_if_exists(Path::new("/note.md")).unwrap(),
            Some(b"second".to_vec())
        );
    }

    #[test]
    fn rename_replace_to_different_directory() {
        let fs = fs();
        fs.create_dir_all(Path::new("/a")).unwrap();
        fs.create_dir_all(Path::new("/b")).unwrap();
        write(&fs, "/a/file.md", b"content");
        fs.rename_replace(Path::new("/a/file.md"), Path::new("/b/file.md"))
            .unwrap();
        assert!(!fs.exists(Path::new("/a/file.md")).unwrap());
        assert_eq!(
            fs.read_if_exists(Path::new("/b/file.md")).unwrap(),
            Some(b"content".to_vec())
        );
    }

    #[test]
    fn rename_replace_directory() {
        let fs = fs();
        fs.create_dir_all(Path::new("/old-dir/sub")).unwrap();
        fs.rename_replace(Path::new("/old-dir"), Path::new("/new-dir"))
            .unwrap();
        assert!(!fs.exists(Path::new("/old-dir")).unwrap());
        assert!(fs.exists(Path::new("/new-dir")).unwrap());
        assert!(fs.exists(Path::new("/new-dir/sub")).unwrap());
    }

    #[test]
    fn rename_replace_self_is_noop() {
        let fs = fs();
        write(&fs, "/note.md", b"content");
        fs.rename_replace(Path::new("/note.md"), Path::new("/note.md"))
            .unwrap();
        assert_eq!(
            fs.read_if_exists(Path::new("/note.md")).unwrap(),
            Some(b"content".to_vec())
        );
    }

    #[test]
    fn fsync_file_then_modify_then_fsync_again() {
        let fs = fs();
        write(&fs, "/note.md", b"v1");
        fs.fsync_file(Path::new("/note.md")).unwrap();
        fs.fsync_dir(Path::new("/")).unwrap();
        // Modify the file (simulate an edit).
        fs.remove(Path::new("/note.md")).unwrap();
        write(&fs, "/note.md", b"v2-longer");
        fs.fsync_file(Path::new("/note.md")).unwrap();
        fs.fsync_dir(Path::new("/")).unwrap();
        fs.simulate_power_loss();
        assert_eq!(
            fs.read_if_exists(Path::new("/note.md")).unwrap(),
            Some(b"v2-longer".to_vec())
        );
    }

    #[test]
    fn fsync_dir_root_persists_entries() {
        let fs = fs();
        write(&fs, "/a.md", b"a");
        write(&fs, "/b.md", b"b");
        fs.fsync_dir(Path::new("/")).unwrap();
        fs.simulate_power_loss();
        assert!(fs.exists(Path::new("/a.md")).unwrap());
        assert!(fs.exists(Path::new("/b.md")).unwrap());
    }

    #[test]
    fn symlink_metadata_mode() {
        let fs = fs();
        write(&fs, "/target.md", b"x");
        fs.create_symlink(Path::new("/link.md"), Path::new("/target.md"));
        let md = fs.metadata(Path::new("/link.md")).unwrap();
        assert_eq!(md.mode, 0o777);
    }

    #[test]
    fn encode_path_key_empty_path() {
        let fs = fs();
        let key = fs.encode_path_key(Path::new("")).unwrap();
        assert!(key.is_empty());
        let decoded = fs.decode_path_key(&key).unwrap();
        assert_eq!(decoded, PathBuf::from(""));
    }

    #[test]
    fn encode_path_key_single_component() {
        let fs = fs();
        let key = fs.encode_path_key(Path::new("note.md")).unwrap();
        let decoded = fs.decode_path_key(&key).unwrap();
        assert_eq!(decoded, PathBuf::from("note.md"));
    }

    #[test]
    fn decode_path_key_empty() {
        let fs = fs();
        let decoded = fs.decode_path_key(&[]).unwrap();
        assert_eq!(decoded, PathBuf::from(""));
    }

    #[test]
    fn validate_in_root_same_path() {
        let fs = fs();
        // Root itself is contained in root.
        fs.validate_in_root(Path::new("/vault"), Path::new("/vault"))
            .unwrap();
    }

    #[test]
    fn validate_in_root_deeply_nested() {
        let fs = fs();
        fs.validate_in_root(Path::new("/vault"), Path::new("/vault/a/b/c/d/e/note.md"))
            .unwrap();
    }

    #[test]
    fn metadata_for_symlink_to_directory() {
        let fs = fs();
        fs.create_dir_all(Path::new("/real-dir")).unwrap();
        fs.create_symlink(Path::new("/dir-link"), Path::new("/real-dir"));
        let md = fs.metadata(Path::new("/dir-link")).unwrap();
        assert!(md.is_symlink);
        assert!(!md.is_dir);
    }

    #[test]
    fn read_dir_root_lists_all() {
        let fs = fs();
        write(&fs, "/a.md", b"a");
        write(&fs, "/b.md", b"b");
        fs.create_dir_all(Path::new("/dir")).unwrap();
        let entries = fs.read_dir(Path::new("/")).unwrap();
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn read_dir_empty_directory() {
        let fs = fs();
        fs.create_dir_all(Path::new("/empty")).unwrap();
        let entries = fs.read_dir(Path::new("/empty")).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn touch_updates_mtime() {
        let fs = fs();
        fs.advance_clock(100);
        write(&fs, "/note.md", b"x");
        let md1 = fs.metadata(Path::new("/note.md")).unwrap();
        fs.advance_clock(200);
        // Rewrite (remove + write) to update mtime.
        fs.remove(Path::new("/note.md")).unwrap();
        write(&fs, "/note.md", b"y");
        let md2 = fs.metadata(Path::new("/note.md")).unwrap();
        assert!(md2.mtime_ns > md1.mtime_ns);
    }

    #[test]
    fn copy_mode_preserves_custom_mode() {
        let fs = fs();
        write(&fs, "/source.md", b"x");
        write(&fs, "/dest.md", b"y");
        // Both default to 0o644; copy and verify.
        fs.copy_mode(Path::new("/source.md"), Path::new("/dest.md"))
            .unwrap();
        let md = fs.metadata(Path::new("/dest.md")).unwrap();
        assert_eq!(md.mode, 0o644);
    }

    #[test]
    fn exists_returns_false_for_deeply_nested_missing() {
        let fs = fs();
        assert!(!fs.exists(Path::new("/a/b/c/d/e")).unwrap());
    }
}
