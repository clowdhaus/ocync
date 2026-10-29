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

/// The live tokens, each stamped with the epoch at which it was stored.
#[derive(Debug, Default)]
struct Tokens {
    by_key: HashMap<String, (Token, u64)>,
    /// Counts the stores, so a fetch can tell whether its key was stored
    /// since it began.
    epoch: u64,
}

/// Coalescing token cache. See the module docs.
///
/// A fetch that lands never overwrites a token stored for its key after the
/// fetch began; the callers that waited on it still get its token.
#[derive(Debug, Default)]
pub(super) struct TokenCache {
    cache: Mutex<Tokens>,
    flights: Flights<String, (u64, Result<Token, Error>)>,
}

impl TokenCache {
    pub(super) fn new() -> Self {
        Self::default()
    }

    fn tokens(&self) -> MutexGuard<'_, Tokens> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Number of entries currently in the token cache. Used for log
    /// diagnostics on invalidation.
    pub(super) fn len(&self) -> usize {
        self.tokens().by_key.len()
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
                .by_key
                .get(&key)
                .map(|(token, _)| token)
                .filter(|token| token.is_valid())
                .cloned()
                // The epoch of a stored token is not read.
                .map(|token| (0, Ok(token)))
        };
        let fetch = || {
            let began = self.tokens().epoch;
            async move { (began, fetch().await) }
        };
        let land = |landed: &(u64, Result<Token, Error>)| {
            let (began, result) = landed;
            if let Ok(token) = result {
                let mut tokens = self.tokens();
                let overtaken = tokens
                    .by_key
                    .get(&key)
                    .is_some_and(|(_, stored_at)| stored_at > began);
                if !overtaken {
                    tokens.epoch += 1;
                    let stored_at = tokens.epoch;
                    tokens
                        .by_key
                        .insert(key.clone(), (token.clone(), stored_at));
                }
            }
            Some(landed.clone())
        };
        match self.flights.run(&key, stored, fetch, land).await {
            Landed::Own((_, result)) | Landed::Shared((_, result)) => result,
        }
    }

    /// Drop every cached token that is `refused`, and if any was, detach
    /// the exchanges in flight: a caller that asks afterwards exchanges
    /// again rather than joining one that may be using the credential
    /// behind the refused token. A detached exchange still serves the
    /// callers waiting on it, and stores its token only if nothing newer is
    /// stored for its scope. Returns whether any token was dropped. When
    /// none was, the cache and the exchanges in flight are left alone.
    pub(super) fn drop_refused(&self, refused: &Token) -> bool {
        let mut dropped = false;
        self.flights.forget(|| {
            self.tokens().by_key.retain(|_, (token, _)| {
                let keep = token != refused;
                dropped |= !keep;
                keep
            });
            dropped
        });
        dropped
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

    /// When an exchange is in flight for a scope whose kept token is
    /// `kept`, run `exchange` for a refusal of `refused`, then ask again:
    /// the exchange count says whether the second ask joined the first.
    async fn ask_during_a_flight(refused: &str) -> (Token, Token, usize) {
        let cache = TokenCache::new();
        let kept = Token::with_ttl("kept", Duration::from_secs(10));
        cache
            .get_or_fetch("scope".to_string(), || async { Ok(kept) })
            .await
            .unwrap();
        let exchanges = std::cell::Cell::new(0);
        let exchange = |value: &'static str| {
            let exchanges = &exchanges;
            async move {
                exchanges.set(exchanges.get() + 1);
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
                Ok(token(value))
            }
        };
        let (first, second) = tokio::join!(
            cache.get_or_fetch("scope".to_string(), || exchange("first")),
            async {
                tokio::task::yield_now().await;
                cache.drop_refused(&token(refused));
                cache
                    .get_or_fetch("scope".to_string(), || exchange("second"))
                    .await
            },
        );
        (first.unwrap(), second.unwrap(), exchanges.get())
    }

    /// Dropping the refused token detaches the exchange in flight, so a
    /// caller that asks afterwards exchanges for itself. Breaks if the drop
    /// leaves the exchange to be joined.
    #[tokio::test(flavor = "current_thread")]
    async fn dropping_the_refused_token_detaches_the_exchange_in_flight() {
        let (first, second, exchanges) = ask_during_a_flight("kept").await;
        assert_eq!(first.value(), "first");
        assert_eq!(second.value(), "second");
        assert_eq!(exchanges, 2);
    }

    /// A refusal of some other token leaves the exchange in flight to be
    /// joined. Breaks if any refusal detaches it, as a stale refusal then
    /// costs an extra exchange.
    #[tokio::test(flavor = "current_thread")]
    async fn a_refusal_of_another_token_leaves_the_exchange_alone() {
        let (first, second, exchanges) = ask_during_a_flight("other").await;
        assert_eq!(first.value(), "first");
        assert_eq!(second.value(), "first");
        assert_eq!(exchanges, 1);
    }

    /// An exchange detached by a drop does not overwrite the token a later
    /// exchange stored for its scope. Breaks if a landing stores over a
    /// token stored after it began.
    #[tokio::test(flavor = "current_thread")]
    async fn a_detached_exchange_does_not_overwrite_a_newer_token() {
        let cache = TokenCache::new();
        let kept = Token::with_ttl("kept", Duration::from_secs(10));
        cache
            .get_or_fetch("scope".to_string(), || async { Ok(kept.clone()) })
            .await
            .unwrap();
        let slow = async {
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
            Ok(token("old"))
        };
        let (old, new) = tokio::join!(cache.get_or_fetch("scope".to_string(), || slow), async {
            tokio::task::yield_now().await;
            cache.drop_refused(&kept);
            cache
                .get_or_fetch("scope".to_string(), || async { Ok(token("new")) })
                .await
        },);
        assert_eq!(old.unwrap().value(), "old");
        assert_eq!(new.unwrap().value(), "new");
        let kept = cache
            .get_or_fetch("scope".to_string(), || async { Ok(token("unexpected")) })
            .await
            .unwrap();
        assert_eq!(kept.value(), "new");
    }

    /// An exchange for one scope that lands after another scope's keeps its
    /// token. Breaks if a store for one scope makes the landing of another
    /// look overtaken, and every caller of it exchanges again.
    #[tokio::test(flavor = "current_thread")]
    async fn a_store_for_another_scope_does_not_overtake_an_exchange() {
        let cache = TokenCache::new();
        let slow = async {
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            Ok(token("a"))
        };
        let (a, b) = tokio::join!(
            cache.get_or_fetch("scope-a".to_string(), || slow),
            cache.get_or_fetch("scope-b".to_string(), || async { Ok(token("b")) }),
        );
        assert_eq!((a.unwrap().value(), b.unwrap().value()), ("a", "b"));
        let exchanges = std::cell::Cell::new(0);
        for scope in ["scope-a", "scope-b"] {
            cache
                .get_or_fetch(scope.to_string(), || async {
                    exchanges.set(exchanges.get() + 1);
                    Ok(token("again"))
                })
                .await
                .unwrap();
        }
        assert_eq!(exchanges.get(), 0);
    }

    /// Only the refused token is dropped. Breaks if a refusal drops every
    /// token, or one that is not the refused one.
    #[tokio::test(flavor = "current_thread")]
    async fn drop_refused_drops_only_the_refused_token() {
        let cache = TokenCache::new();
        for (scope, value) in [("scope-a", "a"), ("scope-b", "b")] {
            cache
                .get_or_fetch(scope.to_string(), || async { Ok(token(value)) })
                .await
                .unwrap();
        }
        assert_eq!(cache.len(), 2);

        assert!(!cache.drop_refused(&token("other")));
        assert_eq!(cache.len(), 2);

        assert!(cache.drop_refused(&token("a")));
        assert_eq!(cache.len(), 1);

        // The refused scope reaches the fetcher again; the other does not.
        let attempts = Arc::new(AtomicUsize::new(0));
        for (scope, expected) in [("scope-a", "second"), ("scope-b", "b")] {
            let attempts = Arc::clone(&attempts);
            let t = cache
                .get_or_fetch(scope.to_string(), || async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Ok(token("second"))
                })
                .await
                .unwrap();
            assert_eq!(t.value(), expected);
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
