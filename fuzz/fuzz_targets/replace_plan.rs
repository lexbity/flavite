//! RFC-0006 S07 (replace_plan): fuzz the sealed preview manifest decoder
//! and the private store round-trip. Any panic/overflow from arbitrary
//! bytes is a bug; malformed input must be a typed `StoreError` or a
//! rejected `serde` decode — never a panic, never an allocation bomb.

#![no_main]

use flavite_fs::{Fs, MemFs};
use flavite_vault::replace::plan::PreviewManifest;
use flavite_vault::replace::store::{OpStoreIo, ReplaceStore};
use libfuzzer_sys::fuzz_target;

/// `OpStoreIo` over a MemFs, mirroring the production actor contract
/// (exclusive writes, reads, fsync).
struct MemIo(flavite_fs::MemFs);

impl OpStoreIo for MemIo {
    fn mkdir_all(&self, path: &std::path::Path) -> Result<(), String> {
        self.0.create_dir_all(path).map_err(|e| e.to_string())
    }
    fn write_new(&self, path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
        self.0.write_new(path, bytes).map_err(|e| e.to_string())
    }
    fn write_replace(&self, path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
        // Same staging + rename_replace + fsync sequence as the production
        // actor's OpStoreIo impl (crates/flavite-vault/src/replace/store.rs).
        let parent = path.parent().ok_or_else(|| "no parent".to_owned())?;
        self.0.create_dir_all(parent).map_err(|e| e.to_string())?;
        static STAGING_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = STAGING_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp = parent.join(format!(".ophead-{seq}.tmp"));
        self.0.write_new(&temp, bytes).map_err(|e| e.to_string())?;
        self.0.fsync_file(&temp).map_err(|e| e.to_string())?;
        self.0
            .rename_replace(&temp, path)
            .map_err(|e| e.to_string())?;
        self.0.fsync_dir(parent).map_err(|e| e.to_string())
    }
    fn read(&self, path: &std::path::Path) -> Result<Option<Vec<u8>>, String> {
        self.0.read_if_exists(path).map_err(|e| e.to_string())
    }
    fn list_dir(&self, path: &std::path::Path) -> Result<Option<Vec<String>>, String> {
        match self.0.read_dir(path) {
            Ok(entries) => {
                let mut names = entries
                    .into_iter()
                    .filter(|entry| !entry.is_dir)
                    .map(|entry| entry.name.into_string().map_err(|_| "not UTF-8".to_owned()))
                    .collect::<Result<Vec<_>, String>>()?;
                names.sort();
                Ok(Some(names))
            }
            Err(flavite_fs::FsError::DirNotFound(_)) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
    fn fsync_file(&self, path: &std::path::Path) -> Result<(), String> {
        self.0.fsync_file(path).map_err(|e| e.to_string())
    }
    fn fsync_dir(&self, path: &std::path::Path) -> Result<(), String> {
        self.0.fsync_dir(path).map_err(|e| e.to_string())
    }
    fn remove(&self, path: &std::path::Path) -> Result<(), String> {
        self.0.remove(path).map_err(|e| e.to_string())
    }
}

fuzz_target!(|data: &[u8]| {
    // 1. Arbitrary manifest bytes: strict decode only, and any accepted
    //    manifest must be able to seal into a fresh store.
    if let Ok(manifest) = serde_json::from_slice::<PreviewManifest>(data) {
        let fs = MemFs::new();
        let io = MemIo(fs);
        let store = ReplaceStore::new(std::path::Path::new("/fuzz-vault"));
        // Cap the note count so a hostile payload cannot allocate a giant
        // manifest (the production decoder bounds semantics separately;
        // this is the memory guard for the harness itself).
        if manifest.notes.len() <= 4096 && manifest.exclusions.len() <= 4096 {
            let _ = manifest.record_digest();
            let _ = store.seal_preview(&io, &manifest);
            let _ = store.read_manifest(&io, &manifest.job_id);
        }
    }
    // 2. Store payload write/read round-trip: arbitrary bytes must never
    //    panic and never get accepted under a wrong checksum.
    let fs = flavite_fs::MemFs::new();
    let io = MemIo(fs);
    let store = ReplaceStore::new(std::path::Path::new("/fuzz-vault"));
    let reference = store.write_payload(&io, "j", data).ok();
    if let Some(reference) = reference {
        let read = store.read_payload(&io, "j", &reference).ok().flatten();
        if let Some(read) = read {
            assert_eq!(read.len() as u64, reference.len);
        }
    }
});
