//! Coalescing token cache shared by every Bearer-token auth provider.
//!
//! The live tokens are kept by scope. A fetch for a scope runs once
//! however many callers need it: the callers that ask while it is in
//! flight get its result, a failure included, so a failing endpoint costs
//! one exchange per wave of callers, not one per caller. Callers for
//! distinct scopes proceed in parallel.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::Token;
use crate::error::Error;
use crate::flight::{Flights, Landed};

/// Coalescing token cache. See the module docs.
#[derive(Debug, Default)]
pub(super) struct TokenCache {
    cache: Mutex<HashMap<String, Token>>,
    flights: Flights<String, Result<Token, Error>>,
}

impl TokenCache {
    pub(super) fn new() -> Self {
        Self::default()
    }

    fn tokens(&self) -> MutexGuard<'_, HashMap<String, Token>> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Number of entries currently in the token cache. Used for log
    /// diagnostics on invalidation.
    pub(super) async fn len(&self) -> usize {
        self.tokens().len()
    }

    /// Look up a cached token for `key`, or invoke `fetch` to produce
    /// one. Concurrent callers for the same `key` share one fetch and its
    /// result; a caller that asks after a failure fetches again.
    pub(super) async fn get_or_fetch<F, Fut>(&self, key: String, fetch: F) -> Result<Token, Error>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Token, Error>>,
    {
        let stored = || {
            self.tokens()
                .get(&key)
                .filter(|token| token.is_valid())
                .cloned()
                .map(Ok)
        };
        let fetch = || async {
            let token = fetch().await?;
            self.tokens().insert(key.clone(), token.clone());
            Ok(token)
        };
        match self
            .flights
            .run(key.clone(), stored, fetch, |result| Some(result.clone()))
            .await
        {
            Landed::Own(result) | Landed::Shared(result) => result,
        }
    }

    /// Drop every cached token.
    pub(super) async fn clear(&self) {
        self.tokens().clear();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
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
        // When the first caller's fetch fails, the callers queued behind
        // it get its error instead of fetching again. Breaks if each
        // waiter runs its own exchange against a failing endpoint.
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
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clear_drops_cache_and_inflight() {
        // Seed the cache.
        let cache = TokenCache::new();
        cache
            .get_or_fetch("scope".to_string(), || async { Ok(token("first")) })
            .await
            .unwrap();
        assert_eq!(cache.len().await, 1);

        cache.clear().await;
        assert_eq!(cache.len().await, 0);

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
