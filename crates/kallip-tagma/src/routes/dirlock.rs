//! Directory write-lock HTTP endpoints.
//!
//! Surface for the `kallip dirlock` CLI (which agents drive through
//! `bash_exec`). The tagma build unconditionally enables landlock enforcement
//! on Linux, so locks are mandatory there; on other platforms they are advisory.
//! See `kallip_runtime::dirlock` for the coordinator and its invariants.
//!
//! # Authorization
//!
//! Acquire/release use `require_self_or_operator` (operator or the agent
//! itself) — a supervisor must NOT be able to acquire/release a child's lock as
//! the child (that would defeat autonomous mutual exclusion). `who`/status
//! return only the holder's [`AgentId`], never role/description, to avoid
//! metadata disclosure and to keep the manager free of registry reads (lock
//! order: registry before manager).

use std::path::PathBuf;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use kallip_common::protocol::ApiError;
use serde::{Deserialize, Serialize};

use crate::state::SharedState;
use kallip_common::AgentId;
use kallip_runtime::dirlock::AcquireOutcome;

/// Default per-acquire retry budget when the caller omits `timeout_secs`. Kept
/// short so a blocked `bash_exec` returns holder info to the LLM quickly for
/// negotiation rather than hanging the round.
const DEFAULT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll interval during the bounded acquire retry.
const ACQUIRE_POLL: Duration = Duration::from_millis(200);

#[derive(Debug, Deserialize)]
pub struct AcquireRequest {
    pub path: String,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct AcquireResponse {
    pub acquired: bool,
    pub already_held: bool,
    /// Present only when acquisition failed (busy) — the holder's id, for
    /// inter-agent negotiation.
    pub holder: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReleaseRequest {
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct WhoQuery {
    pub dir: String,
}

#[derive(Debug, Serialize)]
pub struct WhoResponse {
    /// The holder's id, or null if the directory is unlocked.
    pub holder: Option<String>,
}

/// POST /agents/{id}/dirlocks — acquire the write-lock on `path` for agent `id`.
///
/// Bounded retry: returns the holder on timeout (409) so the caller can
/// negotiate. No unbounded await, so no deadlock.
pub async fn acquire(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Json(req): Json<AcquireRequest>,
) -> Result<Json<AcquireResponse>, ApiError> {
    // Compute the delegation ancestor chain under the read lock, then DROP the
    // guard before the retry loop. `created_by` is immutable post-creation, so
    // the owned id snapshot is stable across retries. Holding the read guard
    // across the loop's `sleep` would write-starve every registry writer
    // (create/remove/message handlers) for the full timeout.
    let chain: Vec<AgentId> = {
        let registry = state.registry.read().await;
        registry.require_self_or_operator(auth.identity(), &id)?;
        match registry.get(&id) {
            Some(entry) => match entry.identity().config.created_by.as_ref() {
                Some(supervisor_id) => registry.supervisor_chain_ids(supervisor_id)?,
                None => Vec::new(),
            },
            // require_self_or_operator passed, so the agent exists for an
            // operator call; unreachable for a self call.
            None => Vec::new(),
        }
    };
    let path = PathBuf::from(&req.path);
    let deadline = Duration::from_secs(
        req.timeout_secs
            .unwrap_or_else(|| DEFAULT_ACQUIRE_TIMEOUT.as_secs()),
    );

    let start = tokio::time::Instant::now();
    loop {
        match state.lock_manager.acquire(&id, &path, &chain) {
            Ok(AcquireOutcome::Acquired) => {
                return Ok(Json(AcquireResponse {
                    acquired: true,
                    already_held: false,
                    holder: None,
                }));
            }
            Ok(AcquireOutcome::AlreadyHeld) => {
                return Ok(Json(AcquireResponse {
                    acquired: true,
                    already_held: true,
                    holder: None,
                }));
            }
            Ok(AcquireOutcome::Busy { holder, conflict }) => {
                if start.elapsed() >= deadline {
                    return Err(ApiError::conflict(format!(
                        "directory write-lock on {} held by agent {holder}; \
                         peer-message the holder to coordinate, then retry",
                        conflict.display()
                    )));
                }
                tokio::time::sleep(ACQUIRE_POLL).await;
            }
            Err(e) => {
                // Path-canonicalize failure, per-agent cap, or self-overlap (the
                // path nests under / widens a lock you already hold): surface as
                // bad request (client-actionable) rather than a generic 5xx.
                return Err(ApiError::bad_request(e.to_string()));
            }
        }
    }
}

/// DELETE /agents/{id}/dirlocks — release the write-lock on `path` for agent `id`.
/// Idempotent.
pub async fn release(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Json(req): Json<ReleaseRequest>,
) -> Result<StatusCode, ApiError> {
    {
        let registry = state.registry.read().await;
        registry.require_self_or_operator(auth.identity(), &id)?;
    }
    let path = PathBuf::from(&req.path);
    state
        .lock_manager
        .release(&id, &path)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

/// GET /agents/{id}/dirlocks — this agent's currently held write-locks (canonical
/// paths).
pub async fn status(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<Json<Vec<String>>, ApiError> {
    {
        let registry = state.registry.read().await;
        registry.require_self_or_operator(auth.identity(), &id)?;
    }
    let paths = state
        .lock_manager
        .write_paths(&id)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(
        paths.into_iter().map(|p| p.display().to_string()).collect(),
    ))
}

/// GET /dirlocks?dir=PATH — who holds the write-lock on `dir`, if anyone.
///
/// A per-path lookup for inter-agent negotiation: a blocked caller needs the
/// holder's id to peer-message it (the same id `acquire` returns on conflict).
/// It is not a fleet-wide enumeration — the caller must already name a path.
/// Any authenticated identity may query (returns holder id only); this matches
/// the current permissive posture of `list_agents`. Revisit in a fleet-wide
/// authz pass if lock topology ever becomes sensitive.
pub async fn who(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Query(q): Query<WhoQuery>,
) -> Result<Json<WhoResponse>, ApiError> {
    let path = PathBuf::from(&q.dir);
    let holder = state
        .lock_manager
        .holder(&path)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(WhoResponse {
        holder: holder.map(|id| id.to_string()),
    }))
}
