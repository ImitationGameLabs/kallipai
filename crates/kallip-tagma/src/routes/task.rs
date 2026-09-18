//! The task coordination domain: REST over the task ledger. The tagma
//! process is the SOLE writer of tasks.sqlite — these routes are the
//! only write path; CLI processes never touch the file, they go
//! through this API.
//!
//! Every write verb maps 1:1 to a `kallip task` subcommand; responses
//! reuse the store's export face (stable field names, ISO 8601 UTC) so
//! the CLI renders exactly what `task export --json` prints. Gate and
//! transition errors are 409s (the state refused), unknown ids are 404s,
//! malformed request input (association keys, dossier paths) is 400s, and
//! anything unexpected is a 500 with the store error embedded.

use axum::Json;
use axum::extract::{Path, Query, State};
use kallip_common::protocol::{
    ApiError, TaskChainOpRequest, TaskCheckpointRequest, TaskCloseRequest, TaskCreateRequest,
    TaskDispatchRequest, TaskForceRequest, TaskListQuery, TaskNoteRequest,
};
use kallip_task::{TaskFilter, TaskStore};

use crate::bus::TaskChanged;
use crate::state::SharedState;
use tracing::warn;

/// The store lives behind a OnceLock installed at boot; a route seeing
/// None means the tagma booted without it, which is an operator-facing
/// configuration failure, not a caller error.
fn store(state: &SharedState) -> Result<&TaskStore, ApiError> {
    state
        .tasks
        .get()
        .map(|a| a.as_ref())
        .ok_or_else(|| ApiError::internal("task store not installed"))
}

fn blobs(state: &SharedState) -> Option<std::sync::Arc<dyn kallip_task::BlobStore>> {
    state.task_blobs.get().cloned()
}

/// The store's error taxonomy to the HTTP face: state refusals are
/// conflicts, unknown ids are misses, everything else is ours.
fn api_error(err: kallip_task::Error) -> ApiError {
    use kallip_task::Error as E;
    match &err {
        E::NotFound { id } => ApiError::not_found(format!("task {id} not found")),
        E::InvalidTransition { .. }
        | E::SerialGate { .. }
        | E::ReceiptGate { .. }
        | E::DispatchGate { .. }
        | E::GateReportGate { .. }
        | E::ArchiveGate { .. } => ApiError::conflict(err.to_string()),
        // Malformed association keys are a caller input problem, not a
        // server fault: 400, not 500.
        E::AssociationInvalid { .. } => ApiError::bad_request(err.to_string()),
        // DossierNotDir is about a caller-supplied dossier_path (the
        // create body registered it), so it stays in the 400 family;
        // the file system merely reports the bad input.
        E::DossierNotDir { .. } => ApiError::bad_request(err.to_string()),
        _ => ApiError::internal(err.to_string()),
    }
}

type TaskResult<T> = Result<Json<T>, ApiError>;

/// One wake broadcast per successful write verb. Publish failure is
/// log-and-drop by bus contract: no subscribers (watcher not running)
/// is benign, and the next verb on the task re-announces state.
fn notify(state: &SharedState, verb: &str, export: &kallip_task::TaskExport) {
    if let Err(err) = state.bus.publish(TaskChanged {
        task_id: export.id,
        title: export.title.clone(),
        status: export.status.to_string(),
        verb: verb.to_string(),
        creator: export.creator.clone(),
        assignee: export.assignee.clone(),
        seats: export.seats.clone(),
    }) {
        warn!(task = export.id, verb, error = %err, "task wake broadcast dropped");
    }
}
async fn create(
    State(state): State<SharedState>,
    Json(body): Json<TaskCreateRequest>,
) -> TaskResult<kallip_task::TaskExport> {
    let task = store(&state)?.create(body).await.map_err(api_error)?;
    let id = task.id;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    notify(&state, "create", &export);
    Ok(Json(export))
}

async fn start(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    Json(body): Json<TaskForceRequest>,
) -> TaskResult<kallip_task::TaskExport> {
    store(&state)?
        .start(id, &body.actor, body.force)
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    notify(&state, "start", &export);
    Ok(Json(export))
}

/// The list face is the compact store row (no trail); the export face is
/// the per-task detail. List first, then export what you need.
async fn list(
    State(state): State<SharedState>,
    Query(query): Query<TaskListQuery>,
) -> TaskResult<Vec<kallip_task::TaskExport>> {
    let filter = TaskFilter {
        status: query.status,
        assignee: query.assignee,
        archived: query.archived,
        time: query.time,
        since: query.since,
        until: query.until,
        limit: query.limit,
        offset: query.offset,
    };
    let rows = store(&state)?.list(filter).await.map_err(api_error)?;
    let all = store(&state)?.export_all().await.map_err(api_error)?;
    let ids: std::collections::HashSet<i64> = rows.iter().map(|t| t.id).collect();
    Ok(Json(
        all.into_iter().filter(|e| ids.contains(&e.id)).collect(),
    ))
}

async fn show(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> TaskResult<kallip_task::TaskExport> {
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    Ok(Json(export))
}

async fn export_one(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> TaskResult<kallip_task::TaskExport> {
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    Ok(Json(export))
}

async fn export_all(State(state): State<SharedState>) -> TaskResult<Vec<kallip_task::TaskExport>> {
    let all = store(&state)?.export_all().await.map_err(api_error)?;
    Ok(Json(all))
}

async fn checkpoint(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    Json(body): Json<TaskCheckpointRequest>,
) -> TaskResult<kallip_task::TaskExport> {
    store(&state)?
        .checkpoint(id, body)
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    notify(&state, "checkpoint", &export);
    Ok(Json(export))
}

async fn annotate(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    Json(body): Json<TaskNoteRequest>,
) -> TaskResult<kallip_task::TaskExport> {
    store(&state)?
        .annotate(id, &body.actor, body.note)
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    notify(&state, "annotate", &export);
    Ok(Json(export))
}

async fn gate_report(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    Json(body): Json<TaskNoteRequest>,
) -> TaskResult<kallip_task::TaskExport> {
    store(&state)?
        .gate_report(id, &body.actor, body.note)
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    notify(&state, "gate_report", &export);
    Ok(Json(export))
}

async fn dispatch(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    Json(body): Json<TaskDispatchRequest>,
) -> TaskResult<kallip_task::TaskExport> {
    store(&state)?
        .dispatch(id, &body.actor, body.seats)
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    notify(&state, "dispatch", &export);
    Ok(Json(export))
}

async fn chain_op(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    Json(body): Json<TaskChainOpRequest>,
) -> TaskResult<kallip_task::TaskExport> {
    store(&state)?
        .chain_op(id, &body.actor, &body.op, body.detail, body.force)
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    notify(&state, "chain_op", &export);
    Ok(Json(export))
}

async fn close(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    Json(body): Json<TaskCloseRequest>,
) -> TaskResult<kallip_task::TaskExport> {
    store(&state)?
        .close(
            id,
            &body.actor,
            body.reason,
            body.summary,
            body.force,
            blobs(&state),
        )
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    notify(&state, "close", &export);
    Ok(Json(export))
}

async fn reopen(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    Json(body): Json<TaskForceRequest>,
) -> TaskResult<kallip_task::TaskExport> {
    store(&state)?
        .reopen(id, &body.actor, body.force)
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    notify(&state, "reopen", &export);
    Ok(Json(export))
}

async fn archive(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    Json(body): Json<TaskForceRequest>,
) -> TaskResult<kallip_task::TaskExport> {
    store(&state)?
        .archive_task(id, &body.actor, body.force)
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    notify(&state, "archive", &export);
    Ok(Json(export))
}
/// Serves the closed-task archive blob: canonical tar bytes, verbatim
/// from the content-addressed store. 404 when the task has no closed
/// archive (never closed, or not yet archived).
async fn fetch_archive(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> Result<axum::response::Response, ApiError> {
    use axum::response::IntoResponse;
    let store = store(&state)?;
    let (task, _) = store.get(id).await.map_err(api_error)?;
    let blob = TaskStore::archive_blob_id(&task)
        .map_err(api_error)?
        .ok_or_else(|| ApiError::not_found(format!("task {id} has no closed archive")))?;
    let blobs = blobs(&state).ok_or_else(|| ApiError::internal("blob store not installed"))?;
    let bytes = blobs
        .get(&blob)
        .await
        .map_err(|err| ApiError::internal(err.to_string()))?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/x-tar")],
        bytes,
    )
        .into_response())
}

/// The task-domain router: mounted at /tasks by the root router.
pub(crate) fn router() -> axum::Router<SharedState> {
    axum::Router::new()
        .route("/", axum::routing::post(create).get(list))
        .route("/export", axum::routing::get(export_all))
        .route("/{id}", axum::routing::get(show))
        .route("/{id}/export", axum::routing::get(export_one))
        .route("/{id}/start", axum::routing::post(start))
        .route("/{id}/checkpoint", axum::routing::post(checkpoint))
        .route("/{id}/annotate", axum::routing::post(annotate))
        .route("/{id}/gate-report", axum::routing::post(gate_report))
        .route("/{id}/dispatch", axum::routing::post(dispatch))
        .route("/{id}/chain-op", axum::routing::post(chain_op))
        .route("/{id}/close", axum::routing::post(close))
        .route("/{id}/reopen", axum::routing::post(reopen))
        .route(
            "/{id}/archive",
            axum::routing::post(archive).get(fetch_archive),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gate taxonomy is the 409 family: every state refusal maps to
    /// conflict, unknown ids to not-found, and anything else (storage,
    /// serialization) stays internal — a wrong arm here would turn "the
    /// task refused" into "we broke", or worse, the reverse.
    #[test]
    fn gate_errors_map_to_conflict() {
        for err in [
            kallip_task::Error::InvalidTransition {
                id: 1,
                from: "queued".into(),
                action: "checkpoint".into(),
                expected: "in_progress|review".into(),
            },
            kallip_task::Error::SerialGate {
                assignee: "scout".into(),
                blocked_by: 1,
                title: "t".into(),
            },
            kallip_task::Error::ReceiptGate {
                missing: "scout".into(),
            },
            kallip_task::Error::DispatchGate { id: 1 },
            kallip_task::Error::GateReportGate { id: 1 },
            kallip_task::Error::ArchiveGate {
                id: 1,
                status: "queued".into(),
            },
        ] {
            assert_eq!(api_error(err).status, 409, "gates are conflicts");
        }
    }

    #[test]
    fn unknown_ids_map_to_not_found() {
        let err = kallip_task::Error::NotFound { id: 7 };
        assert_eq!(api_error(err).status, 404);
    }

    /// Malformed association keys ride in the request body: the caller's
    /// input problem is a 400, never a 500.
    #[test]
    fn association_invalid_maps_to_bad_request() {
        let err = kallip_task::Error::AssociationInvalid {
            detail: "empty range".into(),
        };
        assert_eq!(api_error(err).status, 400);
    }

    /// Same 400 family for a caller-supplied dossier path that turns out
    /// not to be a directory.
    #[test]
    fn dossier_not_dir_maps_to_bad_request() {
        let err = kallip_task::Error::DossierNotDir {
            path: "/no/such/dir".into(),
        };
        assert_eq!(api_error(err).status, 400);
    }

    #[test]
    fn anything_else_stays_internal() {
        let err = kallip_task::Error::Other("storage went away".into());
        assert_eq!(api_error(err).status, 500);
    }
}
