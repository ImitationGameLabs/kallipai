//! The management face: authenticated admin CRUD over the registry and the
//! credential store, with a fail-closed change audit.
//!
//! Two credential families are kept physically apart. The proxy key
//! authorizes the distribution/forwarding faces (resolved against stored
//! key hashes); the management credential authorizes this face. They share
//! no extractor, no route, and no failure path: a management request is
//! authenticated by [`AdminToken`] alone and never consults the proxy-key
//! tables, so no compound judgment can mix the families (a stolen proxy
//! key says nothing to this face and vice versa). Failure vocabulary: a
//! missing/malformed/unknown management credential is 401 from
//! [`AdminToken`]; 403 is the trait contract's "authenticated but not
//! authorized" shape, reserved for future credential implementations with
//! scoped principals -- the static token never yields it.
//!
//! The credential contract is the [`ManagementAuth`] trait; the static
//! management token (injected via process config, read once at boot) is
//! the first implementation. Rotation means restarting the process: the
//! token is hashed into the authenticator at startup and never re-read.
//! With no token configured the face is closed, not absent: [`Disabled`]
//! rejects every request with 401 while the distribution and forwarding
//! faces run unaffected.

pub mod routes;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use crate::state::AppState;
use kallip_common::protocol::ApiError;

/// The management credential contract. `verify` judges one presented
/// bearer: `Ok(())` lets the request through; the carried [`ApiError`] is
/// the route's response (401 unknown/invalid credential, 403 authenticated
/// but unauthorized -- only reachable for implementations that resolve
/// scoped principals, e.g. a future archeion-backed credential).
#[async_trait::async_trait]
pub trait ManagementAuth: Send + Sync {
    async fn verify(&self, bearer: &str) -> Result<(), ApiError>;
}

/// The static management token: one process-wide credential from the boot
/// configuration. Comparison is constant-time over SHA-256 hashes
/// ([`kallip_common::authtoken::TokenHash::ct_eq`]): a timing side channel
/// must not reveal how much of a guess matched.
#[derive(Clone)]
pub struct StaticManagementToken {
    hash: kallip_common::authtoken::TokenHash,
}

impl StaticManagementToken {
    /// Hash the configured token once (boot time); the plaintext is not
    /// retained by this type -- the caller's config string is free to drop.
    pub fn new(token: &str) -> Self {
        Self {
            hash: kallip_common::authtoken::TokenHash::of(token),
        }
    }
}

#[async_trait::async_trait]
impl ManagementAuth for StaticManagementToken {
    async fn verify(&self, bearer: &str) -> Result<(), ApiError> {
        if self
            .hash
            .ct_eq(&kallip_common::authtoken::TokenHash::of(bearer))
        {
            Ok(())
        } else {
            Err(ApiError::unauthorized("invalid management token"))
        }
    }
}

/// The closed face: installed when no management token is configured, so
/// the routes exist (their absence would leak that fact differently) but
/// every request is refused.
pub struct Disabled;

#[async_trait::async_trait]
impl ManagementAuth for Disabled {
    async fn verify(&self, _bearer: &str) -> Result<(), ApiError> {
        Err(ApiError::unauthorized("management face is not configured"))
    }
}

/// Extractor: authenticate the management request or fail closed with 401.
/// Every management route takes this before touching state; there is no
/// unauthenticated management handler by construction.
pub struct AdminToken;

impl FromRequestParts<AppState> for AdminToken {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let bearer = kallip_common::auth_header::extract_bearer_token(&parts.headers)?;
        state.management.verify(bearer).await?;
        Ok(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn verdict(
        auth: std::sync::Arc<dyn ManagementAuth>,
        bearer: &str,
    ) -> Result<(), ApiError> {
        auth.verify(bearer).await
    }

    /// The static token accepts its own credential and nothing else; both
    /// outcomes flow through the dyn trait object (the state shape pins
    /// the object-safety, the assertions pin the vocabulary).
    #[tokio::test]
    async fn static_token_accepts_only_itself() {
        let auth: std::sync::Arc<dyn ManagementAuth> =
            std::sync::Arc::new(StaticManagementToken::new("mgmt-secret-0123"));
        assert!(verdict(auth.clone(), "mgmt-secret-0123").await.is_ok());
        let err = verdict(auth, "mgmt-secret-wrong").await.unwrap_err();
        assert_eq!(err.status, 401);
    }

    /// A differently-sized bearer (shorter than the configured token) is
    /// also a 401: the hash-and-compare path never branches on length.
    #[tokio::test]
    async fn static_token_rejects_short_bearer() {
        let auth: std::sync::Arc<dyn ManagementAuth> =
            std::sync::Arc::new(StaticManagementToken::new("mgmt-secret-0123"));
        let err = verdict(auth, "x").await.unwrap_err();
        assert_eq!(err.status, 401);
    }

    /// The closed face refuses everything with the same 401 vocabulary --
    /// an unconfigured deployment must not answer differently for existing
    /// vs absent credentials.
    #[tokio::test]
    async fn disabled_rejects_everything() {
        let auth: std::sync::Arc<dyn ManagementAuth> = std::sync::Arc::new(Disabled);
        let err = verdict(auth.clone(), "anything").await.unwrap_err();
        assert_eq!(err.status, 401);
        let err = verdict(auth, "").await.unwrap_err();
        assert_eq!(err.status, 401);
    }
}
