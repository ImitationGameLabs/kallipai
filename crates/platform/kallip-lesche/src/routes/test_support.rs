//! Shared test fixtures for the route tests: an ephemeral Postgres-backed
//! `SharedConvState` wired to a mock registry (the identity oracle), plus
//! principal constructors. Each test that calls [`db_state`] must keep the
//! returned container bound so Postgres is not torn down mid-test.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use kallip_agora_common::ids::{ParticipantId, ParticipantKind, TagmaId, UserId};
use kallip_agora_common::principal::Principal;
use sea_orm::{ActiveModelTrait, ActiveValue::Set};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::{ContainerAsync, runners::AsyncRunner};
use time::OffsetDateTime;

use crate::auth::AuthPrincipal;
use crate::db::Db;
use crate::db::entity::{room_members, rooms};
use crate::state::{ConversationsState, Registry, SharedConvState};
use crate::test_support::MockControlPlane;

pub fn as_user(u: &UserId) -> AuthPrincipal {
    AuthPrincipal(Principal::User(u.clone()))
}

pub fn as_tagma(t: &TagmaId) -> AuthPrincipal {
    AuthPrincipal(Principal::Tagma(t.clone()))
}

/// Provision an ephemeral Postgres, apply the lesche migrations, and wire a
/// `SharedConvState` with that store + a fresh mock registry (identity oracle).
pub async fn db_state() -> (
    SharedConvState,
    Arc<MockControlPlane>,
    ContainerAsync<Postgres>,
) {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::ERROR)
        .try_init();
    let container = Postgres::default()
        .with_db_name("postgres")
        .with_user("postgres")
        .with_password("postgres")
        .start()
        .await
        .expect("start postgres");
    let port = container.get_host_port_ipv4(5432).await.expect("host port");
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let db = crate::db::connect_and_migrate(&url).await.expect("migrate");
    let control = Arc::new(MockControlPlane::new());
    let state: SharedConvState = Arc::new(ConversationsState {
        control: control.clone(),
        registry: RwLock::new(Registry::new()),
        pending_key_exchange: Mutex::new(HashMap::new()),
        proof_skew_secs: 60,
        key_exchange_timeout: Duration::from_secs(10),
        db: Some(db),
        agent_profiles: crate::state::AgentProfileCache::default(),
    });
    (state, control, container)
}

/// Seed a room row + its live membership (human users + agent tagmas) directly
/// into the durable store, so a delivery test can exercise routing without
/// driving the full management flow. `membership_epoch` starts at 1.
pub async fn seed_room(
    db: &Db,
    room_id: &str,
    created_by: &UserId,
    users: &[&UserId],
    tagmas: &[&TagmaId],
) {
    let now = OffsetDateTime::now_utc();
    rooms::ActiveModel {
        id: Set(room_id.to_string()),
        created_by_user_id: Set(created_by.to_string()),
        created_at: Set(now),
        membership_epoch: Set(1),
        name: Set(String::new()),
        description: Set(String::new()),
        visibility: Set(kallip_lesche_common::rooms::Visibility::Private
            .as_str()
            .to_string()),
    }
    .insert(db)
    .await
    .expect("insert room");
    let founder_pid = ParticipantId::for_user(created_by);
    room_members::ActiveModel {
        room_id: Set(room_id.to_string()),
        member_id: Set(founder_pid.as_ref().to_string()),
        kind: Set(ParticipantKind::Human.as_str().to_string()),
        source_id: Set(created_by.to_string()),
        joined_at: Set(now),
        added_by: Set(created_by.to_string()),
    }
    .insert(db)
    .await
    .expect("insert founder");
    for u in users {
        if u.as_ref() == created_by.as_ref() {
            continue;
        }
        room_members::ActiveModel {
            room_id: Set(room_id.to_string()),
            member_id: Set(ParticipantId::for_user(u).as_ref().to_string()),
            kind: Set(ParticipantKind::Human.as_str().to_string()),
            source_id: Set(u.to_string()),
            joined_at: Set(now),
            added_by: Set(created_by.to_string()),
        }
        .insert(db)
        .await
        .expect("insert user member");
    }
    for t in tagmas {
        room_members::ActiveModel {
            room_id: Set(room_id.to_string()),
            member_id: Set(ParticipantId::for_tagma(t).as_ref().to_string()),
            kind: Set(ParticipantKind::Agent.as_str().to_string()),
            source_id: Set(t.to_string()),
            joined_at: Set(now),
            added_by: Set(created_by.to_string()),
        }
        .insert(db)
        .await
        .expect("insert tagma member");
    }
}
