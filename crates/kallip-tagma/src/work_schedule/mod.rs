//! Work schedule: time-based agent lifecycle orchestration.
//!
//! The work schedule is a single tagma-wide resource (the root agent
//! carries it and delegates when woken): one spec, GET/PUT, no list.
//! The engine evaluates the spec and fires duty transitions; this module
//! provides the store-facing types and HTTP routes.

pub mod eval;
pub mod migration;
pub mod spec;
mod store;

pub use store::WorkScheduleStore;

use axum::Json;
use axum::extract::State;
use kallip_common::protocol::ApiError;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tracing::info;

use crate::state::SharedState;

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

/// The tagma's single work schedule. The spec is the structured form the
/// UI edits; warn minutes and the wake prompt keep their v1 semantics.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkSchedule {
    pub id: String,
    pub spec: spec::Spec,
    #[serde(default = "default_pre_warn")]
    pub pre_warn_minutes: u32,
    #[serde(default = "default_final_warn")]
    pub final_warn_minutes: u32,
    pub wake_prompt: String,
    #[serde(default)]
    pub status: WorkScheduleStatus,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

fn default_pre_warn() -> u32 {
    10
}
fn default_final_warn() -> u32 {
    5
}

/// Request body for PUT /work-schedule. The spec is required; a
/// status-only toggle (the UI's master switch) echoes the stored spec.
#[derive(Debug, Deserialize)]
pub struct PutWorkScheduleRequest {
    pub spec: spec::Spec,
    #[serde(default = "default_pre_warn")]
    pub pre_warn_minutes: u32,
    #[serde(default = "default_final_warn")]
    pub final_warn_minutes: u32,
    pub wake_prompt: Option<String>,
    #[serde(default)]
    pub status: WorkScheduleStatus,
}

fn get_store(state: &SharedState) -> Result<&WorkScheduleStore, ApiError> {
    state
        .work_schedules
        .get()
        .ok_or_else(|| ApiError::unavailable("work schedules not configured"))
}

// --- Route handlers ---

/// GET /work-schedule — the tagma's schedule. Always present: migration 04
/// seeds the singleton row, so the "unset" state no longer exists. A
/// missing row is invariant corruption, not a user-visible state.
pub async fn get_work_schedule(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
) -> Result<Json<WorkSchedule>, ApiError> {
    crate::auth::require_operator(auth.identity())?;
    let schedule = get_store(&state)?
        .get_singleton()
        .await
        .map_err(ApiError::internal)?;
    let schedule = schedule.ok_or_else(|| {
        tracing::error!("work-schedule row missing after migration 04");
        ApiError::internal("work-schedule row missing")
    })?;
    Ok(Json(schedule))
}

/// PUT /work-schedule — create (first PUT) or replace the tagma schedule.
///
/// The schedule is tagma-wide: it needs a root agent to carry it. An
/// interval spec re-anchors at save time — the rotation restarts from
/// now, keeping the requested minute-of-hour (the UI's M field) —
/// unless the request echoes the stored anchor verbatim (a
/// status-only toggle), which keeps it.
pub async fn put_work_schedule(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Json(req): Json<PutWorkScheduleRequest>,
) -> Result<Json<WorkSchedule>, ApiError> {
    crate::auth::require_operator(auth.identity())?;
    {
        let registry = state.registry.read().await;
        if registry.root_agent().is_none() {
            return Err(ApiError::bad_request(
                "the tagma has no root agent to carry the schedule",
            ));
        }
    }
    if let Err(e) = req.spec.validate() {
        return Err(ApiError::bad_request(format!("invalid spec: {e}")));
    }
    if req.pre_warn_minutes < req.final_warn_minutes {
        return Err(ApiError::bad_request(
            "pre_warn_minutes must be >= final_warn_minutes",
        ));
    }
    let mut spec = req.spec.clone();
    let store = get_store(&state)?;
    let existing = store.get_singleton().await.map_err(ApiError::internal)?;
    if let (
        spec::Spec::Interval {
            every_hours,
            length_min,
            anchor: req_anchor,
        },
        spec::Spec::Interval { anchor, .. },
    ) = (&req.spec, &mut spec)
    {
        // The stored anchor survives only the verbatim echo. Any changed
        // anchor — a new rhythm, or the UI editing the start minute M —
        // re-anchors at save time so the rotation restarts from now,
        // preserving the requested minute-of-hour as the phase.
        let rhythm_unchanged = matches!(
            existing.as_ref().map(|s| &s.spec),
            Some(spec::Spec::Interval {
                every_hours: h,
                length_min: l,
                ..
            }) if h == every_hours && l == length_min
        );
        let stored_anchor = match existing.as_ref().map(|s| &s.spec) {
            Some(spec::Spec::Interval { anchor, .. }) => Some(*anchor),
            _ => None,
        };
        if !(rhythm_unchanged && Some(*req_anchor) == stored_anchor) {
            *anchor = OffsetDateTime::now_utc()
                .replace_minute(req_anchor.minute())
                .unwrap()
                .replace_second(0)
                .unwrap()
                .replace_nanosecond(0)
                .unwrap();
        }
    }
    let was_active = existing
        .as_ref()
        .map(|s| s.status == WorkScheduleStatus::Active)
        .unwrap_or(false);
    let now_paused = was_active && req.status == WorkScheduleStatus::Paused;
    let (schedule, is_update) = match existing {
        Some(mut s) => {
            s.spec = spec;
            s.pre_warn_minutes = req.pre_warn_minutes;
            s.final_warn_minutes = req.final_warn_minutes;
            s.wake_prompt = req.wake_prompt.unwrap_or(s.wake_prompt);
            s.status = req.status;
            (s, true)
        }
        None => (
            WorkSchedule {
                id: uuid::Uuid::new_v4().to_string(),
                spec,
                pre_warn_minutes: req.pre_warn_minutes,
                final_warn_minutes: req.final_warn_minutes,
                wake_prompt: req.wake_prompt.unwrap_or_default(),
                status: req.status,
                created_at: OffsetDateTime::now_utc(),
            },
            false,
        ),
    };
    if is_update {
        let updated = store.update(&schedule).await.map_err(ApiError::internal)?;
        if !updated {
            return Err(ApiError::internal("work schedule write failed"));
        }
    } else {
        store.create(&schedule).await.map_err(ApiError::internal)?;
    }
    if now_paused {
        // Pausing releases the root from duty so messages are not
        // buffered indefinitely (mirrors the v1 pause semantics).
        if let Some((root_id, _)) = state.registry.read().await.root_agent() {
            state
                .duty
                .set(root_id.clone(), crate::duty::DutyStatus::OnDuty);
        }
        info!("work schedule paused, duty reset to on-duty");
    }
    info!(schedule_id = %schedule.id, "work schedule saved");
    Ok(Json(schedule))
}

#[cfg(test)]
mod route_tests {
    use super::*;
    use crate::test_helpers::{add_root, make_state};
    use axum::extract::State as ExtractState;

    async fn make_ws_state() -> SharedState {
        let state = make_state();
        let store = WorkScheduleStore::open_in_memory().await;
        state.work_schedules.set(store).ok();
        let root: crate::state::AgentId = "agent-1".parse().unwrap();
        let mut reg = state.registry.write().await;
        add_root(&mut reg, &root);
        drop(reg);
        state
    }

    fn put_req() -> PutWorkScheduleRequest {
        PutWorkScheduleRequest {
            spec: spec::Spec::Weekly {
                days: 0b0001_1111,
                start_minute: 540,
                end_minute: 1020,
            },
            pre_warn_minutes: 10,
            final_warn_minutes: 5,
            wake_prompt: Some("Good morning.".into()),
            status: WorkScheduleStatus::Active,
        }
    }

    #[tokio::test]
    async fn first_put_replaces_seed_and_round_trips() {
        let state = make_ws_state().await;
        let saved = put_work_schedule(
            ExtractState(state.clone()),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            axum::Json(put_req()),
        )
        .await
        .unwrap();
        let got = get_work_schedule(
            ExtractState(state),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
        )
        .await
        .unwrap();
        // The store keeps unix-second timestamps, so the sub-second part
        // of `created_at` does not survive the round trip.
        let mut expected = saved.0;
        expected.created_at = got.0.created_at;
        assert_eq!(got.0, expected);
    }

    #[tokio::test]
    async fn second_put_replaces_not_appends() {
        let state = make_ws_state().await;
        put_work_schedule(
            ExtractState(state.clone()),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            axum::Json(put_req()),
        )
        .await
        .unwrap();
        let mut req = put_req();
        req.status = WorkScheduleStatus::Paused;
        put_work_schedule(
            ExtractState(state.clone()),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            axum::Json(req),
        )
        .await
        .unwrap();
        let got = get_work_schedule(
            ExtractState(state),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(got.status, WorkScheduleStatus::Paused);
    }

    #[tokio::test]
    async fn put_rejects_invalid_spec() {
        let state = make_ws_state().await;
        let mut req = put_req();
        req.spec = spec::Spec::Weekly {
            days: 0,
            start_minute: 0,
            end_minute: 60,
        };
        let err = put_work_schedule(
            ExtractState(state),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            axum::Json(req),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("at least one day"));
    }

    #[tokio::test]
    async fn put_rejects_missing_root_agent() {
        let state = make_state();
        let store = WorkScheduleStore::open_in_memory().await;
        state.work_schedules.set(store).ok();
        let err = put_work_schedule(
            ExtractState(state),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            axum::Json(put_req()),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no root agent"));
    }

    #[tokio::test]
    async fn interval_put_normalizes_anchor_to_now() {
        let state = make_ws_state().await;
        let mut req = put_req();
        let stale = time::macros::datetime!(2020-01-01 0:00 UTC);
        req.spec = spec::Spec::Interval {
            every_hours: 5,
            length_min: 90,
            anchor: stale,
        };
        let saved = put_work_schedule(
            ExtractState(state),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            axum::Json(req),
        )
        .await
        .unwrap();
        match saved.0.spec {
            spec::Spec::Interval { anchor, .. } => {
                // Re-anchored to save time, keeping the requested
                // minute-of-hour (M=0): within the hour, aligned.
                assert!(
                    anchor.minute() == 0
                        && anchor.second() == 0
                        && (anchor - OffsetDateTime::now_utc()).whole_seconds().abs() < 3600
                );
            }
            _ => panic!("expected interval spec"),
        }
    }

    #[tokio::test]
    async fn status_only_interval_put_keeps_anchor() {
        let state = make_ws_state().await;
        let mut req = put_req();
        req.spec = spec::Spec::Interval {
            every_hours: 5,
            length_min: 90,
            anchor: time::macros::datetime!(2020-01-01 0:00 UTC),
        };
        put_work_schedule(
            ExtractState(state.clone()),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            axum::Json(req),
        )
        .await
        .unwrap();

        // Force a distinguishable stored anchor, as if saved long ago.
        let store = state.work_schedules.get().unwrap();
        let mut row = store.get_singleton().await.unwrap().unwrap();
        let stale = time::macros::datetime!(2020-01-01 0:00 UTC);
        if let spec::Spec::Interval { anchor, .. } = &mut row.spec {
            *anchor = stale;
        }
        assert!(store.update(&row).await.unwrap());

        // A status-only toggle echoes the stored spec: anchor survives.
        let toggle = PutWorkScheduleRequest {
            spec: row.spec.clone(),
            pre_warn_minutes: row.pre_warn_minutes,
            final_warn_minutes: row.final_warn_minutes,
            wake_prompt: None,
            status: WorkScheduleStatus::Paused,
        };
        let saved = put_work_schedule(
            ExtractState(state),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            axum::Json(toggle),
        )
        .await
        .unwrap();
        match saved.0.spec {
            spec::Spec::Interval { anchor, .. } => assert_eq!(anchor, stale),
            _ => panic!("expected interval spec"),
        }
    }

    #[tokio::test]
    async fn interval_minute_edit_reanchors_preserving_minute() {
        let state = make_ws_state().await;
        let mut req = put_req();
        req.spec = spec::Spec::Interval {
            every_hours: 5,
            length_min: 90,
            anchor: time::macros::datetime!(2020-01-01 0:00 UTC),
        };
        put_work_schedule(
            ExtractState(state.clone()),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            axum::Json(req),
        )
        .await
        .unwrap();

        // Same rhythm, but the UI asks for a different start minute M=37:
        // the anchor moves to save time carrying the new minute.
        let stored = state.work_schedules.get().unwrap();
        let row = stored.get_singleton().await.unwrap().unwrap();
        let mut edited = row.clone();
        if let spec::Spec::Interval { anchor, .. } = &mut edited.spec {
            *anchor = OffsetDateTime::now_utc()
                .replace_minute(37)
                .unwrap()
                .replace_second(0)
                .unwrap()
                .replace_nanosecond(0)
                .unwrap();
        }
        let saved = put_work_schedule(
            ExtractState(state),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            axum::Json(PutWorkScheduleRequest {
                spec: edited.spec,
                pre_warn_minutes: row.pre_warn_minutes,
                final_warn_minutes: row.final_warn_minutes,
                wake_prompt: None,
                status: row.status,
            }),
        )
        .await
        .unwrap();
        match saved.0.spec {
            spec::Spec::Interval { anchor, .. } => {
                assert_eq!(anchor.minute(), 37);
                assert_eq!(anchor.second(), 0);
                assert!((anchor - OffsetDateTime::now_utc()).whole_seconds().abs() < 3600);
            }
            _ => panic!("expected interval spec"),
        }
    }
}
