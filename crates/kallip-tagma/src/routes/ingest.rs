//! `POST /agents/{id}/attachments/ingest` — the image-read entrance the
//! `kallip image read` CLI drives: the tagma fetches the media bytes from
//! the files service, assembles the multimodal message, and records the
//! turn (live store push plus the history sidecar append). Self-scoped: an
//! agent ingests into its own context; the operator may target any agent.
//!
//! The bound set's effective modalities gate the request — the live twin
//! of the wake gate, running the same `ProfileSet::ensure_supports`. The
//! refusal carries the required and served modalities plus the rebind
//! action. A gated modality is refused before any files round-trip.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use just_llm_client::types::generation::Message;
use kallip_common::agentid::AgentId;
use kallip_common::protocol::{
    ApiError, AttachmentIngestRequest, AttachmentIngestResponse, Modality,
};
use kallip_runtime::ProfileSnapshot;
use kallip_runtime::context::{ContextStore, IngestImage, ingest_message};
use kallip_runtime::history::{AttachmentRef, HistoryWriter, RecordKind};
use tokio::sync::{Mutex, Notify};

use crate::auth::{AuthIdentity, Identity};
use crate::state::{RegistryEntry, SharedState};

/// POST /agents/{id}/attachments/ingest.
pub(crate) async fn ingest_attachment(
    State(state): State<SharedState>,
    auth: AuthIdentity,
    Path(id): Path<AgentId>,
    Json(req): Json<AttachmentIngestRequest>,
) -> Result<Json<AttachmentIngestResponse>, ApiError> {
    match auth.identity() {
        Identity::Operator => {}
        Identity::Agent { id: caller } if caller == &id => {}
        Identity::Agent { .. } => {
            return Err(ApiError::forbidden(
                "agents may only ingest into their own context",
            ));
        }
    }
    // Image parts are the only assembly wired; audio and video arrive with
    // their own part shapes (and their own CLI subcommands).
    if req.modality != Modality::Image {
        return Err(ApiError::bad_request(
            "only image ingest is wired; audio and video have no part assembly yet",
        ));
    }

    // The runtime handles are cloned out from under the registry lock;
    // nothing below the lookup holds it.
    let target = {
        let registry = state.registry.read().await;
        match registry.get(&id) {
            Some(RegistryEntry::Live(entry)) => IngestTarget::of(entry),
            Some(RegistryEntry::Faulted(_)) => {
                return Err(ApiError::unavailable(
                    "agent is faulted; there is no live context to ingest into",
                ));
            }
            None => return Err(ApiError::not_found(format!("no agent {id}"))),
        }
    };

    // Enforcement precedes the fetch: a gated modality must not spend a
    // files round-trip.
    enforce(&state, &target, req.modality).await?;

    let bytes = fetch_bytes(&state.files_http, req.record_id).await?;
    record_ingest(&target, &req, bytes).await
}

/// The live handles an ingest touches, cloned out from under the registry
/// lock.
struct IngestTarget {
    store: Arc<Mutex<ContextStore>>,
    notify: Arc<Notify>,
    profile_snapshot: Arc<std::sync::Mutex<ProfileSnapshot>>,
    agent_dir: Option<PathBuf>,
}

impl IngestTarget {
    fn of(entry: &crate::state::AgentEntry) -> Self {
        Self {
            store: Arc::clone(&entry.agent.store),
            notify: Arc::clone(&entry.agent.notify),
            profile_snapshot: Arc::clone(&entry.agent.profile_snapshot),
            agent_dir: entry.identity.agent_dir.clone(),
        }
    }
}

/// The enforcement choke: the bound set — the live snapshot's set name
/// resolved against the loaded profile bundle — must serve the requested
/// modality. The same `ensure_supports` the wake gate runs, so the rule
/// has one source and two entrances.
async fn enforce(
    state: &SharedState,
    target: &IngestTarget,
    modality: Modality,
) -> Result<(), ApiError> {
    let set_name = target
        .profile_snapshot
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .set_name
        .clone();
    let bundle = state.profiles.load();
    let set = bundle
        .registry
        .select_set(&set_name)
        .map_err(ApiError::internal)?;
    let required = BTreeSet::from([modality]);
    set.ensure_supports(&required)
        .map_err(|blocked| ApiError::forbidden(blocked.to_string()))
}

/// Fetch the record's bytes from the files service (the tagma process's
/// own credentials). Whole-body: the service caps uploads, so a stream
/// would add plumbing without changing the memory story.
async fn fetch_bytes(http: &reqwest::Client, record_id: uuid::Uuid) -> Result<Vec<u8>, ApiError> {
    let base = std::env::var("KALLIP_FILES_URL").map_err(|_| {
        ApiError::unavailable("KALLIP_FILES_URL is not set; the tagma cannot fetch media")
    })?;
    let token = std::env::var("KALLIP_FILES_TOKEN").map_err(|_| {
        ApiError::unavailable("KALLIP_FILES_TOKEN is not set; the tagma cannot fetch media")
    })?;
    let response = http
        .get(format!("{base}/v1/files/{record_id}"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| ApiError::unavailable(format!("files fetch failed: {e}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(files_fetch_error(record_id, status));
    }
    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| ApiError::unavailable(format!("files read failed: {e}")))
}

/// Map a files-service failure status: only a missing record means the
/// caller named something that does not exist; any other status is an
/// upstream fault (credentials, service health) surfaced as a gateway
/// error that keeps the upstream status for triage.
fn files_fetch_error(record_id: uuid::Uuid, status: reqwest::StatusCode) -> ApiError {
    if status == reqwest::StatusCode::NOT_FOUND {
        ApiError::not_found(format!("files record {record_id} does not exist"))
    } else {
        ApiError::bad_gateway(format!(
            "files fetch for {record_id}: upstream HTTP {status}"
        ))
    }
}

/// Record the ingest: live-store push plus the history sidecar append.
/// History failures warn, never fail the ingest — the runtime's own
/// appender has the same posture.
///
/// The two stores diverge by design: live context takes the assembled
/// parts message in memory, while history stores the text form (the
/// caption plus a pointer line) with the image referenced by the sidecar
/// attachment — the bytes never enter the record.
async fn record_ingest(
    target: &IngestTarget,
    req: &AttachmentIngestRequest,
    bytes: Vec<u8>,
) -> Result<Json<AttachmentIngestResponse>, ApiError> {
    let media_type = req
        .media_type
        .clone()
        .unwrap_or_else(|| "image/png".to_owned());
    let text = req
        .caption
        .clone()
        .unwrap_or_else(|| format!("[image {}]", req.record_id));
    let attachments = vec![AttachmentRef {
        modality: req.modality,
        record_id: req.record_id,
        media_type: media_type.clone(),
        caption: req.caption.clone(),
    }];
    let message = ingest_message(&text, &[IngestImage { media_type, bytes }]);

    // History stores the text form, never the assembled bytes: the
    // Base64 payload lives only in this in-memory message and the
    // upstream request. A files deletion leaves no image data in the
    // record; a restart rebuilds live context from text, and image
    // re-assembly is the compose path's job.
    let pointer = format!("[image {}]", req.record_id);
    let history_message = match &req.caption {
        Some(caption) => Message::user(format!("{caption}\n{pointer}")),
        None => Message::user(pointer),
    };

    let (turn_id, estimated) = {
        let mut guard = target.store.lock().await;
        guard.push_turn(vec![message.clone()])
    };
    if let Some(dir) = &target.agent_dir
        && let Err(e) = HistoryWriter::new(dir.clone()).append(
            Some(turn_id.0),
            std::slice::from_ref(&history_message),
            estimated,
            RecordKind::Turn,
            None,
            &attachments,
        )
    {
        tracing::warn!(turn_id = turn_id.0, "history write failed: {e:#}");
    }
    // Wake the agent so an idle one sees the new content on its next loop
    // pass; a running agent picks it up on the round after the current one.
    target.notify.notify_one();

    Ok(Json(AttachmentIngestResponse { turn_id: turn_id.0 }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{add_root, make_entry, make_state};

    fn request(record_id: uuid::Uuid) -> AttachmentIngestRequest {
        AttachmentIngestRequest {
            record_id,
            modality: Modality::Image,
            media_type: Some("image/png".to_owned()),
            caption: Some("a chart".to_owned()),
        }
    }

    fn target_for(state: &SharedState, id: &AgentId) -> IngestTarget {
        let registry = state.registry.try_read().unwrap();
        IngestTarget::of(registry.get(id).unwrap().as_live().unwrap())
    }

    // The engine trait must be in scope for `STANDARD.encode`.
    use base64::Engine as _;
    use just_llm_client::types::generation::{ContentPart, ImageSource};

    #[tokio::test]
    async fn enforce_refuses_a_gated_modality_with_both_values() {
        let state = make_state();
        let id = AgentId::random();
        {
            let mut reg = state.registry.write().await;
            add_root(&mut reg, &id);
        }
        let target = target_for(&state, &id);
        // The live snapshot must name a real set — seed it the way the
        // runtime's FailoverState would have.
        *target.profile_snapshot.lock().unwrap() = kallip_runtime::ProfileSnapshot {
            set_name: "default".into(),
            profile_id: "default-p".into(),
            provider: "oc-go".into(),
            model: "m".into(),
        };
        let err = enforce(&state, &target, Modality::Image).await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("image"), "got: {message}");
        assert!(message.contains("text"), "got: {message}");
        assert!(message.contains("rebind"), "got: {message}");

        // The serving modality passes the same choke.
        enforce(&state, &target, Modality::Text).await.unwrap();
    }

    #[tokio::test]
    async fn record_ingest_pushes_parts_and_appends_the_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let mut entry = make_entry(None, "tok".to_owned());
        entry.identity.agent_dir = Some(dir.path().to_owned());

        let target = IngestTarget::of(&entry);
        let record_id = uuid::Uuid::from_bytes([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7]);
        let req = request(record_id);

        let response = record_ingest(&target, &req, vec![1, 2, 3, 4])
            .await
            .unwrap();

        // Live store: one turn whose message is Parts([Text, Image{Base64}]).
        let store = target.store.lock().await;
        assert_eq!(store.turns().len(), 1);
        let message = &store.turns()[0].messages[0];
        let parts = message.content_parts().unwrap();
        assert_eq!(parts.len(), 2);
        assert!(matches!(parts[0], ContentPart::Text { .. }));
        match &parts[1] {
            ContentPart::Image {
                source: ImageSource::Base64 { data, media_type },
                ..
            } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(
                    data,
                    &base64::engine::general_purpose::STANDARD.encode([1, 2, 3, 4])
                );
            }
            other => panic!("expected a base64 image part, got {other:?}"),
        }
        drop(store);

        // History: the sidecar fields land on the record.
        let file = std::fs::read_dir(dir.path().join("history"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let line = std::fs::read_to_string(&file).unwrap();
        let record: kallip_runtime::history::HistoryRecord =
            serde_json::from_str(line.lines().next().unwrap()).unwrap();
        assert_eq!(record.attachments.len(), 1);
        let attachment = &record.attachments[0];
        assert_eq!(attachment.record_id, record_id);
        assert_eq!(attachment.modality, Modality::Image);
        assert_eq!(attachment.media_type, "image/png");
        assert_eq!(attachment.caption.as_deref(), Some("a chart"));
        assert_eq!(response.turn_id, record.turn_id.unwrap());
        // History stores the text form: the caption plus the pointer
        // line. The assembled bytes exist only in the live store's
        // in-memory message and the upstream request.
        let stored = record.messages[0]
            .content()
            .expect("history stores a plain-text message");
        assert!(stored.contains("a chart"));
        assert!(stored.contains(&format!("[image {record_id}]")));
        assert!(!line.contains(&base64::engine::general_purpose::STANDARD.encode([1, 2, 3, 4])));
    }

    #[test]
    fn files_fetch_error_keeps_404_and_folds_the_rest_into_bad_gateway() {
        assert_eq!(
            files_fetch_error(uuid::Uuid::nil(), reqwest::StatusCode::NOT_FOUND).status,
            404
        );
        for status in [401, 403, 500, 503] {
            let err = files_fetch_error(
                uuid::Uuid::nil(),
                reqwest::StatusCode::from_u16(status).unwrap(),
            );
            assert_eq!(err.status, 502, "upstream {status} must not report 404");
        }
    }
}
