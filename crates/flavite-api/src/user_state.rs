//! Device-local user state for hosts (RFC-0003 §22, W06, D-11).
//!
//! Workspace layouts and device preferences live in the OS data directory
//! keyed by vault UUID. The facade exposes only fixed kinds, generation
//! receipts, and typed failures; hosts never see or choose file paths.

pub use flavite_vault::user_state::{StoredUserState, UserStateError, UserStateKind};

use crate::{Flavite, FlaviteError};

/// The most recently opened vault root on this device, if any (W10 no-vault
/// startup, FR-22). Device-local app-scope state, independent of any vault,
/// so it is readable before a vault is open.
///
/// # Errors
/// Returns [`UserStateError`] on read/corruption failures; an absent record
/// is `Ok(None)` and a missing OS data directory is `Ok(None)` as well (the
/// start screen simply offers no remembered vault).
pub fn read_last_vault_device() -> Result<Option<std::path::PathBuf>, UserStateError> {
    let Ok(store) =
        flavite_vault::user_state::UserStateStore::open_app_scope(Box::new(crate::OsFs::new()))
    else {
        return Ok(None);
    };
    let Some(stored) = store.read(UserStateKind::AppLaunch)? else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_slice(&stored.payload)
        .map_err(|error| UserStateError::Corrupt(error.to_string()))?;
    Ok(value
        .get("root")
        .and_then(serde_json::Value::as_str)
        .map(std::path::PathBuf::from))
}

/// Record the most recently opened vault root on this device (W10).
///
/// # Errors
/// Returns [`UserStateError`]; callers treat failures as non-fatal because
/// they never affect note safety.
pub fn write_last_vault_device(root: &std::path::Path) -> Result<(), UserStateError> {
    let mut store =
        flavite_vault::user_state::UserStateStore::open_app_scope(Box::new(crate::OsFs::new()))?;
    let expected = store
        .read(UserStateKind::AppLaunch)?
        .map_or(0, |stored| stored.generation);
    let payload = serde_json::json!({ "root": root.display().to_string() });
    store.write(
        UserStateKind::AppLaunch,
        expected,
        &serde_json::to_vec(&payload).expect("bounded json payload"),
    )?;
    Ok(())
}

impl Flavite {
    /// Read a device-local user-state document for this vault.
    ///
    /// # Errors
    /// Returns [`FlaviteError::UserState`] on filesystem, protection, or
    /// corruption failures.
    pub fn read_user_state(
        &self,
        kind: UserStateKind,
    ) -> Result<Option<StoredUserState>, FlaviteError> {
        let Some(store) = self.user_state.as_ref() else {
            return Err(FlaviteError::UserState(UserStateError::Unavailable(
                "no device data directory is available".into(),
            )));
        };
        store.read(kind).map_err(FlaviteError::UserState)
    }

    /// Write a device-local user-state document under a compare-and-set
    /// generation receipt; returns the new generation.
    ///
    /// # Errors
    /// Returns [`FlaviteError::UserState`]; a stale receipt is a typed
    /// generation mismatch, never a silent overwrite.
    pub fn write_user_state(
        &mut self,
        kind: UserStateKind,
        expected_generation: u64,
        payload: &[u8],
    ) -> Result<u64, FlaviteError> {
        let Some(store) = self.user_state.as_mut() else {
            return Err(FlaviteError::UserState(UserStateError::Unavailable(
                "no device data directory is available".into(),
            )));
        };
        store
            .write(kind, expected_generation, payload)
            .map_err(FlaviteError::UserState)
    }

    /// Whether device-local persistence is available on this machine.
    #[must_use]
    pub const fn has_user_state(&self) -> bool {
        self.user_state.is_some()
    }

    /// Test-only seam: point the store at an explicit root so device-local
    /// persistence is exercised against a disposable directory. Never part
    /// of the production path.
    #[doc(hidden)]
    pub fn set_user_state_root_for_test(&mut self, root: std::path::PathBuf) {
        let uuid = self.vault_ref().uuid.to_string();
        self.user_state = Some(flavite_vault::user_state::UserStateStore::at_root(
            Box::new(flavite_fs::MemFs::new()),
            root.join(uuid),
        ));
    }
}
