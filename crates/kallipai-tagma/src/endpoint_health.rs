//! The shared upstream-fact table (see `kallipai_common::facts`): one
//! tagma-wide view of which endpoints and accounts the upstream is refusing,
//! keyed by an identity only this module computes.
//!
//! The key is a credential fingerprint — the first 16 hex of a SHA-256 hash of the
//! API key, through the shared [`TokenHash`] core. The key itself is never stored,
//! never persisted and never sent; readers see an 8-hex hint of the fingerprint.
//! Facts are keyed at two levels:
//! account-level ones follow the credential (every endpoint on it is affected),
//! endpoint-level ones name one endpoint of that credential.
//!
//! Fail-open by construction: a provider id the live config no longer knows is
//! dropped, and every read is a snapshot of what is readable *now* — an expired
//! fact simply is not there. Nothing in this module can fail a round.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use kallipai_adk::profile::Provider;
use kallipai_common::authtoken::TokenHash;
use kallipai_common::facts::{
    Block, BlockedFact, EndpointHealth, EndpointRef, FACT_TABLE_CAPACITY, FactView, display_until,
    is_active,
};
use tracing::debug;

use crate::state::ProfileBundle;

/// A credential's stable identity inside this process. Derives `Hash`/`Eq` so
/// it can key the table; those compares run over a hash, not over the secret.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CredentialFingerprint(String);

impl CredentialFingerprint {
    fn of(api_key: &str) -> Self {
        let digest = TokenHash::of(api_key);
        Self(hex(digest.as_bytes())[..16].to_string())
    }

    /// The 8-hex face of the fingerprint: all a reader outside this module sees.
    fn hint(&self) -> String {
        self.0[..8].to_string()
    }
}

/// The two levels a fact can be keyed at.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum FactKey {
    /// The credential alone: the fact is about the account, not one endpoint.
    Account(CredentialFingerprint),
    /// The credential plus the endpoint it was used against.
    Endpoint(CredentialFingerprint, EndpointRef),
}

/// The tagma-wide fact table. One instance per tagma, shared by every agent
/// (cloned into each `AgentContext`) and read by the status surfaces.
pub struct EndpointHealthTable {
    /// The live profile bundle: the provider registry the keys are computed
    /// from. Loaded per call, so a `PUT /profiles` swap is picked up.
    profiles: Arc<ArcSwap<ProfileBundle>>,
    facts: Mutex<HashMap<FactKey, Block>>,
}

impl EndpointHealthTable {
    /// The one construction point (the tagma's `AppState`).
    pub fn new(profiles: Arc<ArcSwap<ProfileBundle>>) -> Arc<Self> {
        Arc::new(Self {
            profiles,
            facts: Mutex::new(HashMap::new()),
        })
    }

    /// The provider this id names, against the live bundle. `None` after a
    /// profile swap retired the provider — the caller drops the fact.
    fn provider(&self, provider_id: &str) -> Option<Provider> {
        self.profiles
            .load()
            .config
            .endpoints
            .get(provider_id)
            .cloned()
    }

    /// The two keys one provider maps to: its account, and one of its
    /// endpoints. Computed here and nowhere else.
    fn keys_for(&self, provider_id: &str) -> Option<(FactKey, FactKey)> {
        let provider = self.provider(provider_id)?;
        let fingerprint = CredentialFingerprint::of(&provider.api_key);
        let endpoint = EndpointRef {
            family: provider.family.clone(),
            base_url: provider.base_url.clone(),
        };
        Some((
            FactKey::Account(fingerprint.clone()),
            FactKey::Endpoint(fingerprint, endpoint),
        ))
    }

    /// The table lock, poison-tolerant: a panicking writer must not be able to
    /// disable the table for the rest of the process (same recovery as
    /// `TokenBudget`).
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<FactKey, Block>> {
        self.facts.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl EndpointHealth for EndpointHealthTable {
    fn record(&self, provider_id: &str, block: Block) {
        let Some((account, endpoint)) = self.keys_for(provider_id) else {
            // Fail-open: a fact nobody can be keyed against is dropped rather
            // than guessed at.
            debug!(
                provider = provider_id,
                "no such provider in the live config: dropping upstream fact"
            );
            return;
        };
        let key = if block.kind.is_account_level() {
            account
        } else {
            endpoint
        };
        insert_fact(&mut self.lock(), key, block);
    }

    fn blocked_for(&self, provider_id: &str, now: u64) -> Option<BlockedFact> {
        let (account, endpoint) = self.keys_for(provider_id)?;
        let facts = self.lock();
        // Account level first: a credential-wide fact answers for every
        // endpoint on it, so it outranks the endpoint's own.
        for key in [account, endpoint] {
            let Some(block) = facts.get(&key) else {
                continue;
            };
            // The one hit predicate, applied on the read side: an expired
            // fact is not there.
            if !is_active(block.recorded_at, block.until, now) {
                continue;
            }
            return Some(BlockedFact {
                block: block.clone(),
                account_hint: key_fingerprint(&key).hint(),
            });
        }
        None
    }

    fn snapshot(&self, now: u64) -> Vec<FactView> {
        let mut views: Vec<FactView> = self
            .lock()
            .iter()
            .filter_map(|(key, block)| {
                // The one hit predicate, applied on the read side: what is not
                // readable now never reaches a surface.
                if !is_active(block.recorded_at, block.until, now) {
                    return None;
                }
                let (endpoint, account_hint) = match key {
                    FactKey::Account(fingerprint) => (None, fingerprint.hint()),
                    FactKey::Endpoint(fingerprint, endpoint) => {
                        (Some(endpoint.clone()), fingerprint.hint())
                    }
                };
                Some(FactView {
                    kind: block.kind,
                    until: display_until(block.recorded_at, block.until),
                    detail: block.detail.clone(),
                    recorded_at: block.recorded_at,
                    endpoint,
                    account_hint,
                })
            })
            .collect();
        // Newest first, then the rest of the key so two facts recorded in the
        // same second still render in one order: the account hint, the kind, and
        // the endpoint the fact names. Distinct keys differ in one of them, short
        // of a hint collision: two facts of one kind in the same second, naming
        // the same endpoint or both account-level, whose 8-hex account hints
        // match compare equal and keep the source map's iteration order — an
        // arbitrary per-process order, not one stable across runs.
        views.sort_by(|a, b| {
            b.recorded_at
                .cmp(&a.recorded_at)
                .then_with(|| a.account_hint.cmp(&b.account_hint))
                .then_with(|| a.kind.as_str().cmp(b.kind.as_str()))
                .then_with(|| {
                    endpoint_order(a.endpoint.as_ref()).cmp(&endpoint_order(b.endpoint.as_ref()))
                })
        });
        views
    }
}

/// The endpoint a view names, in a form that orders: an account-level fact
/// (`None`) sorts first, then the endpoint's family and base URL.
fn endpoint_order(endpoint: Option<&EndpointRef>) -> (Option<&str>, Option<&str>) {
    match endpoint {
        Some(endpoint) => (Some(endpoint.family.as_str()), endpoint.base_url.as_deref()),
        None => (None, None),
    }
}

/// Insert one fact, evicting the oldest by `recorded_at` when a new key arrives
/// at capacity. Free-standing so the capacity rule is testable without a
/// profile bundle behind it.
fn insert_fact(facts: &mut HashMap<FactKey, Block>, key: FactKey, block: Block) {
    if !facts.contains_key(&key)
        && facts.len() >= FACT_TABLE_CAPACITY
        && let Some(oldest) = facts
            .iter()
            .min_by_key(|(_, block)| block.recorded_at)
            .map(|(key, _)| key.clone())
    {
        facts.remove(&oldest);
    }
    facts.insert(key, block);
}

/// The credential fingerprint a key is built on, whichever level it names:
/// the account hint reads it.
fn key_fingerprint(key: &FactKey) -> &CredentialFingerprint {
    match key {
        FactKey::Account(fingerprint) | FactKey::Endpoint(fingerprint, _) => fingerprint,
    }
}

/// Lowercase hex of a digest (the house helper shape).
fn hex(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use just_llm_client::family;
    use kallipai_adk::profile::{Profile, ProfileConfig, ProfileRegistry, ProfileSet};
    use kallipai_common::facts::{BlockKind, FACT_TTL_MAX_SECS, QUOTA_RESET_TRUST_MAX_SECS};
    use std::collections::BTreeMap;

    const T0: u64 = 1_700_000_000;
    /// sha256("sk-shared-secret") — asserted, not derived, so a change to the
    /// fingerprint recipe fails the test instead of silently re-keying facts.
    const SHARED_HINT: &str = "344a49b6";

    /// A bundle over the given providers, each in its own one-profile set.
    fn bundle(providers: &[(&str, &str, &str)]) -> Arc<ArcSwap<ProfileBundle>> {
        let mut endpoints = HashMap::new();
        let mut profiles = Vec::new();
        for (id, api_key, base_url) in providers {
            endpoints.insert(
                (*id).to_string(),
                Provider {
                    id: (*id).to_string(),
                    family: family::OPENAI_COMPATIBLE.into(),
                    api_key: (*api_key).to_string(),
                    base_url: Some((*base_url).to_string()),
                    model_list_base_url: None,
                },
            );
            profiles.push(Profile {
                id: (*id).to_string(),
                endpoint: (*id).to_string(),
                model: "test".into(),
                max_context_window: 128_000,
                store: None,
                effort: None,
                modalities: Profile::default_modalities(),
            });
        }
        let cfg = ProfileConfig {
            sets: BTreeMap::from([(
                "default".to_string(),
                ProfileSet {
                    name: "default".into(),
                    description: None,
                    profiles,
                },
            )]),
            default: "default".into(),
            endpoints,
            parking: vec![],
        };
        let source = crate::backend::build_backends(
            &cfg,
            just_llm_client::client::BackendFactory::new(),
            crate::backend::DEFAULT_USER_AGENT,
        )
        .expect("test backends build");
        let registry = Arc::new(ProfileRegistry::new(cfg.sets.clone(), source).expect("registry"));
        Arc::new(ArcSwap::from_pointee(ProfileBundle {
            config: cfg,
            registry,
        }))
    }

    fn block(kind: BlockKind, recorded_at: u64, until: Option<u64>) -> Block {
        Block {
            kind,
            until,
            detail: "HTTP 429".into(),
            status: None,
            recorded_at,
        }
    }

    #[test]
    fn an_account_level_fact_is_keyed_by_the_credential_alone() {
        let table =
            EndpointHealthTable::new(bundle(&[("a", "sk-shared-secret", "https://a.example")]));
        table.record("a", block(BlockKind::Quota, T0, None));

        let views = table.snapshot(T0);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].kind, BlockKind::Quota);
        assert_eq!(views[0].account_hint, SHARED_HINT);
        assert!(
            views[0].endpoint.is_none(),
            "an account-level fact is not about one endpoint"
        );
    }

    #[test]
    fn an_endpoint_level_fact_names_its_endpoint() {
        let table =
            EndpointHealthTable::new(bundle(&[("a", "sk-shared-secret", "https://a.example")]));
        table.record("a", block(BlockKind::EndpointAuth, T0, None));

        let views = table.snapshot(T0);
        assert_eq!(views.len(), 1);
        assert_eq!(
            views[0].endpoint,
            Some(EndpointRef {
                family: family::OPENAI_COMPATIBLE.into(),
                base_url: Some("https://a.example".into()),
            })
        );
    }

    #[test]
    fn shared_credentials_share_the_account_key_only() {
        let table = EndpointHealthTable::new(bundle(&[
            ("a", "sk-shared-secret", "https://a.example"),
            ("b", "sk-shared-secret", "https://b.example"),
            ("c", "sk-other-secret", "https://a.example"),
        ]));
        let (account_a, endpoint_a) = table.keys_for("a").expect("a resolves");
        let (account_b, endpoint_b) = table.keys_for("b").expect("b resolves");
        let (account_c, _) = table.keys_for("c").expect("c resolves");

        assert_eq!(account_a, account_b, "one credential, one account key");
        assert_ne!(
            endpoint_a, endpoint_b,
            "different endpoints, different keys"
        );
        assert_ne!(
            account_a, account_c,
            "different credentials, different keys"
        );
    }

    #[test]
    fn an_expired_fact_is_not_snapshotted() {
        let table =
            EndpointHealthTable::new(bundle(&[("a", "sk-shared-secret", "https://a.example")]));
        table.record("a", block(BlockKind::Quota, T0, None));

        assert_eq!(table.snapshot(T0 + FACT_TTL_MAX_SECS - 1).len(), 1);
        assert!(table.snapshot(T0 + FACT_TTL_MAX_SECS).is_empty());
    }

    #[test]
    fn a_reset_beyond_the_trust_bound_displays_as_unknown() {
        let table =
            EndpointHealthTable::new(bundle(&[("a", "sk-shared-secret", "https://a.example")]));
        let far = T0 + QUOTA_RESET_TRUST_MAX_SECS + 60;
        table.record("a", block(BlockKind::Quota, T0, Some(far)));
        assert_eq!(table.snapshot(T0)[0].until, None);

        let near = T0 + 600;
        table.record("a", block(BlockKind::Quota, T0, Some(near)));
        assert_eq!(table.snapshot(T0)[0].until, Some(near));
    }

    #[test]
    fn capacity_evicts_the_oldest_fact() {
        let mut facts = HashMap::new();
        for i in 0..FACT_TABLE_CAPACITY as u64 {
            insert_fact(
                &mut facts,
                FactKey::Account(CredentialFingerprint(format!("{i:016x}"))),
                block(BlockKind::Quota, T0 + i, None),
            );
        }
        let newest = FactKey::Account(CredentialFingerprint("ffffffffffffffff".into()));
        insert_fact(
            &mut facts,
            newest.clone(),
            block(BlockKind::Quota, T0 + 10_000, None),
        );

        assert_eq!(facts.len(), FACT_TABLE_CAPACITY);
        assert!(facts.contains_key(&newest));
        let oldest = FactKey::Account(CredentialFingerprint(format!("{:016x}", 0)));
        assert!(!facts.contains_key(&oldest), "the oldest fact is evicted");
    }

    #[test]
    fn the_api_key_never_reaches_a_view() {
        let table =
            EndpointHealthTable::new(bundle(&[("a", "sk-shared-secret", "https://a.example")]));
        table.record("a", block(BlockKind::Payment, T0, None));

        let views = table.snapshot(T0);
        let json = serde_json::to_string(&views).expect("views serialize");
        assert!(!json.contains("sk-shared-secret"));
        assert!(json.contains(SHARED_HINT));
        assert_eq!(views[0].account_hint.len(), 8);
    }

    #[test]
    fn a_provider_the_config_no_longer_knows_drops_the_fact() {
        let table =
            EndpointHealthTable::new(bundle(&[("a", "sk-shared-secret", "https://a.example")]));
        table.record("retired", block(BlockKind::Quota, T0, None));
        assert!(table.snapshot(T0).is_empty());
    }

    /// The read side answers for the endpoint that carries an endpoint-level
    /// fact, and only for it.
    #[test]
    fn a_read_answers_for_the_endpoint_that_carries_the_fact() {
        let table = EndpointHealthTable::new(bundle(&[
            ("a", "sk-shared-secret", "https://a.example"),
            ("b", "sk-other-secret", "https://b.example"),
        ]));
        table.record("a", block(BlockKind::EndpointAuth, T0, None));

        let fact = table.blocked_for("a", T0).expect("the endpoint carries it");
        assert_eq!(fact.block.kind, BlockKind::EndpointAuth);
        assert_eq!(fact.account_hint, SHARED_HINT);
        assert!(
            table.blocked_for("b", T0).is_none(),
            "another account's endpoint is untouched"
        );
    }

    /// An account-level fact answers for every endpoint on the credential: a
    /// reader that asks about the sibling learns the account is out.
    #[test]
    fn an_account_level_fact_answers_for_every_endpoint_on_the_credential() {
        let table = EndpointHealthTable::new(bundle(&[
            ("a", "sk-shared-secret", "https://a.example"),
            ("b", "sk-shared-secret", "https://b.example"),
        ]));
        table.record("a", block(BlockKind::Quota, T0, None));

        let fact = table.blocked_for("b", T0).expect("the account answers");
        assert_eq!(fact.block.kind, BlockKind::Quota);
        assert_eq!(fact.account_hint, SHARED_HINT);
    }

    /// A fact past its own window is not there: the read applies the one hit
    /// predicate rather than trusting the row.
    #[test]
    fn an_expired_fact_is_not_read() {
        let table =
            EndpointHealthTable::new(bundle(&[("a", "sk-shared-secret", "https://a.example")]));
        table.record("a", block(BlockKind::Quota, T0, Some(T0 + 60)));

        assert!(table.blocked_for("a", T0 + 59).is_some());
        assert!(
            table.blocked_for("a", T0 + 60).is_none(),
            "the upstream's own moment ends the window"
        );
        assert!(
            table.blocked_for("a", T0 + FACT_TTL_MAX_SECS).is_none(),
            "the TTL is a hard window of its own"
        );
    }

    /// An account-level fact outranks an endpoint-level one on the same endpoint:
    /// a reader asking about it learns the account is out, not merely that one
    /// endpoint is. Answering with the weaker fact would let a sibling on that
    /// account look like a candidate — the walk would switch into the same wall.
    #[test]
    fn an_account_level_fact_outranks_the_endpoint_level_one() {
        let table = EndpointHealthTable::new(bundle(&[
            ("a", "sk-shared-secret", "https://a.example"),
            ("b", "sk-shared-secret", "https://b.example"),
        ]));
        table.record("a", block(BlockKind::EndpointAuth, T0, None));
        table.record("a", block(BlockKind::Quota, T0, None));

        let fact = table
            .blocked_for("a", T0)
            .expect("both levels are readable");
        assert_eq!(
            fact.block.kind,
            BlockKind::Quota,
            "the account-level fact answers, not the endpoint-level one"
        );
        assert_eq!(fact.account_hint, SHARED_HINT);

        let sibling = table
            .blocked_for("b", T0)
            .expect("the account-level fact answers for the sibling too");
        assert_eq!(sibling.block.kind, BlockKind::Quota);
        assert_eq!(sibling.account_hint, SHARED_HINT);
    }

    /// Facts recorded in the same second on one credential share a hint, so the
    /// order has to come from the rest of the key: the kind, then the endpoint.
    /// Three facts, one second, one hint — the tie-breaks decide, and the
    /// assertion pins the whole order.
    #[test]
    fn facts_recorded_in_the_same_second_render_in_one_order() {
        let table = EndpointHealthTable::new(bundle(&[
            ("a", "sk-shared-secret", "https://a.example"),
            ("b", "sk-shared-secret", "https://b.example"),
            ("c", "sk-shared-secret", "https://c.example"),
        ]));
        table.record("c", block(BlockKind::EndpointShape, T0, None));
        table.record("a", block(BlockKind::EndpointShape, T0, None));
        table.record("b", block(BlockKind::EndpointAuth, T0, None));

        let views = table.snapshot(T0);

        let order: Vec<(&str, BlockKind, Option<&str>)> = views
            .iter()
            .map(|view| {
                (
                    view.account_hint.as_str(),
                    view.kind,
                    view.endpoint
                        .as_ref()
                        .and_then(|endpoint| endpoint.base_url.as_deref()),
                )
            })
            .collect();
        assert_eq!(
            order,
            vec![
                (
                    SHARED_HINT,
                    BlockKind::EndpointAuth,
                    Some("https://b.example")
                ),
                (
                    SHARED_HINT,
                    BlockKind::EndpointShape,
                    Some("https://a.example")
                ),
                (
                    SHARED_HINT,
                    BlockKind::EndpointShape,
                    Some("https://c.example")
                ),
            ],
            "the kind decides first, the endpoint breaks the remaining tie"
        );
    }
}
