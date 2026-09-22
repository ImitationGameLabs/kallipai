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
    ApiError, TaskCloseRequest, TaskConfirmRequest, TaskCreateRequest, TaskForceRequest,
    TaskListQuery, TaskNoteRequest,
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

/// The event actor for a verb, from the authenticated identity: the agent
/// id for agent tokens, the literal `operator` for the operator token.
/// The tagma records who called; callers cannot claim an actor.
fn acting_agent(auth: &crate::auth::AuthIdentity) -> String {
    match auth.identity() {
        crate::auth::Identity::Operator => "operator".to_string(),
        crate::auth::Identity::Agent { id } => id.to_string(),
    }
}

/// One id→role snapshot per request, cloned under the registry read lock
/// and used afterwards with no lock held (the mapping body is sync).
async fn role_snapshot(state: &SharedState) -> std::collections::HashMap<String, String> {
    state
        .registry
        .read()
        .await
        .iter()
        .map(|(id, entry)| (id.to_string(), entry.identity().config.role.clone()))
        .collect()
}

/// Fill `actor_role` on every event: the role for a registered agent id,
/// `None` for anything else (historical confirmer-name actors, the literal
/// `operator`, deregistered ids, null actors). Read and write responses
/// share this one mapping, so the wire type has a single fill policy.
fn inject_actor_roles(
    mut export: kallip_task::TaskExport,
    roles: &std::collections::HashMap<String, String>,
) -> kallip_task::TaskExport {
    for event in &mut export.events {
        event.actor_role = event
            .actor
            .as_deref()
            .and_then(|actor| roles.get(actor))
            .cloned();
    }
    export
}

/// The create verb's confirmer resolver, from the same snapshot shape:
/// role name → agent id. Unresolvable names yield `None` (the store
/// reports `ConfirmerUnresolved`).
async fn confirmer_resolver(
    state: &SharedState,
) -> std::sync::Arc<dyn Fn(&str) -> Option<String> + Send + Sync> {
    let registry = state.registry.read().await;
    let mut by_role: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (id, entry) in registry.iter() {
        by_role.insert(entry.identity().config.role.clone(), id.to_string());
    }
    drop(registry);
    std::sync::Arc::new(move |name| by_role.get(name).cloned())
}

/// The store's error taxonomy to the HTTP face: state refusals are
/// conflicts, unknown ids are misses, everything else is ours.
fn api_error(err: kallip_task::Error) -> ApiError {
    use kallip_task::Error as E;
    match &err {
        E::NotFound { id } => ApiError::not_found(format!("task {id} not found")),
        E::InvalidTransition { .. }
        | E::SerialGate { .. }
        | E::ConfirmationGate { .. }
        | E::ConfirmerUnresolved { .. }
        | E::ArchiveGate { .. } => ApiError::conflict(err.to_string()),
        // Malformed association keys are a caller input problem, not a
        // server fault: 400, not 500.
        E::AssociationInvalid { .. } => ApiError::bad_request(err.to_string()),
        // DossierNotDir is about a caller-supplied dossier_path (the
        // create body registered it), so it stays in the 400 family;
        // the file system merely reports the bad input.
        E::DossierNotDir { .. } => ApiError::bad_request(err.to_string()),
        // A confirm report over the shared byte cap is caller input:
        // 400, not 500 (the cap is REPORT_MAX_BYTES).
        E::ReportTooLarge { .. } => ApiError::bad_request(err.to_string()),
        _ => ApiError::internal(err.to_string()),
    }
}

type TaskResult<T> = Result<Json<T>, ApiError>;

/// One wake per successful write verb: bump the snapshot-pump
/// invalidation generation, then publish the TaskChanged wake.
/// Publish failure is log-and-drop by bus contract: no subscribers
/// (watcher not running) is benign, and the next verb on the task
/// re-announces state.
/// The generation is level-triggered: concurrent writers can coalesce
/// onto one step, so a wake reads "state moved", not "exactly one write".
fn notify(state: &SharedState, verb: &str, export: &kallip_task::TaskExport) {
    state.invalidate();
    if let Err(err) = state.bus.publish(TaskChanged {
        task_id: export.id,
        title: export.title.clone(),
        status: export.status.to_string(),
        verb: verb.to_string(),
        creator: export.creator.clone(),
        assignee: export.assignee.clone(),
        confirmers: export.confirmers.clone(),
    }) {
        warn!(task = export.id, verb, error = %err, "task wake broadcast dropped");
    }
}
async fn create(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Json(body): Json<TaskCreateRequest>,
) -> TaskResult<kallip_task::TaskExport> {
    let actor = acting_agent(&auth);
    let snapshot = role_snapshot(&state).await;
    let task = store(&state)?
        .create(body, &actor, confirmer_resolver(&state).await)
        .await
        .map_err(api_error)?;
    let id = task.id;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    let export = inject_actor_roles(export, &snapshot);
    notify(&state, "create", &export);
    Ok(Json(export))
}

async fn start(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<i64>,
    Json(body): Json<TaskForceRequest>,
) -> TaskResult<kallip_task::TaskExport> {
    let actor = acting_agent(&auth);
    let snapshot = role_snapshot(&state).await;
    store(&state)?
        .start(id, &actor, body.force)
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    let export = inject_actor_roles(export, &snapshot);
    notify(&state, "start", &export);
    Ok(Json(export))
}

async fn list(
    State(state): State<SharedState>,
    Query(query): Query<TaskListQuery>,
) -> TaskResult<kallip_common::protocol::TaskListPage> {
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
    let page = store(&state)?.list_page(filter).await.map_err(api_error)?;
    Ok(Json(page))
}

async fn show(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> TaskResult<kallip_task::TaskExport> {
    let snapshot = role_snapshot(&state).await;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    Ok(Json(inject_actor_roles(export, &snapshot)))
}

async fn export_one(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> TaskResult<kallip_task::TaskExport> {
    let snapshot = role_snapshot(&state).await;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    Ok(Json(inject_actor_roles(export, &snapshot)))
}

async fn export_all(State(state): State<SharedState>) -> TaskResult<Vec<kallip_task::TaskExport>> {
    let snapshot = role_snapshot(&state).await;
    let all = store(&state)?.export_all().await.map_err(api_error)?;
    Ok(Json(
        all.into_iter()
            .map(|export| inject_actor_roles(export, &snapshot))
            .collect(),
    ))
}

async fn confirm(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<i64>,
    Json(body): Json<TaskConfirmRequest>,
) -> TaskResult<kallip_task::TaskExport> {
    let actor = acting_agent(&auth);
    let snapshot = role_snapshot(&state).await;
    store(&state)?
        .confirm(id, &actor, body)
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    let export = inject_actor_roles(export, &snapshot);
    notify(&state, "confirm", &export);
    Ok(Json(export))
}

async fn review(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<i64>,
) -> TaskResult<kallip_task::TaskExport> {
    let actor = acting_agent(&auth);
    let snapshot = role_snapshot(&state).await;
    store(&state)?.review(id, &actor).await.map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    let export = inject_actor_roles(export, &snapshot);
    notify(&state, "review", &export);
    Ok(Json(export))
}

async fn pause(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<i64>,
) -> TaskResult<kallip_task::TaskExport> {
    let actor = acting_agent(&auth);
    let snapshot = role_snapshot(&state).await;
    store(&state)?.pause(id, &actor).await.map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    let export = inject_actor_roles(export, &snapshot);
    notify(&state, "pause", &export);
    Ok(Json(export))
}

async fn resume(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<i64>,
    Json(body): Json<TaskForceRequest>,
) -> TaskResult<kallip_task::TaskExport> {
    let actor = acting_agent(&auth);
    let snapshot = role_snapshot(&state).await;
    store(&state)?
        .resume(id, &actor, body.force)
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    let export = inject_actor_roles(export, &snapshot);
    notify(&state, "resume", &export);
    Ok(Json(export))
}

async fn note(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<i64>,
    Json(body): Json<TaskNoteRequest>,
) -> TaskResult<kallip_task::TaskExport> {
    let actor = acting_agent(&auth);
    let snapshot = role_snapshot(&state).await;
    store(&state)?
        .note(id, &actor, body.note)
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    let export = inject_actor_roles(export, &snapshot);
    notify(&state, "note", &export);
    Ok(Json(export))
}

async fn close(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<i64>,
    Json(body): Json<TaskCloseRequest>,
) -> TaskResult<kallip_task::TaskExport> {
    let actor = acting_agent(&auth);
    let snapshot = role_snapshot(&state).await;
    let closed = store(&state)?.close(
        id,
        &actor,
        body.reason,
        body.summary,
        body.force,
        blobs(&state),
    );
    match closed.await {
        Ok(_) => {}
        // The gate message carries identity ids; the human face wants
        // role names, so map through the same snapshot the events use.
        Err(kallip_task::Error::ConfirmationGate { missing }) => {
            let names: Vec<String> = missing
                .split(", ")
                .map(|id| snapshot.get(id).cloned().unwrap_or_else(|| id.to_string()))
                .collect();
            return Err(ApiError::conflict(format!(
                "confirmation gate: missing confirmations from registered confirmers: {}; --force to override (escape is recorded)",
                names.join(", ")
            )));
        }
        Err(e) => return Err(api_error(e)),
    };
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    let export = inject_actor_roles(export, &snapshot);
    notify(&state, "close", &export);
    Ok(Json(export))
}

async fn reopen(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<i64>,
    Json(body): Json<TaskForceRequest>,
) -> TaskResult<kallip_task::TaskExport> {
    let actor = acting_agent(&auth);
    let snapshot = role_snapshot(&state).await;
    store(&state)?
        .reopen(id, &actor, body.force)
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    let export = inject_actor_roles(export, &snapshot);
    notify(&state, "reopen", &export);
    Ok(Json(export))
}

async fn archive(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<i64>,
    Json(body): Json<TaskForceRequest>,
) -> TaskResult<kallip_task::TaskExport> {
    let actor = acting_agent(&auth);
    let snapshot = role_snapshot(&state).await;
    store(&state)?
        .archive_task(id, &actor, body.force)
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    let export = inject_actor_roles(export, &snapshot);
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
        .route("/{id}/confirm", axum::routing::post(confirm))
        .route("/{id}/review", axum::routing::post(review))
        .route("/{id}/pause", axum::routing::post(pause))
        .route("/{id}/resume", axum::routing::post(resume))
        .route("/{id}/note", axum::routing::post(note))
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
    use crate::test_helpers::*;
    use std::sync::Arc;

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
                action: "confirm".into(),
                expected: "in_progress|review".into(),
            },
            kallip_task::Error::SerialGate {
                assignee: "scout".into(),
                blocked_by: 1,
                title: "t".into(),
            },
            kallip_task::Error::ConfirmationGate {
                missing: "scout".into(),
            },
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

    /// Same 400 family: a confirm report over the shared cap is
    /// caller input, not a server fault.
    #[test]
    fn report_too_large_maps_to_bad_request() {
        let err = kallip_task::Error::ReportTooLarge {
            size: kallip_common::protocol::REPORT_MAX_BYTES + 1,
            max: kallip_common::protocol::REPORT_MAX_BYTES,
        };
        assert_eq!(api_error(err).status, 400);
    }

    /// A create-time confirmer name resolving to no registered identity is a
    /// state refusal (409), not a caller mistake.
    #[test]
    fn confirmer_unresolved_maps_to_conflict() {
        let err = kallip_task::Error::ConfirmerUnresolved {
            confirmer: "ghost".into(),
        };
        assert_eq!(api_error(err).status, 409);
    }

    /// The actor is the authenticated identity: agent ids pass
    /// through, the operator token records the literal `operator`.
    #[test]
    fn acting_agent_follows_the_authenticated_identity() {
        let agent = crate::auth::AuthIdentity::test_new(crate::auth::Identity::Agent {
            id: "agent-9".parse().unwrap(),
        });
        assert_eq!(acting_agent(&agent), "agent-9");
        let op = crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator);
        assert_eq!(acting_agent(&op), "operator");
    }

    /// The single fill policy: a registered agent id gains its role,
    /// everything else (confirmer names, the operator literal, deregistered
    /// ids, null actors) keeps actor_role null.
    #[test]
    fn actor_role_injection_covers_every_actor_shape() {
        let roles =
            std::collections::HashMap::from([("agent-1".to_string(), "reviewer-c".to_string())]);
        let export = inject_actor_roles(
            export_with_events(&[
                Some("agent-1"),
                Some("reviewer-c"),
                Some("agent-gone"),
                Some("operator"),
                None,
            ]),
            &roles,
        );
        let got: Vec<Option<&str>> = export
            .events
            .iter()
            .map(|e| e.actor_role.as_deref())
            .collect();
        assert_eq!(got, vec![Some("reviewer-c"), None, None, None, None]);
    }

    /// Test fixture: an export whose events differ only in actor.
    fn export_with_events(actors: &[Option<&str>]) -> kallip_task::TaskExport {
        kallip_task::TaskExport {
            id: 1,
            title: "t".into(),
            status: "queued".into(),
            creator: None,
            assignee: None,
            confirmers: vec![],
            created_at: None,
            updated_at: None,
            started_at: None,
            ended_at: None,
            archived: false,
            archived_at: None,
            closed_reason: None,
            close_summary: None,
            association: None,
            dossier_path: None,
            archive_hash: None,
            events: actors
                .iter()
                .map(|a| kallip_common::protocol::EventExport {
                    id: 0,
                    kind: "action".into(),
                    name: "confirm".into(),
                    actor: a.map(|s| s.to_string()),
                    actor_role: None,
                    assignee: None,
                    from_status: None,
                    to_status: None,
                    payload: None,
                    created_at: None,
                })
                .collect(),
        }
    }

    /// Every write verb wakes the snapshot pumps: the handlers share one
    /// `notify` tail, so a verb that skips the bump would leave the
    /// header stale until a fallback ticker catches up. `close` runs
    /// twice because `reopen` and `archive` each need a closed task in
    /// front of them.
    #[tokio::test]
    async fn every_write_verb_bumps_the_invalidation_generation() {
        let state = make_state();
        state
            .tasks
            .set(Arc::new(kallip_task::TaskStore::open_in_memory().await))
            .ok()
            .expect("task store installs once");
        let mut rx = state.subscribe_invalidations();
        let mut generation = *rx.borrow();
        let auth = crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator);
        let force = Json(TaskForceRequest::default());
        let created = create(
            State(Arc::clone(&state)),
            auth.clone(),
            Json(TaskCreateRequest {
                title: "gen".into(),
                ..Default::default()
            }),
        )
        .await
        .expect("create succeeds");
        let id = created.0.id;
        expect_bump(&mut rx, &mut generation, "create").await;
        let _ = start(
            State(Arc::clone(&state)),
            auth.clone(),
            Path(id),
            force.clone(),
        )
        .await
        .expect("start succeeds");
        expect_bump(&mut rx, &mut generation, "start").await;
        let _ = note(
            State(Arc::clone(&state)),
            auth.clone(),
            Path(id),
            Json(TaskNoteRequest { note: "n".into() }),
        )
        .await
        .expect("note succeeds");
        expect_bump(&mut rx, &mut generation, "note").await;
        let _ = pause(State(Arc::clone(&state)), auth.clone(), Path(id))
            .await
            .expect("pause succeeds");
        expect_bump(&mut rx, &mut generation, "pause").await;
        let _ = resume(
            State(Arc::clone(&state)),
            auth.clone(),
            Path(id),
            force.clone(),
        )
        .await
        .expect("resume succeeds");
        expect_bump(&mut rx, &mut generation, "resume").await;
        let _ = confirm(
            State(Arc::clone(&state)),
            auth.clone(),
            Path(id),
            Json(TaskConfirmRequest::default()),
        )
        .await
        .expect("confirm succeeds");
        expect_bump(&mut rx, &mut generation, "confirm").await;
        let _ = review(State(Arc::clone(&state)), auth.clone(), Path(id))
            .await
            .expect("review succeeds");
        expect_bump(&mut rx, &mut generation, "review").await;
        let close_req = || {
            Json(TaskCloseRequest {
                reason: kallip_common::protocol::ClosedReason::Completed,
                summary: None,
                force: false,
            })
        };
        let _ = close(
            State(Arc::clone(&state)),
            auth.clone(),
            Path(id),
            close_req(),
        )
        .await
        .expect("close succeeds");
        expect_bump(&mut rx, &mut generation, "close").await;
        let _ = reopen(
            State(Arc::clone(&state)),
            auth.clone(),
            Path(id),
            force.clone(),
        )
        .await
        .expect("reopen succeeds");
        expect_bump(&mut rx, &mut generation, "reopen").await;
        let _ = close(
            State(Arc::clone(&state)),
            auth.clone(),
            Path(id),
            close_req(),
        )
        .await
        .expect("second close succeeds");
        expect_bump(&mut rx, &mut generation, "close").await;
        let _ = archive(
            State(Arc::clone(&state)),
            auth.clone(),
            Path(id),
            force.clone(),
        )
        .await
        .expect("archive succeeds");
        expect_bump(&mut rx, &mut generation, "archive").await;
        // A refused verb skips the notify tail: confirm needs
        // in_progress|review, so confirming a fresh queued task is a 409
        // refusal that must leave the generation where it was.
        let queued = create(
            State(Arc::clone(&state)),
            auth.clone(),
            Json(TaskCreateRequest {
                title: "refused".into(),
                ..Default::default()
            }),
        )
        .await
        .expect("second create succeeds");
        expect_bump(&mut rx, &mut generation, "create").await;
        let refused = confirm(
            State(Arc::clone(&state)),
            auth.clone(),
            Path(queued.0.id),
            Json(TaskConfirmRequest::default()),
        )
        .await;
        assert!(refused.is_err(), "confirm on queued is refused");
        assert_eq!(
            *rx.borrow(),
            generation,
            "a refused verb leaves the generation alone"
        );
    }

    /// The per-verb assertion: each successful verb advances the watch
    /// generation by exactly one — more would mean double wakes, fewer
    /// would mean a verb skipped its notify tail.
    async fn expect_bump(
        rx: &mut tokio::sync::watch::Receiver<u64>,
        generation: &mut u64,
        verb: &str,
    ) {
        let changed = tokio::time::timeout(std::time::Duration::from_secs(5), rx.changed())
            .await
            .unwrap_or_else(|_| panic!("{verb}: generation did not advance"));
        changed.expect("the invalidation sender lives on the AppState");
        *generation += 1;
        assert_eq!(
            *rx.borrow(),
            *generation,
            "{verb} advances the generation by exactly one"
        );
    }
}
