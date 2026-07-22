//! Skill promote request routes: submit, list, show, approve/deny.
//!
//! The promote-request system lets any agent submit a skill for review.
//! Root agents or operators review and decide. Content is snapshotted at
//! submission time (TOCTOU protection).
//!
//! Skills are identified by their path relative to the skills root (e.g.
//! `code/refactoring`), not by the `name` field in YAML frontmatter.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use kallip_common::agentid::AgentId;
use kallip_common::promote::{CreatePromoteRequest, NO_REASON_PROVIDED, SkillPromoteStatus};
use kallip_common::protocol::{
    ApiError, ListSkillPromoteRecordsResponse, PromoteDecision, SkillPromoteDecisionBody,
    SkillPromoteRecordEntry, SkillPromoteShowResponse, SkillPromoteSubmitResponse,
};
use kallip_runtime::tools::skill::promote_skill_from_content;
use kallip_runtime::tools::{
    META_SKILL_NAME, parse_frontmatter_meta, skill_dir, validate_skill_name,
};
use serde::Deserialize;
use tracing::{info, warn};

use crate::state::SharedState;

/// Query parameters for GET /skill-promote-requests.
#[derive(Debug, Default, Deserialize)]
pub struct ListPromoteQuery {
    pub status: Option<String>,
}

// ---------------------------------------------------------------------------
// POST /agents/{id}/skills/{name}/promote-request — submit
// ---------------------------------------------------------------------------

pub async fn submit_promote_request(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path((id, skill_name)): Path<(AgentId, String)>,
) -> Result<impl IntoResponse, ApiError> {
    // Auth: caller must be the agent itself or operator.
    {
        let registry = state.registry.read().await;
        registry.require_self_or_operator(auth.identity(), &id)?;
    }

    // Validate skill name.
    validate_skill_name(&skill_name).map_err(|e| ApiError::bad_request(e.to_string()))?;

    if skill_name == META_SKILL_NAME {
        return Err(ApiError::bad_request(format!(
            "cannot promote the '{META_SKILL_NAME}' skill; \
             it is managed by the skill system"
        )));
    }

    // Extract agent_dir and snapshot content under registry read lock.
    let (content, meta) = {
        let registry = state.registry.read().await;
        let entry = registry
            .get(&id)
            .ok_or_else(|| ApiError::not_found("agent not found"))?;
        let agent_dir = entry
            .identity()
            .agent_dir
            .clone()
            .ok_or_else(|| ApiError::not_found("agent has no persistent directory"))?;

        let src = agent_dir.join("skills").join(format!("{skill_name}.md"));
        if !src.exists() {
            return Err(ApiError::not_found(format!(
                "local skill '{skill_name}' not found at {}",
                src.display()
            )));
        }

        let content = std::fs::read_to_string(&src)
            .map_err(|e| ApiError::internal(format!("failed to read local skill: {e}")))?;

        // Verify frontmatter and capture metadata in a single pass.
        let meta = parse_frontmatter_meta(&content).ok_or_else(|| {
            ApiError::bad_request(format!(
                "skill '{skill_name}' has no valid frontmatter; \
                 a 'name' field in YAML frontmatter is required"
            ))
        })?;

        (content, meta)
    };

    // Snapshot old content from shared directory (no lock needed — read-only).
    let shared = skill_dir().map_err(ApiError::internal)?;
    let shared_path = shared.join(format!("{skill_name}.md"));
    let old_content = if shared_path.exists() {
        Some(
            std::fs::read_to_string(&shared_path)
                .map_err(|e| ApiError::internal(format!("failed to read shared skill: {e}")))?,
        )
    } else {
        None
    };

    let has_existing = old_content.is_some();
    let description = meta.description;
    let request_id = {
        let mut store = state.skill_promote_store.lock().await;
        store.create(CreatePromoteRequest {
            skill_name: skill_name.clone(),
            has_existing,
            new_content: content,
            old_content,
            description,
            requested_by: id.clone(),
        })
    };

    // Notify the tagma's single root agent.
    {
        let registry = state.registry.read().await;
        let notification = format!(
            "[Skill Promotion Request] Agent {id} requests to promote skill '{skill_name}' \
             to the shared directory.\n\
             Request ID: {request_id}\n\n\
             Use `kallip skill promote approve {request_id}` to approve\n\
             or `kallip skill promote deny {request_id} <reason>` to deny."
        );
        if let Some((root_id, entry)) = registry.root_agent() {
            // A faulted root cannot review a promotion (no prompt channel); skip it.
            if let Some(live) = entry.as_live()
                && let Err(e) = live.agent.prompt_tx.try_send(notification)
            {
                warn!(root_id = %root_id, "failed to notify root agent of promote request: {e}");
            }
        }
    }

    info!(id = %id, skill = %skill_name, request_id = %request_id, "promote request submitted");

    Ok((
        StatusCode::CREATED,
        Json(SkillPromoteSubmitResponse {
            request_id,
            skill_name,
            status: SkillPromoteStatus::Pending,
            has_existing,
        }),
    ))
}

// ---------------------------------------------------------------------------
// GET /skill-promote-requests — list
// ---------------------------------------------------------------------------

pub async fn list_promote_requests(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Query(query): Query<ListPromoteQuery>,
) -> Result<impl IntoResponse, ApiError> {
    // Open to any authenticated agent: subagents may help review promote requests.

    let status_filter = match query.status.as_deref() {
        Some(s) => Some(SkillPromoteStatus::from_str_name(s).ok_or_else(|| {
            ApiError::bad_request(format!(
                "invalid status filter '{s}'; \
                     valid values: pending, approved, denied"
            ))
        })?),
        None => None,
    };

    let store = state.skill_promote_store.lock().await;
    let items: Vec<SkillPromoteRecordEntry> = store
        .list(status_filter.as_ref())
        .into_iter()
        .map(SkillPromoteRecordEntry::from_record)
        .collect();
    let total = items.len();

    Ok(Json(ListSkillPromoteRecordsResponse { items, total }))
}

// ---------------------------------------------------------------------------
// GET /skill-promote-requests/{id} — show
// ---------------------------------------------------------------------------

pub async fn show_promote_request(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(request_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    // Open to any authenticated agent: subagents may help review promote requests.

    let store = state.skill_promote_store.lock().await;
    let record = store
        .get(&request_id)
        .ok_or_else(|| ApiError::not_found(format!("promote request '{request_id}' not found")))?;

    Ok(Json(SkillPromoteShowResponse::from_record(record)))
}

// ---------------------------------------------------------------------------
// POST /skill-promote-requests/{id} — approve or deny
// ---------------------------------------------------------------------------

pub async fn respond_promote_request(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(request_id): Path<String>,
    Json(body): Json<SkillPromoteDecisionBody>,
) -> Result<StatusCode, ApiError> {
    {
        let registry = state.registry.read().await;
        registry.require_root_or_operator(auth.identity())?;
    }

    match body.decision {
        PromoteDecision::Approve => handle_approve(&state, &request_id).await,
        PromoteDecision::Deny => handle_deny(&state, &request_id, body.reason.as_deref()).await,
    }
}

async fn handle_approve(state: &SharedState, request_id: &str) -> Result<StatusCode, ApiError> {
    // Hold the store lock for the entire approve operation to prevent
    // concurrent approve/deny races. Promote requests are infrequent
    // human-initiated operations and the skill files are small, so holding
    // the lock across file I/O is acceptable.
    //
    // Note: the file I/O below uses std::fs (blocking) rather than
    // tokio::fs. tokio::fs wraps std::fs in spawn_blocking, which avoids
    // blocking the executor but adds scheduling overhead per call. For
    // sub-10μs reads/writes on a cold path, direct std::fs under a
    // tokio::sync::Mutex is simpler and faster.
    let mut store = state.skill_promote_store.lock().await;

    // Step 1: Validate the request is still Pending and get its data.
    let record = store.get_pending(request_id).map_err(ApiError::from)?;

    // Acquire the shared skill directory write lock. Lock order:
    // skill_promote_store → skill_write_lock (see AppState::skill_write_lock).
    // This serializes concurrent approve operations on different requests so
    // the consistency check + write cannot interleave.
    let _write_guard = state.skill_write_lock.lock().await;

    // Step 2: Consistency check — re-read the current shared file and compare
    // with snapshotted old_content.
    let shared = skill_dir().map_err(ApiError::internal)?;
    let skill_name = &record.skill_name;
    let shared_path = shared.join(format!("{skill_name}.md"));

    if record.old_content.is_some() {
        // Shared skill existed at submission time — verify it hasn't changed.
        let current = std::fs::read_to_string(&shared_path)
            .map_err(|e| ApiError::internal(format!("failed to read shared skill: {e}")))?;
        if current != record.old_content.as_deref().unwrap() {
            return Err(ApiError::conflict(format!(
                "shared skill '{}' was modified while this request was pending; \
                 please resubmit",
                record.skill_name
            )));
        }
    } else if shared_path.exists() {
        // Shared skill didn't exist at submission, but one appeared — race.
        return Err(ApiError::conflict(format!(
            "a shared skill '{}' was created while this request was pending; \
             please resubmit",
            record.skill_name
        )));
    }

    // Step 3: Execute promotion using snapshotted content. The consistency
    // check above already validated that the shared file state matches the
    // submission-time snapshot, so unconditional overwrite is correct here.
    promote_skill_from_content(&record.skill_name, &record.new_content, &shared)
        .map_err(ApiError::internal)?;

    // Step 4: File I/O succeeded — commit the status transition.
    // Since we hold the store lock throughout, no concurrent deny can
    // interfere between steps 1 and 4.
    store.commit_approved(request_id).map_err(ApiError::from)?;

    // Release the store lock before notification (which acquires registry lock).
    drop(store);

    info!(
        id = %request_id,
        skill = %record.skill_name,
        "promote request approved and executed"
    );

    // Notify requesting agent.
    notify_requesting_agent(
        state,
        &record.requested_by,
        &format!(
            "[Skill Promotion Result] Your request to promote skill '{}' has been approved. \
             The skill is now available at the shared directory.",
            record.skill_name
        ),
    )
    .await;

    Ok(StatusCode::OK)
}

async fn handle_deny(
    state: &SharedState,
    request_id: &str,
    reason: Option<&str>,
) -> Result<StatusCode, ApiError> {
    let (requested_by, skill_name) = {
        let mut store = state.skill_promote_store.lock().await;
        store.deny(request_id, reason).map_err(ApiError::from)?
    };

    info!(id = %request_id, skill = %skill_name, reason = ?reason, "promote request denied");

    // Display-layer placeholder: the stored `deny_reason` is None;
    // the notification message substitutes the shared constant.
    let reason_clause = match reason {
        Some(r) => format!("denied: {r}"),
        None => format!("denied: {NO_REASON_PROVIDED}"),
    };
    notify_requesting_agent(
        state,
        &requested_by,
        &format!(
            "[Skill Promotion Result] Your request to promote a skill has been {reason_clause}."
        ),
    )
    .await;

    Ok(StatusCode::OK)
}

/// Send a notification to the requesting agent.
/// Fails silently if the agent is terminated or its channel is full (logged, not an error).
async fn notify_requesting_agent(state: &SharedState, agent_id: &AgentId, message: &str) {
    let registry = state.registry.read().await;
    if let Some(entry) = registry.get(agent_id)
        && let Some(live) = entry.as_live()
    {
        if let Err(e) = live.agent.prompt_tx.try_send(message.to_owned()) {
            warn!(id = %agent_id, "failed to notify agent: {e}");
        }
    } else {
        warn!(id = %agent_id, "requesting agent not available for notification");
    }
}
