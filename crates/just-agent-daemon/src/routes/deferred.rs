use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use just_agent_common::types::{
    DeferredActionDecisionBody, DeferredActionEntry, DeferredActionStatus,
    ListDeferredActionsResponse, SseEvent,
};
use just_agent_runtime::persistence;

use super::ListDeferredActionsQuery;
use crate::state::SharedState;

/// Maximum number of items returned by a single list request.
///
/// Client `limit` values are clamped to `[1, MAX_PAGE_SIZE]`.
const MAX_PAGE_SIZE: usize = 20;

pub async fn list_deferred_actions(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Query(params): Query<ListDeferredActionsQuery>,
) -> Result<Json<ListDeferredActionsResponse>, (StatusCode, String)> {
    let registry = state.registry.read().await;

    let mut entries: Vec<DeferredActionEntry> = Vec::new();
    for (agent_id, entry) in registry.iter() {
        if registry.require_superior(&auth.0, agent_id).is_err() {
            continue;
        }
        if let Some(ref filter_agent) = params.requested_by
            && agent_id != filter_agent
        {
            continue;
        }
        let q = entry.agent.deferred.lock().await;
        for info in q.list(None) {
            // Pending actions are not yet committed — not visible to superiors.
            if info.status == DeferredActionStatus::Pending {
                continue;
            }
            entries.push(DeferredActionEntry {
                id: info.id,
                requested_by: agent_id.clone(),
                content: info.content,
                reason: info.reason,
                dangerous: info.dangerous,
                status: info.status,
                deny_reason: info.deny_reason,
                created_at: info.created_at,
            });
        }
    }
    drop(registry);

    // Filter by status.
    if let Some(ref status_str) = params.status
        && let Some(filter) = DeferredActionStatus::from_str_name(status_str)
    {
        entries.retain(|e| e.status == filter);
    }

    // Sort by created_at with configurable direction.
    let descending = params.order.as_deref() != Some("asc");
    entries.sort_by(|a, b| {
        if descending {
            b.created_at.cmp(&a.created_at)
        } else {
            a.created_at.cmp(&b.created_at)
        }
    });

    let total = entries.len();
    let offset: usize = match params.offset.unwrap_or(0).try_into() {
        Ok(v) => v,
        Err(e) => return Err((StatusCode::BAD_REQUEST, format!("invalid offset: {e}"))),
    };
    let limit = params.limit.unwrap_or(5).clamp(1, MAX_PAGE_SIZE as u64) as usize;
    let items: Vec<_> = entries.into_iter().skip(offset).take(limit).collect();

    Ok(Json(ListDeferredActionsResponse { items, total }))
}

pub async fn get_deferred_action(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<String>,
) -> Result<Json<DeferredActionEntry>, (StatusCode, String)> {
    let registry = state.registry.read().await;
    for (agent_id, entry) in registry.iter() {
        let deferred = entry.agent.deferred.lock().await;
        if let Some(info) = deferred.get(&id) {
            registry.require_superior(&auth.0, agent_id)?;
            return Ok(Json(DeferredActionEntry {
                id: info.id,
                requested_by: agent_id.clone(),
                content: info.content,
                reason: info.reason,
                dangerous: info.dangerous,
                status: info.status,
                deny_reason: info.deny_reason,
                created_at: info.created_at,
            }));
        }
    }
    Err((StatusCode::NOT_FOUND, "approval not found".into()))
}

pub async fn respond_deferred_action(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<String>,
    Json(req): Json<DeferredActionDecisionBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let registry = state.registry.read().await;

    // Find the owning agent and apply the decision in a single deferred-lock
    // acquisition to prevent TOCTOU races with the agent loop.
    for (agent_id, entry) in registry.iter() {
        let mut deferred = entry.agent.deferred.lock().await;
        if !deferred.contains(&id) {
            continue;
        }

        registry.require_superior(&auth.0, agent_id)?;

        let json = match req.decision.as_str() {
            "approve" => {
                deferred
                    .approve(&id)
                    .map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;
                entry
                    .agent
                    .events_tx
                    .send(SseEvent::DeferredActionUpdated {
                        id: id.clone(),
                        status: DeferredActionStatus::Approved,
                    })
                    .ok();
                serde_json::to_string(&*deferred).ok()
            }
            "deny" => {
                let reason = req.reason.as_deref().unwrap_or("denied").to_owned();
                deferred
                    .deny(&id, &reason)
                    .map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;
                entry
                    .agent
                    .events_tx
                    .send(SseEvent::DeferredActionUpdated {
                        id: id.clone(),
                        status: DeferredActionStatus::Denied,
                    })
                    .ok();
                serde_json::to_string(&*deferred).ok()
            }
            _ => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "decision must be 'approve' or 'deny'".into(),
                ));
            }
        };

        // Persist while still holding the lock so the agent loop's
        // concurrent persist() cannot interleave a stale write.
        if let (Some(json), Some(dir)) = (json, entry.agent.session_dir.as_ref())
            && let Err(e) = persistence::persist_deferred(&json, dir)
        {
            tracing::error!("deferred persist after decision failed: {e:#}");
        }

        return Ok(StatusCode::OK);
    }

    Err((StatusCode::NOT_FOUND, "approval not found".into()))
}
