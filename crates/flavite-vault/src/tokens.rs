//! Write token management for self-write classification (D5).
//!
//! Write tokens prevent concurrent writes to the same file and track
//! self-write events for classification. Tokens are used to distinguish
//! between external changes and our own writes.
//!
//! Ownership: `acquire` replaces any existing token for a path, so at most
//! one token per path is *live*. `release` honors that ownership: it removes
//! the path's token only when the released token is the live one, matched on
//! [`WriteToken::token_id`]. Releasing a stale, replaced token is a silent
//! no-op — it must never revoke the current writer's token (that
//! misclassification would let a concurrent writer read a false "no token"
//! and treat its own write as external).

use crate::sync::RwLockExt;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// A write token for tracking self-write events.
///
/// Tokens are created when we initiate a write and expire after a timeout.
/// They store the expected hash so we can detect if an external write
/// occurred during our operation.
///
/// Cloning preserves `token_id`: a clone represents the same write right,
/// so releasing a clone releases the token. That is intentional.
#[derive(Debug, Clone)]
pub struct WriteToken {
    /// Monotonic identity; release matches on this, not on path or hash.
    pub token_id: u64,
    /// The path being written.
    pub path: PathBuf,
    /// The expected hash of the file after our write.
    pub expected_hash: [u8; 32],
    /// When the token was acquired.
    pub acquired_at: Instant,
    /// When the token expires.
    pub expires_at: Instant,
}

impl WriteToken {
    /// Create a new write token, drawing its identity from the manager's
    /// counter. Public construction goes through `WriteTokenManager` only.
    fn new(counter: &AtomicU64, path: PathBuf, expected_hash: [u8; 32], timeout: Duration) -> Self {
        Self {
            // Relaxed is sufficient: ids are compared for equality only,
            // never used to order operations across threads.
            token_id: counter.fetch_add(1, Ordering::Relaxed),
            path,
            expected_hash,
            acquired_at: Instant::now(),
            expires_at: Instant::now() + timeout,
        }
    }

    /// Check if the token has expired.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        Instant::now() > self.expires_at
    }

    /// Check if the hash matches the expected value.
    ///
    /// Note: the hash is not identity — two saves of unchanged content
    /// produce the same hash. Release and classification use `token_id`.
    #[must_use]
    pub fn matches(&self, hash: &[u8; 32]) -> bool {
        &self.expected_hash == hash
    }

    /// Get the remaining time until expiration.
    #[must_use]
    pub fn remaining(&self) -> Duration {
        let now = Instant::now();
        if now > self.expires_at {
            Duration::from_secs(0)
        } else {
            self.expires_at - now
        }
    }
}

/// Manager for write tokens.
///
/// This manages a set of active write tokens and provides methods
/// for acquiring, releasing, and checking tokens.
#[derive(Debug)]
pub struct WriteTokenManager {
    /// Map from path to token.
    tokens: RwLock<HashMap<PathBuf, WriteToken>>,
    /// Monotonic source of `WriteToken::token_id` values, local to this
    /// manager. A clone starts a fresh sequence with an empty map, so no
    /// live token of another manager can reference the clone's ids.
    next_id: AtomicU64,
    /// Default timeout for new tokens.
    default_timeout: Duration,
}

impl Default for WriteTokenManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for WriteTokenManager {
    /// Clone yields a fresh, empty manager: the token map does not carry
    /// over and ids restart at zero. An empty map makes id collisions with
    /// the source manager's tokens unreachable.
    fn clone(&self) -> Self {
        Self {
            tokens: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            default_timeout: self.default_timeout,
        }
    }
}

impl WriteTokenManager {
    /// Create a new token manager with the default timeout.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tokens: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            default_timeout: Duration::from_secs(5), // 5 seconds as per D5
        }
    }

    /// Create a new token manager with a custom timeout.
    #[must_use]
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            tokens: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            default_timeout: timeout,
        }
    }

    /// Set the default timeout.
    pub fn set_default_timeout(&mut self, timeout: Duration) {
        self.default_timeout = timeout;
    }

    /// Get the default timeout.
    #[must_use]
    pub fn default_timeout(&self) -> Duration {
        self.default_timeout
    }

    /// Acquire a write token for a path.
    ///
    /// If a token already exists for the path, it is replaced: the returned
    /// token becomes the path's live token, and the replaced token's
    /// `release` (or guard drop) becomes a no-op.
    pub fn acquire(&self, path: PathBuf, expected_hash: [u8; 32]) -> WriteToken {
        let token = WriteToken::new(
            &self.next_id,
            path.clone(),
            expected_hash,
            self.default_timeout,
        );
        let mut tokens = self.tokens.write_recovering();
        tokens.insert(path, token.clone());
        token
    }

    /// Acquire a write token with a custom timeout.
    ///
    /// Replacement semantics are as for [`WriteTokenManager::acquire`].
    pub fn acquire_with_timeout(
        &self,
        path: PathBuf,
        expected_hash: [u8; 32],
        timeout: Duration,
    ) -> WriteToken {
        let token = WriteToken::new(&self.next_id, path.clone(), expected_hash, timeout);
        let mut tokens = self.tokens.write_recovering();
        tokens.insert(path, token.clone());
        token
    }

    /// Release a write token.
    ///
    /// Only the token that currently *is* the path's live token may remove
    /// it, matched on `token_id`. Releasing a token that was replaced by a
    /// newer `acquire` for the same path is a silent no-op: expiry +
    /// replacement + late drop is a normal lifecycle, and the guard API has
    /// no channel to report a failed release.
    pub fn release(&self, token: &WriteToken) {
        let mut tokens = self.tokens.write_recovering();
        if tokens
            .get(&token.path)
            .is_some_and(|live| live.token_id == token.token_id)
        {
            tokens.remove(&token.path);
        }
    }

    /// Check if a token exists for a path.
    #[must_use]
    pub fn has_token(&self, path: &Path) -> bool {
        let tokens = self.tokens.read_recovering();
        tokens.contains_key(path)
    }

    /// Get the token for a path, if any.
    #[must_use]
    pub fn get_token(&self, path: &Path) -> Option<WriteToken> {
        let tokens = self.tokens.read_recovering();
        tokens.get(path).cloned()
    }

    /// Check if a path has an active, non-expired token.
    #[must_use]
    pub fn has_active_token(&self, path: &Path) -> bool {
        self.get_token(path)
            .is_some_and(|token| !token.is_expired())
    }

    /// Check if the expected hash matches for a path.
    #[must_use]
    pub fn matches_expected(&self, path: &Path, hash: &[u8; 32]) -> bool {
        self.get_token(path)
            .is_some_and(|token| !token.is_expired() && token.matches(hash))
    }

    /// Remove expired tokens.
    pub fn cleanup_expired(&self) -> usize {
        let mut tokens = self.tokens.write_recovering();
        let now = Instant::now();
        let mut count = 0;

        tokens.retain(|_, token| {
            if token.expires_at > now {
                true
            } else {
                count += 1;
                false
            }
        });

        count
    }

    /// Clear all tokens.
    pub fn clear(&self) {
        let mut tokens = self.tokens.write_recovering();
        tokens.clear();
    }

    /// Get the number of active tokens.
    #[must_use]
    pub fn len(&self) -> usize {
        let tokens = self.tokens.read_recovering();
        tokens.len()
    }

    /// Check if there are no tokens.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get all token paths.
    #[must_use]
    pub fn paths(&self) -> Vec<PathBuf> {
        let tokens = self.tokens.read_recovering();
        tokens.keys().cloned().collect()
    }
}

/// Guard that releases a token on drop.
#[derive(Debug)]
pub struct TokenGuard {
    manager: Arc<WriteTokenManager>,
    token: WriteToken,
}

impl TokenGuard {
    /// Create a new token guard.
    pub fn new(manager: Arc<WriteTokenManager>, token: WriteToken) -> Self {
        Self { manager, token }
    }

    /// Get the token.
    #[must_use]
    pub fn token(&self) -> &WriteToken {
        &self.token
    }

    /// Release the token early.
    pub fn release(self) {
        self.manager.release(&self.token);
    }
}

impl Drop for TokenGuard {
    fn drop(&mut self) {
        self.manager.release(&self.token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_token_creation() {
        let manager = WriteTokenManager::new();
        let hash = [0u8; 32];
        let path = PathBuf::from("/test/file.md");

        let token = manager.acquire(path.clone(), hash);

        assert!(!token.is_expired());
        assert!(token.matches(&hash));
        assert!(manager.has_token(&path));
        assert!(manager.get_token(&path).is_some());
    }

    #[test]
    fn manager_replacement_clone_guards_and_cleanup_cover_all_lifecycles() {
        let manager = Arc::new(WriteTokenManager::with_timeout(Duration::from_secs(2)));
        assert_eq!(manager.default_timeout(), Duration::from_secs(2));
        let path = PathBuf::from("/test/lifecycle.md");
        let first = manager.acquire(path.clone(), [1; 32]);
        let replacement =
            manager.acquire_with_timeout(path.clone(), [2; 32], Duration::from_secs(1));
        assert!(first.matches(&[1; 32]));
        assert!(!manager.matches_expected(&path, &[1; 32]));
        assert!(manager.matches_expected(&path, &[2; 32]));
        assert!(manager.has_token(&path));
        assert_eq!(manager.paths(), vec![path.clone()]);
        assert_eq!(manager.len(), 1);
        assert!(!manager.is_empty());
        assert!(!replacement.is_expired());
        assert!(replacement.remaining() <= Duration::from_secs(1));
        assert!(manager.has_active_token(&path));
        assert_eq!(manager.cleanup_expired(), 0);
        assert!(manager.has_token(&path));

        // The audit gap: releasing the replaced token must not revoke the
        // live replacement (identity-matched release, S1/H2).
        manager.release(&first);
        assert!(
            manager.has_token(&path),
            "replacement revoked by stale release"
        );
        assert!(manager.matches_expected(&path, &[2; 32]));

        manager.release(&replacement);
        assert!(!manager.has_token(&path));

        let token = manager.acquire(path.clone(), [3; 32]);
        let guard = TokenGuard::new(manager.clone(), token.clone());
        assert!(manager.has_token(&path));
        drop(guard);
        assert!(!manager.has_token(&path));

        let token = manager.acquire(path.clone(), [4; 32]);
        TokenGuard::new(manager.clone(), token).release();
        assert!(manager.is_empty());
        let token = manager.acquire(path.clone(), [5; 32]);
        manager.release(&token);
        assert!(manager.is_empty());
        manager.acquire(PathBuf::from("/test/clear.md"), [0; 32]);
        manager.clear();
        assert!(manager.is_empty());

        let cloned = manager.as_ref().clone();
        assert_eq!(cloned.default_timeout(), manager.default_timeout());
        assert!(cloned.is_empty());
    }

    /// H2 regression: releasing a token that was replaced must never revoke
    /// the replacement. On release-by-path this test fails — A's release
    /// deletes B.
    #[test]
    fn stale_guard_release_does_not_revoke_replacement() {
        let manager = WriteTokenManager::new();
        let path = PathBuf::from("/test/stale-release.md");
        let hash_a = [1u8; 32];
        let hash_b = [2u8; 32];

        let token_a = manager.acquire(path.clone(), hash_a);
        let token_b = manager.acquire(path.clone(), hash_b);

        // Stale release: must be a no-op against the live replacement.
        manager.release(&token_a);
        assert!(
            manager.has_token(&path),
            "replacement revoked by stale release"
        );
        assert!(manager.matches_expected(&path, &hash_b));
        assert!(!manager.matches_expected(&path, &hash_a));

        // The live token still releases normally.
        manager.release(&token_b);
        assert!(!manager.has_token(&path));
    }

    /// Same contract through the guard's Drop path: dropping the guard of a
    /// replaced token leaves the replacement in place.
    #[test]
    fn token_guard_of_replaced_token_is_a_no_op() {
        let manager = Arc::new(WriteTokenManager::new());
        let path = PathBuf::from("/test/stale-guard.md");

        let token_a = manager.acquire(path.clone(), [1u8; 32]);
        let token_b = manager.acquire(path.clone(), [2u8; 32]);
        let guard_a = TokenGuard::new(manager.clone(), token_a);
        let guard_b = TokenGuard::new(manager.clone(), token_b);

        // Dropping the stale guard must not revoke the live token.
        drop(guard_a);
        assert!(
            manager.has_token(&path),
            "replacement revoked by stale guard drop"
        );
        assert!(manager.matches_expected(&path, &[2u8; 32]));

        // Dropping the live guard releases normally.
        drop(guard_b);
        assert!(!manager.has_token(&path));
    }

    /// R5 coverage: cloning resets the id sequence (fresh manager, empty
    /// map), so ids legitimately repeat across managers — but a release is
    /// always scoped to the manager that holds the token, and releasing on
    /// the source manager must never touch the clone's map.
    #[test]
    fn clone_manager_then_old_id_cannot_release() {
        let path = PathBuf::from("/test/clone-id-collision.md");
        let m1 = WriteTokenManager::new();
        let token_a = m1.acquire(path.clone(), [1u8; 32]);

        let m2 = m1.clone();
        assert!(m2.is_empty(), "clone starts with an empty map");
        let token_a2 = m2.acquire(path.clone(), [2u8; 32]);

        // The collision case: fresh sequences hand out the same id.
        assert_eq!(token_a.token_id, token_a2.token_id);

        // Releasing on the source manager cannot reach the clone's map.
        m1.release(&token_a);
        assert!(!m1.has_token(&path));
        assert!(
            m2.has_token(&path),
            "clone's token touched by source release"
        );

        // The clone's own live token releases normally.
        m2.release(&token_a2);
        assert!(m2.is_empty());
    }

    #[test]
    fn test_token_expiration() {
        let manager = WriteTokenManager::new();
        let hash = [0u8; 32];
        let path = PathBuf::from("/test/file.md");

        let token = manager.acquire_with_timeout(path.clone(), hash, Duration::from_millis(10));

        // Should not be expired yet
        assert!(!token.is_expired());

        // Wait for expiration
        std::thread::sleep(Duration::from_millis(20));

        // Now should be expired
        assert!(token.is_expired());
    }

    #[test]
    fn test_token_release() {
        let manager = WriteTokenManager::new();
        let hash = [0u8; 32];
        let path = PathBuf::from("/test/file.md");

        let token = manager.acquire(path.clone(), hash);
        assert!(manager.has_token(&path));

        manager.release(&token);
        assert!(!manager.has_token(&path));
    }

    #[test]
    fn test_token_guard() {
        let manager = Arc::new(WriteTokenManager::new());
        let hash = [0u8; 32];
        let path = PathBuf::from("/test/file.md");

        let token = manager.acquire(path.clone(), hash);

        {
            let _guard = TokenGuard::new(manager.clone(), token.clone());
            assert!(manager.has_token(&path));

            // Guard keeps the token alive
        }

        // After guard is dropped, token should be released
        assert!(!manager.has_token(&path));
    }

    #[test]
    fn test_cleanup_expired() {
        let manager = WriteTokenManager::new();
        let hash = [0u8; 32];

        // Add some tokens with very short timeouts
        manager.acquire_with_timeout(
            PathBuf::from("/test/file1.md"),
            hash,
            Duration::from_millis(10),
        );
        manager.acquire_with_timeout(
            PathBuf::from("/test/file2.md"),
            hash,
            Duration::from_secs(10),
        );

        assert_eq!(manager.len(), 2);

        // Wait for the first to expire
        std::thread::sleep(Duration::from_millis(20));

        // Cleanup should remove only the expired one
        let cleaned = manager.cleanup_expired();
        assert_eq!(cleaned, 1);
        assert_eq!(manager.len(), 1);
    }
}
