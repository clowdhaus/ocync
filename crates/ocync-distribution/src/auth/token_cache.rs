//! Coalescing token cache shared by every Bearer-token auth provider.
//!
//! The live tokens are kept by scope. A fetch for a scope runs once
//! however many callers need it: the callers that ask while it is in
//! flight get its result, a failure included, so a failing endpoint costs
//! one exchange per wave of callers, not one per caller. Callers for
//! distinct scopes proceed in parallel. The cache also owns the registry's
//! cached `WWW-Authenticate` challenge, which a refused token calls into
//! question along with the tokens.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::Token;
use super::token_exchange::ChallengeCache;
use crate::error::Error;
use crate::flight::{Flights, Landed};

/// Coalescing token cache. See the module docs.
#[derive(Debug, Default)]
pub(super) struct TokenCache {
    tokens: Mutex<HashMap<String, Token>>,
    flights: Flights<String, Result<Token, Error>>,
    challenge: ChallengeCache,
}

impl TokenCache {
    pub(super) fn new() -> Self {
        Self::default()
    }

    fn tokens(&self) -> MutexGuard<'_, HashMap<String, Token>> {
        self.tokens.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The registry's cached challenge.
    pub(super) fn challenge(&self) -> &ChallengeCache {
        &self.challenge
    }

    /// Number of tokens currently cached.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
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
        let land = |result: &Result<Token, Error>| {
            if let Ok(token) = result {
                self.tokens().insert(key.clone(), token.clone());
            }
            Some(result.clone())
        };
        match self.flights.run(&key, stored, fetch, land).await {
            Landed::Own(result) | Landed::Shared(result) => result,
        }
    }

    /// The registry refused `refused`. Drop every cached token that sends
    /// the same header, and the challenge if any was. A token that has
    /// replaced the refused one is kept. An exchange in flight is left
    /// alone: it mints a token with a credential the registry did not
    /// judge, and the callers that ask meanwhile join it.
    pub(super) async fn refuse(&self, refused: &Token) {
        let dropped = {
            let mut tokens = self.tokens();
            let before = tokens.len();
            tokens.retain(|_, token| !token.sends_same_header(refused));
            before - tokens.len()
        };
        tracing::debug!(dropped, "registry refused a token");
        if dropped > 0 {
            self.challenge.clear().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::sync::Barrier;

    use super::super::token_exchange::WwwAuthenticate;
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

    /// A token stale enough that its scope is exchanged again.
    fn stale(value: &str) -> Token {
        Token::with_ttl(value, Duration::from_secs(10))
    }

    /// Yield `count` times, then answer `value`.
    async fn after(count: usize, value: &'static str) -> Result<Token, Error> {
        for _ in 0..count {
            tokio::task::yield_now().await;
        }
        Ok(token(value))
    }

    /// A refusal that drops the kept token leaves the exchange in flight,
    /// and a caller that asks afterwards joins it. Breaks if a refusal
    /// detaches the exchange, and the caller exchanges again.
    #[tokio::test(flavor = "current_thread")]
    async fn a_refusal_leaves_the_exchange_in_flight_to_be_joined() {
        let cache = TokenCache::new();
        cache
            .get_or_fetch("scope".to_string(), || async { Ok(stale("kept")) })
            .await
            .unwrap();
        let exchanges = std::cell::Cell::new(0);
        let exchange = |value: &'static str| {
            let exchanges = &exchanges;
            async move {
                exchanges.set(exchanges.get() + 1);
                after(2, value).await
            }
        };
        let (first, second) = tokio::join!(
            cache.get_or_fetch("scope".to_string(), || exchange("first")),
            async {
                tokio::task::yield_now().await;
                cache.refuse(&stale("kept")).await;
                cache
                    .get_or_fetch("scope".to_string(), || exchange("second"))
                    .await
            },
        );
        assert_eq!(first.unwrap().value(), "first");
        assert_eq!(second.unwrap().value(), "first");
        assert_eq!(exchanges.get(), 1);
        let kept = cache
            .get_or_fetch("scope".to_string(), || after(0, "unexpected"))
            .await
            .unwrap();
        assert_eq!(kept.value(), "first");
    }

    /// Only the refused token is dropped. Breaks if a refusal drops every
    /// token, or one that is not the refused one.
    #[tokio::test(flavor = "current_thread")]
    async fn a_refusal_drops_only_the_refused_token() {
        let cache = TokenCache::new();
        for (scope, value) in [("scope-a", "a"), ("scope-b", "b")] {
            cache
                .get_or_fetch(scope.to_string(), || async { Ok(token(value)) })
                .await
                .unwrap();
        }
        assert_eq!(cache.len(), 2);

        cache.refuse(&token("other")).await;
        assert_eq!(cache.len(), 2);

        cache.refuse(&token("a")).await;
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

    /// A refusal that matches a token drops the challenge, and one that
    /// matches none keeps it. Breaks if either clears it wrongly.
    #[tokio::test(flavor = "current_thread")]
    async fn a_refusal_drops_the_challenge_only_when_a_token_matched() {
        let cache = TokenCache::new();
        cache
            .get_or_fetch("scope".to_string(), || async { Ok(token("kept")) })
            .await
            .unwrap();
        cache
            .challenge()
            .set(WwwAuthenticate {
                realm: "https://auth.example/token".to_owned(),
                service: None,
            })
            .await;

        cache.refuse(&token("other")).await;
        assert!(cache.challenge().get().await.is_some());
        cache.refuse(&token("kept")).await;
        assert!(cache.challenge().get().await.is_none());
    }
}
