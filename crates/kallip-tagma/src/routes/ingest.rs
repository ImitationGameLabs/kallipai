//! Attachment entrances. `POST /agents/{id}/attachments/ingest` is
//! the record form the `kallip image read` CLI drives: the tagma
//! fetches the media bytes from the files service. `POST
//! /agents/{id}/attachments` is the path form: the bytes ride inline
//! and land in the tagma's attachment blob store as the master copy.
//! Both assemble the multimodal message and record the turn (live
//! store push plus the history sidecar append). Self-scoped: an agent
//! ingests into its own context; the operator may target any agent.
//! The bound set's effective modalities gate both — the live twin of
//! the wake gate, refused before any files round-trip or blob write.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, header::CONTENT_TYPE};
use just_llm_client::types::generation::Message;
use kallip_common::agentid::AgentId;
use kallip_common::protocol::{
    ApiError, AttachmentIngestLocalResponse, AttachmentIngestRequest, AttachmentIngestResponse,
    Modality,
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

    let target = resolve_target(&state, &id).await?;

    // Enforcement precedes the fetch: a gated modality must not spend a
    // files round-trip.
    enforce(&state, &target, req.modality).await?;

    let bytes = crate::files::fetch_record_bytes(&state.files_http, req.record_id).await?;
    let blob_id = crate::files::store_mirror(state.attachment_blobs.get(), &bytes).await;
    let turn_id = record_ingest(
        &target,
        req.modality,
        req.media_type
            .clone()
            .unwrap_or_else(|| "image/png".to_owned()),
        req.caption.clone(),
        req.record_id,
        bytes,
        blob_id,
    )
    .await?;
    Ok(Json(AttachmentIngestResponse { turn_id }))
}

/// POST /agents/{id}/attachments — the path form. The media bytes ride
/// inline (the body), the media type is the `Content-Type` (defaulting
/// to `image/png`), and an optional file name arrives percent-encoded
/// in `X-Kallip-File-Name` (decoded for tracing). The bytes are stored
/// in the tagma's attachment blob store before the turn is recorded:
/// the blob is the master copy of a path-form reference, so a store
/// failure fails the ingest (fail closed) instead of recording a
/// reference whose bytes can never be re-assembled.
pub(crate) async fn store_attachment(
    State(state): State<SharedState>,
    auth: AuthIdentity,
    Path(id): Path<AgentId>,
    headers: HeaderMap,
    Query(query): Query<AttachmentQuery>,
    body: axum::body::Bytes,
) -> Result<Json<AttachmentIngestLocalResponse>, ApiError> {
    match auth.identity() {
        Identity::Operator => {}
        Identity::Agent { id: caller } if caller == &id => {}
        Identity::Agent { .. } => {
            return Err(ApiError::forbidden(
                "agents may only ingest into their own context",
            ));
        }
    }
    // Only image parts are wired (the same posture as the record form).
    let media_type = match headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or_default().trim())
    {
        Some(media_type) if media_type.starts_with("image/") => media_type.to_owned(),
        Some(media_type) => {
            return Err(ApiError::bad_request(format!(
                "only image ingest is wired; got media type {media_type:?}"
            )));
        }
        None => "image/png".to_owned(),
    };
    let file_name = headers
        .get("x-kallip-file-name")
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            percent_encoding::percent_decode_str(value)
                .decode_utf8_lossy()
                .into_owned()
        });

    let target = resolve_target(&state, &id).await?;

    // The gate precedes every write: a gated modality must not spend a
    // blob write any more than the record form spends a files fetch.
    enforce(&state, &target, Modality::Image).await?;

    let blob_id = store_blob(&state, &body).await?;
    let turn_id = record_ingest(
        &target,
        Modality::Image,
        media_type.clone(),
        query.caption.clone(),
        uuid::Uuid::nil(),
        body.to_vec(),
        Some(blob_id.as_str().to_owned()),
    )
    .await?;

    tracing::info!(
        agent = %id,
        turn_id,
        blob_id = blob_id.as_str(),
        media_type = %media_type,
        size = body.len(),
        file_name = file_name.as_deref().unwrap_or(""),
        "attachment stored from inline bytes"
    );
    Ok(Json(AttachmentIngestLocalResponse {
        turn_id,
        blob_id: blob_id.as_str().to_owned(),
    }))
}

/// Query parameters of the path-form store: an optional caption.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct AttachmentQuery {
    caption: Option<String>,
}

/// The fail-closed blob write backing the path form: the attachment
/// store must hold the bytes before the turn referencing them exists.
async fn store_blob(
    state: &SharedState,
    bytes: &[u8],
) -> Result<kallip_blob_store::BlobId, ApiError> {
    let Some(blobs) = state.attachment_blobs.get() else {
        return Err(ApiError::unavailable(
            "attachment blob store is not configured",
        ));
    };
    blobs
        .put(&mut std::io::Cursor::new(bytes))
        .await
        .map_err(|e| {
            tracing::warn!("attachment blob store write failed: {e}");
            ApiError::unavailable(format!("attachment blob store write failed: {e}"))
        })
}

/// The registry lookup both entrances share: resolve the target
/// agent's live handles, refusing faulted (no live context) and
/// unknown ids.
async fn resolve_target(state: &SharedState, id: &AgentId) -> Result<IngestTarget, ApiError> {
    // The runtime handles are cloned out from under the registry lock;
    // nothing below the lookup holds it.
    let registry = state.registry.read().await;
    match registry.get(id) {
        Some(RegistryEntry::Live(entry)) => Ok(IngestTarget::of(entry)),
        Some(RegistryEntry::Faulted(_)) => Err(ApiError::unavailable(
            "agent is faulted; there is no live context to ingest into",
        )),
        None => Err(ApiError::not_found(format!("no agent {id}"))),
    }
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

/// Record an ingest: live-store push plus the history sidecar append.
/// Both entrances funnel through here, carrying the modality, the
/// resolved media type, the caption, the reference record id, the
/// bytes, and the optional blob anchor. History failures warn, never
/// fail the ingest — the runtime's own appender has the same posture.
///
/// The two stores diverge by design: live context takes the assembled
/// parts message in memory, while history stores the text form (the
/// caption plus a pointer line) with the image referenced by the sidecar
/// attachment — the bytes never enter the record.
async fn record_ingest(
    target: &IngestTarget,
    modality: Modality,
    media_type: String,
    caption: Option<String>,
    record_id: uuid::Uuid,
    bytes: Vec<u8>,
    blob_id: Option<String>,
) -> Result<u64, ApiError> {
    let text = caption
        .clone()
        .unwrap_or_else(|| format!("[image {record_id}]"));
    let attachments = vec![AttachmentRef {
        modality,
        record_id,
        media_type: media_type.clone(),
        caption: caption.clone(),
        blob_id,
    }];
    let message = ingest_message(&text, &[IngestImage { media_type, bytes }]);

    // History stores the text form, never the assembled bytes: the
    // Base64 payload lives only in this in-memory message and the
    // upstream request. A files deletion leaves no image data in the
    // record; a restart re-assembles the parts message from the sidecar
    // reference via the compose path (context::compose::reassemble_attachments).
    let pointer = format!("[image {record_id}]");
    let history_message = match &caption {
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

    Ok(turn_id.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{add_root, make_entry, make_state, make_state_with_image_set};

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

        let turn = record_ingest(
            &target,
            Modality::Image,
            "image/png".to_owned(),
            Some("a chart".to_owned()),
            record_id,
            vec![1, 2, 3, 4],
            None,
        )
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
        assert_eq!(turn, record.turn_id.unwrap());
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

    #[tokio::test]
    async fn record_ingest_writes_the_mirror_anchor_into_the_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let mirror_dir = tempfile::tempdir().unwrap();
        let backend = kallip_blob_store::LocalBackend::arc(mirror_dir.path().to_owned());
        let mut entry = make_entry(None, "tok".to_owned());
        entry.identity.agent_dir = Some(dir.path().to_owned());

        let target = IngestTarget::of(&entry);
        let record_id = uuid::Uuid::from_bytes([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9]);

        let blob_id = crate::files::store_mirror(Some(&backend), &[7, 8])
            .await
            .unwrap();
        let _turn = record_ingest(
            &target,
            Modality::Image,
            "image/png".to_owned(),
            Some("a chart".to_owned()),
            record_id,
            vec![7, 8],
            Some(blob_id.clone()),
        )
        .await
        .unwrap();

        let file = std::fs::read_dir(dir.path().join("history"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let line = std::fs::read_to_string(&file).unwrap();
        let record: kallip_runtime::history::HistoryRecord =
            serde_json::from_str(line.lines().next().unwrap()).unwrap();
        assert_eq!(
            record.attachments[0].blob_id.as_deref(),
            Some(blob_id.as_str())
        );
        // The mirror holds the same bytes under that address.
        let stored = backend
            .get(&kallip_blob_store::BlobId::parse(&blob_id).unwrap())
            .await
            .unwrap();
        assert_eq!(stored, vec![7, 8]);
    }

    fn op_headers(media_type: &str, file_name: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, media_type.parse().unwrap());
        if let Some(name) = file_name {
            headers.insert("x-kallip-file-name", name.parse().unwrap());
        }
        headers
    }

    /// Seed the live snapshot the way the runtime's FailoverState would
    /// have, so `enforce` resolves the bound set (see the enforce test).
    fn seed_default_snapshot(state: &SharedState, id: &AgentId) {
        let registry = state.registry.try_read().unwrap();
        let entry = registry.get(id).unwrap().as_live().unwrap();
        *entry.agent.profile_snapshot.lock().unwrap() = kallip_runtime::ProfileSnapshot {
            set_name: "default".into(),
            profile_id: "default-p".into(),
            provider: "oc-go".into(),
            model: "m".into(),
        };
    }

    #[tokio::test]
    async fn store_attachment_records_turn_sidecar_and_blob() {
        let state = make_state_with_image_set();
        let dir = tempfile::tempdir().unwrap();
        let mirror_dir = tempfile::tempdir().unwrap();
        let backend = kallip_blob_store::LocalBackend::arc(mirror_dir.path().to_owned());
        assert!(state.attachment_blobs.set(backend.clone()).is_ok());
        let id = AgentId::random();
        let mut entry = make_entry(None, "tok".to_owned());
        entry.identity.agent_dir = Some(dir.path().to_owned());
        {
            let mut reg = state.registry.write().await;
            reg.register(id.clone(), RegistryEntry::Live(entry));
        }
        seed_default_snapshot(&state, &id);

        let response = store_attachment(
            State(state.clone()),
            AuthIdentity::test_new(Identity::Operator),
            Path(id.clone()),
            op_headers("image/jpeg", Some("team%20chart.png")),
            Query(AttachmentQuery {
                caption: Some("a chart".to_owned()),
            }),
            axum::body::Bytes::from_static(&[1, 2, 3, 4]),
        )
        .await
        .unwrap();

        // The blob is stored and the response carries its address.
        let blob_id =
            kallip_blob_store::BlobId::parse(&response.blob_id).expect("a canonical blob id");
        let stored = backend.get(&blob_id).await.unwrap();
        assert_eq!(stored, vec![1, 2, 3, 4]);

        // Live store: one turn whose message is Parts([Text, Image{Base64}]).
        let registry = state.registry.read().await;
        let target = IngestTarget::of(registry.get(&id).unwrap().as_live().unwrap());
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
                assert_eq!(media_type, "image/jpeg");
                assert_eq!(
                    data,
                    &base64::engine::general_purpose::STANDARD.encode([1, 2, 3, 4])
                );
            }
            other => panic!("expected a base64 image part, got {other:?}"),
        }
        drop(store);

        // History: the sidecar pins the nil record id plus the blob id.
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
        assert_eq!(attachment.record_id, uuid::Uuid::nil());
        assert_eq!(attachment.media_type, "image/jpeg");
        assert_eq!(
            attachment.blob_id.as_deref(),
            Some(response.blob_id.as_str())
        );
        assert_eq!(response.turn_id, record.turn_id.unwrap());
        // The pointer keeps the nil-UUID text form (machine-parseable).
        let stored = record.messages[0]
            .content()
            .expect("history stores a plain-text message");
        assert!(stored.contains("a chart"));
        assert!(stored.contains("[image 00000000-0000-0000-0000-000000000000]"));
    }

    #[tokio::test]
    async fn store_attachment_gates_before_any_persistence() {
        let state = make_state();
        let mirror_dir = tempfile::tempdir().unwrap();
        let backend = kallip_blob_store::LocalBackend::arc(mirror_dir.path().to_owned());
        assert!(state.attachment_blobs.set(backend.clone()).is_ok());
        let id = AgentId::random();
        {
            let mut reg = state.registry.write().await;
            add_root(&mut reg, &id);
        }
        seed_default_snapshot(&state, &id);

        let err = store_attachment(
            State(state.clone()),
            AuthIdentity::test_new(Identity::Operator),
            Path(id.clone()),
            op_headers("image/png", None),
            Query(AttachmentQuery { caption: None }),
            axum::body::Bytes::from_static(&[9, 9]),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("image"), "got: {err}");
        // Nothing was persisted: the blob store stays empty and the
        // live store has no turn.
        let anchor = kallip_blob_store::BlobId::for_bytes(&[9, 9]);
        assert_eq!(backend.stat(&anchor).await.unwrap(), None);
        let registry = state.registry.read().await;
        let entry = registry.get(&id).unwrap().as_live().unwrap();
        assert!(entry.agent.store.lock().await.turns().is_empty());
    }

    #[tokio::test]
    async fn store_attachment_rejects_non_image_media_types() {
        let state = make_state();
        let mirror_dir = tempfile::tempdir().unwrap();
        let backend = kallip_blob_store::LocalBackend::arc(mirror_dir.path().to_owned());
        assert!(state.attachment_blobs.set(backend.clone()).is_ok());
        let id = AgentId::random();
        {
            let mut reg = state.registry.write().await;
            add_root(&mut reg, &id);
        }
        seed_default_snapshot(&state, &id);

        let err = store_attachment(
            State(state.clone()),
            AuthIdentity::test_new(Identity::Operator),
            Path(id.clone()),
            op_headers("text/plain", None),
            Query(AttachmentQuery { caption: None }),
            axum::body::Bytes::from_static(&[9, 9]),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("only image"), "got: {err}");
        let anchor = kallip_blob_store::BlobId::for_bytes(&[9, 9]);
        assert_eq!(backend.stat(&anchor).await.unwrap(), None);
    }

    #[tokio::test]
    async fn store_attachment_fails_closed_when_the_blob_write_fails() {
        let state = make_state_with_image_set();
        let dir = tempfile::tempdir().unwrap();
        // The store root is a regular file: every put fails.
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let backend = kallip_blob_store::LocalBackend::arc(blocker);
        assert!(state.attachment_blobs.set(backend).is_ok());
        let id = AgentId::random();
        let mut entry = make_entry(None, "tok".to_owned());
        entry.identity.agent_dir = Some(dir.path().to_owned());
        {
            let mut reg = state.registry.write().await;
            reg.register(id.clone(), RegistryEntry::Live(entry));
        }
        seed_default_snapshot(&state, &id);

        let err = store_attachment(
            State(state.clone()),
            AuthIdentity::test_new(Identity::Operator),
            Path(id.clone()),
            op_headers("image/png", None),
            Query(AttachmentQuery { caption: None }),
            axum::body::Bytes::from_static(&[7, 8]),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("blob store"), "got: {err}");
        // Fail closed: no turn, no sidecar row.
        let registry = state.registry.read().await;
        let entry = registry.get(&id).unwrap().as_live().unwrap();
        assert!(entry.agent.store.lock().await.turns().is_empty());
        assert!(!dir.path().join("history").exists());
    }

    #[tokio::test]
    async fn store_attachment_is_idempotent_for_the_same_bytes() {
        let state = make_state_with_image_set();
        let mirror_dir = tempfile::tempdir().unwrap();
        let backend = kallip_blob_store::LocalBackend::arc(mirror_dir.path().to_owned());
        assert!(state.attachment_blobs.set(backend.clone()).is_ok());
        let id = AgentId::random();
        {
            let mut reg = state.registry.write().await;
            add_root(&mut reg, &id);
        }
        seed_default_snapshot(&state, &id);

        let first = store_attachment(
            State(state.clone()),
            AuthIdentity::test_new(Identity::Operator),
            Path(id.clone()),
            op_headers("image/png", None),
            Query(AttachmentQuery { caption: None }),
            axum::body::Bytes::from_static(&[5, 5]),
        )
        .await
        .unwrap();
        let second = store_attachment(
            State(state),
            AuthIdentity::test_new(Identity::Operator),
            Path(id),
            op_headers("image/png", None),
            Query(AttachmentQuery { caption: None }),
            axum::body::Bytes::from_static(&[5, 5]),
        )
        .await
        .unwrap();
        // Same bytes, same content address.
        assert_eq!(first.blob_id, second.blob_id);
    }
}
