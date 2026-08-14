//! Work schedule: time-based agent lifecycle orchestration.
//!
//! A work schedule defines when an agent goes on-duty (start) and off-duty
//! (end), with pre/final warning prompts before end-of-shift. The scheduling
//! engine reads stored schedules and fires duty transitions; this
//! module provides the CRUD layer and HTTP routes.

pub mod migration;
mod store;

pub use store::WorkScheduleStore;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use kallip_common::protocol::ApiError;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tracing::info;

use crate::state::{AgentId, SharedState};

/// Whether a work schedule is active or paused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkScheduleStatus {
    #[default]
    Active,
    Paused,
}

impl std::fmt::Display for WorkScheduleStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Paused => write!(f, "paused"),
        }
    }
}

impl std::str::FromStr for WorkScheduleStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "paused" => Ok(Self::Paused),
            _ => Err(format!("invalid status: {s}")),
        }
    }
}

/// A work schedule definition.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkSchedule {
    pub id: String,
    pub name: String,
    pub agent_id: AgentId,
    pub start_cron: String,
    pub end_cron: String,
    #[serde(default = "default_pre_warn")]
    pub pre_warn_minutes: u32,
    #[serde(default = "default_final_warn")]
    pub final_warn_minutes: u32,
    pub wake_prompt: String,
    #[serde(default)]
    pub status: WorkScheduleStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

fn default_pre_warn() -> u32 { 10 }
fn default_final_warn() -> u32 { 5 }

/// Request body for creating a work schedule.
#[derive(Debug, Deserialize)]
pub struct CreateWorkScheduleRequest {
    pub name: String,
    pub agent_id: AgentId,
    pub start_cron: String,
    pub end_cron: String,
    #[serde(default = "default_pre_warn")]
    pub pre_warn_minutes: u32,
    #[serde(default = "default_final_warn")]
    pub final_warn_minutes: u32,
    pub wake_prompt: String,
    #[serde(default)]
    pub status: WorkScheduleStatus,
    #[serde(default)]
    pub timezone: Option<String>,
}

/// Request body for updating a work schedule (all fields optional except id).
#[derive(Debug, Deserialize)]
pub struct UpdateWorkScheduleRequest {
    pub name: Option<String>,
    pub start_cron: Option<String>,
    pub end_cron: Option<String>,
    pub pre_warn_minutes: Option<u32>,
    pub final_warn_minutes: Option<u32>,
    pub wake_prompt: Option<String>,
    pub status: Option<WorkScheduleStatus>,
    pub timezone: Option<Option<String>>,
}

/// Query params for listing work schedules.
#[derive(Debug, Default, Deserialize)]
pub struct ListWorkSchedulesQuery {
    pub agent_id: Option<AgentId>,
    pub status: Option<WorkScheduleStatus>,
}

/// Validate cron expressions and warn-minute fields.
fn validate_schedule(
    start_cron: &str, end_cron: &str, pre_warn: u32, final_warn: u32,
) -> Result<(), ApiError> {
    let start = crate::cron::CronExpr::parse(start_cron).map_err(|e| {
        ApiError::bad_request(format!("invalid start_cron: {e}"))
    })?;
    let end = crate::cron::CronExpr::parse(end_cron).map_err(|e| {
        ApiError::bad_request(format!("invalid end_cron: {e}"))
    })?;
    // Reject cron expressions that will never fire (e.g. Feb 30).
    let now = time::OffsetDateTime::now_utc();
    start.next_after(now).map_err(|_| {
        ApiError::bad_request("start_cron will never fire within the next 5 years")
    })?;
    end.next_after(now).map_err(|_| {
        ApiError::bad_request("end_cron will never fire within the next 5 years")
    })?;
    if pre_warn < final_warn {
        return Err(ApiError::bad_request(
            "pre_warn_minutes must be >= final_warn_minutes",
        ));
    }
    Ok(())
}


/// Get the work-schedule store from AppState, returning 503 if not configured.
fn get_store(state: &SharedState) -> Result<&WorkScheduleStore, ApiError> {
    state.work_schedules.get().ok_or_else(|| {
        ApiError::unavailable("work schedules not configured")
    })
}

// --- Route handlers ---

/// POST /work-schedules — create a new work schedule.
pub async fn create_work_schedule(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Json(req): Json<CreateWorkScheduleRequest>,
) -> Result<(StatusCode, Json<WorkSchedule>), ApiError> {
    crate::auth::require_operator(auth.identity())?;
    if req.timezone.is_some() {
        return Err(ApiError::bad_request(
            "timezone scheduling is not yet supported; use UTC cron expressions",
        ));
    }
    validate_schedule(&req.start_cron, &req.end_cron, req.pre_warn_minutes, req.final_warn_minutes)?;
    let schedule = WorkSchedule {
        id: uuid::Uuid::new_v4().to_string(),
        name: req.name,
        agent_id: req.agent_id,
        start_cron: req.start_cron,
        end_cron: req.end_cron,
        pre_warn_minutes: req.pre_warn_minutes,
        final_warn_minutes: req.final_warn_minutes,
        wake_prompt: req.wake_prompt,
        status: req.status,
        timezone: req.timezone,
        created_at: OffsetDateTime::now_utc(),
    };
    get_store(&state)?.create(&schedule)
        .await
        .map_err(ApiError::internal)?;
    info!(schedule_id = %schedule.id, "work schedule created");
    Ok((StatusCode::CREATED, Json(schedule)))
}

/// GET /work-schedules — list work schedules with optional filters.
pub async fn list_work_schedules(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Query(query): Query<ListWorkSchedulesQuery>,
) -> Result<Json<Vec<WorkSchedule>>, ApiError> {
    crate::auth::require_operator(auth.identity())?;
    let schedules = get_store(&state)?.list(query.agent_id.as_ref(), query.status)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(schedules))
}

/// GET /work-schedules/{id} — get a single work schedule.
pub async fn get_work_schedule(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<String>,
) -> Result<Json<WorkSchedule>, ApiError> {
    crate::auth::require_operator(auth.identity())?;
    let schedule = get_store(&state)?.get(&id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("work schedule not found"))?;
    Ok(Json(schedule))
}

/// PUT /work-schedules/{id} — update a work schedule.
pub async fn update_work_schedule(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<String>,
    Json(req): Json<UpdateWorkScheduleRequest>,
) -> Result<Json<WorkSchedule>, ApiError> {
    crate::auth::require_operator(auth.identity())?;
    // Reject timezone updates (engine is UTC-only).
    if let Some(Some(_)) = req.timezone {
        return Err(ApiError::bad_request(
            "timezone scheduling is not yet supported; use UTC cron expressions",
        ));
    }
    let mut existing = get_store(&state)?.get(&id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("work schedule not found"))?;

    if let Some(name) = req.name { existing.name = name; }
    if let Some(start_cron) = req.start_cron { existing.start_cron = start_cron; }
    if let Some(end_cron) = req.end_cron { existing.end_cron = end_cron; }
    if let Some(pre) = req.pre_warn_minutes { existing.pre_warn_minutes = pre; }
    if let Some(fin) = req.final_warn_minutes { existing.final_warn_minutes = fin; }
    if let Some(prompt) = req.wake_prompt { existing.wake_prompt = prompt; }
    let was_active = get_store(&state)?.get(&id).await.ok().flatten()
        .map(|s| s.status == WorkScheduleStatus::Active)
        .unwrap_or(false);

    if let Some(status) = req.status { existing.status = status; }

    validate_schedule(&existing.start_cron, &existing.end_cron,
        existing.pre_warn_minutes, existing.final_warn_minutes)?;

    // If the schedule was Active and is now Paused, reset the agent to
    // OnDuty so messages are not buffered indefinitely. NOTE: this assumes
    // at most one active schedule per agent (multi-schedule conflict
    // detection is not yet implemented).
    let now_paused = was_active && existing.status == WorkScheduleStatus::Paused;
    if now_paused {
        state.duty.set(existing.agent_id.clone(), crate::duty::DutyStatus::OnDuty);
        info!(schedule_id = %id, agent = %existing.agent_id,
              "schedule paused, duty reset to on-duty");
    }

    let updated = get_store(&state)?.update(&existing)
        .await
        .map_err(ApiError::internal)?;
    if !updated {
        return Err(ApiError::not_found("work schedule not found"));
    }
    info!(schedule_id = %id, "work schedule updated");
    Ok(Json(existing))
}

/// DELETE /work-schedules/{id} — delete a work schedule.
pub async fn delete_work_schedule(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    crate::auth::require_operator(auth.identity())?;
    // Look up the schedule before deleting to get the agent_id.
    let schedule = get_store(&state)?.get(&id)
        .await
        .map_err(ApiError::internal)?;
    let deleted = get_store(&state)?.delete(&id)
        .await
        .map_err(ApiError::internal)?;
    if !deleted {
        return Err(ApiError::not_found("work schedule not found"));
    }
    // Reset the agent to OnDuty (the default) — same single-schedule
    // assumption as the pause path above.
    if let Some(sched) = schedule {
        if sched.status == WorkScheduleStatus::Active {
            state.duty.set(sched.agent_id.clone(), crate::duty::DutyStatus::OnDuty);
            info!(schedule_id = %id, agent = %sched.agent_id,
                  "schedule deleted, duty reset to on-duty");
        }
    }
    info!(schedule_id = %id, "work schedule deleted");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod route_tests {
    use super::*;
    use crate::test_helpers::make_state;

    /// Create a test state with the work-schedule store installed.
    async fn make_ws_state() -> SharedState {
        let state = make_state();
        let store = WorkScheduleStore::open_in_memory().await;
        state.work_schedules.set(store).ok();
        state
    }

    fn create_req(agent: &str) -> CreateWorkScheduleRequest {
        CreateWorkScheduleRequest {
            name: "Day shift".into(),
            agent_id: agent.parse().unwrap(),
            start_cron: "0 9 * * 1-5".into(),
            end_cron: "0 17 * * 1-5".into(),
            pre_warn_minutes: 10,
            final_warn_minutes: 5,
            wake_prompt: "Good morning.".into(),
            status: WorkScheduleStatus::Active,
            timezone: None,
        }
    }

    #[tokio::test]
    async fn create_then_get() {
        let state = make_ws_state().await;
        let resp = create_work_schedule(
            State(state.clone()),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            Json(create_req("agent-1")),
        ).await.unwrap();
        assert_eq!(resp.0, StatusCode::CREATED);
        let id = resp.1.id.clone();

        let got = get_work_schedule(
            State(state),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            Path(id.clone()),
        ).await.unwrap();
        assert_eq!(got.0.name, "Day shift");
    }

    #[tokio::test]
    async fn list_returns_created() {
        let state = make_ws_state().await;
        create_work_schedule(
            State(state.clone()),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            Json(create_req("agent-1")),
        ).await.unwrap();
        create_work_schedule(
            State(state.clone()),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            Json(create_req("agent-2")),
        ).await.unwrap();

        let resp = list_work_schedules(
            State(state),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            Query(ListWorkSchedulesQuery { agent_id: None, status: None }),
        ).await.unwrap();
        assert_eq!(resp.0.len(), 2);
    }

    #[tokio::test]
    async fn update_changes_name() {
        let state = make_ws_state().await;
        let resp = create_work_schedule(
            State(state.clone()),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            Json(create_req("agent-1")),
        ).await.unwrap();
        let id = resp.1.id.clone();

        let updated = update_work_schedule(
            State(state.clone()),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            Path(id.clone()),
            Json(UpdateWorkScheduleRequest {
                name: Some("Night shift".into()),
                start_cron: None, end_cron: None,
                pre_warn_minutes: None, final_warn_minutes: None,
                wake_prompt: None, status: None, timezone: None,
            }),
        ).await.unwrap();
        assert_eq!(updated.0.name, "Night shift");
    }

    #[tokio::test]
    async fn delete_removes() {
        let state = make_ws_state().await;
        let resp = create_work_schedule(
            State(state.clone()),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            Json(create_req("agent-1")),
        ).await.unwrap();
        let id = resp.1.id.clone();

        let status = delete_work_schedule(
            State(state.clone()),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            Path(id),
        ).await.unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT);

        let err = get_work_schedule(
            State(state),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            Path(resp.1.id.clone()),
        ).await.unwrap_err();
        assert_eq!(err.status, 404);
    }

    #[tokio::test]
    async fn invalid_cron_rejected() {
        let state = make_ws_state().await;
        let mut req = create_req("agent-1");
        req.start_cron = "not a cron".into();
        let err = create_work_schedule(
            State(state),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            Json(req),
        ).await.unwrap_err();
        assert_eq!(err.status, 400);
    }

    #[tokio::test]
    async fn get_missing_returns_404() {
        let state = make_ws_state().await;
        let err = get_work_schedule(
            State(state),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            Path("nonexistent".into()),
        ).await.unwrap_err();
        assert_eq!(err.status, 404);
    }

    #[tokio::test]
    async fn impossible_cron_rejected() {
        let state = make_ws_state().await;
        let mut req = create_req("agent-1");
        req.start_cron = "0 0 30 2 *".into(); // Feb 30 never exists
        let err = create_work_schedule(
            State(state),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            Json(req),
        ).await.unwrap_err();
        assert_eq!(err.status, 400);
        assert!(err.message.contains("never fire"));
    }

    #[tokio::test]
    async fn timezone_rejected_on_create() {
        let state = make_ws_state().await;
        let mut req = create_req("agent-1");
        req.timezone = Some("America/New_York".into());
        let err = create_work_schedule(
            State(state),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            Json(req),
        ).await.unwrap_err();
        assert_eq!(err.status, 400);
        assert!(err.message.contains("timezone"));
    }

    #[tokio::test]
    async fn timezone_rejected_on_update() {
        let state = make_ws_state().await;
        let created = create_work_schedule(
            State(state.clone()),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            Json(create_req("agent-1")),
        ).await.unwrap();
        let id = created.1.id.clone();
        let err = update_work_schedule(
            State(state),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            Path(id),
            Json(UpdateWorkScheduleRequest {
                name: None, start_cron: None, end_cron: None,
                pre_warn_minutes: None, final_warn_minutes: None,
                wake_prompt: None, status: None,
                timezone: Some(Some("America/New_York".into())),
            }),
        ).await.unwrap_err();
        assert_eq!(err.status, 400);
        assert!(err.message.contains("timezone"));
    }

    #[tokio::test]
    async fn pause_resets_duty_to_on_duty() {
        let state = make_ws_state().await;
        let agent: AgentId = "agent-pause".parse().unwrap();
        let created = create_work_schedule(
            State(state.clone()),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            Json(create_req("agent-pause")),
        ).await.unwrap();
        let id = created.1.id.clone();
        // Simulate the engine setting the agent off-duty.
        state.duty.set(agent.clone(), crate::duty::DutyStatus::OffDuty);
        assert!(state.duty.is_off_duty(&agent));
        // Pause the schedule.
        update_work_schedule(
            State(state.clone()),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            Path(id),
            Json(UpdateWorkScheduleRequest {
                name: None, start_cron: None, end_cron: None,
                pre_warn_minutes: None, final_warn_minutes: None,
                wake_prompt: None,
                status: Some(WorkScheduleStatus::Paused),
                timezone: None,
            }),
        ).await.unwrap();
        assert_eq!(state.duty.get(&agent), crate::duty::DutyStatus::OnDuty);
    }

    #[tokio::test]
    async fn delete_resets_duty_to_on_duty() {
        let state = make_ws_state().await;
        let agent: AgentId = "agent-del".parse().unwrap();
        let created = create_work_schedule(
            State(state.clone()),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            Json(create_req("agent-del")),
        ).await.unwrap();
        let id = created.1.id.clone();
        // Simulate the engine setting the agent off-duty.
        state.duty.set(agent.clone(), crate::duty::DutyStatus::OffDuty);
        assert!(state.duty.is_off_duty(&agent));
        // Delete the schedule.
        delete_work_schedule(
            State(state.clone()),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            Path(id),
        ).await.unwrap();
        assert_eq!(state.duty.get(&agent), crate::duty::DutyStatus::OnDuty);
    }
}
