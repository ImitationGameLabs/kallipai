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
use kallipai_common::protocol::{
    ApiError, TaskCloseRequest, TaskConfirmRequest, TaskCreateRequest, TaskForceRequest,
    TaskListQuery, TaskNoteRequest,
};
use kallipai_task::{TaskFilter, TaskStore};

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

fn blobs(state: &SharedState) -> Result<std::sync::Arc<dyn kallipai_task::BlobStore>, ApiError> {
    // Boot installs the store unconditionally; a missing install is a
    // wiring bug, not a caller problem.
    state
        .task_blobs
        .get()
        .cloned()
        .ok_or_else(|| ApiError::internal("blob store not installed"))
}

/// The event actor for a verb, from the authenticated identity: the agent
/// id for agent tokens, the literal `operator` for the operator token.
/// The tagma records who called; callers cannot claim an actor.
fn acting_agent(auth: &crate::auth::AuthIdentity) -> String {
    match auth.identity() {
        crate::auth::Identity::Operator => OPERATOR.to_string(),
        crate::auth::Identity::Agent { id } => id.to_string(),
    }
}

/// The operator token's own identity value: not an agent, but a sender and
/// an owner the ledger records and shows.
const OPERATOR: &str = "operator";

/// One registry snapshot per request, cloned under the read lock and used
/// afterwards with no lock held. It maps both ways because a task verb
/// needs both: `by_id` turns a stored identity into its role name for the
/// wire, `by_role` turns the word a caller typed into the identity to
/// store. An alias keeps the request's two directions on one snapshot, so
/// a create cannot resolve an assignee against one roster and show roles
/// from another.
struct Identities {
    by_id: std::collections::HashMap<String, String>,
    by_role: std::collections::HashMap<String, String>,
}

async fn identities(state: &SharedState) -> std::sync::Arc<Identities> {
    let registry = state.registry.read().await;
    let mut by_id = std::collections::HashMap::with_capacity(registry.len());
    let mut by_role = std::collections::HashMap::with_capacity(registry.len());
    for (id, entry) in registry.iter() {
        let role = entry.identity().config.role.clone();
        by_id.insert(id.to_string(), role.clone());
        by_role.insert(role, id.to_string());
    }
    std::sync::Arc::new(Identities { by_id, by_role })
}

impl Identities {
    /// The identity a caller's word names: an agent id passes through, a
    /// role name resolves to the agent holding it, and the operator literal
    /// names itself. The ledger stores what this returns, so a word it
    /// cannot place never reaches a row. Create and the list filter share
    /// this one vocabulary.
    fn resolve(&self, raw: &str) -> Option<String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        if raw == OPERATOR {
            return Some(raw.to_owned());
        }
        // The ledger stores an id in its canonical spelling while a caller may
        // type any spelling `Uuid::parse_str` accepts, so normalize before the
        // lookup: a differently spelled id still names its agent.
        let identity = match uuid::Uuid::parse_str(raw) {
            Ok(id) => id.to_string(),
            Err(_) => raw.to_owned(),
        };
        if self.by_id.contains_key(&identity) {
            return Some(identity);
        }
        self.by_role.get(&identity).cloned()
    }

    /// The value a filter matches against: `resolve`, plus an id-shaped
    /// word kept as typed. A row can outlive its agent (the ledger keeps
    /// the id), and the wire shows that id when no role answers for it,
    /// so a caller copying what it sees must find the row. Create does
    /// not share this: assigning a new task to an identity no agent
    /// holds is refused.
    fn address(&self, raw: &str) -> Option<String> {
        let raw = raw.trim();
        if let Some(id) = self.resolve(raw) {
            return Some(id);
        }
        // `resolve` normalizes what it can place; this arm keeps a well-formed
        // id the registry has let go, in the canonical spelling the ledger
        // would have stored.
        uuid::Uuid::parse_str(raw).ok().map(|id| id.to_string())
    }

    /// The alternatives a refusal shows: the registered role names plus the
    /// operator literal, sorted. An agent id also works and is not listed —
    /// a caller who has one is already holding it.
    fn names(&self) -> String {
        let mut names: Vec<&str> = self.by_role.keys().map(String::as_str).collect();
        names.push(OPERATOR);
        names.sort_unstable();
        names.join(", ")
    }
}

/// A word typed as an assignee, a confirmer, or a filter names nothing the
/// ledger knows. The refusal lists the role names and the operator literal
/// that would have worked (an agent id works too, which is why the list need
/// not carry one): the ledger stores identities, so every verb needs a word
/// it can turn into one, and naming the alternatives is what makes the
/// caller's next attempt work.
fn no_such_identity(raw: &str, identities: &Identities) -> ApiError {
    ApiError::bad_request(format!(
        "no agent named '{}'; available: {}",
        raw.trim(),
        identities.names()
    ))
}

/// The one identity rule: a value the registry holds resolves to that
/// agent's role name, and everything else (the literal `operator`, an id the
/// registry has let go, nothing at all) resolves to `None`. Every wire field
/// that carries a role calls this, so a new identity field calls it instead
/// of copying the lookup.
fn role_of(identities: &Identities, raw: Option<&str>) -> Option<String> {
    raw.and_then(|raw| identities.by_id.get(raw).cloned())
}

/// Fill `actor_role` on every event, `agent_role` on every usage row, and
/// `creator_role` / `assignee_role` / `confirmer_roles` on the task, all
/// through `role_of`. Read and write responses share this one mapping, so
/// the identity fields it covers (event actors, usage agents, creator,
/// assignee, confirmers) cannot disagree with each other in a pane.
fn inject_wire_roles(
    mut export: kallipai_task::TaskExport,
    identities: &Identities,
) -> kallipai_task::TaskExport {
    for event in &mut export.events {
        event.actor_role = role_of(identities, event.actor.as_deref());
    }
    // The task's own identity fields follow the event policy exactly.
    export.creator_role = role_of(identities, export.creator.as_deref());
    export.assignee_role = role_of(identities, export.assignee.as_deref());
    export.confirmer_roles = export
        .confirmers
        .iter()
        .map(|confirmer| role_of(identities, Some(confirmer.as_str())))
        .collect();
    if let Some(usage) = export.usage.as_mut() {
        // One fill for every agent row in the usage face: the top-level
        // sums and the per-window rows are the same wire field.
        let fill = |agents: &mut Vec<kallipai_common::protocol::UsageAgentExport>| {
            for agent in agents {
                agent.agent_role = role_of(identities, Some(agent.agent_id.as_str()));
            }
        };
        fill(&mut usage.agents);
        for segment in &mut usage.segments {
            fill(&mut segment.agents);
        }
    }
    export
}

/// The list face carries the same assignee role as the detail export:
/// the row holds an identity, so it reads back as the role name that
/// identity holds, or nothing when it holds none.
fn inject_row_roles(
    mut row: kallipai_common::protocol::TaskRow,
    identities: &Identities,
) -> kallipai_common::protocol::TaskRow {
    row.assignee_role = role_of(identities, row.assignee.as_deref());
    row
}

/// The store's error taxonomy to the HTTP face: state refusals are
/// conflicts, unknown ids are misses, everything else is ours.
fn api_error(err: kallipai_task::Error) -> ApiError {
    use kallipai_task::Error as E;
    match &err {
        E::NotFound { id } => ApiError::not_found(format!("task {id} not found")),
        E::InvalidTransition { .. }
        | E::SerialGate { .. }
        | E::ConfirmationGate { .. }
        | E::ArchiveGate { .. } => ApiError::conflict(err.to_string()),
        // Malformed association keys are a caller input problem, not a
        // server fault: 400, not 500.
        E::AssociationInvalid { .. } => ApiError::bad_request(err.to_string()),
        // Oversized bulk payloads (a mistyped --dossier, a report over
        // the cap), undecodable base64 bodies, non-tar archive payloads,
        // and non-UTF-8 report files are caller input: 400, not 500.
        E::DossierTooLarge { .. }
        | E::ReportTooLarge { .. }
        | E::MalformedBase64 { .. }
        | E::MalformedArchive { .. }
        | E::ReportNotUtf8 => ApiError::bad_request(err.to_string()),
        // Everything left is a server-side fault (storage, io, encoding,
        // time): logged at error with the store context, then surfaced as
        // an internal error. Client-refusal variants are all matched above.
        _ => {
            tracing::error!(error = %err, "storage failure: task store operation failed");
            ApiError::internal(err.to_string())
        }
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
fn notify(state: &SharedState, verb: &str, actor: &str, export: &kallipai_task::TaskExport) {
    state.invalidate();
    if let Err(err) = state.bus.publish(TaskChanged {
        task_id: export.id,
        title: export.title.clone(),
        status: export.status.to_string(),
        verb: verb.to_string(),
        actor: actor.to_string(),
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
    Json(mut body): Json<TaskCreateRequest>,
) -> TaskResult<kallipai_task::TaskExport> {
    let actor = acting_agent(&auth);
    let snapshot = identities(&state).await;
    // The caller's words are addresses, the rows store identities: resolve
    // them here so no unresolvable word can reach the ledger. A blank word
    // is a field left unfilled, not a name nobody answers to.
    body.assignee = match body.assignee.as_deref().map(str::trim) {
        Some(raw) if !raw.is_empty() => Some(
            snapshot
                .resolve(raw)
                .ok_or_else(|| no_such_identity(raw, &snapshot))?,
        ),
        _ => None,
    };
    let roster: Vec<String> = body
        .require
        .iter()
        .map(|raw| raw.trim())
        .filter(|raw| !raw.is_empty())
        .map(|raw| {
            snapshot
                .resolve(raw)
                .ok_or_else(|| no_such_identity(raw, &snapshot))
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    body.require = roster;
    let task = store(&state)?
        .create(body, &actor)
        .await
        .map_err(api_error)?;
    let id = task.id;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    let export = inject_wire_roles(export, &snapshot);
    notify(&state, "create", &actor, &export);
    Ok(Json(export))
}

async fn start(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<i64>,
    Json(body): Json<TaskForceRequest>,
) -> TaskResult<kallipai_task::TaskExport> {
    let actor = acting_agent(&auth);
    let snapshot = identities(&state).await;
    let usage = crate::usage_snapshot::capture_usage(&state).await;
    store(&state)?
        .start(id, &actor, body.force, Some(usage))
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    let export = inject_wire_roles(export, &snapshot);
    notify(&state, "start", &actor, &export);
    Ok(Json(export))
}

async fn list(
    State(state): State<SharedState>,
    Query(query): Query<TaskListQuery>,
) -> TaskResult<kallipai_common::protocol::TaskListPage> {
    let snapshot = identities(&state).await;
    // A filter word is an address: resolve a role name before matching,
    // and keep an id-shaped word as typed, so a row left by a removed
    // agent stays findable.
    let assignee = match query.assignee.as_deref().map(str::trim) {
        Some(raw) if !raw.is_empty() => Some(
            snapshot
                .address(raw)
                .ok_or_else(|| no_such_identity(raw, &snapshot))?,
        ),
        _ => None,
    };
    let filter = TaskFilter {
        status: query.status,
        assignee,
        archived: query.archived,
        time: query.time,
        since: query.since,
        until: query.until,
        limit: query.limit,
        offset: query.offset,
    };
    let mut page = store(&state)?.list_page(filter).await.map_err(api_error)?;
    page.rows = page
        .rows
        .into_iter()
        .map(|row| inject_row_roles(row, &snapshot))
        .collect();
    Ok(Json(page))
}

async fn show(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> TaskResult<kallipai_task::TaskExport> {
    let snapshot = identities(&state).await;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    Ok(Json(inject_wire_roles(export, &snapshot)))
}

async fn export_one(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> TaskResult<kallipai_task::TaskExport> {
    let snapshot = identities(&state).await;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    Ok(Json(inject_wire_roles(export, &snapshot)))
}

async fn export_all(
    State(state): State<SharedState>,
) -> TaskResult<Vec<kallipai_task::TaskExport>> {
    let snapshot = identities(&state).await;
    let all = store(&state)?.export_all().await.map_err(api_error)?;
    Ok(Json(
        all.into_iter()
            .map(|export| inject_wire_roles(export, &snapshot))
            .collect(),
    ))
}

async fn confirm(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<i64>,
    Json(body): Json<TaskConfirmRequest>,
) -> TaskResult<kallipai_task::TaskExport> {
    let actor = acting_agent(&auth);
    let snapshot = identities(&state).await;
    store(&state)?
        .confirm(id, &actor, body, blobs(&state)?)
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    let export = inject_wire_roles(export, &snapshot);
    notify(&state, "confirm", &actor, &export);
    Ok(Json(export))
}

async fn review(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<i64>,
) -> TaskResult<kallipai_task::TaskExport> {
    let actor = acting_agent(&auth);
    let snapshot = identities(&state).await;
    store(&state)?.review(id, &actor).await.map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    let export = inject_wire_roles(export, &snapshot);
    notify(&state, "review", &actor, &export);
    Ok(Json(export))
}

async fn pause(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<i64>,
) -> TaskResult<kallipai_task::TaskExport> {
    let actor = acting_agent(&auth);
    let snapshot = identities(&state).await;
    let usage = crate::usage_snapshot::capture_usage(&state).await;
    store(&state)?
        .pause(id, &actor, Some(usage))
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    let export = inject_wire_roles(export, &snapshot);
    notify(&state, "pause", &actor, &export);
    Ok(Json(export))
}

async fn resume(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<i64>,
    Json(body): Json<TaskForceRequest>,
) -> TaskResult<kallipai_task::TaskExport> {
    let actor = acting_agent(&auth);
    let snapshot = identities(&state).await;
    let usage = crate::usage_snapshot::capture_usage(&state).await;
    store(&state)?
        .resume(id, &actor, body.force, Some(usage))
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    let export = inject_wire_roles(export, &snapshot);
    notify(&state, "resume", &actor, &export);
    Ok(Json(export))
}

async fn note(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<i64>,
    Json(body): Json<TaskNoteRequest>,
) -> TaskResult<kallipai_task::TaskExport> {
    let actor = acting_agent(&auth);
    let snapshot = identities(&state).await;
    store(&state)?
        .note(id, &actor, body.note)
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    let export = inject_wire_roles(export, &snapshot);
    notify(&state, "note", &actor, &export);
    Ok(Json(export))
}

async fn close(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<i64>,
    Json(body): Json<TaskCloseRequest>,
) -> TaskResult<kallipai_task::TaskExport> {
    let actor = acting_agent(&auth);
    let snapshot = identities(&state).await;
    let usage = crate::usage_snapshot::capture_usage(&state).await;
    // Captured before the store call: `close` ingests the dossier archive
    // first, and that ingest's spend lands in no window. The boundary is
    // deliberate — moving the capture after the ingest would cost the
    // same-transaction guarantee the snapshot rests on.
    let closed = store(&state)?.close(id, &actor, body, blobs(&state)?, Some(usage));
    match closed.await {
        Ok(_) => {}
        // The gate message carries identity ids; the human face wants
        // role names, so map through the same snapshot the events use.
        Err(kallipai_task::Error::ConfirmationGate { missing }) => {
            let names: Vec<String> = missing
                .split(", ")
                .map(|id| {
                    snapshot
                        .by_id
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| id.to_string())
                })
                .collect();
            return Err(ApiError::conflict(format!(
                "confirmation gate: missing confirmations from registered confirmers: {}; --force to override (escape is recorded)",
                names.join(", ")
            )));
        }
        Err(e) => return Err(api_error(e)),
    };
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    let export = inject_wire_roles(export, &snapshot);
    notify(&state, "close", &actor, &export);
    Ok(Json(export))
}

async fn reopen(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<i64>,
    Json(body): Json<TaskForceRequest>,
) -> TaskResult<kallipai_task::TaskExport> {
    let actor = acting_agent(&auth);
    let snapshot = identities(&state).await;
    let usage = crate::usage_snapshot::capture_usage(&state).await;
    store(&state)?
        .reopen(id, &actor, body.force, Some(usage))
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    let export = inject_wire_roles(export, &snapshot);
    notify(&state, "reopen", &actor, &export);
    Ok(Json(export))
}

async fn archive(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<i64>,
    Json(body): Json<TaskForceRequest>,
) -> TaskResult<kallipai_task::TaskExport> {
    let actor = acting_agent(&auth);
    let snapshot = identities(&state).await;
    store(&state)?
        .archive_task(id, &actor, body.force)
        .await
        .map_err(api_error)?;
    let export = store(&state)?.export(id).await.map_err(api_error)?;
    let export = inject_wire_roles(export, &snapshot);
    notify(&state, "archive", &actor, &export);
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
    let blobs = blobs(&state)?;
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

/// Serves one confirmation report body: the tagma checks that the blob
/// is referenced by this task's confirm trail, then returns the decoded
/// text. Content addressing makes the id unforgeable; the task binding
/// keeps one task's reports from being read through another's id.
async fn fetch_report(
    State(state): State<SharedState>,
    Path((id, blob)): Path<(i64, String)>,
) -> Result<axum::response::Response, ApiError> {
    use axum::response::IntoResponse;
    let body = store(&state)?
        .report_body(id, &blob, blobs(&state)?.as_ref())
        .await
        .map_err(api_error)?;
    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        body,
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
        .route(
            "/{id}/confirm",
            axum::routing::post(confirm).layer(axum::extract::DefaultBodyLimit::max(
                kallipai_common::protocol::TASK_BODY_LIMIT_BYTES,
            )),
        )
        .route("/{id}/review", axum::routing::post(review))
        .route("/{id}/pause", axum::routing::post(pause))
        .route("/{id}/resume", axum::routing::post(resume))
        .route("/{id}/note", axum::routing::post(note))
        .route(
            "/{id}/close",
            axum::routing::post(close).layer(axum::extract::DefaultBodyLimit::max(
                kallipai_common::protocol::TASK_BODY_LIMIT_BYTES,
            )),
        )
        .route("/{id}/reports/{blob}", axum::routing::get(fetch_report))
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

    /// A registry snapshot for the pure helpers: (id, role) pairs, with
    /// both directions derived from them, so a test cannot accidentally
    /// pin a mapping the live snapshot would never build.
    fn identities_of(pairs: &[(&str, &str)]) -> Identities {
        Identities {
            by_id: pairs
                .iter()
                .map(|(id, role)| (id.to_string(), role.to_string()))
                .collect(),
            by_role: pairs
                .iter()
                .map(|(id, role)| (role.to_string(), id.to_string()))
                .collect(),
        }
    }

    /// The gate taxonomy is the 409 family: every state refusal maps to
    /// conflict, unknown ids to not-found, and anything else (storage,
    /// serialization) stays internal — a wrong arm here would turn "the
    /// task refused" into "we broke", or worse, the reverse.
    #[test]
    fn gate_errors_map_to_conflict() {
        for err in [
            kallipai_task::Error::InvalidTransition {
                id: 1,
                from: "queued".into(),
                action: "confirm".into(),
                expected: "in_progress|review".into(),
            },
            kallipai_task::Error::SerialGate {
                assignee: "scout".into(),
                blocked_by: 1,
                title: "t".into(),
            },
            kallipai_task::Error::ConfirmationGate {
                missing: "scout".into(),
            },
            kallipai_task::Error::ArchiveGate {
                id: 1,
                status: "queued".into(),
            },
        ] {
            assert_eq!(api_error(err).status, 409, "gates are conflicts");
        }
    }

    #[test]
    fn unknown_ids_map_to_not_found() {
        let err = kallipai_task::Error::NotFound { id: 7 };
        assert_eq!(api_error(err).status, 404);
    }

    /// Malformed association keys ride in the request body: the caller's
    /// input problem is a 400, never a 500.
    #[test]
    fn association_invalid_maps_to_bad_request() {
        let err = kallipai_task::Error::AssociationInvalid {
            detail: "empty range".into(),
        };
        assert_eq!(api_error(err).status, 400);
    }

    #[test]
    fn anything_else_stays_internal() {
        let err = kallipai_task::Error::Other("storage went away".into());
        assert_eq!(api_error(err).status, 500);
    }

    /// Same 400 family: a confirm report over the shared cap is
    /// caller input, not a server fault.
    #[test]
    fn report_too_large_maps_to_bad_request() {
        let err = kallipai_task::Error::ReportTooLarge {
            size: kallipai_common::protocol::REPORT_MAX_BYTES + 1,
            max: kallipai_common::protocol::REPORT_MAX_BYTES,
        };
        assert_eq!(api_error(err).status, 400);
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
        let roles = identities_of(&[("agent-1", "reviewer-c")]);
        let export = inject_wire_roles(
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

    /// The task's own identity fields follow that same policy: the creator
    /// and each confirmer resolve to a role when the registry knows them,
    /// and stay null otherwise. The role vector keeps the roster's order
    /// and length, so the page can pair them by index.
    #[test]
    fn creator_and_confirmer_roles_follow_the_same_policy() {
        let roles = identities_of(&[("agent-1", "reviewer-c")]);
        let mut export = export_with_events(&[]);
        export.creator = Some("agent-1".to_string());
        export.assignee = Some("agent-1".to_string());
        export.confirmers = vec![
            "agent-1".to_string(),
            "reviewer-c".to_string(),
            "agent-gone".to_string(),
        ];
        let export = inject_wire_roles(export, &roles);
        assert_eq!(export.creator_role.as_deref(), Some("reviewer-c"));
        assert_eq!(export.assignee_role.as_deref(), Some("reviewer-c"));
        assert_eq!(
            export
                .confirmer_roles
                .iter()
                .map(|r| r.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("reviewer-c"), None, None]
        );
    }

    /// A null creator resolves to a null role rather than a fabricated one.
    #[test]
    fn a_null_creator_yields_a_null_role() {
        let roles = identities_of(&[("agent-1", "reviewer-c")]);
        let export = inject_wire_roles(export_with_events(&[]), &roles);
        assert_eq!(export.creator_role, None);
        assert!(export.confirmer_roles.is_empty());
        assert_eq!(export.assignee_role, None);
    }

    /// A value the registry does not know (the literal `operator`, an id
    /// of a deregistered agent) gets no role and the wire value stands:
    /// the display never fabricates a name.
    #[test]
    fn an_assignee_that_names_no_agent_gets_no_role() {
        let roles = identities_of(&[("agent-1", "reviewer-c")]);
        let mut export = export_with_events(&[]);
        export.assignee = Some("reviewer-c".to_string());
        let export = inject_wire_roles(export, &roles);
        assert_eq!(export.assignee.as_deref(), Some("reviewer-c"));
        assert_eq!(export.assignee_role, None);
    }

    /// Test fixture: an export whose events differ only in actor.
    fn export_with_events(actors: &[Option<&str>]) -> kallipai_task::TaskExport {
        kallipai_task::TaskExport {
            id: 1,
            title: "t".into(),
            status: "queued".into(),
            creator: None,
            assignee: None,
            confirmers: vec![],
            creator_role: None,
            assignee_role: None,
            confirmer_roles: vec![],
            created_at: None,
            updated_at: None,
            started_at: None,
            ended_at: None,
            archived: false,
            archived_at: None,
            closed_reason: None,
            close_summary: None,
            association: None,
            archive_hash: None,
            archive_entries: None,
            usage: None,
            events: actors
                .iter()
                .map(|a| kallipai_common::protocol::EventExport {
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

    /// The list face fills its own assignee role; `inject_row_roles` is the
    /// only place a `TaskRow` gets one, so it carries its own assertions.
    #[test]
    fn the_list_row_fills_its_assignee_role() {
        let roles = identities_of(&[("agent-1", "reviewer-c")]);
        let by_id = inject_row_roles(row_with_assignee(Some("agent-1")), &roles);
        assert_eq!(by_id.assignee_role.as_deref(), Some("reviewer-c"));
        let by_name = inject_row_roles(row_with_assignee(Some("reviewer-c")), &roles);
        assert_eq!(by_name.assignee.as_deref(), Some("reviewer-c"));
        assert_eq!(by_name.assignee_role, None);
    }

    /// Test fixture: a list row whose only interesting field is the assignee.
    fn row_with_assignee(assignee: Option<&str>) -> kallipai_common::protocol::TaskRow {
        kallipai_common::protocol::TaskRow {
            id: 1,
            title: "t".into(),
            status: "queued".into(),
            creator: None,
            assignee: assignee.map(|s| s.to_string()),
            confirmers: vec![],
            assignee_role: None,
            created_at: None,
            updated_at: None,
            started_at: None,
            ended_at: None,
            archived: false,
            archived_at: None,
            closed_reason: None,
            close_summary: None,
            association: None,
            archive_hash: None,
            archive_entries: None,
            has_reports: false,
        }
    }

    /// One usage row, with the role left for the fill to supply.
    fn usage_agent(
        agent_id: &str,
        consumed_tokens: u64,
    ) -> kallipai_common::protocol::UsageAgentExport {
        kallipai_common::protocol::UsageAgentExport {
            agent_id: agent_id.to_string(),
            agent_role: None,
            consumed_tokens,
            prompt_tokens: consumed_tokens,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        }
    }

    /// The usage face carries agent rows at two levels; one role map fills
    /// both, and an id that no longer resolves stays null.
    #[test]
    fn usage_agent_rows_get_roles_at_both_levels() {
        let mut export = export_with_events(&[]);
        let rows = vec![usage_agent("agent-1", 20), usage_agent("gone", 10)];
        export.usage = Some(kallipai_common::protocol::TaskUsageExport {
            total_tokens: 30,
            open: false,
            open_since: None,
            segments: vec![kallipai_common::protocol::UsageSegmentExport {
                from_event: "start".into(),
                to_event: "close".into(),
                from_at: "2025-10-09T08:53:20Z".into(),
                to_at: "2025-10-09T09:53:20Z".into(),
                total_tokens: 30,
                agents: rows.clone(),
            }],
            agents: rows,
            gaps: vec![],
            partial: false,
            non_monotonic: vec![],
        });
        let roles = identities_of(&[("agent-1", "reviewer-c")]);

        let export = inject_wire_roles(export, &roles);
        let usage = export.usage.expect("usage");
        assert_eq!(usage.agents[0].agent_role.as_deref(), Some("reviewer-c"));
        assert_eq!(usage.agents[1].agent_role, None);
        let window_rows = &usage.segments[0].agents;
        assert_eq!(window_rows[0].agent_role.as_deref(), Some("reviewer-c"));
        assert_eq!(window_rows[1].agent_role, None);
    }

    /// Close with a packed dossier: the content address and entry count
    /// land in the store, and the bytes come back from the blob store.
    /// Everything rides the request body - the store reads no paths.
    #[tokio::test]
    async fn close_with_a_dossier_archives_it() {
        let state = make_state();
        state
            .tasks
            .set(Arc::new(kallipai_task::TaskStore::open_in_memory().await))
            .ok()
            .expect("task store installs once");
        let blobs = Arc::new(kallipai_blob_store::LocalBackend::new(
            std::env::temp_dir().join(format!("kallipai-tagma-close-blobs-{}", std::process::id())),
        ));
        state.task_blobs.set(blobs).ok();

        let auth = crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator);
        let created = create(
            State(Arc::clone(&state)),
            auth.clone(),
            Json(TaskCreateRequest {
                title: "archived".into(),
                ..Default::default()
            }),
        )
        .await
        .expect("create succeeds");
        let id = created.0.id;
        let _ = start(
            State(Arc::clone(&state)),
            auth.clone(),
            Path(id),
            Json(TaskForceRequest::default()),
        )
        .await
        .expect("start succeeds");

        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(6);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "plan.md", "# plan\n".as_bytes())
            .unwrap();
        let packed = builder.into_inner().unwrap();
        use base64::Engine as _;
        let tar_b64 = base64::engine::general_purpose::STANDARD.encode(&packed);

        let closed = close(
            State(Arc::clone(&state)),
            auth.clone(),
            Path(id),
            Json(TaskCloseRequest {
                reason: kallipai_common::protocol::ClosedReason::Completed,
                summary: None,
                force: false,
                dossier_tar_b64: Some(tar_b64),
            }),
        )
        .await
        .expect("close with dossier succeeds");
        assert!(closed.0.archive_hash.is_some());
        assert_eq!(closed.0.archive_entries, Some(1));
    }

    /// A confirm report round-trips through the blob store: the event
    /// carries only the pointer, and the report endpoint serves the
    /// decoded body bound to the task that owns it.
    #[tokio::test]
    async fn confirm_report_round_trips_through_the_blob_store() {
        let state = make_state();
        state
            .tasks
            .set(Arc::new(kallipai_task::TaskStore::open_in_memory().await))
            .ok()
            .expect("task store installs once");
        let blobs = Arc::new(kallipai_blob_store::LocalBackend::new(
            std::env::temp_dir().join(format!(
                "kallipai-tagma-report-blobs-{}",
                std::process::id()
            )),
        ));
        state.task_blobs.set(blobs).ok();

        let auth = crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator);
        let created = create(
            State(Arc::clone(&state)),
            auth.clone(),
            Json(TaskCreateRequest {
                title: "reported".into(),
                ..Default::default()
            }),
        )
        .await
        .expect("create succeeds");
        let id = created.0.id;
        let _ = start(
            State(Arc::clone(&state)),
            auth.clone(),
            Path(id),
            Json(TaskForceRequest::default()),
        )
        .await
        .expect("start succeeds");

        let confirmed = confirm(
            State(Arc::clone(&state)),
            auth.clone(),
            Path(id),
            Json(TaskConfirmRequest {
                note: Some("signed".into()),
                file: Some(kallipai_common::protocol::TaskConfirmFile {
                    file_name: "review.md".into(),
                    file_b64: base64::engine::general_purpose::STANDARD.encode("the report body"),
                }),
            }),
        )
        .await
        .expect("confirm succeeds");
        use base64::Engine as _;
        let file = confirmed
            .0
            .events
            .iter()
            .rev()
            .find(|e| e.name == "confirm")
            .and_then(|e| e.payload.as_ref())
            .and_then(|p| p.get("file"))
            .and_then(|f| f.get("file_blob"))
            .and_then(|b| b.as_str())
            .map(str::to_string)
            .expect("confirm event carries the report pointer");

        let response = fetch_report(State(Arc::clone(&state)), Path((id, file)))
            .await
            .expect("report body served");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("report body bytes");
        assert_eq!(&bytes[..], b"the report body");
    }

    /// An undecodable base64 body is caller input: 400 on both bulk
    /// routes, before any state access.
    #[tokio::test]
    async fn close_and_confirm_refuse_undecodable_base64() {
        let state = make_state();
        state
            .tasks
            .set(Arc::new(kallipai_task::TaskStore::open_in_memory().await))
            .ok()
            .expect("task store installs once");
        let blobs = Arc::new(kallipai_blob_store::LocalBackend::new(
            std::env::temp_dir().join(format!("kallipai-tagma-b64-blobs-{}", std::process::id())),
        ));
        state.task_blobs.set(blobs).ok();

        let auth = crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator);
        let created = create(
            State(Arc::clone(&state)),
            auth.clone(),
            Json(TaskCreateRequest {
                title: "b64".into(),
                ..Default::default()
            }),
        )
        .await
        .expect("create succeeds");
        let id = created.0.id;

        let err = match close(
            State(Arc::clone(&state)),
            auth.clone(),
            Path(id),
            Json(TaskCloseRequest {
                reason: kallipai_common::protocol::ClosedReason::Completed,
                summary: None,
                force: false,
                dossier_tar_b64: Some("not base64!!!".into()),
            }),
        )
        .await
        {
            // Match, not expect_err: the Ok half carries no Debug.
            Err(err) => err,
            Ok(_) => panic!("bad base64 refuses"),
        };
        assert_eq!(err.status, 400);
        assert!(err.message.contains("not valid base64"));

        let err = match confirm(
            State(Arc::clone(&state)),
            auth,
            Path(id),
            Json(TaskConfirmRequest {
                note: None,
                file: Some(kallipai_common::protocol::TaskConfirmFile {
                    file_name: "review.md".into(),
                    file_b64: "!!!".into(),
                }),
            }),
        )
        .await
        {
            Err(err) => err,
            Ok(_) => panic!("bad base64 refuses"),
        };
        assert_eq!(err.status, 400);
        assert!(err.message.contains("not valid base64"));
    }

    /// The report route binds the blob to the task: a fabricated task id
    /// and a second task's id both refuse with not-found and never serve
    /// the bytes.
    #[tokio::test]
    async fn fetch_report_refuses_foreign_or_fabricated_task_ids() {
        let state = make_state();
        state
            .tasks
            .set(Arc::new(kallipai_task::TaskStore::open_in_memory().await))
            .ok()
            .expect("task store installs once");
        let blobs = Arc::new(kallipai_blob_store::LocalBackend::new(
            std::env::temp_dir().join(format!(
                "kallipai-tagma-report-blobs-{}",
                std::process::id()
            )),
        ));
        state.task_blobs.set(blobs).ok();

        let auth = crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator);
        let created = create(
            State(Arc::clone(&state)),
            auth.clone(),
            Json(TaskCreateRequest {
                title: "bound".into(),
                ..Default::default()
            }),
        )
        .await
        .expect("create succeeds");
        let id = created.0.id;
        let _ = start(
            State(Arc::clone(&state)),
            auth.clone(),
            Path(id),
            Json(TaskForceRequest::default()),
        )
        .await
        .expect("start succeeds");
        let confirmed = confirm(
            State(Arc::clone(&state)),
            auth.clone(),
            Path(id),
            Json(TaskConfirmRequest {
                note: Some("signed".into()),
                file: Some(kallipai_common::protocol::TaskConfirmFile {
                    file_name: "review.md".into(),
                    file_b64: base64::engine::general_purpose::STANDARD.encode("secret body"),
                }),
            }),
        )
        .await
        .expect("confirm succeeds");
        use base64::Engine as _;
        let file = confirmed
            .0
            .events
            .iter()
            .rev()
            .find(|e| e.name == "confirm")
            .and_then(|e| e.payload.as_ref())
            .and_then(|p| p.get("file"))
            .and_then(|f| f.get("file_blob"))
            .and_then(|b| b.as_str())
            .map(str::to_string)
            .expect("confirm event carries the report pointer");

        let err =
            match fetch_report(State(Arc::clone(&state)), Path((i64::MAX, file.clone()))).await {
                Err(err) => err,
                Ok(_) => panic!("a fabricated task id refuses"),
            };
        assert_eq!(err.status, 404);

        let other = create(
            State(Arc::clone(&state)),
            auth,
            Json(TaskCreateRequest {
                title: "other".into(),
                ..Default::default()
            }),
        )
        .await
        .expect("create succeeds");
        let err = match fetch_report(State(Arc::clone(&state)), Path((other.0.id, file))).await {
            Err(err) => err,
            Ok(_) => panic!("a foreign task id refuses"),
        };
        assert_eq!(err.status, 404);
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
            .set(Arc::new(kallipai_task::TaskStore::open_in_memory().await))
            .ok()
            .expect("task store installs once");
        state
            .task_blobs
            .set(Arc::new(kallipai_blob_store::LocalBackend::new(
                std::env::temp_dir()
                    .join(format!("kallipai-tagma-bump-blobs-{}", std::process::id())),
            )))
            .ok();
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
                reason: kallipai_common::protocol::ClosedReason::Completed,
                summary: None,
                force: false,
                dossier_tar_b64: None,
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

    /// The same fill one level up: the write and collection handlers read
    /// the live registry themselves, so a role seeded there reaches the
    /// create response and the collection face, not only the pure helper.
    #[tokio::test]
    async fn handler_faces_fill_actor_roles_from_the_registry() {
        let state = make_state();
        state
            .tasks
            .set(Arc::new(kallipai_task::TaskStore::open_in_memory().await))
            .ok()
            .expect("task store installs once");
        let id = kallipai_common::agentid::AgentId::random();
        let (mut entry, _rx) = make_entry_with_rx(None, format!("tok-{id}"));
        entry.identity.config.role = "reviewer-c".to_string();
        state
            .registry
            .write()
            .await
            .register(id.clone(), crate::state::RegistryEntry::Live(entry));
        let auth =
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Agent { id: id.clone() });

        let created = create(
            State(Arc::clone(&state)),
            auth,
            Json(TaskCreateRequest {
                title: "role fill".into(),
                ..Default::default()
            }),
        )
        .await
        .expect("create succeeds");
        assert_eq!(created.0.events[0].actor.as_deref(), Some(id.as_ref()));
        assert_eq!(
            created.0.events[0].actor_role.as_deref(),
            Some("reviewer-c")
        );

        let all = export_all(State(Arc::clone(&state)))
            .await
            .expect("export_all succeeds");
        assert_eq!(all.0.len(), 1);
        assert_eq!(all.0[0].events[0].actor_role.as_deref(), Some("reviewer-c"));
    }

    /// A tagma state with an in-memory task ledger and one live agent
    /// holding `role`: the two directions the task verbs need.
    async fn state_with_agent(
        role: &str,
    ) -> (crate::state::SharedState, kallipai_common::agentid::AgentId) {
        let state = make_state();
        state
            .tasks
            .set(Arc::new(kallipai_task::TaskStore::open_in_memory().await))
            .ok()
            .expect("task store installs once");
        let id = kallipai_common::agentid::AgentId::random();
        let (mut entry, _rx) = make_entry_with_rx(None, format!("tok-{id}"));
        entry.identity.config.role = role.to_string();
        state
            .registry
            .write()
            .await
            .register(id.clone(), crate::state::RegistryEntry::Live(entry));
        (state, id)
    }

    /// The create verb stores what the caller's word names: a role name
    /// becomes its holder's id, an id passes through, and the response
    /// fills the role from the same snapshot.
    #[tokio::test]
    async fn create_stores_the_identity_that_an_assignee_word_names() {
        let (state, id) = state_with_agent("dev").await;
        let auth =
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Agent { id: id.clone() });
        let by_role = create(
            State(Arc::clone(&state)),
            auth,
            Json(TaskCreateRequest {
                title: "named".into(),
                assignee: Some("dev".into()),
                ..Default::default()
            }),
        )
        .await
        .expect("create by role name succeeds");
        assert_eq!(by_role.0.assignee.as_deref(), Some(id.as_ref()));
        assert_eq!(by_role.0.assignee_role.as_deref(), Some("dev"));

        let auth =
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Agent { id: id.clone() });
        let by_id = create(
            State(Arc::clone(&state)),
            auth,
            Json(TaskCreateRequest {
                title: "identified".into(),
                assignee: Some(id.to_string()),
                ..Default::default()
            }),
        )
        .await
        .expect("create by identity succeeds");
        assert_eq!(by_id.0.assignee.as_deref(), Some(id.as_ref()));
    }

    /// A word that names no agent is refused at the door: the ledger
    /// never sees it, and the refusal names the words that would work.
    #[tokio::test]
    async fn an_unknown_assignee_is_refused_with_the_names_that_would_work() {
        let (state, _id) = state_with_agent("dev").await;
        let caller = kallipai_common::agentid::AgentId::random();
        let auth = crate::auth::AuthIdentity::test_new(crate::auth::Identity::Agent { id: caller });
        let err = create(
            State(Arc::clone(&state)),
            auth,
            Json(TaskCreateRequest {
                title: "unknown".into(),
                assignee: Some("nobody".into()),
                ..Default::default()
            }),
        )
        .await
        .err()
        .expect("an unplaceable word is refused");
        assert_eq!(err.status, 400, "a bad word is caller input");
        assert!(
            err.message.contains("no agent named 'nobody'"),
            "{}",
            err.message
        );
        assert!(
            err.message.contains("available: dev, operator"),
            "{}",
            err.message
        );
        // The filter and create are not one rule: a filter passes an
        // id-shaped value through so an orphaned row stays findable, while
        // create must not assign a task to an identity no agent holds.
        let auth = crate::auth::AuthIdentity::test_new(crate::auth::Identity::Agent {
            id: kallipai_common::agentid::AgentId::random(),
        });
        let orphan = kallipai_common::agentid::AgentId::random().to_string();
        let err = create(
            State(Arc::clone(&state)),
            auth,
            Json(TaskCreateRequest {
                title: "orphan".into(),
                assignee: Some(orphan),
                ..Default::default()
            }),
        )
        .await
        .err()
        .expect("an id no agent holds cannot be assigned");
        assert_eq!(err.status, 400, "create stays strict about identities");
        let all = export_all(State(Arc::clone(&state)))
            .await
            .expect("export_all succeeds");
        assert!(all.0.is_empty(), "a refused create registers nothing");
    }

    /// The list filter is an address too: a role name matches the rows
    /// its holder owns, and a word that names nobody is refused rather
    /// than silently matching nothing.
    #[tokio::test]
    async fn the_list_filter_resolves_a_role_name_before_matching() {
        let (state, id) = state_with_agent("dev").await;
        for (title, assignee) in [("assigned", Some("dev".to_string())), ("loose", None)] {
            let auth = crate::auth::AuthIdentity::test_new(crate::auth::Identity::Agent {
                id: id.clone(),
            });
            let created = create(
                State(Arc::clone(&state)),
                auth,
                Json(TaskCreateRequest {
                    title: title.into(),
                    assignee,
                    ..Default::default()
                }),
            )
            .await
            .expect("create succeeds");
            assert_eq!(created.0.title, title, "the fixture registers the row");
        }

        let page = list(
            State(Arc::clone(&state)),
            Query(kallipai_common::protocol::TaskListQuery {
                assignee: Some("dev".into()),
                ..Default::default()
            }),
        )
        .await
        .expect("the filter resolves");
        assert_eq!(page.0.rows.len(), 1, "only the assigned row matches");
        assert_eq!(page.0.rows[0].assignee.as_deref(), Some(id.as_ref()));
        assert_eq!(page.0.rows[0].assignee_role.as_deref(), Some("dev"));

        let err = list(
            State(Arc::clone(&state)),
            Query(kallipai_common::protocol::TaskListQuery {
                assignee: Some("nobody".into()),
                ..Default::default()
            }),
        )
        .await
        .expect_err("a filter word that names nobody is refused");
        assert_eq!(err.status, 400);
    }

    /// A row can outlive its agent: the ledger keeps the id, and the wire
    /// shows that id where no role answers for it. A caller who copies what
    /// the row shows into the filter must land on that row. Create does not
    /// share this looseness, so the fixture writes the orphan through the
    /// store instead of the verb.
    #[tokio::test]
    async fn the_list_filter_finds_a_row_left_by_a_deregistered_agent() {
        let (state, _id) = state_with_agent("dev").await;
        let ghost = kallipai_common::agentid::AgentId::random();
        store(&state)
            .expect("the store is installed")
            .create(
                TaskCreateRequest {
                    title: "orphan".into(),
                    assignee: Some(ghost.to_string()),
                    ..Default::default()
                },
                "operator",
            )
            .await
            .expect("the ledger stores what it is handed");

        let page = list(
            State(Arc::clone(&state)),
            Query(kallipai_common::protocol::TaskListQuery {
                assignee: Some(ghost.to_string()),
                ..Default::default()
            }),
        )
        .await
        .expect("an id-shaped word is accepted");
        assert_eq!(page.0.rows.len(), 1, "the orphaned row is findable");
        assert_eq!(page.0.rows[0].assignee.as_deref(), Some(ghost.as_ref()));
        assert_eq!(
            page.0.rows[0].assignee_role, None,
            "no registry entry answers for a deregistered id"
        );

        let upper = list(
            State(Arc::clone(&state)),
            Query(kallipai_common::protocol::TaskListQuery {
                assignee: Some(ghost.to_string().to_uppercase()),
                ..Default::default()
            }),
        )
        .await
        .expect("a differently spelled id is still an id");
        assert_eq!(
            upper.0.rows.len(),
            1,
            "the filter compares the canonical spelling"
        );

        let blank = list(
            State(Arc::clone(&state)),
            Query(kallipai_common::protocol::TaskListQuery {
                assignee: Some("   ".into()),
                ..Default::default()
            }),
        )
        .await
        .expect("a blank word is a field left unfilled");
        assert_eq!(blank.0.rows.len(), 1, "a blank filter matches every row");
    }

    /// A second registered agent, for rosters that need two identities.
    async fn add_agent(
        state: &crate::state::SharedState,
        role: &str,
    ) -> kallipai_common::agentid::AgentId {
        let id = kallipai_common::agentid::AgentId::random();
        let (mut entry, _rx) = make_entry_with_rx(None, format!("tok-{id}"));
        entry.identity.config.role = role.to_string();
        state
            .registry
            .write()
            .await
            .register(id.clone(), crate::state::RegistryEntry::Live(entry));
        id
    }

    /// The roster runs through the same snapshot as the assignee, and the
    /// store receives identities alone: a role name and an id both land as
    /// the agent behind them, and a word that names nobody is refused with
    /// the assignee refusal's own shape.
    #[tokio::test]
    async fn the_require_roster_reaches_the_registry_as_identities() {
        let (state, dev) = state_with_agent("dev").await;
        let quality = add_agent(&state, "reviewer-quality").await;
        let caller =
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Agent { id: dev.clone() });
        let created = create(
            State(Arc::clone(&state)),
            caller,
            Json(TaskCreateRequest {
                title: "reviewed".into(),
                require: vec!["dev".into(), quality.to_string()],
                ..Default::default()
            }),
        )
        .await
        .expect("both words name an agent");
        assert_eq!(
            created.0.confirmers,
            vec![dev.to_string(), quality.to_string()],
            "the ledger holds identities, not the words that named them"
        );
        assert_eq!(
            created.0.confirmer_roles,
            vec![
                Some("dev".to_string()),
                Some("reviewer-quality".to_string())
            ]
        );

        let caller =
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Agent { id: dev.clone() });
        let err = create(
            State(Arc::clone(&state)),
            caller,
            Json(TaskCreateRequest {
                title: "ghost roster".into(),
                require: vec!["nobody".into()],
                ..Default::default()
            }),
        )
        .await
        .err()
        .expect("a roster word that names nobody is refused");
        assert_eq!(err.status, 400, "the assignee refusal's own shape");
        assert!(
            err.message.contains("no agent named 'nobody'"),
            "{}",
            err.message
        );
    }

    /// The operator literal is an identity this ledger records (an operator
    /// pickup of an unassigned task), so create takes it and the filter finds
    /// the row by the very word the wire shows.
    #[tokio::test]
    async fn the_operator_literal_is_an_identity_on_both_sides() {
        let (state, _dev) = state_with_agent("dev").await;
        let operator = crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator);
        let created = create(
            State(Arc::clone(&state)),
            operator,
            Json(TaskCreateRequest {
                title: "claimed".into(),
                assignee: Some("operator".into()),
                require: vec!["operator".into()],
                ..Default::default()
            }),
        )
        .await
        .expect("the operator is a sender this ledger knows");
        assert_eq!(created.0.assignee.as_deref(), Some("operator"));
        assert_eq!(created.0.assignee_role, None, "the operator holds no role");
        assert_eq!(
            created.0.confirmers,
            vec!["operator".to_string()],
            "the roster's one vocabulary reaches the operator too"
        );
        assert_eq!(
            created.0.confirmer_roles,
            vec![None],
            "the operator is a confirmer with no role name"
        );

        let page = list(
            State(Arc::clone(&state)),
            Query(kallipai_common::protocol::TaskListQuery {
                assignee: Some("operator".into()),
                ..Default::default()
            }),
        )
        .await
        .expect("the filter knows the same vocabulary");
        assert_eq!(page.0.rows.len(), 1, "the claimed row is findable");
    }

    /// Both identity fields read a blank word as unfilled: the assignee stays
    /// unset and a blank roster entry never becomes a confirmer.
    #[tokio::test]
    async fn a_blank_identity_word_counts_as_not_given() {
        let (state, dev) = state_with_agent("dev").await;
        let caller =
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Agent { id: dev.clone() });
        let created = create(
            State(Arc::clone(&state)),
            caller,
            Json(TaskCreateRequest {
                title: "half filled".into(),
                assignee: Some("   ".into()),
                require: vec!["".into(), "dev".into()],
                ..Default::default()
            }),
        )
        .await
        .expect("blank words are not names to refuse");
        assert_eq!(created.0.assignee, None, "a blank assignee fills nothing");
        assert_eq!(
            created.0.confirmers,
            vec![dev.to_string()],
            "the blank roster entry is dropped, the named one is resolved"
        );
    }

    /// The ledger stores an id one way; a caller may type another. Remembering
    /// the canonical form is the store's job, so a create with an upper-case id
    /// lands on the agent it names.
    #[tokio::test]
    async fn a_create_lands_an_id_in_its_canonical_spelling() {
        let (state, dev) = state_with_agent("dev").await;
        let caller =
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Agent { id: dev.clone() });
        let created = create(
            State(Arc::clone(&state)),
            caller,
            Json(TaskCreateRequest {
                title: "shouted".into(),
                assignee: Some(dev.to_string().to_uppercase()),
                ..Default::default()
            }),
        )
        .await
        .expect("a differently spelled id still names its agent");
        assert_eq!(created.0.assignee.as_deref(), Some(dev.as_ref()));
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
