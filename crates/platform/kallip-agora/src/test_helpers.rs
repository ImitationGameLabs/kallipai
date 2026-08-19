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
use crate::db::entity::{emails, passkeys, tagma_tokens, tagmata, users};
use crate::db::migration::Migrator;
use kallip_agora_common::bytes::Ed25519PublicKey;
use kallip_agora_common::ids::{TagmaId, UserId};
use kallip_common::authtoken::{MintedToken, TokenHash};
use sea_orm::Statement;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ConnectionTrait, Database, DatabaseBackend};
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ImageExt;
use testcontainers_modules::testcontainers::ReuseDirective;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use time::OffsetDateTime;
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::state::{AppState, Limits, SharedState};
use crate::token::TAGMA;

/// Process-global test Postgres: started once, the container is intentionally
/// leaked so it outlives every test. Each [`make_state`] call carves out a
/// unique database within it.
static SHARED_PG_PORT: OnceCell<u16> = OnceCell::const_new();

/// Monotonic counter for unique per-test database names.
static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Cold-start errors worth a retry: the bind failure when an ephemeral
/// outbound source port snatches RootlessKit's picked port ("address
/// already in use"), and docker's 409 when a concurrently cold-started
/// sibling binary won the shared-container create ("is already in use
/// by container"). String matching is fragile, but test-only and the
/// cheapest signal for this docker error chain.
fn retryable_start_error(e: &str) -> bool {
    e.contains("already in use")
}
async fn shared_pg_port() -> &'static u16 {
    SHARED_PG_PORT
        .get_or_init(|| async {
            // Same request as kallip-lesche's test_support (name + tag +
            // credentials) so both binaries reuse ONE container; bump the
            // tag => pick a new name or `docker rm -f` the old container.
            let make_request = || {
                Postgres::default()
                    .with_db_name("postgres")
                    .with_user("postgres")
                    .with_password("postgres")
                    .with_tag("16-alpine")
                    .with_container_name("kallipai-testcontainers-pg-16alpine")
                    .with_reuse(ReuseDirective::Always)
            };
            // Seatbelt for the rare cold start: an ephemeral outbound
            // source port can snatch RootlessKit's picked port, or a
            // sibling test binary cold-starting at the same moment wins
            // the shared-container create and this one gets a docker 409
            // name conflict. Steady state never reaches this error path.
            for attempt in 0..4 {
                match make_request().start().await {
                    Ok(container) => {
                        let port = container.get_host_port_ipv4(5432).await.expect("host port");
                        sweep_dead_test_dbs(port).await;
                        // Leak the container so it stays up for the whole test process.
                        std::mem::forget(container);
                        return port;
                    }
                    Err(e) if attempt < 3 && retryable_start_error(&e.to_string()) => {
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    }
                    Err(e) => panic!("start postgres: {e}"),
                }
            }
            unreachable!("retry loop always returns or panics")
        })
        .await
}
/// A test database is owned by the process whose pid is encoded in its
/// name (`{lesche|agora}_test_{pid}_{n}`). The zero-connection SQL
/// prefilter cannot distinguish "dead" from "just created, not yet
/// connected", so a candidate is dropped only when its owner process no
/// longer exists (`/proc/{pid}`). Unparseable names are never dropped:
/// failing to prove the owner dead means leaving the database alone.
fn owner_dead(db_name: &str) -> bool {
    let rest = db_name
        .strip_prefix("agora_test_")
        .or_else(|| db_name.strip_prefix("lesche_test_"));
    let Some(pid) = rest
        .and_then(|r| r.split_once('_'))
        .and_then(|(pid, _)| pid.parse::<u32>().ok())
    else {
        return false;
    };
    !std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// Best-effort cleanup, once per process, of test databases left dead by
/// earlier runs (the reusable container never drops them). Sweeps BOTH
/// crates' prefixes: the shared container's hygiene must not depend on
/// which crate is running. Twin of kallip-lesche's
/// test_support::sweep_dead_test_dbs.
async fn sweep_dead_test_dbs(port: u16) {
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    // Hygiene, not a correctness path: on any failure just leave the dead
    // databases to accumulate, exactly as before this sweep existed.
    let root = match Database::connect(&url).await {
        Ok(root) => root,
        Err(e) => {
            eprintln!("dead-db sweep: connect failed: {e}");
            return;
        }
    };
    // Zero-connection SQL prefilter plus an owner-liveness gate before
    // each DROP: zero connections alone cannot tell a dead database from
    // a concurrently cold-started sibling's just-created, not-yet-
    // connected one (see `owner_dead`). This process's own databases
    // appear only after the sweep, so only dead-owner databases drop.

    // DROP DATABASE cannot run inside a transaction block, so a DO-block
    // sweep silently drops nothing; select candidates first, then issue
    // one autocommit DROP per database, each failure swallowed.
    let candidates = match root
        .query_all(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT datname FROM pg_database d \
             WHERE d.datname ~ '^(lesche|agora)_test_' \
             AND NOT EXISTS \
             (SELECT 1 FROM pg_stat_activity a WHERE a.datname = d.datname)"
                .to_owned(),
        ))
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("dead-db sweep: list failed: {e}");
            return;
        }
    };
    for name in candidates
        .iter()
        .filter_map(|row| row.try_get::<String>("", "datname").ok())
    {
        if !owner_dead(&name) {
            continue;
        }
        let quoted = format!("\"{}\"", name.replace('"', "\"\""));
        let drop = format!("DROP DATABASE IF EXISTS {quoted}");
        if let Err(e) = root
            .execute(Statement::from_string(DatabaseBackend::Postgres, drop))
            .await
        {
            eprintln!("dead-db sweep: drop {name} failed: {e}");
        }
    }
}

/// Connect to a fresh, isolated database within the shared Postgres and run
/// migrations. Parallel-safe: each call carves out a database named after
/// the process and a per-process counter, and defensively drops a stale
/// same-named database first (pid reuse). Dead databases disappear with the
/// container (`docker rm -f kallipai-testcontainers-pg-16alpine`); no other cleanup
/// exists by design.
pub(crate) async fn setup_test_db() -> Db {
    let port = *shared_pg_port().await;
    let n = DB_COUNTER.fetch_add(1, Ordering::Relaxed);
    let db_name = format!("agora_test_{}_{n}", std::process::id());
    let root_url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let root = Database::connect(&root_url)
        .await
        .expect("connect to postgres maintenance db");
    root.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("DROP DATABASE IF EXISTS \"{db_name}\""),
    ))
    .await
    .expect("drop stale test database");
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

/// Build an `AppState` backed by a fresh test database, with a dummy admin
/// hash and a test `Limits`.
pub async fn make_state() -> SharedState {
    make_state_with(100, 100).await
}

/// Like [`make_state`] but with a custom rate-limiter shape, for tests that
/// assert rate-limit wiring (a tight bucket trips the limiter in a few calls).
pub async fn make_state_with(
    auth_rate_capacity: u32,
    auth_rate_refill_per_sec: u32,
) -> SharedState {
    build_state(
        auth_rate_capacity,
        auth_rate_refill_per_sec,
        Default::default(),
        true,
    )
    .await
}

/// Like [`make_state`] but with a custom OAuth provider registry, so handler
/// tests inject a mock provider (the trait is the test seam) and exercise the
/// full begin/finish/login/create/link logic without any real provider
/// round-trip. `signup_enabled` defaults to true.
pub async fn make_state_with_oauth(
    providers: Vec<Box<dyn crate::oauth::OAuthProvider>>,
) -> SharedState {
    build_state(
        100,
        100,
        crate::oauth::ProviderRegistry::new(providers),
        true,
    )
    .await
}

/// Like [`make_state_with_oauth`] but with `signup_enabled = false`, to test the
/// runtime kill switch (the create branch must surface a generic 401).
pub async fn make_state_with_oauth_signup_disabled(
    providers: Vec<Box<dyn crate::oauth::OAuthProvider>>,
) -> SharedState {
    build_state(
        100,
        100,
        crate::oauth::ProviderRegistry::new(providers),
        false,
    )
    .await
}

async fn build_state(
    auth_rate_capacity: u32,
    auth_rate_refill_per_sec: u32,
    oauth_providers: crate::oauth::ProviderRegistry,
    signup_enabled: bool,
) -> SharedState {
    let db = setup_test_db().await;
    let admin_hash = TokenHash::of("test-admin");
    let limits = Limits {
        max_body_size_bytes: 1024 * 1024,
        enrollment_code_ttl: Duration::from_secs(600),
    };
    let rp_origin = url::Url::parse("http://localhost:7100").expect("valid url");
    let (webauthn, webauthn_core) =
        crate::state::build_webauthn_pair("kallip", "localhost", &rp_origin, true, false)
            .expect("build test webauthn pair");
    let session_cfg = crate::session::SessionCfg {
        ttl: Duration::from_secs(3600),
        cookie_secure: false,
        cookie_domain: None,
    };
    let auth_rate_limiter =
        crate::ratelimit::IpRateLimiter::new(auth_rate_capacity, auth_rate_refill_per_sec);
    let pair_rate_limiter = crate::ratelimit::GlobalRateLimiter::new(100, 100);
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("build test reqwest client");
    std::sync::Arc::new(AppState::new(
        admin_hash,
        limits,
        db,
        webauthn,
        webauthn_core,
        session_cfg,
        auth_rate_limiter,
        pair_rate_limiter,
        Vec::new(),
        http,
        oauth_providers,
        signup_enabled,
    ))
}

/// Insert a user row with `username` and return its id. Users live in the
/// durable store; sessions carry web auth, and the data-plane tests construct
/// `Principal::User` directly. `display_name` is left `None`. Email is an
/// optional contact channel -- use [`seed_email`] to link one. The username is
/// stored verbatim (no normalization) so each test controls the exact lookup
/// key (login resolves by `username`).
pub async fn seed_user(state: &SharedState, username: &str) -> UserId {
    let user_id = UserId::random();
    let now = OffsetDateTime::now_utc();
    users::ActiveModel {
        id: Set(user_id.to_string()),
        username: Set(username.to_string()),
        display_name: Set(None),
        created_at: Set(now),
        disabled_at: Set(None),
    }
    .insert(&state.db)
    .await
    .expect("insert user");
    user_id
}

/// Link an email address to `user_id`, optionally primary and optionally
/// verified. The address is stored verbatim (no canonicalization) so each test
/// controls the exact value. Returns the email row id.
///
/// NOTE: this bypasses the `/me/emails` flow invariants (it can seed an
/// unverified primary, which the live `add_email`/`make_primary` paths forbid)
/// -- it is a direct row insert for fixture setup, not a path through the
/// handlers.
pub async fn seed_email(
    state: &SharedState,
    user_id: &UserId,
    address: &str,
    is_primary: bool,
    verified: bool,
) -> uuid::Uuid {
    let now = OffsetDateTime::now_utc();
    emails::ActiveModel {
        id: Set(Uuid::new_v4()),
        account_id: Set(user_id.to_string()),
        address: Set(address.to_string()),
        is_primary: Set(is_primary),
        verified_at: Set(verified.then_some(now)),
        verification_token_hash: Set(None),
        verification_token_expires_at: Set(None),
        added_at: Set(now),
    }
    .insert(&state.db)
    .await
    .expect("insert email")
    .id
}

/// Register an enrolled tagma owned by `owner`, pinning `pinned_key`, and
/// return the id plus the tagma-token plaintext. The tagma + tagma token are
/// persisted (mirrors production enroll: `enrolled_at` set, no code fields).
pub async fn seed_tagma(
    state: &SharedState,
    owner: &UserId,
    pinned_key: Ed25519PublicKey,
) -> (TagmaId, String) {
    let (tagma_id, plaintext) =
        seed_tagma_with_id(state, owner, TagmaId::random(), pinned_key).await;
    (tagma_id, plaintext)
}

/// Like [`seed_tagma`] but with a caller-supplied id -- for tests that need the
/// tagma id to match some other fixture (e.g. a room member whose derived
/// participant identity is the tagma id). Mints the tagma token the same way.
pub async fn seed_tagma_with_id(
    state: &SharedState,
    owner: &UserId,
    tagma_id: TagmaId,
    pinned_key: Ed25519PublicKey,
) -> (TagmaId, String) {
    let token = MintedToken::generate(TAGMA);
    let plaintext = token.secret().to_string();
    let now = OffsetDateTime::now_utc();
    tagmata::ActiveModel {
        id: Set(tagma_id.to_string()),
        owner_user_id: Set(owner.to_string()),
        pinned_public_key: Set(Some(pinned_key.0.clone())),
        created_at: Set(now),
        label: Set(None),
        last_tunnel_proof_ts: Set(None),
        revoked_at: Set(None),
        enrolled_at: Set(Some(now)),
        enrollment_code_hash: Set(None),
        enrollment_code_masked: Set(None),
        expires_at: Set(None),
    }
    .insert(&state.db)
    .await
    .expect("insert tagma");
    tagma_tokens::ActiveModel {
        token_hash: Set(token.hash().as_bytes().to_vec()),
        tagma_id: Set(tagma_id.to_string()),
        issued_at: Set(now),
    }
    .insert(&state.db)
    .await
    .expect("insert tagma token");
    (tagma_id, plaintext)
}

// Note: data-plane fixtures (seed_conversation, seed_presence) lived here when
// the agora was a monolith. They moved to the `kallip-lesche` test harness
// along with the relay's soft state.

/// Insert a live passkey row for `user_id` (test fixture; the credential JSONB
/// is a placeholder -- neither the list nor revoke helpers read it). Returns the
/// server-assigned `passkeys.id` (the device id passkey-based APIs key on).
pub async fn seed_passkey(state: &SharedState, user_id: &UserId, cred_id: Vec<u8>) -> Uuid {
    let id = Uuid::new_v4();
    passkeys::ActiveModel {
        id: Set(id),
        user_id: Set(user_id.to_string()),
        cred_id: Set(cred_id),
        credential: Set(serde_json::json!({})),
        label: Set("Device".to_string()),
        created_at: Set(OffsetDateTime::now_utc()),
        last_used_at: Set(OffsetDateTime::now_utc()),
        discoverable: Set(false),
    }
    .insert(&state.db)
    .await
    .expect("insert passkey");
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_start_error_matches_both_cold_start_conflicts() {
        assert!(retryable_start_error(
            "Bind for 127.0.0.1:5432 failed: port is already allocated: address already in use",
        ));
        assert!(retryable_start_error(
            "Conflict. The container name \"kallipai-testcontainers-pg-16alpine\" is already in use by container 4f0c",
        ));
        assert!(!retryable_start_error("pull access denied"));
    }

    #[test]
    fn owner_dead_requires_a_dead_encoded_pid() {
        // This process is alive, so its own databases are never swept.
        assert!(!owner_dead(&format!("agora_test_{}_0", std::process::id())));
        // 4e9 is far beyond Linux pid_max: no such process exists.
        assert!(owner_dead("agora_test_4000000000_0"));
        assert!(owner_dead("lesche_test_4000000000_0"));
        assert!(!owner_dead("agora_test_4000000000"));
        assert!(!owner_dead("other_test_4000000000_0"));
    }
}
