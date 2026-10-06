//! The secret face: consuming identities and upstream provider
//! credentials.
//!
//! This module is the only place credential material may live or flow
//! through. The compile-time guard is structural:
//! [`ProviderCredential`] has no accessor for the API key bytes --
//! their single egress point is
//! [`ProviderCredential::auth_headers`], which converts them into the
//! wire family's credential headers on the outgoing upstream request.
//! Nothing else in the crate can read a credential, and the
//! distribution face's types never contain one.
//!
//! Consuming identities carry no gateway-side credential store at all:
//! a tagma presents its platform enrollment token, the archeion's
//! verify-bearer answers `Principal::Tagma`, and the enrollment lookup
//! names the owning account. Nothing is stored here -- not even a
//! hash.

pub mod provider_credential;

use sea_orm::DatabaseConnection;
use sea_orm::prelude::*;

use kallipai_common::authtoken::TokenHash;

use crate::registry::profile;
use crate::state::AppState;

/// A resolved consuming identity: the tagma behind a verified platform
/// token, with the owning account the archeion's enrollment names.
#[derive(Clone, Debug)]
pub struct ResolvedIdentity {
    /// The owning account per the archeion's enrollment, read at
    /// resolution time (the visibility domain's root).
    pub account_id: String,
    /// The tagma the bearer resolved to (the selection store's key;
    /// not credential material, so it prints freely).
    pub tagma_id: String,
}

/// The outcome of resolving a presented bearer. `Unknown` is the one
/// answer for every non-tagma presentation -- an invalid token, a
/// revoked tagma, a disabled owner, a tagma the archeion cannot
/// address -- the bare 401 with no existence oracle.
pub(crate) enum Resolution {
    Resolved(ResolvedIdentity),
    Unknown,
}

/// The process-local identity cache: presented token hash to the last
/// resolved identity. Pure TTL staleness (no local invalidation
/// writer exists); a poisoned lock degrades to a miss. The bound is
/// what makes an authority-side change (revocation, disable) bite
/// within a bounded delay.
#[derive(Default)]
pub(crate) struct IdentityCache {
    inner: std::sync::RwLock<
        std::collections::HashMap<Vec<u8>, (ResolvedIdentity, std::time::Instant)>,
    >,
}

/// How long a cached identity may serve past its authority reads:
/// aligned with the verifier-side enrollment TTL, so an authority-side
/// change lands within a bounded delay.
const IDENTITY_ENTRY_STALENESS_BOUND: std::time::Duration = std::time::Duration::from_secs(60);

impl IdentityCache {
    /// The cached identity for `hash`, if it is still inside the
    /// staleness bound. A poisoned lock or a lapsed entry reads as a
    /// miss: both fall through to the authority, which stays the
    /// truth.
    fn get(&self, hash: &[u8]) -> Option<ResolvedIdentity> {
        self.inner
            .read()
            .ok()?
            .get(hash)
            .filter(|(_, at)| at.elapsed() < IDENTITY_ENTRY_STALENESS_BOUND)
            .map(|(identity, _)| identity.clone())
    }

    /// Warm the cache with a fresh authority resolution.
    fn insert(&self, hash: Vec<u8>, identity: ResolvedIdentity) {
        if let Ok(mut inner) = self.inner.write() {
            inner.insert(hash, (identity, std::time::Instant::now()));
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.read().map(|i| i.len()).unwrap_or(0)
    }
}

/// Resolve a presented platform bearer to the consuming identity:
/// the archeion's verify-bearer names the tagma, the enrollment
/// lookup names its owner, and both reads stay behind the TTL caches
/// (this one for the identity, the verifier's for the enrollment).
/// `Unknown` is the bare 401 (no oracle for guessed tokens or guessed
/// enrollments); a backend fault fails closed as the shared 503.
pub(crate) async fn resolve(
    state: &AppState,
    bearer: &str,
) -> Result<Resolution, kallipai_common::protocol::ApiError> {
    let hash = TokenHash::of(bearer).as_bytes().to_vec();
    if let Some(hit) = state.identity_cache.get(&hash) {
        return Ok(Resolution::Resolved(hit));
    }
    let verifier = match &state.management {
        crate::management::AdminAuth::Disabled => return Ok(Resolution::Unknown),
        crate::management::AdminAuth::Platform(verifier) => verifier,
    };
    let principal = verifier
        .verify_bearer(bearer)
        .await
        .map_err(|_| crate::management::backend_unavailable())?;
    let tagma_id = match principal {
        Some(kallipai_archeion_common::principal::Principal::Tagma(id)) => id.to_string(),
        _ => return Ok(Resolution::Unknown),
    };
    let lookup = match crate::management::enrollment_lookup(state, &tagma_id).await? {
        Some(lookup) => lookup,
        None => return Ok(Resolution::Unknown),
    };
    let identity = ResolvedIdentity {
        account_id: lookup.user_id.to_string(),
        tagma_id,
    };
    state.identity_cache.insert(hash, identity.clone());
    Ok(Resolution::Resolved(identity))
}

/// The provider credential behind a profile: the profile's
/// provider link resolved through the pool (one extra hop on the
/// selection path). The handle stays stable across both spaces.
pub async fn credential_for_profile(
    db: &DatabaseConnection,
    profile: &profile::Model,
) -> Result<Option<ProviderCredential>, DbErr> {
    let Some(provider) =
        crate::registry::provider_by_id(db, &profile.owner, &profile.provider_id).await?
    else {
        return Ok(None);
    };
    let row = provider_credential::Entity::find_by_id((
        profile.owner.clone(),
        profile.provider_id.clone(),
    ))
    .one(db)
    .await?;
    Ok(row.map(|r| ProviderCredential {
        endpoint_prefix: provider.base_url.unwrap_or_default(),
        api_key: r.api_key,
    }))
}

/// A provider credential. The API key bytes are write-only from
/// the crate's perspective: there is deliberately no getter -- the key
/// leaves only through the credential-stamping egress point
/// ([`Self::auth_headers`], which the pingora data plane calls to
/// stamp the upstream request), which converts it into the wire
/// family's credential headers. The endpoint
/// prefix is not secret (it routes the request) and reads freely
/// inside the crate. For the same reason `Debug` is a manual impl: it
/// prints the key as `[REDACTED]`.
#[derive(Clone)]
pub struct ProviderCredential {
    endpoint_prefix: String,
    api_key: String,
}

/// The API version stamp the Anthropic wire expects alongside the
/// key (the same default the client stack pins).
pub const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// The credential header shape a wire family uses: `Authorization:
/// Bearer` for the OpenAI vocabulary, `x-api-key` plus the version
/// header for the Anthropic wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthStyle {
    Bearer,
    XApiKey,
}

impl ProviderCredential {
    /// The upstream URL prefix; the forwarding path appends the wire path.
    pub fn endpoint_prefix(&self) -> &str {
        &self.endpoint_prefix
    }

    /// The credential egress point: the wire family's header pairs,
    /// for callers that stamp headers directly (the pingora data
    /// plane). Bearer for the OpenAI vocabulary, x-api-key plus the
    /// version header for the Anthropic wire.
    pub fn auth_headers(&self, auth: AuthStyle) -> Vec<(&'static str, String)> {
        match auth {
            AuthStyle::Bearer => vec![("authorization", format!("Bearer {}", self.api_key))],
            AuthStyle::XApiKey => vec![
                ("x-api-key", self.api_key.clone()),
                ("anthropic-version", ANTHROPIC_API_VERSION.to_owned()),
            ],
        }
    }
}

impl std::fmt::Debug for ProviderCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderCredential")
            .field("endpoint_prefix", &self.endpoint_prefix)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(account: &str) -> ResolvedIdentity {
        ResolvedIdentity {
            account_id: account.to_owned(),
            tagma_id: format!("{account}-tagma"),
        }
    }

    #[test]
    fn an_entry_expires_past_the_staleness_bound() {
        let cache = IdentityCache::default();
        let hash = TokenHash::of("test-tagma-bearer").as_bytes().to_vec();
        cache.insert(hash.clone(), identity("acct-test"));
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&hash).is_some());
        // Past the bound the entry reads as a miss (the authority is
        // the truth again).
        let mut inner = cache.inner.write().unwrap();
        let (_, at) = inner.get_mut(&hash).expect("entry present");
        *at = std::time::Instant::now()
            - IDENTITY_ENTRY_STALENESS_BOUND
            - std::time::Duration::from_secs(1);
        drop(inner);
        assert!(cache.get(&hash).is_none());
    }
}
