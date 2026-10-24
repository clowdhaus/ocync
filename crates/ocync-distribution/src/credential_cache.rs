//! A credential cached until it nears expiry, shared by the cloud auth
//! providers and the AWS API client.

use std::future::Future;
use std::time::{Duration, SystemTime};

use tokio::sync::RwLock;

use crate::auth::Token;

/// Time until `until`, or zero once it has passed.
pub(crate) fn remaining(until: SystemTime) -> Duration {
    until
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO)
}

/// Cached credential with TTL tracking.
///
/// Generic cache for credentials that uses a read-lock fast path
/// for concurrent cache hits and a write-lock with double-check for
/// refreshes. Used by the ECR, ECR Public, Azure and Google auth providers
/// and the AWS API client.
pub(crate) struct CredentialCache<T> {
    cached: RwLock<Option<CacheEntry<T>>>,
}

struct CacheEntry<T> {
    value: T,
    /// Tracks TTL via `is_valid()`.
    lifetime: Token,
}

impl<T: Clone + Send + Sync> CredentialCache<T> {
    /// Create an empty cache.
    pub(crate) fn new() -> Self {
        Self {
            cached: RwLock::new(None),
        }
    }

    /// Return the cached value if still valid, otherwise await `refresh` to
    /// obtain a new one.
    ///
    /// `label` is used as the `Token` value for TTL tracking (not for auth).
    /// The refresh future is created at the call site but only polled on
    /// cache miss, so the API call is never issued when the cache is warm.
    pub(crate) async fn get_or_refresh<E>(
        &self,
        label: &str,
        refresh: impl Future<Output = Result<(T, Duration), E>>,
    ) -> Result<T, E> {
        // Fast path: read-lock check allows concurrent readers.
        {
            let cached = self.cached.read().await;
            if let Some(ref entry) = *cached {
                if entry.lifetime.is_valid() {
                    return Ok(entry.value.clone());
                }
            }
        }

        // Slow path: write-lock for refresh.
        let mut cached = self.cached.write().await;

        // Double-check after acquiring write lock -- another task may have
        // already refreshed while we waited.
        if let Some(ref entry) = *cached {
            if entry.lifetime.is_valid() {
                return Ok(entry.value.clone());
            }
        }

        let (value, ttl) = refresh.await?;

        *cached = Some(CacheEntry {
            value: value.clone(),
            lifetime: Token::with_ttl(label, ttl),
        });

        Ok(value)
    }

    /// Clear the cached value, forcing the next `get_or_refresh` to call
    /// the refresh function.
    pub(crate) async fn clear(&self) {
        *self.cached.write().await = None;
    }

    /// Replace the cached value (test helper).
    #[cfg(test)]
    pub(crate) async fn set(&self, value: T, ttl: Duration) {
        *self.cached.write().await = Some(CacheEntry {
            value,
            lifetime: Token::with_ttl("test-lifetime", ttl),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_expiry_in_the_past_leaves_no_time() {
        assert_eq!(remaining(SystemTime::UNIX_EPOCH), Duration::ZERO);
    }
}
