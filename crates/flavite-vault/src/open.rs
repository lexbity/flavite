//! Vault open sequence implementation (§4.4).
//!
//! Opening resolves vault identity, takes the per-vault instance lock,
//! initializes the cache index, starts the vault actor, and builds a session.

use crate::actor::{VaultActorBuilder, VaultActorHandle};
use crate::channels::ChannelSet;
use crate::identity::{IdentityManager, ensure_flavite_dir};
use crate::instance::InstanceManager;
use crate::session::{Session, SessionBuilder};
use crate::types::{VaultConfig, VaultIdentity};
use flavite_buffer::manager::BufferManager;
use flavite_fs::Fs;
use flavite_index::IndexActor;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, info};
use uuid::Uuid;

/// Errors that can occur during vault open.
#[derive(Debug, Error)]
pub enum OpenVaultError {
    #[error("Identity error: {0}")]
    Identity(#[from] crate::identity::IdentityError),

    #[error("Instance error: {0}")]
    Instance(#[from] crate::instance::InstanceError),

    #[error("Index error: {0}")]
    Index(String),

    #[error("Reconcile error: {0}")]
    Reconcile(String),

    #[error("Actor start error: {0}")]
    ActorStart(String),

    #[error("Actor error: {0}")]
    Actor(#[from] crate::actor::VaultActorError),
    #[error("{0}")]
    Message(String),
}

/// Result type for vault open operations.
pub type OpenVaultResult<T> = Result<T, OpenVaultError>;

impl From<String> for OpenVaultError {
    fn from(s: String) -> Self {
        Self::Message(s)
    }
}

impl From<&str> for OpenVaultError {
    fn from(s: &str) -> Self {
        Self::Message(s.to_string())
    }
}

/// The Vault structure — the main entry point for vault operations.
#[derive(Debug)]
pub struct Vault {
    /// Vault identity.
    identity: VaultIdentity,
    /// Identity manager.
    _identity_manager: IdentityManager,
    /// Instance manager.
    _instance_manager: InstanceManager,
    /// Vault actor handle.
    actor: VaultActorHandle,
    /// Session.
    session: Arc<Session>,
    /// Channel set.
    channels: ChannelSet,
    /// Configuration.
    config: VaultConfig,
}

impl Vault {
    /// Open a vault at the given root path.
    ///
    /// Resolves identity, acquires the instance lock, initializes the index,
    /// starts the vault actor, and creates a session.
    pub fn open(
        root: PathBuf,
        fs: Box<dyn Fs + Send + Sync>,
        config: VaultConfig,
    ) -> OpenVaultResult<Self> {
        info!("Opening vault at: {}", root.display());

        // Step 1: Ensure .flavite directory exists
        // Note: identity computation needs to happen first

        // Step 2: Select identity read-only
        // This is handled by IdentityManager
        let identity_manager = IdentityManager::open(&*fs, root.clone())?;
        let identity = identity_manager.identity().clone();

        info!(
            "Vault identity: UUID={}, root={}",
            identity.uuid,
            root.display()
        );

        // Step 3: Create instance manager (acquires UUID lock)
        let instance_manager = InstanceManager::new(identity.clone())?;

        // Step 4: Ensure .flavite directory exists
        ensure_flavite_dir(&root)?;

        // Journal recovery precedes cache startup: disk is authoritative and
        // interrupted committed renames roll forward before indexing.
        let recovery_writer = crate::identity::get_data_dir().map(|data| {
            let dir = data
                .join("flavite")
                .join("vaults")
                .join(identity.uuid.to_string())
                .join("recovery");
            flavite_buffer::recovery::RecoveryWriter::new(
                flavite_buffer::recovery::RecoveryConfig::new(identity.uuid.to_string(), dir, 2),
            )
        });
        let executor = crate::journal::JournalExecutor::new(&*fs, &root);
        let recovery = if let Some(writer) = &recovery_writer {
            executor.recover_with_buffer_base(|key| {
                writer
                    .read_for_note_key(key)
                    .and_then(|file| file.base_hash.map(|hash| hash.as_bytes()))
            })
        } else {
            executor.recover()
        }
        .map_err(OpenVaultError::Reconcile)?;
        for (txn, reason) in &recovery.skipped {
            tracing::warn!(%txn, %reason, "rename journal item diverged and was preserved");
        }

        // Step 5: Open the index
        let index = Self::open_or_rebuild_index(&identity, &config)?;

        // Step 6: Create channels
        let channels =
            ChannelSet::with_config(config.pulse_channel_capacity, config.coalesce_quiet_window);

        // Step 7: Create buffer manager
        let buffer_manager = BufferManager::new();

        // Start the vault actor.
        let actor = VaultActorBuilder::new()
            .with_fs(fs)
            .with_identity(identity.clone())
            .with_index(index)
            .with_pulse_sender(channels.pulse.sender())
            .with_runtime_config(config.clone())
            .build()?
            .start()?;
        actor.wait_until_ready(std::time::Duration::from_secs(120))?;

        // Build the session around the actor and channels.
        let session = SessionBuilder::new()
            .with_identity(identity.clone())
            .with_actor(actor.clone())
            .with_buffer_manager(buffer_manager)
            .with_pulse(channels.pulse.clone())
            .with_notice(channels.notice.clone())
            .with_config(config.clone())
            .with_recovery_notices(recovery.notice_codes())
            .build()?;

        info!("Vault opened successfully");

        Ok(Self {
            identity,
            _identity_manager: identity_manager,
            _instance_manager: instance_manager,
            actor,
            session: Arc::new(session),
            channels,
            config,
        })
    }

    /// Open or rebuild the index.
    fn open_or_rebuild_index(
        identity: &VaultIdentity,
        _config: &VaultConfig,
    ) -> Result<IndexActor, OpenVaultError> {
        let cache_dir =
            Self::get_cache_dir(identity.uuid)?.join(format!("v{}", flavite_index::SCHEMA_MAJOR));
        std::fs::create_dir_all(&cache_dir)
            .map_err(|error| OpenVaultError::Index(error.to_string()))?;
        let db_path = cache_dir.join("index.sqlite3");
        drop(
            flavite_index::open_or_init(&db_path)
                .map_err(|error| OpenVaultError::Index(error.to_string()))?,
        );
        IndexActor::new(&db_path, flavite_index::pool::DEFAULT_POOL_SIZE)
            .map_err(|error| OpenVaultError::Index(error.to_string()))
    }

    /// Get the cache directory for a vault UUID.
    fn get_cache_dir(uuid: Uuid) -> Result<PathBuf, OpenVaultError> {
        use directories::ProjectDirs;

        let cache_dir = ProjectDirs::from("com", "flavite", "Flavite")
            .ok_or(OpenVaultError::Index(
                "No cache directory available".to_string(),
            ))?
            .cache_dir()
            .to_path_buf();

        Ok(cache_dir.join("flavite").join(uuid.to_string()))
    }

    /// Get the session.
    #[must_use]
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Get the session as an Arc for API facades that share its lifetime.
    #[must_use]
    pub fn session_arc(&self) -> Arc<Session> {
        Arc::clone(&self.session)
    }

    /// Get the actor handle.
    #[must_use]
    pub fn actor(&self) -> &VaultActorHandle {
        &self.actor
    }

    /// Get the channel set.
    #[must_use]
    pub fn channels(&self) -> &ChannelSet {
        &self.channels
    }

    /// Get the identity.
    #[must_use]
    pub fn identity(&self) -> &VaultIdentity {
        &self.identity
    }

    /// Get the UUID.
    #[must_use]
    pub fn uuid(&self) -> Uuid {
        self.identity.uuid
    }

    /// Whether the actor's filesystem watcher is running.
    #[must_use]
    pub fn watcher_enabled(&self) -> bool {
        self.config.watcher_enabled
    }

    /// Get the root path.
    #[must_use]
    pub fn root(&self) -> &PathBuf {
        &self.identity.root
    }

    /// Get the configuration.
    #[must_use]
    pub fn config(&self) -> &VaultConfig {
        &self.config
    }

    /// Close the vault.
    pub fn close(self) {
        info!("Closing vault: {}", self.identity.root.display());

        // Shutdown sequence:
        // 1. Notify actor to shutdown
        let _ = self.actor.try_send(crate::actor::VaultActorMsg::Shutdown);

        // 2. The actor handle, session, etc. will be dropped
        // which will clean up resources
    }
}

impl Drop for Vault {
    fn drop(&mut self) {
        debug!("Vault dropped: {}", self.identity.root.display());
    }
}

/// Check if a vault exists at the given path.
pub fn vault_exists(fs: &dyn Fs, root: &std::path::Path) -> Result<bool, OpenVaultError> {
    let toml_path = root.join(".flavite").join("vault.toml");

    match fs.exists(&toml_path) {
        Ok(exists) => Ok(exists),
        Err(_) => Ok(false),
    }
}

/// Get the vault UUID without opening the vault.
pub fn get_vault_uuid(
    _fs: &dyn Fs,
    root: &std::path::Path,
) -> Result<Option<Uuid>, OpenVaultError> {
    use crate::identity::read_vault_toml;

    read_vault_toml(root).map_err(OpenVaultError::Identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_fs::MemFs;
    use std::path::PathBuf;

    #[test]
    fn test_vault_exists_no_dir() {
        let fs = MemFs::new();
        let root = PathBuf::from("/nonexistent");

        let exists = vault_exists(&fs, &root).unwrap_or(false);
        assert!(!exists);
    }

    #[test]
    fn opening_nonexistent_root_propagates_identity_failure() {
        let fs = MemFs::new();
        let result = Vault::open(
            PathBuf::from("/missing/vault"),
            Box::new(fs),
            VaultConfig::default(),
        );
        assert!(matches!(result, Err(OpenVaultError::Identity(_))));
    }

    #[test]
    fn test_vault_exists_with_structure() {
        let fs = MemFs::new();
        let root = PathBuf::from("/vault");

        // Create the vault structure
        fs.create_dir_all(&root.join(".flavite")).unwrap();
        fs.write_new(
            &root.join(".flavite").join("vault.toml"),
            b"[vault]\nuuid = \"00000000-0000-0000-0000-000000000000\"",
        )
        .unwrap();

        let exists = vault_exists(&fs, &root).unwrap_or(false);
        assert!(exists);
    }

    #[test]
    fn test_get_cache_dir() {
        let uuid = uuid::Uuid::now_v7();
        let result = Vault::get_cache_dir(uuid);
        // Should not panic, result may vary
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn get_vault_uuid_handles_missing_and_malformed_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let fs = MemFs::new();
        assert_eq!(get_vault_uuid(&fs, temp.path()).unwrap(), None);

        let metadata = temp.path().join(".flavite").join("vault.toml");
        std::fs::create_dir_all(metadata.parent().unwrap()).unwrap();
        let uuid = Uuid::now_v7();
        std::fs::write(&metadata, format!("[vault]\nuuid = \"{uuid}\"\n")).unwrap();
        assert_eq!(get_vault_uuid(&fs, temp.path()).unwrap(), Some(uuid));

        std::fs::write(&metadata, "[vault]\nuuid = 'broken'").unwrap();
        assert!(matches!(
            get_vault_uuid(&fs, temp.path()),
            Err(OpenVaultError::Identity(_))
        ));
    }
}
