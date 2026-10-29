//! Recovery from a token endpoint that refuses the cloud credential an
//! exchange used, shared by the Google, ECR Public and ACR providers.
//!
//! A credential revoked while kept and still fresh is refused by every
//! exchange that uses it. Each such exchange drops the credential if it is
//! still the one kept and is sent once more with the current one, so a wave
//! of requests refused with one credential costs one refresh. A credential
//! that was refused although it was fetched fresh for the retry means the
//! identity is refused, not the credential: it is remembered, and exchanges
//! that use it again are not retried until a refresh yields a different one.

use std::future::Future;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::error::Error;

/// What a failed exchange says of the cloud credential it used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Judgment {
    /// The token endpoint refused the credential.
    Refused,
    /// Any other failure, such as a scope the identity cannot reach: the
    /// credential is not judged.
    NotJudged,
}

/// The judgment of the status a token endpoint answered with: 401 refused
/// the credentials sent, and any other status, such as a 403 or 404 for one
/// repository, judges nothing.
pub(super) fn judge_status(status: http::StatusCode) -> Judgment {
    if status == http::StatusCode::UNAUTHORIZED {
        Judgment::Refused
    } else {
        Judgment::NotJudged
    }
}

/// The judgment of a failed OCI token exchange, by the token endpoint's
/// answer.
pub(super) fn judge_token_endpoint(error: &Error) -> Judgment {
    match error {
        Error::TokenEndpoint { status, .. } => judge_status(*status),
        _ => Judgment::NotJudged,
    }
}

/// The recovery of one provider's exchanges with one kind of cloud
/// credential `C`.
#[derive(Debug)]
pub(super) struct Recovery<C> {
    /// The credential the token endpoint refused although it was fetched
    /// fresh for the retry.
    refused_fresh: Mutex<Option<C>>,
}

impl<C> Default for Recovery<C> {
    fn default() -> Self {
        Self {
            refused_fresh: Mutex::new(None),
        }
    }
}

impl<C: Clone + PartialEq> Recovery<C> {
    fn refused_fresh(&self) -> MutexGuard<'_, Option<C>> {
        self.refused_fresh
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Run `exchange` with the credential `credential` yields. If it is
    /// judged [`Judgment::Refused`] by `judge`, `refuse` the credential used
    /// and run `exchange` once more with the one `credential` yields now:
    /// the current one if another exchange already replaced it. A retry that
    /// is refused too remembers its credential, and a later exchange refused
    /// with that credential is not retried. `E` carries what `exchange`
    /// fails with, and a failure to get the credential.
    pub(super) async fn exchange<R, E, CredentialFuture, ExchangeFuture>(
        &self,
        credential: impl Fn() -> CredentialFuture,
        exchange: impl Fn(C) -> ExchangeFuture,
        judge: impl Fn(&E) -> Judgment,
        refuse: impl FnOnce(&C),
    ) -> Result<R, E>
    where
        E: From<Error>,
        CredentialFuture: Future<Output = Result<C, Error>>,
        ExchangeFuture: Future<Output = Result<R, E>>,
    {
        let used = credential().await?;
        let refused = match exchange(used.clone()).await {
            Err(failure) if judge(&failure) == Judgment::Refused => failure,
            exchanged => return exchanged,
        };
        if self.is_refused_fresh(&used) {
            return Err(refused);
        }
        refuse(&used);
        let fresh = credential().await?;
        // Sending the credential that was just refused again, or one known
        // to be refused, would only repeat the refusal.
        if fresh == used || self.is_refused_fresh(&fresh) {
            *self.refused_fresh() = Some(fresh);
            return Err(refused);
        }
        let retried = exchange(fresh.clone()).await;
        if matches!(&retried, Err(failure) if judge(failure) == Judgment::Refused) {
            *self.refused_fresh() = Some(fresh);
        }
        retried
    }

    fn is_refused_fresh(&self, credential: &C) -> bool {
        self.refused_fresh().as_ref() == Some(credential)
    }
}
