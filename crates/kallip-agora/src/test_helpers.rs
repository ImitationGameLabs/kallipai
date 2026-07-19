//! Test fixtures: build a seeded [`SharedState`] without the axum extractor or
//! provisioning endpoints. Mirrors the production mint/insert logic so handlers
//! see state shaped exactly as a live agora would produce it.
//!
//! A single ephemeral Postgres (per test process) backs all tests; each
//! [`make_state`] call provisions a fresh database within it so parallel tests
//! never collide. Needs Docker at test time.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::db::Db;
use crate::db::entity::{tagma_tokens, tagmata, users};
use crate::db::migration::Migrator;
use kallip_agora_common::bytes::Ed25519PublicKey;
use kallip_agora_common::herald::HeraldInbound;
use kallip_agora_common::ids::{ConversationId, TagmaId, UserId};
use kallip_common::agentid::AgentId;
use kallip_common::authtoken::{MintedToken, TokenHash};
use sea_orm::Statement;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ConnectionTrait, Database, DatabaseBackend};
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use time::OffsetDateTime;
use tokio::sync::{OnceCell, broadcast};

use crate::state::{AppState, BROADCAST_CAPACITY, Limits, SharedState};
use crate::token::TAGMA;
use webauthn_rs::prelude::WebauthnBuilder;

/// Process-global test Postgres: started once, the container is intentionally
/// leaked so it outlives every test. Each [`make_state`] call carves out a
/// unique database within it.
static SHARED_PG: OnceCell<u16> = OnceCell::const_new();

/// Monotonic counter for unique per-test database names.
static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

async fn shared_pg_port() -> &'static u16 {
    SHARED_PG
        .get_or_init(|| async {
            let image = Postgres::default()
                .with_db_name("postgres")
                .with_user("postgres")
                .with_password("postgres");
            let container = image.start().await.expect("start postgres");
            let port = container.get_host_port_ipv4(5432).await.expect("host port");
            // Leak the container so it stays up for the whole test process.
            std::mem::forget(container);
            port
        })
        .await
}

/// Connect to a fresh, isolated database within the shared Postgres and run
/// migrations. Parallel-safe: each call gets a unique database name.
async fn setup_test_db() -> Db {
    let port = *shared_pg_port().await;
    let n = DB_COUNTER.fetch_add(1, Ordering::Relaxed);
    let db_name = format!("agora_test_{n}");
    let root_url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let root = Database::connect(&root_url)
        .await
        .expect("connect to postgres maintenance db");
    root.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("CREATE DATABASE \"{db_name}\""),
    ))
    .await
    .expect("create test database");
    drop(root);
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/{db_name}");
    let db = Database::connect(&url).await.expect("connect to test db");
    Migrator::up(&db, None).await.expect("run migrations");
    db
}

/// Build an `AppState` backed by a fresh test database, with a dummy admin hash
/// and a test `Limits`, exposing `key_exchange_timeout` so tests that exercise
/// the synchronous KEX can pick a value matched to what they assert.
pub async fn make_state(key_exchange_timeout: Duration) -> SharedState {
    make_state_with(key_exchange_timeout, 100, 100).await
}

/// Like [`make_state`] but with a custom rate-limiter shape, for tests that
/// assert rate-limit wiring (a tight bucket trips the limiter in a few calls).
pub async fn make_state_with(
    key_exchange_timeout: Duration,
    auth_rate_capacity: u32,
    auth_rate_refill_per_sec: u32,
) -> SharedState {
    let db = setup_test_db().await;
    let admin_hash = TokenHash::of("test-admin");
    let limits = Limits {
        max_body_size_bytes: 1024 * 1024,
        enrollment_code_ttl: Duration::from_secs(600),
        invite_default_ttl_secs: 604_800,
        proof_skew_secs: 60,
        max_conversations_per_user: 64,
        key_exchange_timeout,
    };
    let rp_origin = url::Url::parse("http://localhost:7100").expect("valid url");
    let webauthn = WebauthnBuilder::new("localhost", &rp_origin)
        .expect("valid rp config")
        .allow_any_port(true)
        .rp_name("kallip")
        .timeout(Duration::from_secs(60))
        .build()
        .expect("build test webauthn");
    let session_cfg = crate::session::SessionCfg {
        ttl: Duration::from_secs(3600),
        cookie_secure: false,
    };
    let auth_rate_limiter =
        crate::ratelimit::IpRateLimiter::new(auth_rate_capacity, auth_rate_refill_per_sec);
    std::sync::Arc::new(AppState::new(
        admin_hash,
        limits,
        db,
        std::sync::Arc::new(webauthn),
        session_cfg,
        auth_rate_limiter,
        Vec::new(),
    ))
}

/// Insert a user row with `username` and `email` and return its id. Users live
/// in the durable store; sessions carry web auth, and the data-plane tests
/// construct `Principal::User` directly. `display_name` is left `None`. The
/// `email` is stored verbatim (no canonicalization) so each test controls the
/// exact lookup key.
pub async fn seed_user(state: &SharedState, username: &str, email: &str) -> UserId {
    let user_id = UserId::random();
    let now = OffsetDateTime::now_utc();
    users::ActiveModel {
        id: Set(user_id.to_string()),
        username: Set(username.to_string()),
        email: Set(email.to_string()),
        display_name: Set(None),
        created_at: Set(now),
        disabled_at: Set(None),
    }
    .insert(&state.db)
    .await
    .expect("insert user");
    user_id
}

/// Register a tagma owned by `owner`, pinning `pinned_key`, and return the id
/// plus the tagma-token plaintext. The tagma + tagma token are persisted
/// (mirrors production enroll).
pub async fn seed_tagma(
    state: &SharedState,
    owner: &UserId,
    pinned_key: Ed25519PublicKey,
) -> (TagmaId, String) {
    let tagma_id = TagmaId::random();
    let token = MintedToken::generate(TAGMA);
    let plaintext = token.secret().to_string();
    let now = OffsetDateTime::now_utc();
    tagmata::ActiveModel {
        id: Set(tagma_id.to_string()),
        owner_user_id: Set(owner.to_string()),
        pinned_public_key: Set(pinned_key.0.clone()),
        created_at: Set(now),
        label: Set(None),
        last_tunnel_proof_ts: Set(None),
    }
    .insert(&state.db)
    .await
    .expect("insert tagma");
    tagma_tokens::ActiveModel {
        token_hash: Set(token.hash().as_bytes().to_vec()),
        tagma_id: Set(tagma_id.to_string()),
        issued_at: Set(now),
        revoked_at: Set(None),
    }
    .insert(&state.db)
    .await
    .expect("insert tagma token");
    (tagma_id, plaintext)
}

/// Create a conversation owned by `owner` and bound to `(tagma, agent)`. Routes
/// through [`Registry::create_conversation`](crate::state::Registry::create_conversation)
/// so the count lockstep invariant holds; the cap is set comfortably above any
/// fixture's needs. Returns the new conversation id.
pub fn seed_conversation(
    state: &SharedState,
    owner: &UserId,
    tagma: &TagmaId,
    agent: AgentId,
) -> ConversationId {
    let mut reg = state.write().unwrap();
    reg.create_conversation(owner, tagma.clone(), agent, 64)
        .expect("seed conversation under cap")
}

/// Bring a tagma online: insert a herald-tunnel presence entry and return the
/// broadcast sender. The caller MUST `sender.subscribe()` BEFORE spawning any
/// handler that sends into the tunnel (broadcast only delivers to receivers
/// alive at send time).
pub fn seed_presence(state: &SharedState, tagma: &TagmaId) -> broadcast::Sender<HeraldInbound> {
    let (tx, _initial_rx) = broadcast::channel::<HeraldInbound>(BROADCAST_CAPACITY);
    {
        let mut reg = state.write().unwrap();
        reg.register_presence(tagma, tx.clone(), std::sync::Arc::new(()));
    }
    tx
}
