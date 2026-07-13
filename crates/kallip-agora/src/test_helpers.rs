//! Test fixtures: build a seeded [`SharedState`] without the axum extractor or
//! provisioning endpoints. Mirrors the production mint/insert logic so handlers
//! see state shaped exactly as a live agora would produce it.

use std::sync::Arc;
use std::time::Duration;

use kallip_agora_common::bytes::Ed25519PublicKey;
use kallip_agora_common::herald::HeraldInbound;
use kallip_agora_common::ids::{ConversationId, TeamId, UserId};
use kallip_common::agentid::AgentId;
use kallip_common::authtoken::{MintedToken, TokenHash};
use tokio::sync::broadcast;

use crate::state::{AppState, BROADCAST_CAPACITY, Limits, SharedState};
use crate::token::{TEAM, USER};

/// Build an `AppState` with a dummy admin hash and a test `Limits`, exposing
/// `key_exchange_timeout` so tests that exercise the synchronous KEX can pick a
/// value matched to what they assert.
pub fn make_state(key_exchange_timeout: Duration) -> SharedState {
    let admin_hash = TokenHash::of("test-admin");
    let limits = Limits {
        max_body_size_bytes: 1024 * 1024,
        enrollment_code_ttl: Duration::from_secs(600),
        proof_skew_secs: 60,
        max_conversations_per_user: 64,
        key_exchange_timeout,
    };
    Arc::new(AppState::new(admin_hash, limits))
}

/// Register a user (with a fresh access token) and return the id plus the token
/// plaintext. The plaintext is returned for completeness; tests that call
/// handlers directly pass `Principal::User(uid)` and do not need it.
pub fn seed_user(state: &SharedState) -> (UserId, String) {
    let user_id = UserId::random();
    let token = MintedToken::generate(USER);
    let plaintext = token.secret().to_string();
    {
        let mut reg = state.write().unwrap();
        reg.users.insert(user_id.clone());
        reg.access_tokens
            .insert(token.hash().clone(), user_id.clone());
    }
    (user_id, plaintext)
}

/// Register a team owned by `owner`, pinning `pinned_key`, and return the id
/// plus the team-token plaintext.
pub fn seed_team(
    state: &SharedState,
    owner: &UserId,
    pinned_key: Ed25519PublicKey,
) -> (TeamId, String) {
    let team_id = TeamId::random();
    let token = MintedToken::generate(TEAM);
    let plaintext = token.secret().to_string();
    {
        let mut reg = state.write().unwrap();
        reg.teams.insert(
            team_id.clone(),
            crate::state::TeamRecord {
                owner: owner.clone(),
                pinned_public_key: pinned_key,
            },
        );
        reg.team_tokens
            .insert(token.hash().clone(), team_id.clone());
    }
    (team_id, plaintext)
}

/// Create a conversation owned by `owner` and bound to `(team, agent)`. Routes
/// through [`Registry::create_conversation`](crate::state::Registry::create_conversation)
/// so the count lockstep invariant holds; the cap is set comfortably above any
/// fixture's needs. Returns the new conversation id.
pub fn seed_conversation(
    state: &SharedState,
    owner: &UserId,
    team: &TeamId,
    agent: AgentId,
) -> ConversationId {
    let mut reg = state.write().unwrap();
    reg.create_conversation(owner, team.clone(), agent, 64)
        .expect("seed conversation under cap")
}

/// Bring a team online: insert a herald-tunnel presence entry and return the
/// broadcast sender. The caller MUST `sender.subscribe()` BEFORE spawning any
/// handler that sends into the tunnel (broadcast only delivers to receivers
/// alive at send time).
pub fn seed_presence(state: &SharedState, team: &TeamId) -> broadcast::Sender<HeraldInbound> {
    let (tx, _initial_rx) = broadcast::channel::<HeraldInbound>(BROADCAST_CAPACITY);
    {
        let mut reg = state.write().unwrap();
        reg.register_presence(team, tx.clone(), Arc::new(()));
    }
    tx
}
