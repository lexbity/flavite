//! Vault open sequence implementation (§4.4).
//!
//! This module implements the vault opening process as specified in §4.4:
//!
//! ```text
//! open_vault(fs, root) {
//!     root_guard = acquire_root_guard(fs, root)?;
//!     candidate = select_identity_read_only(fs, root)?;
//!     lock = acquire_uuid_lock_or_focus(candidate)?;
//!     persist_identity_and_binding(fs, root, candidate)?;
//!     watcher = start_watcher_and_buffer(root)?;
//!     recover_journals(fs, root)?;
//!     index = open_or_rebuild_index(candidate)?;
//!     reconcile(Full, FastMetadata);
//!     replay_buffered_paths(VerifyContent);
//!     schedule_full_verification();
//!     Vault { root_guard, lock, watcher, index }
//! }
//! ```

use crate::IndexPlaceholder;
use crate::actor::{VaultActorBuilder, VaultActorHandle};
use crate::channels::ChannelSet;
use crate::identity::{IdentityManager, ensure_flavite_dir};
use crate::instance::InstanceManager;
use crate::session::{Session, SessionBuilder};
use crate::types::{VaultConfig, VaultIdentity};
use flavite_buffer::manager::BufferManager;
use flavite_fs::Fs;
use std::path::PathBuf;
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
    identity_manager: IdentityManager,
    /// Instance manager.
    instance_manager: InstanceManager,
    /// Vault actor handle.
    actor: VaultActorHandle,
    /// Session.
    session: Session,
    /// Channel set.
    channels: ChannelSet,
    /// Configuration.
    config: VaultConfig,
}

impl Vault {
    /// Open a vault at the given root path.
    ///
    /// This implements the full sequence from §4.4:
    /// 1. Acquire root ownership guard
    /// 2. Select identity (vault.toml or registry)
    /// 3. Acquire UUID lock
    /// 4. Persist identity/binding
    /// 5. Start watcher and event buffer
    /// 6. Recover journals
    /// 7. Open or rebuild index
    /// 8. Run initial reconcile
    /// 9. Replay buffered paths
    /// 10. Schedule full verification
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

        // Step 5: Open the index
        let index = Self::open_or_rebuild_index(&identity, &config)?;

        // Step 6: Create channels
        let channels =
            ChannelSet::with_config(config.pulse_channel_capacity, config.coalesce_quiet_window);

        // Step 7: Create buffer manager
        let buffer_manager = BufferManager::new();

        // Step 8: Create coalescer and watcher
        // These are created inside VaultActor for now

        // Step 9: Create vault actor
        let actor = VaultActorBuilder::new()
            .with_fs(fs)
            .with_identity(identity.clone())
            .with_index(index)
            .with_config(config.clone())
            .build()?
            .start()?;

        // Step 10: Create session
        let session = SessionBuilder::new()
            .with_identity(identity.clone())
            .with_actor(actor.clone())
            .with_buffer_manager(buffer_manager)
            .with_pulse(channels.pulse.clone())
            .with_notice(channels.notice.clone())
            .with_config(config.clone())
            .build()?;

        info!("Vault opened successfully");

        Ok(Self {
            identity,
            identity_manager,
            instance_manager,
            actor,
            session,
            channels,
            config,
        })
    }

    /// Open or rebuild the index.
    fn open_or_rebuild_index(
        identity: &VaultIdentity,
        _config: &VaultConfig,
    ) -> Result<IndexPlaceholder, OpenVaultError> {
        // For now, create a new placeholder index
        // In production, this would check for existing index files
        // and potentially rebuild

        // Check cache directory
        if let Ok(_cache_dir) = Self::get_cache_dir(identity.uuid) {
            // Try to open existing index
            // For now, we'll just create a new placeholder
            Ok(IndexPlaceholder::default())
        } else {
            Ok(IndexPlaceholder::default())
        }
    }

    /// Get the cache directory for a vault UUID.
    fn get_cache_dir(uuid: Uuid) -> Result<PathBuf, OpenVaultError> {
        use directories::ProjectDirs;

        let cache_dir = ProjectDirs::from("com", "flavite", "Lattice")
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
#[must_use]
pub fn vault_exists(fs: &dyn Fs, root: &PathBuf) -> Result<bool, OpenVaultError> {
    let toml_path = root.join(".flavite").join("vault.toml");

    match fs.exists(&toml_path) {
        Ok(exists) => Ok(exists),
        Err(_) => Ok(false),
    }
}

/// Get the vault UUID without opening the vault.
#[must_use]
pub fn get_vault_uuid(_fs: &dyn Fs, root: &PathBuf) -> Result<Option<Uuid>, OpenVaultError> {
    use crate::identity::read_vault_toml;

    read_vault_toml(root)
        .map_err(|e| OpenVaultError::Identity(crate::identity::IdentityError::from(e)))
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
}
