//! HTTP API key authentication.
//!
//! Uses ulmp's HMAC-SHA256 for token verification. No duplicate crypto.
//! Bearer token auth on every endpoint except /v1/health.

use ulmp::crypto::sha256::sha256;

/// API key store. Holds hashed tokens for constant-time comparison.
pub struct ApiKeyStore {
    /// SHA-256 hashes of valid API keys.
    key_hashes: Vec<[u8; 32]>,
}

impl ApiKeyStore {
    /// Create a store from raw API key strings.
    pub fn new(keys: &[&str]) -> Self {
        Self {
            key_hashes: keys.iter().map(|k| sha256(k.as_bytes())).collect(),
        }
    }

    /// Create from a single key.
    pub fn single(key: &str) -> Self {
        Self::new(&[key])
    }

    /// Create an empty store (no auth required).
    pub fn open() -> Self {
        Self {
            key_hashes: Vec::new(),
        }
    }

    /// Check if auth is required (store has keys).
    pub fn is_enabled(&self) -> bool {
        !self.key_hashes.is_empty()
    }

    /// Verify a bearer token. Constant-time comparison.
    pub fn verify(&self, token: &str) -> bool {
        if !self.is_enabled() {
            return true; // no auth configured = open access
        }

        let token_hash = sha256(token.as_bytes());

        // Constant-time comparison against all stored hashes
        // (prevents timing attacks on which key was tried)
        let mut found = false;
        for stored_hash in &self.key_hashes {
            let mut diff = 0u8;
            for (a, b) in token_hash.iter().zip(stored_hash.iter()) {
                diff |= a ^ b;
            }
            if diff == 0 {
                found = true;
            }
        }
        found
    }

    /// Extract bearer token from Authorization header value.
    pub fn extract_bearer(auth_header: &str) -> Option<&str> {
        let trimmed = auth_header.trim();
        if trimmed.starts_with("Bearer ") {
            Some(trimmed[7..].trim())
        } else {
            None
        }
    }

    pub fn key_count(&self) -> usize {
        self.key_hashes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_correct_key() {
        let store = ApiKeyStore::single("my-secret-key-123");
        assert!(store.verify("my-secret-key-123"));
    }

    #[test]
    fn reject_wrong_key() {
        let store = ApiKeyStore::single("my-secret-key-123");
        assert!(!store.verify("wrong-key"));
    }

    #[test]
    fn multiple_keys() {
        let store = ApiKeyStore::new(&["key-a", "key-b", "key-c"]);
        assert!(store.verify("key-a"));
        assert!(store.verify("key-b"));
        assert!(store.verify("key-c"));
        assert!(!store.verify("key-d"));
    }

    #[test]
    fn open_store_allows_all() {
        let store = ApiKeyStore::open();
        assert!(!store.is_enabled());
        assert!(store.verify("anything"));
    }

    #[test]
    fn extract_bearer() {
        assert_eq!(
            ApiKeyStore::extract_bearer("Bearer my-token"),
            Some("my-token")
        );
        assert_eq!(
            ApiKeyStore::extract_bearer("Bearer  spaced "),
            Some("spaced")
        );
        assert_eq!(ApiKeyStore::extract_bearer("Basic abc"), None);
        assert_eq!(ApiKeyStore::extract_bearer(""), None);
    }

    #[test]
    fn constant_time() {
        // This test verifies the function works, not timing.
        // Timing analysis requires dedicated benchmarks.
        let store = ApiKeyStore::single("test-key");
        for _ in 0..1000 {
            assert!(store.verify("test-key"));
            assert!(!store.verify("wrong-key"));
        }
    }
}
