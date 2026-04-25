use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;

use super::ApprovalRequest;
use crate::state::SharedState;

pub async fn respond_approval(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(req): Json<ApprovalRequest>,
) -> Result<StatusCode, StatusCode> {
    let agents = state.agents.read().await;
    let entry = agents
        .iter()
        .find(|e| e.id == id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let mut deferred = entry.agent.deferred.lock().await;
    match req.decision.as_str() {
        "approve" => deferred
            .approve(&req.request_id)
            .map(|()| StatusCode::OK)
            .map_err(|_| StatusCode::NOT_FOUND),
        "deny" => deferred
            .deny(&req.request_id, req.reason.as_deref().unwrap_or("denied"))
            .map(|()| StatusCode::OK)
            .map_err(|_| StatusCode::NOT_FOUND),
        _ => Err(StatusCode::BAD_REQUEST),
    }
}
