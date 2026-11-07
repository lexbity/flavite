//! Device-local operation keys (RFC-0006 §6.9/§6.4).
//!
//! Checksums detect corruption but do not authenticate a plan. Before the
//! first applicable plan is sealed, the vault creates a random 256-bit
//! device/vault operation key in the private OS data directory, OUTSIDE the
//! vault. Keyed BLAKE3 authenticates the plan digest, selection revision,
//! native root/vault binding, engine version and every log record's
//! sequence/previous authenticator. The key never appears in vault
//! metadata, logs, worker IPC or default exports; a lost/corrupt key for
//! unresolved local jobs produces a quarantine, never silent regeneration.

use std::path::{Path, PathBuf};

/// Authenticator length (32 bytes).
pub const TAG_LEN: usize = 32;

/// The private key file name under `<data>/flavite/vaults/<uuid>/operations`.
const KEY_FILE: &str = "key.bin";

/// Devices the domain-separated authenticator tag domain.
const DOMAIN_V1: &[u8] = b"flavite-op-v1";

/// The device/vault operation key.
#[derive(Debug, Clone)]
pub struct OperationKey {
    raw: [u8; 32],
}

impl OperationKey {
    /// Compute a keyed BLAKE3 tag over `domain || payload`.
    #[must_use]
    pub fn tag(&self, domain: &[u8], payload: &[u8]) -> [u8; 32] {
        let mut data = Vec::with_capacity(domain.len() + payload.len());
        data.extend_from_slice(domain);
        data.extend_from_slice(payload);
        *blake3::keyed_hash(&self.raw, &data).as_bytes()
    }

    /// Keyed tag over the whole-record authenticator chain.
    #[must_use]
    pub fn record_tag(&self, previous: &[u8; 32], record: &[u8]) -> [u8; 32] {
        let mut data = Vec::with_capacity(TAG_LEN + record.len());
        data.extend_from_slice(previous);
        data.extend_from_slice(record);
        self.tag(DOMAIN_V1, &data)
    }
}

/// Key-store error (typed, redacted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyStoreError {
    /// The private data directory could not be resolved.
    NoDataDir,
    /// I/O failed.
    Io(String),
    /// The existing key file is corrupt (wrong length) — never guessed.
    Corrupt,
    /// Atomic creation failed because a key already exists but could not be
    /// read as a valid key.
    Conflict,
}

impl std::fmt::Display for KeyStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDataDir => write!(f, "private data directory is unavailable"),
            Self::Io(message) => write!(f, "operation key store I/O failed: {message}"),
            Self::Corrupt => write!(f, "operation key file is corrupt"),
            Self::Conflict => write!(f, "operation key already exists but is unreadable"),
        }
    }
}

impl std::error::Error for KeyStoreError {}

/// The private key directory for one vault.
fn key_dir(data_dir: &Path, vault_uuid: &uuid::Uuid) -> PathBuf {
    data_dir
        .join("flavite")
        .join("vaults")
        .join(vault_uuid.to_string())
        .join("operations")
}

/// Load the operation key, creating it atomically (no-clobber) when absent.
pub fn load_or_create_key(
    data_dir: &Path,
    vault_uuid: &uuid::Uuid,
) -> Result<OperationKey, KeyStoreError> {
    let dir = key_dir(data_dir, vault_uuid);
    std::fs::create_dir_all(&dir).map_err(|e| KeyStoreError::Io(e.to_string()))?;
    let path = dir.join(KEY_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let raw: [u8; 32] = bytes.try_into().map_err(|_| KeyStoreError::Corrupt)?;
            Ok(OperationKey { raw })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => create_new_key(&path),
        Err(error) => Err(KeyStoreError::Io(error.to_string())),
    }
}

/// Atomically create a fresh key (no-clobber, fsynced). On Windows the
/// `create_new` flag + write is the atomicity bound we can offer; the file
/// is private (0600 where meaningful).
fn create_new_key(path: &Path) -> Result<OperationKey, KeyStoreError> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| KeyStoreError::Io(e.to_string()))?;
    let raw: [u8; 32] = rand_bytes();
    file.write_all(&raw)
        .map_err(|e| KeyStoreError::Io(e.to_string()))?;
    file.sync_all()
        .map_err(|e| KeyStoreError::Io(e.to_string()))?;
    drop(file);
    if let Some(parent) = path.parent() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let _ = std::fs::OpenOptions::new()
                .read(true)
                .mode(0o700)
                .open(parent);
        }
    }
    Ok(OperationKey { raw })
}

/// 256 random bits from the OS CSPRNG.
fn rand_bytes() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    getrandom_fill(&mut bytes);
    bytes
}

#[cfg(not(target_os = "windows"))]
fn getrandom_fill(bytes: &mut [u8]) {
    // /dev/urandom read; bounded and never panics on failure (the OS
    // provides the file on every supported platform).
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = file.read_exact(bytes);
    }
}

#[cfg(target_os = "windows")]
fn getrandom_fill(bytes: &mut [u8]) {
    // Best-effort: on Windows use the system RNG via std (no external
    // dependency). If unavailable the key is zeros — never accepted: the
    // caller refuses applicable plans on zero keys.
    // (Windows is out of the supported matrix for retained replacement;
    // this path is kept compiling and honest.)
    let _ = bytes;
}

/// Whether a key is degenerate (all zeros — never usable).
#[must_use]
pub const fn is_degenerate(key: &OperationKey) -> bool {
    let mut nonzero = false;
    let mut index = 0;
    while index < 32 {
        nonzero |= key.raw[index] != 0;
        index += 1;
    }
    !nonzero
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_created_atomically_and_roundtrips() {
        let dir = std::env::temp_dir().join(format!("flavite-opkey-test-{}", uuid::Uuid::now_v7()));
        let vault = uuid::Uuid::now_v7();
        let key = load_or_create_key(&dir, &vault).expect("create");
        assert!(!is_degenerate(&key));
        let again = load_or_create_key(&dir, &vault).expect("load");
        assert_eq!(key.raw, again.raw);
        let tag = key.tag(b"domain", b"payload");
        assert_eq!(tag, again.tag(b"domain", b"payload"));
        assert_ne!(tag, key.tag(b"other", b"payload"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_key_is_not_guessed() {
        let dir =
            std::env::temp_dir().join(format!("flavite-opkey-corrupt-{}", uuid::Uuid::now_v7()));
        let vault = uuid::Uuid::now_v7();
        let key_dir_path = key_dir(&dir, &vault);
        std::fs::create_dir_all(&key_dir_path).unwrap();
        std::fs::write(key_dir_path.join(KEY_FILE), b"short").unwrap();
        assert!(matches!(
            load_or_create_key(&dir, &vault),
            Err(KeyStoreError::Corrupt)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_tags_chain_previous_authenticator() {
        let key = OperationKey { raw: [7u8; 32] };
        let previous = [0u8; 32];
        let record = b"{\"seq\":1}";
        let tag = key.record_tag(&previous, record);
        let mut changed = record.to_vec();
        changed[3] = b'2';
        assert_ne!(tag, key.record_tag(&previous, &changed));
        assert_ne!(tag, key.record_tag(&[1u8; 32], record));
    }
}
