//! Write token management for self-write classification (D5).
//!
//! Write tokens prevent concurrent writes to the same file and track
//! self-write events for classification. Tokens are used to distinguish
//! between external changes and our own writes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// A write token for tracking self-write events.
///
/// Tokens are created when we initiate a write and expire after a timeout.
/// They store the expected hash so we can detect if an external write
/// occurred during our operation.
#[derive(Debug, Clone)]
pub struct WriteToken {
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
    /// Create a new write token.
    #[must_use]
    pub fn new(path: PathBuf, expected_hash: [u8; 32], timeout: Duration) -> Self {
        Self {
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
#[derive(Debug, Default)]
pub struct WriteTokenManager {
    /// Map from path to token.
    tokens: RwLock<HashMap<PathBuf, WriteToken>>,
    /// Default timeout for new tokens.
    default_timeout: Duration,
}

impl WriteTokenManager {
    /// Create a new token manager with the default timeout.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tokens: RwLock::new(HashMap::new()),
            default_timeout: Duration::from_secs(5), // 5 seconds as per D5
        }
    }

    /// Create a new token manager with a custom timeout.
    #[must_use]
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            tokens: RwLock::new(HashMap::new()),
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
    /// If a token already exists for the path, it is replaced.
    pub fn acquire(&self, path: PathBuf, expected_hash: [u8; 32]) -> WriteToken {
        let token = WriteToken::new(path.clone(), expected_hash, self.default_timeout);
        let mut tokens = self.tokens.write().unwrap();
        tokens.insert(path, token.clone());
        token
    }

    /// Acquire a write token with a custom timeout.
    pub fn acquire_with_timeout(
        &self,
        path: PathBuf,
        expected_hash: [u8; 32],
        timeout: Duration,
    ) -> WriteToken {
        let token = WriteToken::new(path.clone(), expected_hash, timeout);
        let mut tokens = self.tokens.write().unwrap();
        tokens.insert(path, token.clone());
        token
    }

    /// Release a write token.
    pub fn release(&self, token: &WriteToken) {
        let mut tokens = self.tokens.write().unwrap();
        tokens.remove(&token.path);
    }

    /// Check if a token exists for a path.
    #[must_use]
    pub fn has_token(&self, path: &PathBuf) -> bool {
        let tokens = self.tokens.read().unwrap();
        tokens.contains_key(path)
    }

    /// Get the token for a path, if any.
    #[must_use]
    pub fn get_token(&self, path: &PathBuf) -> Option<WriteToken> {
        let tokens = self.tokens.read().unwrap();
        tokens.get(path).cloned()
    }

    /// Get a reference to the token for a path, if any.
    #[must_use]
    pub fn get_token_ref(
        &self,
        path: &PathBuf,
    ) -> Option<std::sync::RwLockReadGuard<'_, HashMap<PathBuf, WriteToken>>> {
        let tokens = self.tokens.read().unwrap();
        if tokens.contains_key(path) {
            Some(tokens)
        } else {
            None
        }
    }

    /// Check if a path has an active, non-expired token.
    #[must_use]
    pub fn has_active_token(&self, path: &PathBuf) -> bool {
        if let Some(token) = self.get_token(path) {
            !token.is_expired()
        } else {
            false
        }
    }

    /// Check if the expected hash matches for a path.
    #[must_use]
    pub fn matches_expected(&self, path: &PathBuf, hash: &[u8; 32]) -> bool {
        if let Some(token) = self.get_token(path) {
            !token.is_expired() && token.matches(hash)
        } else {
            false
        }
    }

    /// Remove expired tokens.
    pub fn cleanup_expired(&self) -> usize {
        let mut tokens = self.tokens.write().unwrap();
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
        let mut tokens = self.tokens.write().unwrap();
        tokens.clear();
    }

    /// Get the number of active tokens.
    #[must_use]
    pub fn len(&self) -> usize {
        let tokens = self.tokens.read().unwrap();
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
        let tokens = self.tokens.read().unwrap();
        tokens.keys().cloned().collect()
    }
}

impl Clone for WriteTokenManager {
    fn clone(&self) -> Self {
        Self {
            tokens: RwLock::new(self.tokens.read().unwrap().clone()),
            default_timeout: self.default_timeout,
        }
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
