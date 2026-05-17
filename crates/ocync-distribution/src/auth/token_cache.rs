//! Coalescing token cache shared by every Bearer-token auth provider.
//!
//! Wraps two maps:
//!
//! - `cache` -- the live tokens, keyed by scope.
//! - `inflight` -- per-scope `Arc<Mutex<()>>` slots so concurrent callers
//!   for the same scope serialize through a single fetch while callers
//!   for distinct scopes proceed in parallel.
//!
//! The double-checked cache read after the per-scope mutex is acquired
//! makes a waiter wake to a populated cache when a peer completes
//! successfully, so a successful fetch produces exactly one token
//! exchange regardless of fan-out.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::Token;
use crate::error::Error;

/// Coalescing token cache. Same-scope fetches serialize, distinct scopes
/// run in parallel. See module-level docs for the synchronization model.
#[derive(Debug, Default)]
pub(super) struct TokenCache {
    cache: Mutex<HashMap<String, Token>>,
    inflight: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl TokenCache {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Number of entries currently in the token cache. Used for log
    /// diagnostics on invalidation.
    pub(super) async fn len(&self) -> usize {
        self.cache.lock().await.len()
    }

    /// Look up a cached token for `key`, or invoke `fetch` to produce
    /// one. Concurrent callers for the same `key` coalesce through a
    /// per-scope mutex; callers for distinct keys hold distinct
    /// mutexes and run in parallel. On a successful fetch the token
    /// is inserted into the cache; on error the entry is left empty
    /// so the next caller can retry.
    pub(super) async fn get_or_fetch<F, Fut>(&self, key: String, fetch: F) -> Result<Token, Error>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Token, Error>>,
    {
        if let Some(token) = self
            .cache
            .lock()
            .await
            .get(&key)
            .filter(|t| t.is_valid())
            .cloned()
        {
            return Ok(token);
        }

        let key_mutex = {
            let mut map = self.inflight.lock().await;
            Arc::clone(
                map.entry(key.clone())
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        let _guard = key_mutex.lock().await;

        // Re-check the cache after acquiring the per-scope mutex: a
        // concurrent caller for the same scope may have populated it.
        if let Some(token) = self
            .cache
            .lock()
            .await
            .get(&key)
            .filter(|t| t.is_valid())
            .cloned()
        {
            return Ok(token);
        }

        let token = fetch().await?;

        self.cache.lock().await.insert(key, token.clone());

        Ok(token)
    }

    /// Drop every cached token and every per-scope coalescing slot.
    pub(super) async fn clear(&self) {
        self.cache.lock().await.clear();
        self.inflight.lock().await.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::sync::Barrier;

    use super::*;

    /// Trivial token builder that doesn't expire so cache hits are stable.
    fn token(value: &str) -> Token {
        Token::new(value)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn same_key_coalesces_to_one_fetch() {
        // 20 concurrent callers for the same scope must produce exactly
        // one underlying fetch.
        let cache = Arc::new(TokenCache::new());
        let fetch_count = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::with_capacity(20);
        for _ in 0..20 {
            let cache = Arc::clone(&cache);
            let fetch_count = Arc::clone(&fetch_count);
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_fetch("scope".to_string(), || async {
                        fetch_count.fetch_add(1, Ordering::SeqCst);
                        // Yield once so other callers get a chance to
                        // queue behind us before the fetch resolves.
                        tokio::task::yield_now().await;
                        Ok(token("shared"))
                    })
                    .await
            }));
        }
        for t in tasks {
            let token = t.await.unwrap().unwrap();
            assert_eq!(token.value(), "shared");
        }
        assert_eq!(
            fetch_count.load(Ordering::SeqCst),
            1,
            "concurrent fetches for the same key must coalesce to one"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn distinct_keys_fetch_concurrently() {
        // A `Barrier(2)` inside each fetch deadlocks unless both
        // fetches are in flight simultaneously. The 5s timeout
        // distinguishes deadlock from real parallel execution.
        let cache = TokenCache::new();
        let barrier = Arc::new(Barrier::new(2));
        let b1 = Arc::clone(&barrier);
        let b2 = Arc::clone(&barrier);

        let outcome = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                cache.get_or_fetch("scope-a".to_string(), move || async move {
                    b1.wait().await;
                    Ok(token("tok-a"))
                }),
                cache.get_or_fetch("scope-b".to_string(), move || async move {
                    b2.wait().await;
                    Ok(token("tok-b"))
                }),
            )
        })
        .await;

        let (a, b) = outcome.expect("distinct-scope fetches must run concurrently, not deadlock");
        assert_eq!(a.unwrap().value(), "tok-a");
        assert_eq!(b.unwrap().value(), "tok-b");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fetch_error_propagates_and_releases_slot() {
        // First caller fails; second caller for the same key must be
        // able to retry and succeed (no leaked guard, no poisoned slot).
        let cache = TokenCache::new();

        let err = cache
            .get_or_fetch("scope".to_string(), || async {
                Err(Error::AuthFailed {
                    registry: "test".into(),
                    reason: "boom".into(),
                })
            })
            .await
            .expect_err("first fetch must propagate the error");
        assert!(matches!(err, Error::AuthFailed { .. }));

        let ok = cache
            .get_or_fetch("scope".to_string(), || async { Ok(token("retry-tok")) })
            .await
            .expect("retry must succeed once the slot is released");
        assert_eq!(ok.value(), "retry-tok");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_same_key_callers_share_one_error() {
        // When the first caller's fetch fails, queued waiters wake to
        // an empty cache and each retry independently. This documents
        // the current behaviour: errors are not shared, but no caller
        // is left dangling.
        let cache = Arc::new(TokenCache::new());
        let attempts = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::with_capacity(5);
        for _ in 0..5 {
            let cache = Arc::clone(&cache);
            let attempts = Arc::clone(&attempts);
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_fetch("scope".to_string(), || async {
                        attempts.fetch_add(1, Ordering::SeqCst);
                        tokio::task::yield_now().await;
                        Err::<Token, _>(Error::AuthFailed {
                            registry: "test".into(),
                            reason: "always".into(),
                        })
                    })
                    .await
            }));
        }
        for t in tasks {
            assert!(t.await.unwrap().is_err());
        }
        // Each waiter wakes to a cache miss and runs its own fetch.
        assert_eq!(attempts.load(Ordering::SeqCst), 5);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clear_drops_cache_and_inflight() {
        // Seed the cache and the inflight map.
        let cache = TokenCache::new();
        cache
            .get_or_fetch("scope".to_string(), || async { Ok(token("first")) })
            .await
            .unwrap();
        assert_eq!(cache.len().await, 1);
        assert!(!cache.inflight.lock().await.is_empty());

        cache.clear().await;
        assert_eq!(cache.len().await, 0);
        assert!(cache.inflight.lock().await.is_empty());

        // Post-clear, a same-key fetch must reach the fetcher again.
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_ref = Arc::clone(&attempts);
        let t = cache
            .get_or_fetch("scope".to_string(), || async move {
                attempts_ref.fetch_add(1, Ordering::SeqCst);
                Ok(token("second"))
            })
            .await
            .unwrap();
        assert_eq!(t.value(), "second");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
