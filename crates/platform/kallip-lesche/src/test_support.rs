//! Test-only support: an in-memory `ControlPlane` mock + relay-state fixtures,
//! so the relay's routing/KEX/presence logic is tested without Docker.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use kallip_agora_common::bytes::Ed25519PublicKey;
use kallip_agora_common::control_plane::{ControlPlane, ControlPlaneError, TagmaIdentity};
use kallip_agora_common::ids::{TagmaId, UserId};
use kallip_agora_common::principal::Principal;

use crate::state::{ConversationsState, SharedConvState};

/// An in-memory `ControlPlane`. No Docker: tagmas, tokens, sessions, and the
/// replay high-water-mark all live in `Mutex<HashMap>`s the test seeds directly.
pub struct MockControlPlane {
    tagmas: Mutex<HashMap<TagmaId, MockTagma>>,
    /// bearer token -> tagma it authenticates as.
    tokens: Mutex<HashMap<String, TagmaId>>,
    /// session cookie value -> user.
    sessions: Mutex<HashMap<String, UserId>>,
    replay_ts: Mutex<HashMap<TagmaId, i64>>,
}

struct MockTagma {
    owner: UserId,
    pinned_key: Option<Ed25519PublicKey>,
    enrolled: bool,
    revoked: bool,
}

impl MockControlPlane {
    pub fn new() -> Self {
        Self {
            tagmas: Mutex::new(HashMap::new()),
            tokens: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            replay_ts: Mutex::new(HashMap::new()),
        }
    }

    /// Seed an enrolled tagma owned by `owner` with the given pinned key, and a
    /// bearer `token` that authenticates as it.
    pub fn enroll_tagma(
        &self,
        tagma: &TagmaId,
        owner: UserId,
        pinned_key: Ed25519PublicKey,
        token: &str,
    ) {
        self.tagmas.lock().unwrap().insert(
            tagma.clone(),
            MockTagma {
                owner,
                pinned_key: Some(pinned_key),
                enrolled: true,
                revoked: false,
            },
        );
        self.tokens
            .lock()
            .unwrap()
            .insert(token.to_string(), tagma.clone());
    }
}

#[async_trait::async_trait]
impl ControlPlane for MockControlPlane {
    async fn verify_session(
        &self,
        cookie_value: &str,
    ) -> Result<Option<UserId>, ControlPlaneError> {
        Ok(self.sessions.lock().unwrap().get(cookie_value).cloned())
    }

    async fn verify_bearer(&self, token: &str) -> Result<Option<Principal>, ControlPlaneError> {
        let Some(tagma) = self.tokens.lock().unwrap().get(token).cloned() else {
            return Ok(None);
        };
        let tagmas = self.tagmas.lock().unwrap();
        let Some(t) = tagmas.get(&tagma) else {
            return Ok(None);
        };
        if t.revoked {
            return Ok(None);
        }
        Ok(Some(Principal::Tagma(tagma)))
    }

    async fn tagma_resolvable_by(
        &self,
        tagma_id: &TagmaId,
        user: &UserId,
    ) -> Result<bool, ControlPlaneError> {
        Ok(self
            .tagmas
            .lock()
            .unwrap()
            .get(tagma_id)
            .map(|t| &t.owner == user && t.enrolled && !t.revoked)
            .unwrap_or(false))
    }

    async fn tagma_identity(
        &self,
        tagma_id: &TagmaId,
    ) -> Result<Option<TagmaIdentity>, ControlPlaneError> {
        Ok(self.tagmas.lock().unwrap().get(tagma_id).and_then(|t| {
            t.pinned_key.clone().map(|k| TagmaIdentity {
                pinned_public_key: k,
                owner_user_id: t.owner.clone(),
            })
        }))
    }

    async fn bump_tunnel_proof_ts(
        &self,
        tagma_id: &TagmaId,
        ts: i64,
    ) -> Result<bool, ControlPlaneError> {
        let mut replay = self.replay_ts.lock().unwrap();
        let fresh = replay.get(tagma_id).copied().is_none_or(|prev| prev < ts);
        if fresh {
            replay.insert(tagma_id.clone(), ts);
        }
        Ok(fresh)
    }
}

/// Build a `SharedConvState` wired to a fresh mock registry. The mock is
/// returned so the test can seed tagmas/tokens.
pub fn make_state(
    proof_skew_secs: i64,
    key_exchange_timeout: std::time::Duration,
) -> (SharedConvState, Arc<MockControlPlane>) {
    let control = Arc::new(MockControlPlane::new());
    let state: SharedConvState = Arc::new(ConversationsState {
        control: control.clone(),
        registry: std::sync::RwLock::new(crate::state::Registry::new()),
        pending_key_exchange: std::sync::Mutex::new(HashMap::new()),
        proof_skew_secs,
        key_exchange_timeout,
    });
    (state, control)
}

/// Insert a presence entry directly (bypassing the tunnel handler's proof
/// machinery) and return the per-connection identity token + the tunnel's
/// inbound broadcast sender. Mirrors what a live herald tunnel establishes.
pub fn seed_presence(
    state: &SharedConvState,
    tagma: &TagmaId,
    owner: UserId,
) -> (
    tokio::sync::broadcast::Sender<kallip_agora_common::herald::HeraldInbound>,
    Arc<()>,
) {
    let (tx, _rx) =
        tokio::sync::broadcast::channel::<kallip_agora_common::herald::HeraldInbound>(128);
    let id = Arc::new(());
    let mut reg = state.registry.write().unwrap();
    reg.register_presence(tagma, owner, tx.clone(), id.clone());
    (tx, id)
}
