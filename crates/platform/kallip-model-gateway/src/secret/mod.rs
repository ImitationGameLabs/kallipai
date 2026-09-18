//! The secret face: proxy keys, their allowed sets, and upstream provider
//! credentials.
//!
//! This module is the only place credential material may live or flow
//! through. The compile-time guard is structural: [`UpstreamCredential`]
//! has no accessor for the API key bytes -- its single consumer is
//! [`UpstreamCredential::inject`], which writes them into an outgoing
//! request's Authorization header. Nothing else in the crate can read a
//! credential, and the distribution face's types never contain one.
//!
//! Proxy keys are stored hash-only ([`kallip_common::authtoken::TokenHash`]
//! bytes in a Postgres `BYTEA`): a presented bearer is hashed and looked
//! up; the plaintext never touches the database.

pub mod proxy_key;
pub mod proxy_key_set;
pub mod tagma;
pub mod upstream_credential;

use sea_orm::prelude::*;
use sea_orm::{ActiveModelTrait, ConnectionTrait, DatabaseConnection, DbErr, Set};
use std::collections::HashMap;

use kallip_common::authtoken::{MintedToken, TokenHash};
use time::OffsetDateTime;

use crate::audit::key_lifecycle_event::{self, Entity as KeyLifecycleEvents};
use proxy_key::Entity as ProxyKeys;
use proxy_key_set::Entity as ProxyKeySets;
use tagma::Entity as Tagmas;

/// A resolved proxy key: the presenting bearer matched a stored hash.
#[derive(Clone, Debug)]
pub struct ResolvedKey {
    /// Carried for the audit rows: the hash identifies the key in
    /// `request_audits` (no plaintext ever returns).
    pub key_hash: Vec<u8>,
    /// Owning tagma identity: the authorization matrix key (the quota
    /// share) and the audit attribution.
    pub tagma_id: String,
    /// The per-tagma side of the matrix, read from the key's `tagmas`
    /// row (the quota share).
    pub tagma_quota: TagmaQuota,
    /// The key's allowed set names (the set-granular authorization unit).
    pub allowed_sets: Vec<String>,
    /// The key's deadline, carried so the cache hit path can
    /// re-check it without a store query.
    pub expires_at: Option<OffsetDateTime>,
}

/// The per-tagma side of the authorization matrix (the quota share),
/// read from the key's `tagmas` row at resolution time.
#[derive(Clone, Debug)]
pub struct TagmaQuota {
    /// BIGINT micro-units of the account currency; the forwarding path
    /// checks it against the audit table's cost sum.
    pub max_budget: Option<i64>,
    pub tpm_limit: Option<i64>,
    pub rpm_limit: Option<i64>,
}

/// The outcome of resolving a presented bearer, split so the
/// distribution face can attach the dedicated rejection codes: a
/// revoked or lapsed key answers with a documented `key_revoked` /
/// `key_expired` error body telling the client to refetch its
/// configuration or mint a fresh key, while an unknown bearer stays
/// the bare 401 (no existence oracle for guessed keys).
#[derive(Clone, Debug)]
pub(crate) enum Resolution {
    /// The bearer matched a live, unexpired key (cache hit or store).
    Resolved(ResolvedKey),
    /// No stored key bears this hash and its lifecycle trail holds no
    /// revoke: presented by nobody, unknown to us.
    Unknown,
    /// The key exists but is past its `expires_at`.
    Expired,
    /// The key was revoked: the row is gone and the lifecycle trail
    /// carries the revoke event.
    Revoked,
}

/// The process-local resolved-key cache: presented hash to last
/// resolved key. Std-only (one `RwLock` around a plain map plus a
/// generation counter): the distribution read path is hot and the
/// authorization matrix changes only through management writes, which
/// clear the whole cache after their transaction commits -- full clear
/// over per-key surgery, the shape that cannot drift from the store.
///
/// The generation defeats the invalidation race: a resolve that reads
/// the store while a management write is in flight captures the older
/// generation, and its backfill is discarded when the generation has
/// moved on -- a store snapshot taken before the write can never
/// outlive the write's invalidation.
#[derive(Default)]
pub(crate) struct KeyCache {
    inner: std::sync::RwLock<CacheInner>,
}

#[derive(Default)]
struct CacheInner {
    entries: HashMap<Vec<u8>, ResolvedKey>,
    generation: u64,
}

impl KeyCache {
    /// The cached resolution for `hash`, if any. A poisoned lock
    /// degrades to a miss: the caller falls through to the store,
    /// which stays the source of truth.
    fn get(&self, hash: &[u8]) -> Option<ResolvedKey> {
        self.inner.read().ok()?.entries.get(hash).cloned()
    }

    /// The generation to capture before a store read. `None` = the
    /// lock is poisoned: the matching backfill will be dropped (never
    /// backfill blind).
    fn generation(&self) -> Option<u64> {
        Some(self.inner.read().ok()?.generation)
    }

    /// Warm the cache with a fresh store resolution. The backfill is
    /// dropped when `generation` no longer matches the current one:
    /// the store snapshot the resolution was read under predates a
    /// management write that has since cleared the cache, and a key it
    /// saw alive may be revoked now.
    fn insert(&self, resolved: ResolvedKey, generation: Option<u64>) {
        let Some(generation) = generation else {
            return;
        };
        if let Ok(mut inner) = self.inner.write()
            && inner.generation == generation
        {
            inner.entries.insert(resolved.key_hash.clone(), resolved);
        }
    }

    /// Evict one hash (a cache hit that just crossed its deadline).
    /// Racing a concurrent clear can only drop a newer entry, which is
    /// a cache miss, never a wrong answer.
    fn remove(&self, hash: &[u8]) {
        if let Ok(mut inner) = self.inner.write() {
            inner.entries.remove(hash);
        }
    }

    /// Drop every cached resolution. Every management write calls this
    /// once its transaction has committed, so a revoke, a rotation, or
    /// an allowed-set change bites on the next request with no TTL to
    /// wait out.
    pub(crate) fn clear(&self) {
        if let Ok(mut inner) = self.inner.write() {
            inner.generation += 1;
            inner.entries.clear();
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.read().map(|i| i.entries.len()).unwrap_or(0)
    }
}

/// Hash-and-resolve a presented bearer token against the cache and the
/// store. `Resolved` continues; `Expired` and `Revoked` carry the
/// dedicated codes; `Unknown` is the bare 401 the extractor emits. A
/// store hit that resolves warms the cache; a cache hit re-checks
/// `expires_at` on the spot and evicts itself when lapsed, so a cached
/// key dies on the first request past its deadline.
pub(crate) async fn resolve(
    db: &DatabaseConnection,
    cache: &KeyCache,
    bearer: &str,
) -> Result<Resolution, DbErr> {
    let hash = TokenHash::of(bearer).as_bytes().to_vec();
    if let Some(hit) = cache.get(&hash) {
        return Ok(match hit.expires_at {
            Some(expires_at) if expires_at <= OffsetDateTime::now_utc() => {
                cache.remove(&hash);
                Resolution::Expired
            }
            _ => Resolution::Resolved(hit),
        });
    }
    // Capture the generation before the store read: a management write
    // that commits while this read is in flight bumps it, and the
    // backfill at the bottom of this function is discarded instead of
    // resurrecting a key the write just killed.
    let generation = cache.generation();
    let Some(key) = ProxyKeys::find_by_id(hash.clone()).one(db).await? else {
        // The hash has no proxy_keys row. A revoke deletes the row and
        // leaves the lifecycle trail, so the trail decides: a revoke
        // event means this key was ours and is now dead (the dedicated
        // code); no trail at all means the bearer is unknown.
        let revoked = KeyLifecycleEvents::find()
            .filter(key_lifecycle_event::Column::KeyHash.eq(hash.clone()))
            .filter(key_lifecycle_event::Column::Event.eq("revoked"))
            .one(db)
            .await?
            .is_some();
        return Ok(if revoked {
            Resolution::Revoked
        } else {
            Resolution::Unknown
        });
    };

    // A lapsed key resolves as expired: expiry is a derived state read
    // here, never written by this path (only mint and revoke write
    // lifecycle events).
    if let Some(expires_at) = key.expires_at
        && expires_at <= OffsetDateTime::now_utc()
    {
        return Ok(Resolution::Expired);
    }
    let rows = ProxyKeySets::find()
        .filter(proxy_key_set::Column::KeyHash.eq(hash.clone()))
        .all(db)
        .await?;
    let quota = Tagmas::find_by_id(&key.tagma_id)
        .one(db)
        .await?
        // The FK makes a miss unreachable while the schema is consistent;
        // if it happens anyway, the matrix check cannot run, so resolve
        // fails closed (the caller maps the error to a 500 and denies).
        .ok_or_else(|| DbErr::Custom("proxy key references a missing tagmas row".to_owned()))?;
    let resolved = ResolvedKey {
        key_hash: hash.clone(),
        tagma_id: key.tagma_id,
        tagma_quota: TagmaQuota {
            max_budget: quota.max_budget,
            tpm_limit: quota.tpm_limit,
            rpm_limit: quota.rpm_limit,
        },
        allowed_sets: rows.into_iter().map(|r| r.set_name).collect(),
        expires_at: key.expires_at,
    };
    cache.insert(resolved.clone(), generation);
    Ok(Resolution::Resolved(resolved))
}

/// Mint a proxy key: generate the bearer (`sk-proxy-` family), land the
/// hash-only row and its allowed-set grants inside the caller's
/// transaction, and hand the plaintext back for the one-time response.
/// The audit rows (lifecycle + management events) are the admin face's
/// side of the same transaction -- the credential-material step and the
/// store step both live here so the plaintext never leaves this module.
pub(crate) async fn mint_key<C: ConnectionTrait>(
    txn: &C,
    tagma_id: &str,
    allowed_sets: &[String],
    expires_at: Option<OffsetDateTime>,
) -> Result<MintedKey, DbErr> {
    let token = MintedToken::generate(proxy_key::PROXY_KEY);
    let key_hash = token.hash().as_bytes().to_vec();
    let created_at = OffsetDateTime::now_utc();
    proxy_key::ActiveModel {
        key_hash: Set(key_hash.clone()),
        tagma_id: Set(tagma_id.to_owned()),
        created_at: Set(created_at),
        expires_at: Set(expires_at),
    }
    .insert(txn)
    .await?;
    for set_name in allowed_sets {
        proxy_key_set::ActiveModel {
            key_hash: Set(key_hash.clone()),
            set_name: Set(set_name.clone()),
        }
        .insert(txn)
        .await?;
    }
    Ok(MintedKey {
        token,
        key_hash,
        created_at,
    })
}

/// The mint outcome: the bearer (`.secret()` = the one-time plaintext,
/// `.hash()` = the stored bytes), the hash written, and the issuance
/// time to echo in the response.
pub(crate) struct MintedKey {
    pub token: MintedToken,
    pub key_hash: Vec<u8>,
    pub created_at: OffsetDateTime,
}

/// The pre-deletion state of a revoked key: what the audit rows record
/// (the grant rows vanish with the delete's cascade, so this is their
/// trace).
pub(crate) struct RevokedKey {
    pub tagma_id: String,
    pub allowed_sets: Vec<String>,
    /// Issuance time and expiry as the row held them (the audit view is
    /// the metadata mirror of the list face).
    pub created_at: OffsetDateTime,
    pub expires_at: Option<OffsetDateTime>,
}

/// Revoke a proxy key by hash: delete the row (the allowed-set grants
/// cascade) inside the caller's transaction and return the row state for
/// the audit rows. `None` = no such key -- including the interleaving
/// where a concurrent revoke deleted the row after this call's initial
/// read: deleting zero rows is reported as `None`, never as a second
/// success (a second `revoked` audit row would be a lie). Presenting
/// the bearer afterward answers the dedicated `key_revoked` code.
pub(crate) async fn revoke_key<C: ConnectionTrait>(
    txn: &C,
    key_hash: &[u8],
) -> Result<Option<RevokedKey>, DbErr> {
    let Some(key) = ProxyKeys::find_by_id(key_hash.to_vec()).one(txn).await? else {
        return Ok(None);
    };
    let rows = ProxyKeySets::find()
        .filter(proxy_key_set::Column::KeyHash.eq(key_hash))
        .all(txn)
        .await?;
    let revoked = RevokedKey {
        tagma_id: key.tagma_id.clone(),
        allowed_sets: rows.into_iter().map(|r| r.set_name).collect(),
        created_at: key.created_at,
        expires_at: key.expires_at,
    };
    let result = ProxyKeys::delete_by_id(key_hash.to_vec()).exec(txn).await?;
    if result.rows_affected == 0 {
        return Ok(None);
    }
    Ok(Some(revoked))
}

/// The upstream provider credential for `profile_id`.
pub async fn credential_for_profile(
    db: &DatabaseConnection,
    profile_id: &str,
) -> Result<Option<UpstreamCredential>, DbErr> {
    let row = upstream_credential::Entity::find_by_id(profile_id)
        .one(db)
        .await?;
    Ok(row.map(|r| UpstreamCredential {
        endpoint_prefix: r.upstream_base_url,
        api_key: r.upstream_api_key,
    }))
}

/// An upstream provider credential. The API key bytes are write-only from
/// the crate's perspective: there is deliberately no getter -- the only
/// consumer is [`Self::inject`], which converts them into an Authorization
/// header on the forwarding request. The endpoint prefix is not secret
/// (it routes the request) and reads freely inside the crate. For the
/// same reason `Debug` is a manual impl: it prints the key as `[REDACTED]`.
#[derive(Clone)]
pub struct UpstreamCredential {
    endpoint_prefix: String,
    api_key: String,
}

impl UpstreamCredential {
    /// The upstream URL prefix; the forwarding path appends the wire path.
    pub fn endpoint_prefix(&self) -> &str {
        &self.endpoint_prefix
    }

    /// The sole credential egress point: stamp the Authorization header.
    pub fn inject(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.bearer_auth(&self.api_key)
    }
}

impl std::fmt::Debug for UpstreamCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamCredential")
            .field("endpoint_prefix", &self.endpoint_prefix)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TEST_BEARER, migrated_test_db, seed_registry};
    use kallip_common::authtoken::TokenHash;
    use sea_orm::Set;

    /// The invalidation race, pinned
    /// deterministically: a resolution whose store snapshot predates a
    /// management write captures a generation the write's clear
    /// invalidates, and its backfill is discarded -- a key the write
    /// killed cannot re-enter the cache. The live interleaving this
    /// guards (SELECT in flight, insert after clear) is covered
    /// end-to-end by `revoked_key_answers_with_the_dedicated_code_
    /// through_a_warm_cache` on the management path; this unit test
    /// pins the mechanism itself, which a scheduler-dependent sleep
    /// race cannot do reliably.
    #[test]
    fn stale_generation_backfill_is_discarded() {
        let cache = KeyCache::default();
        let resolved = ResolvedKey {
            key_hash: b"stale-hash".to_vec(),
            tagma_id: "tagma-under-test".to_owned(),
            tagma_quota: TagmaQuota {
                max_budget: None,
                tpm_limit: None,
                rpm_limit: None,
            },
            allowed_sets: vec!["alpha".to_owned()],
            expires_at: None,
        };
        let generation = cache.generation();
        // The management write commits and clears mid-flight.
        cache.clear();
        cache.insert(resolved.clone(), generation);
        assert_eq!(cache.len(), 0, "pre-write snapshot must not backfill");
        // A resolution read after the clear (current generation) warms.
        cache.insert(resolved, cache.generation());
        assert_eq!(cache.len(), 1);
    }

    /// Expiry is a derived state: NULL never expires, a lapsed key
    /// resolves as expired (the dedicated `key_expired` code), a future
    /// stamp resolves, and the resolution face never writes lifecycle
    /// rows.
    #[tokio::test]
    async fn expiry_is_derived_at_resolution_time() {
        let db = migrated_test_db().await;
        seed_registry(&db, "https://api.upstream.test").await;
        // Seeded keys carry NULL: never expires.
        assert!(matches!(
            resolve(&db, &KeyCache::default(), TEST_BEARER)
                .await
                .expect("resolve"),
            Resolution::Resolved(_)
        ));
        let hash = TokenHash::of(TEST_BEARER).as_bytes().to_vec();
        // Lapse the key.
        let patch = proxy_key::ActiveModel {
            key_hash: Set(hash.clone()),
            expires_at: Set(Some(
                OffsetDateTime::now_utc() - time::Duration::seconds(60),
            )),
            ..Default::default()
        };
        patch.update(&db).await.expect("lapse");
        assert!(matches!(
            resolve(&db, &KeyCache::default(), TEST_BEARER)
                .await
                .expect("resolve"),
            Resolution::Expired
        ));
        // Renew: resolves again.
        let patch = proxy_key::ActiveModel {
            key_hash: Set(hash),
            expires_at: Set(Some(OffsetDateTime::now_utc() + time::Duration::days(30))),
            ..Default::default()
        };
        patch.update(&db).await.expect("renew");
        assert!(matches!(
            resolve(&db, &KeyCache::default(), TEST_BEARER)
                .await
                .expect("resolve"),
            Resolution::Resolved(_)
        ));
        // None of this touched the lifecycle trail.
        let events = crate::audit::key_lifecycle_event::Entity::find()
            .all(&db)
            .await
            .expect("rows");
        assert!(events.is_empty(), "resolution never writes lifecycle rows");
    }
}
