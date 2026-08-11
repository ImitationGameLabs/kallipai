//! HTTP management API for the cron daemon.
//!
//! axum 0.8 house style: `Router<SharedState>`, `cors_layer()` mirrors tagma's
//! (origin/methods wildcarded — the bearer is the real boundary; headers use
//! `AllowHeaders::mirror_request()` so `Authorization` survives preflight),
//! body-limit outermost. Every management request carries the caller's bearer
//! plus the claimed agent id (`agent_id` in the create body, `?agent=` query on
//! the rest); the handler verifies the pair via the tagma and scopes to that
//! agent's own schedules. `PATCH` is status-only — `next_fire`/`last_fire` are
//! never client-mutable, which holds the no-rearm invariant.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use tower_http::cors::{AllowHeaders, Any, CorsLayer};
use uuid::Uuid;

use kallip_common::agentid::AgentId;
use kallip_common::protocol::ApiError;
use kallip_cron_common::{
    CreateScheduleRequest, Schedule, ScheduleStatus, SchedulesListResponse, StatusResponse,
    UpdateScheduleRequest,
};

use crate::auth::{AuthIdentity, verify};
use crate::state::SharedState;
use crate::store::calculate_initial_next_fire;

/// Permissive CORS layer (mirrors `kallip-tagma::routes::cors_layer`). The
/// daemon authenticates every request with a bearer, so CORS is not a security
/// boundary; `mirror_request()` covers `Authorization`, which `Any` (`*`)
/// excludes per the Fetch spec.
pub fn cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(AllowHeaders::mirror_request())
}

/// Build the management router.
pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/schedules", post(create_schedule).get(list_schedules))
        .route("/schedules/next", get(get_next_schedule))
        .route(
            "/schedules/{id}",
            get(get_schedule)
                .patch(update_schedule)
                .delete(delete_schedule),
        )
}

async fn health(State(state): State<SharedState>) -> Result<&'static str, ApiError> {
    // Reflects real scheduler/deliverer progress, not a static "OK": a panicked
    // or wedged loop stops stamping and ages out, flipping this to 503.
    state
        .liveness
        .check()
        .map_err(|stale| ApiError::unavailable(stale.to_string()))?;
    Ok("OK")
}

/// `?agent=<id>` + optional status/tag, used by the read/delete ops.
#[derive(Debug, Deserialize)]
pub struct AgentQuery {
    pub agent: AgentId,
    pub status: Option<String>,
    pub tag: Option<String>,
}

async fn status(
    State(state): State<SharedState>,
    AuthIdentity(bearer): AuthIdentity,
    Query(q): Query<AgentQuery>,
) -> Result<Json<StatusResponse>, ApiError> {
    verify(&state, &bearer, &q.agent).await?;
    let (active, pending, next_fire) = state
        .store
        .stats(&q.agent)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(StatusResponse {
        healthy: state.liveness.check().is_ok(),
        active_schedules: active,
        pending_triggered: pending,
        next_fire,
    }))
}

async fn create_schedule(
    State(state): State<SharedState>,
    AuthIdentity(bearer): AuthIdentity,
    Json(req): Json<CreateScheduleRequest>,
) -> Result<(StatusCode, Json<Schedule>), ApiError> {
    req.trigger
        .validate()
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    verify(&state, &bearer, &req.agent_id).await?;
    let now = time::OffsetDateTime::now_utc();
    let next_fire = calculate_initial_next_fire(&req.trigger, now);
    let schedule = Schedule {
        id: Uuid::new_v4().to_string(),
        name: req.name,
        trigger: req.trigger,
        agent_id: req.agent_id,
        message: req.message,
        tags: req.tags,
        priority: req.priority,
        status: ScheduleStatus::Active,
        created_at: now,
        next_fire: Some(next_fire),
        last_fire: None,
    };
    state
        .store
        .create(&schedule)
        .await
        .map_err(ApiError::internal)?;
    Ok((StatusCode::CREATED, Json(schedule)))
}

async fn list_schedules(
    State(state): State<SharedState>,
    AuthIdentity(bearer): AuthIdentity,
    Query(q): Query<AgentQuery>,
) -> Result<Json<SchedulesListResponse>, ApiError> {
    verify(&state, &bearer, &q.agent).await?;
    // A present-but-unrecognized status is a 400, not a silent unfiltered list
    // (the strict-lowercase FromStr rejects wrong case / unknown values).
    let status = match q.status.as_deref() {
        Some(s) => Some(s.parse().map_err(|e: String| ApiError::bad_request(e))?),
        None => None,
    };
    let schedules = state
        .store
        .list(&q.agent, status, q.tag.as_deref())
        .await
        .map_err(ApiError::internal)?;
    let total = schedules.len();
    Ok(Json(SchedulesListResponse { schedules, total }))
}

async fn get_next_schedule(
    State(state): State<SharedState>,
    AuthIdentity(bearer): AuthIdentity,
    Query(q): Query<AgentQuery>,
) -> Result<Json<Option<Schedule>>, ApiError> {
    verify(&state, &bearer, &q.agent).await?;
    let next = state
        .store
        .get_next(&q.agent)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(next))
}

async fn get_schedule(
    State(state): State<SharedState>,
    AuthIdentity(bearer): AuthIdentity,
    Path(id): Path<String>,
    Query(q): Query<AgentQuery>,
) -> Result<Json<Schedule>, ApiError> {
    verify(&state, &bearer, &q.agent).await?;
    // Cross-owner is indistinguishable from not-found (uniform 404, matching
    // delete + the client's existing NOT_FOUND branch): no ownership oracle.
    let sched = state.store.get(&id).await.map_err(ApiError::internal)?;
    match sched {
        Some(s) if s.agent_id == q.agent => Ok(Json(s)),
        _ => Err(ApiError::not_found("schedule not found")),
    }
}

async fn update_schedule(
    State(state): State<SharedState>,
    AuthIdentity(bearer): AuthIdentity,
    Path(id): Path<String>,
    Query(q): Query<AgentQuery>,
    Json(req): Json<UpdateScheduleRequest>,
) -> Result<Json<Schedule>, ApiError> {
    verify(&state, &bearer, &q.agent).await?;
    // Status-only by design: no field can mutate next_fire/last_fire, so a
    // fired one-timer cannot be re-armed through this endpoint. An empty PATCH
    // is a 400, not a silent no-op.
    let Some(status) = req.status else {
        return Err(ApiError::bad_request(
            "no update specified (status-only endpoint)",
        ));
    };
    // Scoped at the query level: a cross-owner id is a no-op write, then the
    // read below returns None → uniform 404.
    state
        .store
        .update_status(&id, &q.agent, status)
        .await
        .map_err(ApiError::internal)?;
    match state.store.get(&id).await.map_err(ApiError::internal)? {
        Some(s) if s.agent_id == q.agent => Ok(Json(s)),
        _ => Err(ApiError::not_found("schedule not found")),
    }
}

async fn delete_schedule(
    State(state): State<SharedState>,
    AuthIdentity(bearer): AuthIdentity,
    Path(id): Path<String>,
    Query(q): Query<AgentQuery>,
) -> Result<StatusCode, ApiError> {
    verify(&state, &bearer, &q.agent).await?;
    let removed = state
        .store
        .delete(&id, &q.agent)
        .await
        .map_err(ApiError::internal)?;
    Ok(if removed {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    })
}
