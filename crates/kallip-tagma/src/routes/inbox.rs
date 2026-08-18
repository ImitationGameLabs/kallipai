//! HTTP handlers for the agent inbox — list, read, summary, mark-done, clear.
//!
//! All routes are scoped under `/agents/{id}/inbox` and require the caller to
//! be the agent itself or the operator (`require_self_or_operator`).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use kallip_common::protocol::{ApiError, InboxEntry, InboxListQuery, InboxSummary};

use crate::inbox::InboxFilter;
use crate::state::SharedState;

/// Maximum number of items returned by a single list request.
const MAX_PAGE_SIZE: u32 = 200;

/// List messages in an agent's inbox with optional filtering.
pub async fn list_inbox(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<String>,
    Query(params): Query<InboxListQuery>,
) -> Result<Json<Vec<InboxEntry>>, ApiError> {
    let id = parse_agent_id(&id)?;
    authorize(&state, auth, &id).await?;

    let inboxes = state
        .inboxes
        .get()
        .ok_or_else(|| ApiError::internal("inbox store not initialized"))?;

    let filter = InboxFilter {
        status: params.status,
        limit: Some(params.limit.unwrap_or(50).min(MAX_PAGE_SIZE)),
    };
    let entries: Vec<InboxEntry> = inboxes.list(&id, &filter).await;
    Ok(Json(entries))
}

/// Read a single message (marks it as "read").
pub async fn read_inbox_message(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path((id, msg_id)): Path<(String, i64)>,
) -> Result<Json<InboxEntry>, ApiError> {
    let id = parse_agent_id(&id)?;
    authorize(&state, auth, &id).await?;

    let inboxes = state
        .inboxes
        .get()
        .ok_or_else(|| ApiError::internal("inbox store not initialized"))?;

    inboxes
        .read(&id, msg_id)
        .await
        .map(Json)
        .ok_or_else(|| ApiError::not_found("inbox message not found"))
}

/// Get inbox summary counts.
pub async fn inbox_summary(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<String>,
) -> Result<Json<InboxSummary>, ApiError> {
    let id = parse_agent_id(&id)?;
    authorize(&state, auth, &id).await?;

    let inboxes = state
        .inboxes
        .get()
        .ok_or_else(|| ApiError::internal("inbox store not initialized"))?;

    Ok(Json(inboxes.summary(&id).await))
}

/// Mark a message as done.
pub async fn mark_done(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path((id, msg_id)): Path<(String, i64)>,
) -> Result<StatusCode, ApiError> {
    let id = parse_agent_id(&id)?;
    authorize(&state, auth, &id).await?;

    let inboxes = state
        .inboxes
        .get()
        .ok_or_else(|| ApiError::internal("inbox store not initialized"))?;

    if inboxes.mark_done(&id, msg_id).await {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("inbox message not found"))
    }
}

/// Clear messages: done-only by default, all when `?all=true`.
pub async fn clear_inbox(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<String>,
    Query(params): Query<ClearParams>,
) -> Result<Json<ClearResponse>, ApiError> {
    let id = parse_agent_id(&id)?;
    authorize(&state, auth, &id).await?;

    let inboxes = state
        .inboxes
        .get()
        .ok_or_else(|| ApiError::internal("inbox store not initialized"))?;

    let cleared = inboxes.clear(&id, params.all).await;
    Ok(Json(ClearResponse { cleared }))
}

// -- Helpers -----------------------------------------------------------------

fn parse_agent_id(raw: &str) -> Result<crate::state::AgentId, ApiError> {
    raw.parse::<crate::state::AgentId>()
        .map_err(|_| ApiError::bad_request(format!("invalid agent id: {raw}")))
}

async fn authorize(
    state: &SharedState,
    auth: crate::auth::AuthIdentity,
    id: &crate::state::AgentId,
) -> Result<(), ApiError> {
    let registry = state.registry.read().await;
    registry.require_self_or_operator(auth.identity(), id)
}

/// Query params for `DELETE /agents/{id}/inbox`.
#[derive(Debug, serde::Deserialize, Default)]
pub struct ClearParams {
    #[serde(default)]
    pub all: bool,
}

/// Response body for clear.
#[derive(Debug, serde::Serialize)]
pub struct ClearResponse {
    pub cleared: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthIdentity, Identity};
    use crate::inbox::{BufferedEvent, InboxStore};
    use crate::test_helpers::*;
    use axum::extract::Path;
    use kallip_common::agentid::AgentId;
    use time::OffsetDateTime;

    fn event(source: &str, body: &str) -> BufferedEvent {
        BufferedEvent {
            timestamp: OffsetDateTime::now_utc(),
            source: source.to_string(),
            body: body.to_string(),
        }
    }

    #[tokio::test]
    async fn list_inbox_returns_entries() {
        let state = make_state();
        install_inbox_store(&state).await;
        let id = AgentId::random();

        {
            let mut reg = state.registry.write().await;
            add_root(&mut reg, &id);
        }

        let store = state.inboxes.get().unwrap();
        store.push(id.clone(), event("operator", "hello")).await;

        let resp = list_inbox(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path(id.to_string()),
            Query(InboxListQuery::default()),
        )
        .await
        .unwrap();

        assert_eq!(resp.0.len(), 1);
        assert_eq!(resp.0[0].body, "hello");
    }

    #[tokio::test]
    async fn list_inbox_rejects_other_agent() {
        let state = make_state();
        install_inbox_store(&state).await;
        let owner = AgentId::random();
        let other = AgentId::random();

        {
            let mut reg = state.registry.write().await;
            add_root(&mut reg, &owner);
            add_sub(&mut reg, &other, &owner);
        }

        let result = list_inbox(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: other }),
            Path(owner.to_string()),
            Query(InboxListQuery::default()),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().status, 403);
    }

    #[tokio::test]
    async fn summary_returns_counts() {
        let state = make_state();
        install_inbox_store(&state).await;
        let id = AgentId::random();

        {
            let mut reg = state.registry.write().await;
            add_root(&mut reg, &id);
        }

        let store = state.inboxes.get().unwrap();
        store.push(id.clone(), event("op", "one")).await;
        store.push(id.clone(), event("op", "two")).await;

        let resp = inbox_summary(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path(id.to_string()),
        )
        .await
        .unwrap();

        assert_eq!(resp.0.total, 2);
        assert_eq!(resp.0.unread, 2);
    }

    #[tokio::test]
    async fn mark_done_then_clear_done() {
        let state = make_state();
        install_inbox_store(&state).await;
        let id = AgentId::random();

        {
            let mut reg = state.registry.write().await;
            add_root(&mut reg, &id);
        }

        let store = state.inboxes.get().unwrap();
        store.push(id.clone(), event("op", "finish me")).await;
        let msg_id = store.list(&id, &InboxFilter::default()).await[0].id;

        // Mark done
        let status = mark_done(
            State(state.clone()),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path((id.to_string(), msg_id)),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT);

        // Clear done-only
        let resp = clear_inbox(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path(id.to_string()),
            Query(ClearParams { all: false }),
        )
        .await
        .unwrap();
        assert_eq!(resp.0.cleared, 1);
    }
}
