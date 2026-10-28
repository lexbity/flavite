//! Durable roll-forward executor for single-file D9 renames.

use crate::journal::manifest::{
    JournalManifest, TransactionItem, TransactionItemState, TransactionPhase, TransactionState,
};
use flavite_fs::Fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub recovered: Vec<String>,
    pub skipped: Vec<(String, String)>,
}

/// A byte-preserving replacement planned from an indexed link span set.
#[derive(Debug, Clone)]
pub struct FilePatch {
    pub path: PathBuf,
    pub expected_base_hash: [u8; 32],
    pub bytes: Vec<u8>,
}

pub struct JournalExecutor<'a> {
    fs: &'a dyn Fs,
    root: PathBuf,
    journal_dir: PathBuf,
}

impl<'a> JournalExecutor<'a> {
    pub fn new(fs: &'a dyn Fs, root: &Path) -> Self {
        Self {
            fs,
            root: root.to_path_buf(),
            journal_dir: root.join(".flavite/journal"),
        }
    }

    /// Execute an atomic same-vault file rename using a durable roll-forward journal.
    pub fn rename(&self, from: &Path, to: &Path) -> Result<String, String> {
        self.rename_with_patches(from, to, &[])
    }

    /// Rename a file and roll-forward byte patches in the same D9 transaction.
    pub fn rename_with_patches(
        &self,
        from: &Path,
        to: &Path,
        patches: &[FilePatch],
    ) -> Result<String, String> {
        let from = self.safe_vault_path(from)?;
        let to = self.safe_vault_path(to)?;
        if from == to {
            return Err("source and destination are identical".into());
        }
        if self.fs.exists(&to).map_err(display)? {
            return Err("destination already exists".into());
        }
        let bytes = self
            .fs
            .read_if_exists(&from)
            .map_err(display)?
            .ok_or("source does not exist")?;
        let hash = *blake3::hash(&bytes).as_bytes();
        let txn_id = uuid::Uuid::now_v7().to_string();
        let data_dir = self.journal_dir.join(format!("{txn_id}.data"));
        self.fs.create_dir_all(&data_dir).map_err(display)?;
        let mut manifest = JournalManifest::new(txn_id.clone(), 1);
        manifest.state = TransactionState::Preparing;
        manifest.items.push(TransactionItem {
            id: 1,
            from_path: from.strip_prefix(&self.root).unwrap().to_path_buf(),
            to_path: to.strip_prefix(&self.root).unwrap().to_path_buf(),
            phase: TransactionPhase::Stage,
            state: TransactionItemState::Staged,
            expected_base_hash: Some(hash),
            expected_new_hash: Some(hash),
            observed_base_hash: None,
            observed_new_hash: None,
            buffer_id: None,
            buffer_revision: None,
            checkpoint_seq: None,
            operation_markers: Vec::new(),
            error: None,
        });
        let mut payloads = vec![(1u64, bytes)];
        for patch in patches {
            let path = self.safe_vault_path(&patch.path)?;
            let current = self
                .fs
                .read_if_exists(&path)
                .map_err(display)?
                .ok_or_else(|| format!("patch source disappeared: {}", path.display()))?;
            if *blake3::hash(&current).as_bytes() != patch.expected_base_hash {
                return Err(format!(
                    "patch source changed during planning: {}",
                    path.display()
                ));
            }
            let id = manifest.items.len() as u64 + 1;
            let new_hash = *blake3::hash(&patch.bytes).as_bytes();
            manifest.items.push(TransactionItem {
                id,
                from_path: path.strip_prefix(&self.root).unwrap().to_path_buf(),
                to_path: path.strip_prefix(&self.root).unwrap().to_path_buf(),
                phase: TransactionPhase::Stage,
                state: TransactionItemState::Staged,
                expected_base_hash: Some(patch.expected_base_hash),
                expected_new_hash: Some(new_hash),
                observed_base_hash: Some(patch.expected_base_hash),
                observed_new_hash: None,
                buffer_id: None,
                buffer_revision: None,
                checkpoint_seq: None,
                operation_markers: Vec::new(),
                error: None,
            });
            payloads.push((id, patch.bytes.clone()));
        }
        for item in &mut manifest.items {
            item.state = TransactionItemState::Prepared;
        }
        // Durable manifest is the point after which recovery knows which staged
        // payloads belong to this transaction.
        self.persist(&manifest)?;
        self.fs.create_dir_all(&data_dir).map_err(display)?;
        for (id, payload) in payloads {
            let staged = data_dir.join(format!("{id}.bin"));
            self.fs.write_new(&staged, &payload).map_err(display)?;
            self.fs.fsync_file(&staged).map_err(display)?;
        }
        self.fs.fsync_dir(&data_dir).map_err(display)?;
        for item in &mut manifest.items {
            item.state = TransactionItemState::Staged;
            item.phase = TransactionPhase::Stage;
        }
        self.persist(&manifest)?;
        manifest.state = TransactionState::Committed;
        manifest.committed_timestamp = Some(now_secs());
        for item in &mut manifest.items {
            item.phase = TransactionPhase::Apply;
            item.state = TransactionItemState::Committed;
        }
        self.persist(&manifest)?;
        for index in 0..manifest.items.len() {
            self.apply_item(&mut manifest, index)?;
        }
        if manifest
            .items
            .iter()
            .any(|item| item.state == TransactionItemState::Skipped)
        {
            self.persist(&manifest)?;
            return Err(format!(
                "rename transaction {txn_id} partially applied; recovery report required"
            ));
        }
        manifest.state = TransactionState::Complete;
        for item in &mut manifest.items {
            item.phase = TransactionPhase::Close;
        }
        self.persist(&manifest)?;
        self.cleanup(&manifest)?;
        Ok(txn_id)
    }

    /// Replay committed journals. A mismatch is preserved and reported, never guessed over.
    pub fn recover(&self) -> Result<RecoveryReport, String> {
        self.fs.create_dir_all(&self.journal_dir).map_err(display)?;
        let mut report = RecoveryReport::default();
        for entry in self.fs.read_dir(&self.journal_dir).map_err(display)? {
            if entry.is_dir || entry.path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let Some(bytes) = self.fs.read_if_exists(&entry.path).map_err(display)? else {
                continue;
            };
            let mut manifest: JournalManifest = serde_json::from_slice(&bytes)
                .map_err(|e| format!("{}: {e}", entry.path.display()))?;
            if manifest.version != crate::journal::manifest::MANIFEST_VERSION {
                return Err(format!(
                    "unsupported journal version in {}",
                    entry.path.display()
                ));
            }
            if manifest.state == TransactionState::Preparing {
                self.cleanup(&manifest)?;
                let _ = self.fs.remove(&entry.path);
                self.fs.fsync_dir(&self.journal_dir).map_err(display)?;
                continue;
            }
            if manifest.state == TransactionState::Complete
                || manifest.state == TransactionState::Abandoned
            {
                continue;
            }
            for index in 0..manifest.items.len() {
                self.apply_item(&mut manifest, index)?;
            }
            if manifest
                .items
                .iter()
                .any(|item| item.state == TransactionItemState::Skipped)
            {
                let reasons = manifest
                    .items
                    .iter()
                    .filter_map(|item| item.error.as_deref())
                    .collect::<Vec<_>>()
                    .join("; ");
                report.skipped.push((manifest.txn_id.clone(), reasons));
            } else if manifest.items.iter().all(|item| {
                matches!(
                    item.state,
                    TransactionItemState::Saved
                        | TransactionItemState::Skipped
                        | TransactionItemState::Cancelled
                )
            }) {
                manifest.state = TransactionState::Complete;
                self.persist(&manifest)?;
                report.recovered.push(manifest.txn_id.clone());
                self.cleanup(&manifest)?;
            } else {
                report.skipped.push((
                    manifest.txn_id.clone(),
                    "one or more items remain pending".into(),
                ));
            }
        }
        Ok(report)
    }

    /// Durably cancel an unapplied patch after its correlated buffer undo.
    pub fn cancel_item(&self, txn_id: &str, item_id: u64) -> Result<(), String> {
        if txn_id.is_empty()
            || !txn_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err("invalid transaction id".into());
        }
        let path = self.journal_dir.join(format!("{txn_id}.json"));
        let bytes = self
            .fs
            .read_if_exists(&path)
            .map_err(display)?
            .ok_or("journal manifest not found")?;
        let mut manifest: JournalManifest =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        let item = manifest
            .items
            .iter_mut()
            .find(|item| item.id == item_id)
            .ok_or("journal item not found")?;
        if item.state == TransactionItemState::Saved {
            return Err("applied disk item cannot be cancelled by buffer undo".into());
        }
        item.state = TransactionItemState::Cancelled;
        item.error = None;
        let staged = manifest
            .data_dir(&self.journal_dir)
            .join(format!("{item_id}.bin"));
        if self.fs.exists(&staged).map_err(display)? {
            self.fs.remove(&staged).map_err(display)?;
        }
        if let Some(parent) = staged.parent() {
            self.fs.fsync_dir(parent).map_err(display)?;
        }
        self.persist(&manifest)
    }

    fn apply_item(&self, manifest: &mut JournalManifest, item_index: usize) -> Result<(), String> {
        let staged_dir = manifest.data_dir(&self.journal_dir);
        let item_id = manifest.items[item_index].id;
        let item = &mut manifest.items[item_index];
        if matches!(
            item.state,
            TransactionItemState::Saved
                | TransactionItemState::Skipped
                | TransactionItemState::Cancelled
        ) {
            return Ok(());
        }
        let from = self.safe_vault_path(&item.from_path)?;
        let to = self.safe_vault_path(&item.to_path)?;
        let base = item
            .expected_base_hash
            .ok_or("journal item has no base hash")?;
        let new = item
            .expected_new_hash
            .ok_or("journal item has no new hash")?;
        let disk_to = self.hash_if_exists(&to)?;
        if disk_to == Some(new) {
            item.observed_new_hash = disk_to;
            item.state = TransactionItemState::Saved;
            self.persist(manifest)?;
            return Ok(());
        }
        let disk_from = self.hash_if_exists(&from)?;
        if disk_from == Some(base) && (from == to || disk_to.is_none()) {
            if from == to {
                let staged = staged_dir.join(format!("{item_id}.bin"));
                let payload = self
                    .fs
                    .read_if_exists(&staged)
                    .map_err(display)?
                    .ok_or("journal staged payload is missing")?;
                if *blake3::hash(&payload).as_bytes() != new {
                    return Err("journal staged payload hash mismatch".into());
                }
                let parent = from.parent().ok_or("patch path has no parent")?;
                let name = from
                    .file_name()
                    .ok_or("patch path has no filename")?
                    .to_string_lossy();
                let temp = parent.join(format!(
                    ".{name}.flavite-tmp.{}.{}",
                    manifest.txn_id, item.id
                ));
                self.fs.write_new(&temp, &payload).map_err(display)?;
                self.fs.copy_mode(&from, &temp).map_err(display)?;
                self.fs.fsync_file(&temp).map_err(display)?;
                self.fs.rename_replace(&temp, &from).map_err(display)?;
                self.fs.fsync_dir(parent).map_err(display)?;
                if self.hash_if_exists(&from)? != Some(new) {
                    return Err("patched payload failed hash verification".into());
                }
                item.observed_new_hash = Some(new);
                item.state = TransactionItemState::Saved;
                self.persist(manifest)?;
                return Ok(());
            }
            self.fs.rename_no_replace(&from, &to).map_err(display)?;
            if let Some(parent) = from.parent() {
                self.fs.fsync_dir(parent).map_err(display)?;
            }
            if let Some(parent) = to.parent() {
                self.fs.fsync_dir(parent).map_err(display)?;
            }
            if self.hash_if_exists(&to)? != Some(new) {
                return Err("renamed payload failed hash verification".into());
            }
            item.observed_new_hash = Some(new);
            item.state = TransactionItemState::Saved;
            self.persist(manifest)?;
            return Ok(());
        }
        item.state = TransactionItemState::Skipped;
        item.error = Some(format!(
            "disk diverged (source={disk_from:?}, destination={disk_to:?})"
        ));
        self.persist(manifest)
    }

    fn persist(&self, manifest: &JournalManifest) -> Result<(), String> {
        self.fs.create_dir_all(&self.journal_dir).map_err(display)?;
        let path = manifest.manifest_path(&self.journal_dir);
        let temp =
            self.journal_dir
                .join(format!(".{}.{}.tmp", manifest.txn_id, uuid::Uuid::now_v7()));
        let bytes = serde_json::to_vec(manifest).map_err(|e| e.to_string())?;
        self.fs.write_new(&temp, &bytes).map_err(display)?;
        self.fs.fsync_file(&temp).map_err(display)?;
        self.fs.rename_replace(&temp, &path).map_err(display)?;
        self.fs.fsync_dir(&self.journal_dir).map_err(display)
    }

    fn cleanup(&self, manifest: &JournalManifest) -> Result<(), String> {
        let data = manifest.data_dir(&self.journal_dir);
        if self.fs.exists(&data).unwrap_or(false) {
            for entry in self.fs.read_dir(&data).map_err(display)? {
                if !entry.is_dir {
                    let _ = self.fs.remove(&entry.path);
                }
            }
            // Fs has no remove-directory operation; empty transaction data dirs are harmless.
        }
        if manifest.is_terminal() {
            let path = manifest.manifest_path(&self.journal_dir);
            if self.fs.exists(&path).unwrap_or(false) {
                self.fs.remove(&path).map_err(display)?;
                self.fs.fsync_dir(&self.journal_dir).map_err(display)?;
            }
        }
        Ok(())
    }

    fn safe_vault_path(&self, path: &Path) -> Result<PathBuf, String> {
        if path.is_absolute()
            || path.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err("journal path escapes vault".into());
        }
        let full = self.root.join(path);
        self.fs
            .validate_in_root(&self.root, &full)
            .map_err(display)?;
        Ok(full)
    }

    fn hash_if_exists(&self, path: &Path) -> Result<Option<[u8; 32]>, String> {
        Ok(self
            .fs
            .read_if_exists(path)
            .map_err(display)?
            .map(|bytes| *blake3::hash(&bytes).as_bytes()))
    }
}

fn display(error: flavite_fs::FsError) -> String {
    error.to_string()
}
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::manifest::TransactionItem;
    use flavite_fs::{Fs, MemFs};

    #[test]
    fn rename_is_journaled_and_recovery_is_idempotent() {
        let fs = MemFs::new();
        fs.create_dir_all(Path::new("/vault")).unwrap();
        fs.write_new(Path::new("/vault/a.md"), b"# A").unwrap();
        let patches = (0..14)
            .map(|n| {
                let path = PathBuf::from(format!("inbound-{n}.md"));
                fs.write_new(&PathBuf::from("/vault").join(&path), b"See [[a]].")
                    .unwrap();
                FilePatch {
                    path,
                    expected_base_hash: *blake3::hash(b"See [[a]].").as_bytes(),
                    bytes: b"See [[b]].".to_vec(),
                }
            })
            .collect::<Vec<_>>();
        let executor = JournalExecutor::new(&fs, Path::new("/vault"));
        let txn = executor
            .rename_with_patches(Path::new("a.md"), Path::new("b.md"), &patches)
            .unwrap();
        assert_eq!(fs.read_if_exists(Path::new("/vault/a.md")).unwrap(), None);
        assert_eq!(
            fs.read_if_exists(Path::new("/vault/b.md")).unwrap(),
            Some(b"# A".to_vec())
        );
        for n in 0..14 {
            assert_eq!(
                fs.read_if_exists(&PathBuf::from(format!("/vault/inbound-{n}.md")))
                    .unwrap(),
                Some(b"See [[b]].".to_vec())
            );
        }
        assert!(!txn.is_empty());
        assert_eq!(executor.recover().unwrap(), RecoveryReport::default());
    }

    #[test]
    fn recovery_rolls_forward_base_skips_divergence_and_accepts_new() {
        let fs = MemFs::new();
        fs.create_dir_all(Path::new("/vault/.flavite/journal"))
            .unwrap();
        fs.create_dir_all(Path::new("/vault/.flavite/journal/tx-base.data"))
            .unwrap();
        fs.create_dir_all(Path::new("/vault/.flavite/journal/tx-new.data"))
            .unwrap();
        fs.create_dir_all(Path::new("/vault/.flavite/journal/tx-diverged.data"))
            .unwrap();
        fs.write_new(Path::new("/vault/base.md"), b"base").unwrap();
        fs.write_new(Path::new("/vault/new.md"), b"new").unwrap();
        fs.write_new(Path::new("/vault/diverged.md"), b"mine")
            .unwrap();
        let create = |txn: &str, from: &str, to: &str, current: &[u8]| {
            let base = *blake3::hash(current).as_bytes();
            let next = if txn == "tx-base" {
                base
            } else {
                *blake3::hash(b"next").as_bytes()
            };
            let mut m = JournalManifest::new(txn.into(), 1);
            m.state = TransactionState::Committed;
            m.items.push(TransactionItem {
                id: 1,
                from_path: from.into(),
                to_path: to.into(),
                phase: TransactionPhase::Apply,
                state: TransactionItemState::Committed,
                expected_base_hash: Some(base),
                expected_new_hash: Some(next),
                observed_base_hash: None,
                observed_new_hash: None,
                buffer_id: None,
                buffer_revision: None,
                checkpoint_seq: None,
                operation_markers: vec![],
                error: None,
            });
            m
        };
        let executor = JournalExecutor::new(&fs, Path::new("/vault"));
        for m in [
            create("tx-base", "base.md", "base-to.md", b"base"),
            create("tx-new", "new-source.md", "new.md", b"irrelevant"),
            create("tx-diverged", "diverged.md", "diverged-to.md", b"expected"),
        ] {
            executor.persist(&m).unwrap();
        }
        // The destination already contains the new hash for tx-new.
        fs.rename_replace(
            Path::new("/vault/new.md"),
            Path::new("/vault/.flavite/journal/tx-new.data/hold"),
        )
        .unwrap();
        fs.write_new(Path::new("/vault/new.md"), b"next").unwrap();
        // Make the diverged source differ from the recorded base.
        let report = executor.recover().unwrap();
        assert!(report.recovered.contains(&"tx-base".to_owned()));
        assert!(report.recovered.contains(&"tx-new".to_owned()));
        assert!(report.skipped.iter().any(|(txn, _)| txn == "tx-diverged"));
        assert_eq!(
            fs.read_if_exists(Path::new("/vault/base-to.md")).unwrap(),
            Some(b"base".to_vec())
        );
        assert_eq!(
            fs.read_if_exists(Path::new("/vault/diverged.md")).unwrap(),
            Some(b"mine".to_vec())
        );
    }

    #[test]
    fn undo_cancels_patch_before_recovery_can_replay_it() {
        let fs = MemFs::new();
        let journal = Path::new("/vault/.flavite/journal");
        fs.create_dir_all(journal).unwrap();
        fs.create_dir_all(&journal.join("undo-tx.data")).unwrap();
        fs.write_new(&journal.join("undo-tx.data/4.bin"), b"patched")
            .unwrap();
        fs.write_new(Path::new("/vault/note.md"), b"undone")
            .unwrap();
        let mut manifest = JournalManifest::new("undo-tx".into(), 1);
        manifest.state = TransactionState::Committed;
        manifest.items.push(TransactionItem {
            id: 4,
            from_path: "note.md".into(),
            to_path: "note.md".into(),
            phase: TransactionPhase::WaitBuffers,
            state: TransactionItemState::BufferApplied,
            expected_base_hash: Some(*blake3::hash(b"base").as_bytes()),
            expected_new_hash: Some(*blake3::hash(b"patched").as_bytes()),
            observed_base_hash: None,
            observed_new_hash: None,
            buffer_id: Some(flavite_buffer::revision::BufferId::new()),
            buffer_revision: Some(1),
            checkpoint_seq: Some(1),
            operation_markers: Vec::new(),
            error: None,
        });
        let executor = JournalExecutor::new(&fs, Path::new("/vault"));
        executor.persist(&manifest).unwrap();
        executor.cancel_item("undo-tx", 4).unwrap();
        assert!(!fs.exists(&journal.join("undo-tx.data/4.bin")).unwrap());
        let report = executor.recover().unwrap();
        assert!(report.recovered.contains(&"undo-tx".to_owned()));
        assert_eq!(
            fs.read_if_exists(Path::new("/vault/note.md")).unwrap(),
            Some(b"undone".to_vec())
        );
    }

    #[cfg(feature = "fault")]
    #[test]
    fn journal_crash_schedule_converges_at_every_observed_fs_call() {
        use flavite_fs::{CrashMode, FaultBoundary, FaultFs, FaultKind, FaultScript};
        use std::collections::HashSet;

        fn seed() -> MemFs {
            let fs = MemFs::new();
            fs.create_dir_all(Path::new("/vault/.flavite/journal"))
                .unwrap();
            fs.write_new(Path::new("/vault/a.md"), b"# A").unwrap();
            for n in 0..14 {
                fs.write_new(&PathBuf::from(format!("/vault/in-{n}.md")), b"[[a]]")
                    .unwrap();
            }
            fs
        }
        let make_patches = || {
            (0..14)
                .map(|n| FilePatch {
                    path: PathBuf::from(format!("in-{n}.md")),
                    expected_base_hash: *blake3::hash(b"[[a]]").as_bytes(),
                    bytes: b"[[b]]".to_vec(),
                })
                .collect::<Vec<_>>()
        };

        let baseline = FaultFs::new_mem(seed(), FaultScript::new());
        JournalExecutor::new(&baseline, Path::new("/vault"))
            .rename_with_patches(Path::new("a.md"), Path::new("b.md"), &make_patches())
            .unwrap();
        let calls = baseline
            .trace()
            .into_iter()
            .filter(|entry| entry.boundary == FaultBoundary::Before)
            .map(|entry| (entry.method, entry.occurrence))
            .collect::<HashSet<_>>();

        for (method, occurrence) in calls {
            for boundary in [FaultBoundary::Before, FaultBoundary::After] {
                for mode in [CrashMode::ProcessCrash, CrashMode::PowerLoss] {
                    let kind = match mode {
                        CrashMode::ProcessCrash => FaultKind::ProcessCrash,
                        CrashMode::PowerLoss => FaultKind::PowerLoss,
                    };
                    let fs = FaultFs::new_mem(
                        seed(),
                        FaultScript::new().fault_at(method, occurrence, boundary, kind),
                    );
                    let executor = JournalExecutor::new(&fs, Path::new("/vault"));
                    let _ = executor.rename_with_patches(
                        Path::new("a.md"),
                        Path::new("b.md"),
                        &make_patches(),
                    );
                    let revived = fs.revive();
                    JournalExecutor::new(&revived, Path::new("/vault"))
                        .recover()
                        .unwrap();
                    let renamed = revived.exists(Path::new("/vault/b.md")).unwrap();
                    assert_eq!(
                        revived.exists(Path::new("/vault/a.md")).unwrap(),
                        !renamed,
                        "{method}[{occurrence}] {boundary:?} {mode:?}"
                    );
                    for n in 0..14 {
                        let bytes = revived
                            .read_if_exists(&PathBuf::from(format!("/vault/in-{n}.md")))
                            .unwrap()
                            .unwrap();
                        assert_eq!(
                            bytes,
                            if renamed {
                                b"[[b]]".to_vec()
                            } else {
                                b"[[a]]".to_vec()
                            },
                            "{method}[{occurrence}] {boundary:?} {mode:?}"
                        );
                    }
                }
            }
        }
    }
}
