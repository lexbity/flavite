//! Durable roll-forward executor for single-file D9 renames.

use crate::journal::manifest::{
    JournalManifest, TransactionItem, TransactionItemKind, TransactionItemState, TransactionPhase,
    TransactionState,
};
use flavite_fs::Fs;
use std::path::{Component, Path, PathBuf};
use tracing::warn;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Transactions rolled forward during this recovery pass.
    pub rolled_forward: Vec<String>,
    /// Transactions already terminal when recovery began.
    pub done: Vec<String>,
    /// Divergent transactions preserved for user action.
    pub skipped: Vec<(String, String)>,
    /// Invalid manifests moved to quarantine.
    pub quarantined: Vec<String>,
}

impl RecoveryReport {
    /// Codes to surface through the Session's ackable notice channel.
    pub fn notice_codes(&self) -> Vec<&'static str> {
        let mut codes = Vec::new();
        if !self.skipped.is_empty() {
            codes.push("rename-diverged");
        }
        if !self.quarantined.is_empty() {
            codes.push("journal-quarantined");
        }
        codes
    }
}

/// A byte-preserving replacement planned from an indexed link span set.
#[derive(Debug, Clone)]
pub struct FilePatch {
    pub path: PathBuf,
    /// Existing source used for the precondition when `path` is created by
    /// the rename item earlier in this transaction (Alias items).
    pub base_path: Option<PathBuf>,
    pub kind: TransactionItemKind,
    pub expected_base_hash: [u8; 32],
    pub bytes: Vec<u8>,
    pub buffer_id: Option<u64>,
    pub buffer_revision: Option<u64>,
    pub old_stem: Option<String>,
    pub new_stem: Option<String>,
    pub target_key: Option<String>,
    pub recovery_key: Option<Vec<u8>>,
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
        self.rename_with_patches_and_buffers(from, to, patches, |_, _| Ok(()))
    }

    /// Execute a rename while awaiting correlated Session replies for open
    /// source buffers. Buffer edits are durable journal states before disk
    /// application begins, so replay can safely roll forward after a crash.
    pub fn rename_with_patches_and_buffers<F>(
        &self,
        from: &Path,
        to: &Path,
        patches: &[FilePatch],
        mut apply_buffer: F,
    ) -> Result<String, String>
    where
        F: FnMut(&str, &TransactionItem) -> Result<(), String>,
    {
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
        manifest.native_paths = vec![from.clone(), to.clone()];
        manifest.items.push(TransactionItem {
            id: 1,
            kind: TransactionItemKind::Rename,
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
            old_stem: None,
            new_stem: None,
            target_key: None,
            indexed_hash: None,
            recovery_key: None,
            error: None,
        });
        let mut payloads = vec![(1u64, bytes)];
        for patch in patches {
            let path = self.safe_vault_path(&patch.path)?;
            let base_path = patch
                .base_path
                .as_ref()
                .map(|path| self.safe_vault_path(path))
                .transpose()?
                .unwrap_or_else(|| path.clone());
            let current = self
                .fs
                .read_if_exists(&base_path)
                .map_err(display)?
                .ok_or_else(|| format!("patch source disappeared: {}", base_path.display()))?;
            if *blake3::hash(&current).as_bytes() != patch.expected_base_hash {
                return Err(format!(
                    "patch source changed during planning: {}",
                    path.display()
                ));
            }
            let id = manifest.items.len() as u64 + 1;
            let new_hash = *blake3::hash(&patch.bytes).as_bytes();
            manifest.native_paths.push(path.clone());
            manifest.items.push(TransactionItem {
                id,
                kind: patch.kind,
                from_path: path.strip_prefix(&self.root).unwrap().to_path_buf(),
                to_path: path.strip_prefix(&self.root).unwrap().to_path_buf(),
                phase: TransactionPhase::Stage,
                state: TransactionItemState::Staged,
                expected_base_hash: Some(patch.expected_base_hash),
                expected_new_hash: Some(new_hash),
                observed_base_hash: Some(patch.expected_base_hash),
                observed_new_hash: None,
                buffer_id: patch.buffer_id.map(Into::into),
                buffer_revision: patch.buffer_revision,
                checkpoint_seq: None,
                operation_markers: Vec::new(),
                old_stem: patch.old_stem.clone(),
                new_stem: patch.new_stem.clone(),
                target_key: patch.target_key.clone(),
                indexed_hash: Some(patch.expected_base_hash),
                recovery_key: patch.recovery_key.clone(),
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
        for index in 1..manifest.items.len() {
            if manifest.items[index].buffer_id.is_none() {
                continue;
            }
            manifest.items[index].phase = TransactionPhase::WaitBuffers;
            self.persist(&manifest)?;
            match apply_buffer(&txn_id, &manifest.items[index]) {
                Ok(()) => manifest.items[index].state = TransactionItemState::BufferApplied,
                Err(error)
                    if error == "conflicted"
                        || error == "link-absent"
                        || error.starts_with("StaleRevision") =>
                {
                    manifest.items[index].state = TransactionItemState::Skipped;
                    manifest.items[index].error = Some(error);
                }
                Err(error) => {
                    // Transport loss is not a content decision. Leave the item
                    // committed for R7 recovery instead of making it terminal.
                    self.persist(&manifest)?;
                    return Err(error);
                }
            }
            self.persist(&manifest)?;
        }
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

    /// Replay committed journals. Invalid manifests are quarantined and
    /// divergent items become durable Partial transactions.
    pub fn recover(&self) -> Result<RecoveryReport, String> {
        self.recover_with_buffer_base(|_| None)
    }

    /// Replay using durable buffer-base hashes for rename recovery (R7).
    pub fn recover_with_buffer_base<F>(
        &self,
        mut recovery_base_hash: F,
    ) -> Result<RecoveryReport, String>
    where
        F: FnMut(&[u8]) -> Option<[u8; 32]>,
    {
        self.fs.create_dir_all(&self.journal_dir).map_err(display)?;
        let mut report = RecoveryReport::default();
        for entry in self.fs.read_dir(&self.journal_dir).map_err(display)? {
            if entry.is_dir || entry.path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let bytes = self.fs.read_if_exists(&entry.path).map_err(display)?;
            let parsed = bytes
                .as_deref()
                .map(serde_json::from_slice::<JournalManifest>);
            let parse_error = parsed
                .as_ref()
                .and_then(|result| result.as_ref().err())
                .map(ToString::to_string);
            let manifest = parsed.and_then(Result::ok);
            let supported = manifest.as_ref().is_some_and(|m| {
                m.version == crate::journal::manifest::MANIFEST_VERSION
                    && valid_txn_id(&m.txn_id)
                    && entry.path.file_stem().and_then(|stem| stem.to_str())
                        == Some(m.txn_id.as_str())
            });
            let Some(mut manifest) = manifest.filter(|_| supported) else {
                let quarantined = entry.path.with_extension("json.quarantined");
                self.fs
                    .rename_replace(&entry.path, &quarantined)
                    .map_err(display)?;
                self.fs.fsync_dir(&self.journal_dir).map_err(display)?;
                let name = quarantined
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                warn!(counter = "journal.recoveries_total", manifest = %entry.path.display(), quarantined = %quarantined.display(), ?parse_error, outcome = "quarantined", "journal manifest quarantined");
                report.quarantined.push(name);
                continue;
            };
            if manifest.state == TransactionState::Complete
                || manifest.state == TransactionState::Abandoned
            {
                report.done.push(manifest.txn_id.clone());
                tracing::info!(counter = "journal.recoveries_total", txn = %manifest.txn_id, outcome = "done", "journal already terminal");
                continue;
            }
            if manifest.state == TransactionState::Partial {
                let reasons = skipped_item_summaries(&manifest);
                report.skipped.push((manifest.txn_id.clone(), reasons));
                tracing::warn!(counter = "journal.recoveries_total", txn = %manifest.txn_id, outcome = "skipped", "partial journal awaits user action");
                continue;
            }
            if manifest.state == TransactionState::Preparing {
                // Preparing has not crossed the commit point, so its payloads
                // are disposable. Do not let a missing/corrupt data directory
                // prevent the vault from opening; leaving its unreferenced
                // files is safer than traversing a path from this manifest.
                let _ = self.fs.remove(&entry.path);
                self.fs.fsync_dir(&self.journal_dir).map_err(display)?;
                report.done.push(manifest.txn_id.clone());
                tracing::info!(counter = "journal.recoveries_total", txn = %manifest.txn_id, outcome = "done", "discarded uncommitted journal");
                continue;
            }
            for item in &mut manifest.items {
                if item.kind == TransactionItemKind::Patch
                    && item.buffer_id.is_some()
                    && (item.state == TransactionItemState::BufferApplied
                        || item.phase == TransactionPhase::WaitBuffers)
                {
                    let recovered_base = item
                        .recovery_key
                        .as_deref()
                        .and_then(&mut recovery_base_hash);
                    if recovered_base == item.expected_new_hash {
                        item.state = TransactionItemState::BufferApplied;
                    } else if item.state == TransactionItemState::BufferApplied {
                        item.state = TransactionItemState::Skipped;
                        item.error = Some("buffer-authoritative divergence".into());
                    }
                }
            }
            for index in 0..manifest.items.len() {
                self.apply_item(&mut manifest, index)?;
            }
            if manifest
                .items
                .iter()
                .any(|item| item.state == TransactionItemState::Skipped)
            {
                let reasons = skipped_item_summaries(&manifest);
                manifest.state = TransactionState::Partial;
                self.persist(&manifest)?;
                report.skipped.push((manifest.txn_id.clone(), reasons));
                warn!(counter = "journal.recoveries_total", txn = %manifest.txn_id, outcome = "skipped", "rename journal diverged");
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
                report.rolled_forward.push(manifest.txn_id.clone());
                self.cleanup(&manifest)?;
                tracing::info!(counter = "journal.recoveries_total", txn = %manifest.txn_id, outcome = "rolled_forward", "rename journal recovered");
            } else {
                manifest.state = TransactionState::Partial;
                for item in &mut manifest.items {
                    if !matches!(
                        item.state,
                        TransactionItemState::Saved | TransactionItemState::Cancelled
                    ) {
                        item.state = TransactionItemState::Skipped;
                        item.error
                            .get_or_insert_with(|| "recovery left item pending".into());
                    }
                }
                self.persist(&manifest)?;
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
        if !valid_txn_id(txn_id) {
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
        if manifest.txn_id != txn_id {
            return Err("journal transaction id does not match its manifest path".into());
        }
        let item = manifest
            .items
            .iter_mut()
            .find(|item| item.id == item_id)
            .ok_or("journal item not found")?;
        if item.state == TransactionItemState::Saved
            && !(item.kind == TransactionItemKind::Patch && item.buffer_id.is_some())
        {
            return Err("applied disk item cannot be cancelled by buffer undo".into());
        }
        item.state = TransactionItemState::Cancelled;
        item.error = None;
        let staged = self.staged_payload_path(txn_id, item_id)?;
        if self.fs.exists(&staged).map_err(display)? {
            self.fs.remove(&staged).map_err(display)?;
        }
        if let Some(parent) = staged.parent() {
            self.fs.fsync_dir(parent).map_err(display)?;
        }
        self.persist(&manifest)
    }

    fn apply_item(&self, manifest: &mut JournalManifest, item_index: usize) -> Result<(), String> {
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
                let staged = self.staged_payload_path(&manifest.txn_id, item_id)?;
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
                    ".flavite-tmp.{}.{}.{}",
                    manifest.txn_id, item.id, name
                ));
                self.validate_staged_temp(&temp)?;
                // A staged replacement is disposable. Recreate and verify it
                // from the journal payload; a short or partial write gets one
                // fresh attempt before it can affect the target.
                let mut staged = false;
                for _ in 0..2 {
                    if self.fs.exists(&temp).unwrap_or(false) {
                        let _ = self.fs.remove(&temp);
                    }
                    self.fs.write_new(&temp, &payload).map_err(display)?;
                    self.fs.copy_mode(&from, &temp).map_err(display)?;
                    self.fs.fsync_file(&temp).map_err(display)?;
                    if self.hash_if_exists(&temp)? == Some(new) {
                        staged = true;
                        break;
                    }
                    let _ = self.fs.remove(&temp);
                }
                if !staged {
                    return Err("journal temp failed hash verification after re-stage".into());
                }
                self.fs.rename_replace(&temp, &from).map_err(display)?;
                self.fs.fsync_file(&from).map_err(display)?;
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
            self.fs.fsync_file(&to).map_err(display)?;
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
        self.fs.fsync_file(&path).map_err(display)?;
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
        if manifest.is_terminal() && manifest.items.iter().all(|item| item.buffer_id.is_none()) {
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
        if self.fs.exists(&full).map_err(display)? {
            self.fs
                .validate_in_root(&self.root, &full)
                .map_err(display)?;
        } else {
            let parent = full.parent().ok_or("journal path has no parent")?;
            self.fs
                .validate_in_root(&self.root, parent)
                .map_err(display)?;
        }
        Ok(full)
    }

    /// Validate that a deterministic replacement temp is a vault-local
    /// sibling in the reserved `.flavite-tmp` filename namespace.
    fn validate_staged_temp(&self, path: &Path) -> Result<(), String> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("invalid journal temp filename")?;
        if !name.starts_with(".flavite-tmp.") {
            return Err("journal temp is outside the .flavite-tmp namespace".into());
        }
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| "journal temp escapes vault")?;
        self.safe_vault_path(relative).map(|_| ())
    }

    /// Resolve and validate a journal-owned payload path before reading or
    /// removing it. The transaction id must remain a single safe component.
    fn staged_payload_path(&self, txn_id: &str, item_id: u64) -> Result<PathBuf, String> {
        if !valid_txn_id(txn_id) {
            return Err("invalid transaction id in journal payload path".into());
        }
        let path = self
            .journal_dir
            .join(format!("{txn_id}.data"))
            .join(format!("{item_id}.bin"));
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| "journal payload path escapes vault")?;
        let path = self.safe_vault_path(relative)?;
        if !path.starts_with(&self.journal_dir) {
            return Err("journal payload path escapes journal directory".into());
        }
        Ok(path)
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
fn valid_txn_id(txn_id: &str) -> bool {
    !txn_id.is_empty()
        && txn_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}
fn skipped_item_summaries(manifest: &JournalManifest) -> String {
    manifest
        .items
        .iter()
        .filter(|item| item.state == TransactionItemState::Skipped)
        .map(|item| {
            format!(
                "item {} ({} -> {}): {}",
                item.id,
                item.from_path.display(),
                item.to_path.display(),
                item.error.as_deref().unwrap_or("diverged")
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
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
                    base_path: None,
                    kind: TransactionItemKind::Patch,
                    expected_base_hash: *blake3::hash(b"See [[a]].").as_bytes(),
                    bytes: b"See [[b]].".to_vec(),
                    buffer_id: None,
                    buffer_revision: None,
                    old_stem: None,
                    new_stem: None,
                    target_key: None,
                    recovery_key: None,
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
    fn wait_buffers_persists_correlated_item_before_disk_apply() {
        let fs = MemFs::new();
        fs.create_dir_all(Path::new("/vault")).unwrap();
        fs.write_new(Path::new("/vault/a.md"), b"a").unwrap();
        fs.write_new(Path::new("/vault/in.md"), b"[[a]]").unwrap();
        let patch = FilePatch {
            path: PathBuf::from("in.md"),
            base_path: None,
            kind: TransactionItemKind::Patch,
            expected_base_hash: *blake3::hash(b"[[a]]").as_bytes(),
            bytes: b"[[b]]".to_vec(),
            buffer_id: Some(47),
            buffer_revision: Some(12),
            old_stem: Some("a".into()),
            new_stem: Some("b".into()),
            target_key: Some("a".into()),
            recovery_key: Some(b"in.md".to_vec()),
        };
        let executor = JournalExecutor::new(&fs, Path::new("/vault"));
        let seen = std::cell::Cell::new(false);
        let txn = executor
            .rename_with_patches_and_buffers(
                Path::new("a.md"),
                Path::new("b.md"),
                &[patch],
                |txn, item| {
                    assert!(valid_txn_id(txn));
                    assert_eq!(item.phase, TransactionPhase::WaitBuffers);
                    assert_eq!(item.buffer_revision, Some(12));
                    seen.set(true);
                    Ok(())
                },
            )
            .unwrap();
        assert!(seen.get());
        let bytes = fs
            .read_if_exists(&Path::new("/vault/.flavite/journal").join(format!("{txn}.json")))
            .unwrap()
            .unwrap();
        let manifest: JournalManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(manifest.items[1].state, TransactionItemState::Saved);
        assert_eq!(manifest.items[1].buffer_id.unwrap().as_u64(), 47);
        assert_eq!(
            fs.read_if_exists(Path::new("/vault/in.md"))
                .unwrap()
                .unwrap(),
            b"[[b]]"
        );
    }

    #[test]
    fn buffer_applied_recovery_uses_the_durable_recovery_base_hash() {
        let fs = MemFs::new();
        let journal = Path::new("/vault/.flavite/journal");
        fs.create_dir_all(journal).unwrap();
        fs.create_dir_all(&journal.join("r7-match.data")).unwrap();
        fs.create_dir_all(&journal.join("r7-mismatch.data"))
            .unwrap();
        fs.write_new(Path::new("/vault/match.md"), b"base").unwrap();
        fs.write_new(Path::new("/vault/mismatch.md"), b"base")
            .unwrap();
        let base = *blake3::hash(b"base").as_bytes();
        let new = *blake3::hash(b"patched").as_bytes();
        let make_manifest = |txn: &str, path: &str| {
            let mut manifest = JournalManifest::new(txn.into(), 1);
            manifest.state = TransactionState::Committed;
            manifest.items.push(TransactionItem {
                id: 1,
                kind: TransactionItemKind::Patch,
                from_path: path.into(),
                to_path: path.into(),
                phase: TransactionPhase::WaitBuffers,
                state: TransactionItemState::BufferApplied,
                expected_base_hash: Some(base),
                expected_new_hash: Some(new),
                observed_base_hash: None,
                observed_new_hash: None,
                buffer_id: Some(flavite_buffer::revision::BufferId::new()),
                buffer_revision: Some(3),
                checkpoint_seq: None,
                operation_markers: Vec::new(),
                old_stem: Some("old".into()),
                new_stem: Some("new".into()),
                target_key: Some("old".into()),
                indexed_hash: Some(base),
                recovery_key: Some(path.as_bytes().to_vec()),
                error: None,
            });
            manifest
        };
        let matched = make_manifest("r7-match", "match.md");
        fs.write_new(&journal.join("r7-match.data/1.bin"), b"patched")
            .unwrap();
        JournalExecutor::new(&fs, Path::new("/vault"))
            .persist(&matched)
            .unwrap();
        let mismatched = make_manifest("r7-mismatch", "mismatch.md");
        fs.write_new(&journal.join("r7-mismatch.data/1.bin"), b"patched")
            .unwrap();
        JournalExecutor::new(&fs, Path::new("/vault"))
            .persist(&mismatched)
            .unwrap();

        let report = JournalExecutor::new(&fs, Path::new("/vault"))
            .recover_with_buffer_base(|key| (key == b"match.md").then_some(new))
            .unwrap();
        assert!(report.rolled_forward.contains(&"r7-match".to_owned()));
        assert!(report.skipped.iter().any(|(txn, reason)| {
            txn == "r7-mismatch" && reason.contains("buffer-authoritative divergence")
        }));
        assert_eq!(
            fs.read_if_exists(Path::new("/vault/match.md")).unwrap(),
            Some(b"patched".to_vec())
        );
        assert_eq!(
            fs.read_if_exists(Path::new("/vault/mismatch.md")).unwrap(),
            Some(b"base".to_vec())
        );
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
                kind: TransactionItemKind::Rename,
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
                old_stem: None,
                new_stem: None,
                target_key: None,
                indexed_hash: None,
                recovery_key: None,
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
        assert!(report.rolled_forward.contains(&"tx-base".to_owned()));
        assert!(report.rolled_forward.contains(&"tx-new".to_owned()));
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
            kind: TransactionItemKind::Patch,
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
            old_stem: None,
            new_stem: None,
            target_key: None,
            indexed_hash: None,
            recovery_key: None,
            error: None,
        });
        let executor = JournalExecutor::new(&fs, Path::new("/vault"));
        executor.persist(&manifest).unwrap();
        executor.cancel_item("undo-tx", 4).unwrap();
        assert!(!fs.exists(&journal.join("undo-tx.data/4.bin")).unwrap());
        let report = executor.recover().unwrap();
        assert!(report.rolled_forward.contains(&"undo-tx".to_owned()));
        assert_eq!(
            fs.read_if_exists(Path::new("/vault/note.md")).unwrap(),
            Some(b"undone".to_vec())
        );
    }

    fn pending_patch_manifest(txn: &str) -> JournalManifest {
        let mut manifest = JournalManifest::new(txn.into(), 1);
        manifest.state = TransactionState::Committed;
        manifest.items.push(TransactionItem {
            id: 1,
            kind: TransactionItemKind::Patch,
            from_path: "note.md".into(),
            to_path: "note.md".into(),
            phase: TransactionPhase::Apply,
            state: TransactionItemState::Committed,
            expected_base_hash: Some(*blake3::hash(b"base").as_bytes()),
            expected_new_hash: Some(*blake3::hash(b"patched").as_bytes()),
            observed_base_hash: None,
            observed_new_hash: None,
            buffer_id: None,
            buffer_revision: None,
            checkpoint_seq: None,
            operation_markers: Vec::new(),
            old_stem: None,
            new_stem: None,
            target_key: None,
            indexed_hash: None,
            recovery_key: None,
            error: None,
        });
        manifest
    }

    #[test]
    fn recovery_replaces_missing_partial_and_preexisting_temp_files() {
        for with_partial_temp in [false, true] {
            let fs = MemFs::new();
            let journal = Path::new("/vault/.flavite/journal");
            fs.create_dir_all(journal).unwrap();
            fs.create_dir_all(&journal.join("patch-tx.data")).unwrap();
            fs.fsync_dir(Path::new("/vault")).unwrap();
            fs.fsync_dir(Path::new("/vault/.flavite")).unwrap();
            fs.fsync_dir(journal).unwrap();
            fs.write_new(Path::new("/vault/note.md"), b"base").unwrap();
            fs.write_new(&journal.join("patch-tx.data/1.bin"), b"patched")
                .unwrap();
            let executor = JournalExecutor::new(&fs, Path::new("/vault"));
            executor
                .persist(&pending_patch_manifest("patch-tx"))
                .unwrap();

            let temp = Path::new("/vault/.flavite-tmp.patch-tx.1.note.md");
            if with_partial_temp {
                fs.write_new(temp, b"partial garbage").unwrap();
            }
            let report = executor.recover().unwrap();

            assert_eq!(
                fs.read_if_exists(Path::new("/vault/note.md")).unwrap(),
                Some(b"patched".to_vec())
            );
            assert!(!fs.exists(temp).unwrap());
            assert_eq!(report.rolled_forward, vec!["patch-tx"]);
        }
    }

    #[test]
    fn diverged_manifest_becomes_partial_and_is_reported() {
        let fs = MemFs::new();
        fs.create_dir_all(Path::new("/vault/.flavite/journal"))
            .unwrap();
        fs.create_dir_all(Path::new("/vault/.flavite/journal/diverged.data"))
            .unwrap();
        fs.write_new(Path::new("/vault/note.md"), b"user edit")
            .unwrap();
        let executor = JournalExecutor::new(&fs, Path::new("/vault"));
        executor
            .persist(&pending_patch_manifest("diverged"))
            .unwrap();

        let report = executor.recover().unwrap();
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.notice_codes(), vec!["rename-diverged"]);
        let bytes = fs
            .read_if_exists(Path::new("/vault/.flavite/journal/diverged.json"))
            .unwrap()
            .unwrap();
        let persisted: JournalManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(persisted.state, TransactionState::Partial);
        assert_eq!(persisted.items[0].state, TransactionItemState::Skipped);
    }

    #[test]
    fn corrupt_and_v1_manifests_are_quarantined_and_recovery_continues() {
        let fs = MemFs::new();
        let journal = Path::new("/vault/.flavite/journal");
        fs.create_dir_all(journal).unwrap();
        fs.write_new(&journal.join("broken.json"), b"not json")
            .unwrap();
        let mut old = JournalManifest::new("old-version".into(), 1);
        old.version = 1;
        fs.write_new(
            &journal.join("old-version.json"),
            &serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();

        let report = JournalExecutor::new(&fs, Path::new("/vault"))
            .recover()
            .unwrap();
        assert_eq!(report.quarantined.len(), 2);
        assert!(report.notice_codes().contains(&"journal-quarantined"));
        assert!(fs.exists(&journal.join("broken.json.quarantined")).unwrap());
        assert!(
            fs.exists(&journal.join("old-version.json.quarantined"))
                .unwrap()
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
            fs.fsync_dir(Path::new("/vault")).unwrap();
            fs.fsync_dir(Path::new("/vault/.flavite")).unwrap();
            fs.fsync_dir(Path::new("/vault/.flavite/journal")).unwrap();
            fs.write_new(Path::new("/vault/a.md"), b"# A").unwrap();
            fs.fsync_file(Path::new("/vault/a.md")).unwrap();
            for n in 0..14 {
                let path = PathBuf::from(format!("/vault/in-{n}.md"));
                fs.write_new(&path, b"[[a]]").unwrap();
                fs.fsync_file(&path).unwrap();
            }
            fs.fsync_dir(Path::new("/vault")).unwrap();
            fs
        }
        let make_patches = || {
            (0..14)
                .map(|n| FilePatch {
                    path: PathBuf::from(format!("in-{n}.md")),
                    base_path: None,
                    kind: TransactionItemKind::Patch,
                    expected_base_hash: *blake3::hash(b"[[a]]").as_bytes(),
                    bytes: b"[[b]]".to_vec(),
                    buffer_id: None,
                    buffer_revision: None,
                    old_stem: None,
                    new_stem: None,
                    target_key: None,
                    recovery_key: None,
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
            .filter(|entry| {
                entry.boundary == FaultBoundary::After || entry.boundary == FaultBoundary::Progress
            })
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
                    let recovery = JournalExecutor::new(&revived, Path::new("/vault"))
                        .recover()
                        .unwrap_or_else(|error| {
                            panic!("{method}[{occurrence}] {boundary:?} {mode:?}: {error}")
                        });
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
                            "patch {n}: {method}[{occurrence}] {boundary:?} {mode:?}; recovery={recovery:?}"
                        );
                    }
                }
            }
        }
    }

    #[cfg(feature = "fault")]
    #[test]
    fn wait_buffers_crash_after_durable_reply_recovers_r7_base() {
        use flavite_fs::{CrashMode, FaultBoundary, FaultFs, FaultKind, FaultScript};

        fn seed() -> MemFs {
            let fs = MemFs::new();
            fs.create_dir_all(Path::new("/vault/.flavite/journal"))
                .unwrap();
            fs.create_dir_all(Path::new("/vault/.flavite/recovery"))
                .unwrap();
            fs.fsync_dir(Path::new("/vault")).unwrap();
            fs.fsync_dir(Path::new("/vault/.flavite")).unwrap();
            fs.fsync_dir(Path::new("/vault/.flavite/journal")).unwrap();
            fs.fsync_dir(Path::new("/vault/.flavite/recovery")).unwrap();
            fs.write_new(Path::new("/vault/a.md"), b"# A").unwrap();
            fs.write_new(Path::new("/vault/in.md"), b"[[a]]").unwrap();
            fs.fsync_file(Path::new("/vault/a.md")).unwrap();
            fs.fsync_file(Path::new("/vault/in.md")).unwrap();
            fs.fsync_dir(Path::new("/vault")).unwrap();
            fs
        }
        let new_hash = *blake3::hash(b"[[b]]").as_bytes();
        let make_patch = || FilePatch {
            path: PathBuf::from("in.md"),
            base_path: None,
            kind: TransactionItemKind::Patch,
            expected_base_hash: *blake3::hash(b"[[a]]").as_bytes(),
            bytes: b"[[b]]".to_vec(),
            buffer_id: Some(7),
            buffer_revision: Some(3),
            old_stem: Some("a".into()),
            new_stem: Some("b".into()),
            target_key: Some("a".into()),
            recovery_key: Some(b"in.md".to_vec()),
        };

        // Session's correlated success means its recovery checkpoint is
        // already durable before the executor persists BufferApplied.
        let baseline = FaultFs::new_mem(seed(), FaultScript::new());
        let next_manifest_write = std::cell::Cell::new(0);
        JournalExecutor::new(&baseline, Path::new("/vault"))
            .rename_with_patches_and_buffers(
                Path::new("a.md"),
                Path::new("b.md"),
                &[make_patch()],
                |_, _| {
                    let recovery = Path::new("/vault/.flavite/recovery/in.md");
                    baseline.write_new(recovery, &new_hash).unwrap();
                    baseline.fsync_file(recovery).unwrap();
                    baseline.fsync_dir(recovery.parent().unwrap()).unwrap();
                    next_manifest_write.set(baseline.call_count("write_new") + 1);
                    Ok(())
                },
            )
            .unwrap();

        for boundary in [FaultBoundary::Before, FaultBoundary::After] {
            for mode in [CrashMode::ProcessCrash, CrashMode::PowerLoss] {
                let kind = match mode {
                    CrashMode::ProcessCrash => FaultKind::ProcessCrash,
                    CrashMode::PowerLoss => FaultKind::PowerLoss,
                };
                let fs = FaultFs::new_mem(
                    seed(),
                    FaultScript::new().fault_at(
                        "write_new",
                        next_manifest_write.get(),
                        boundary,
                        kind,
                    ),
                );
                let _ = JournalExecutor::new(&fs, Path::new("/vault"))
                    .rename_with_patches_and_buffers(
                        Path::new("a.md"),
                        Path::new("b.md"),
                        &[make_patch()],
                        |_, _| {
                            let recovery = Path::new("/vault/.flavite/recovery/in.md");
                            fs.write_new(recovery, &new_hash).unwrap();
                            fs.fsync_file(recovery).unwrap();
                            fs.fsync_dir(recovery.parent().unwrap()).unwrap();
                            Ok(())
                        },
                    );
                let revived = fs.revive();
                let recovery = JournalExecutor::new(&revived, Path::new("/vault"))
                    .recover_with_buffer_base(|key| {
                        if key == b"in.md" {
                            revived
                                .read_if_exists(Path::new("/vault/.flavite/recovery/in.md"))
                                .ok()
                                .flatten()
                                .and_then(|bytes| bytes.as_slice().try_into().ok())
                        } else {
                            None
                        }
                    })
                    .unwrap();
                assert!(
                    recovery.skipped.is_empty(),
                    "R7 recovery unexpectedly diverged: {recovery:?}"
                );
                assert_eq!(
                    revived.read_if_exists(Path::new("/vault/b.md")).unwrap(),
                    Some(b"# A".to_vec()),
                    "rename recovery at {boundary:?} {mode:?}: {recovery:?}"
                );
                assert_eq!(
                    revived.read_if_exists(Path::new("/vault/in.md")).unwrap(),
                    Some(b"[[b]]".to_vec()),
                    "buffer recovery at {boundary:?} {mode:?}: {recovery:?}"
                );
            }
        }
    }

    #[cfg(feature = "fault")]
    #[test]
    fn recovery_crash_schedule_converges_with_a_preexisting_temp() {
        use flavite_fs::{CrashMode, FaultBoundary, FaultFs, FaultKind, FaultScript};
        use std::collections::HashSet;

        fn seed() -> MemFs {
            let fs = MemFs::new();
            let journal = Path::new("/vault/.flavite/journal");
            fs.create_dir_all(journal).unwrap();
            fs.create_dir_all(&journal.join("patch-tx.data")).unwrap();
            fs.fsync_dir(Path::new("/vault")).unwrap();
            fs.fsync_dir(Path::new("/vault/.flavite")).unwrap();
            fs.fsync_dir(journal).unwrap();
            fs.write_new(Path::new("/vault/note.md"), b"base").unwrap();
            fs.write_new(&journal.join("patch-tx.data/1.bin"), b"patched")
                .unwrap();
            fs.fsync_file(&journal.join("patch-tx.data/1.bin")).unwrap();
            fs.fsync_dir(&journal.join("patch-tx.data")).unwrap();
            fs.write_new(
                Path::new("/vault/.flavite-tmp.patch-tx.1.note.md"),
                b"stale temp",
            )
            .unwrap();
            fs.fsync_file(Path::new("/vault/note.md")).unwrap();
            fs.fsync_file(Path::new("/vault/.flavite-tmp.patch-tx.1.note.md"))
                .unwrap();
            fs.fsync_dir(Path::new("/vault")).unwrap();
            JournalExecutor::new(&fs, Path::new("/vault"))
                .persist(&pending_patch_manifest("patch-tx"))
                .unwrap();
            fs
        }

        let baseline = FaultFs::new_mem(seed(), FaultScript::new());
        JournalExecutor::new(&baseline, Path::new("/vault"))
            .recover()
            .unwrap();
        let calls = baseline
            .trace()
            .into_iter()
            .filter(|entry| {
                entry.boundary == FaultBoundary::After || entry.boundary == FaultBoundary::Progress
            })
            .map(|entry| (entry.method, entry.occurrence))
            .collect::<HashSet<_>>();
        assert!(
            calls.iter().any(|(method, _)| *method == "remove"),
            "calls: {calls:?}"
        );
        assert!(
            calls.iter().any(|(method, _)| *method == "write_new"),
            "calls: {calls:?}"
        );

        let short_write = FaultFs::new_mem(
            seed(),
            FaultScript::new().fault_at(
                "write_new",
                1,
                FaultBoundary::Progress,
                FaultKind::ShortWrite,
            ),
        );
        JournalExecutor::new(&short_write, Path::new("/vault"))
            .recover()
            .unwrap();
        assert_eq!(
            short_write
                .read_if_exists(Path::new("/vault/note.md"))
                .unwrap(),
            Some(b"patched".to_vec())
        );

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
                    let _ = JournalExecutor::new(&fs, Path::new("/vault")).recover();
                    let revived = fs.revive();
                    JournalExecutor::new(&revived, Path::new("/vault"))
                        .recover()
                        .unwrap();
                    assert_eq!(
                        revived.read_if_exists(Path::new("/vault/note.md")).unwrap(),
                        Some(b"patched".to_vec()),
                        "recovery {method}[{occurrence}] {boundary:?} {mode:?}"
                    );
                }
            }
        }
    }
}
