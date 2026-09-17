//! The tagma state plane: the per-tagma read
//! endpoints that serve the stored snapshot (stale reads included -- the
//! table outlives presence), and the per-tagma SSE change-notification
//! stream whose subscription-count flips drive `OwnerSub` over the tagma's
//! tunnel. Snapshots enter via the batched `POST /upstream` channel, whose
//! demux fans into `accept_projection` here.
//!
//! Every client-facing route re-checks ownership (the stored/live owner must be the
//! caller) so cross-tenant reads are 403; the tagma-side push auth lives in
//! the upstream channel's batch-level check, not on the read plane.
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use kallip_archeion_common::ids::{ParticipantId, TagmaId};
use kallip_archeion_common::principal::{Principal, require_user};
use kallip_lesche_common::projection::{ProjectionDirty, ProjectionSnapshot};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::auth::AuthPrincipal;
use crate::sse::{BoxEventStream, OnDrop};
use crate::state::SharedConvState;

pub fn router() -> Router<SharedConvState> {
    Router::new()
        // Registered at the root: the bare resource mounts are
        // /tagmata/{id}/* -- the shapes the
        // tagma pump and the browser client both dial.
        .route("/tagmata/{id}/state", get(state_events))
        .route("/tagmata/{id}/agents", get(read_agents))
        .route("/tagmata/{id}/budget", get(read_budget))
        .route("/tagmata/{id}/work-schedule", get(read_work_schedule))
        .route("/tagmata/{id}/status", get(read_status))
}

/// Owner gate for the read/SSE plane, offline-safe: the owner recorded on the stored
/// projection entry (or the live presence) must be the caller. `None` when
/// the tagma has no stored projection at all.
#[allow(clippy::result_large_err)] // one-line helper; boxing the error costs more
fn require_owner(
    principal: &Principal,
    state: &SharedConvState,
    tagma: &TagmaId,
) -> Result<(), Response> {
    let user = match require_user(principal) {
        Ok(u) => u,
        Err(_) => {
            return Err((StatusCode::UNAUTHORIZED, "operator session required").into_response());
        }
    };
    let reg = state.registry.read().expect("registry lock");
    let owner = match reg.presence.get(&ParticipantId::for_tagma(tagma)) {
        Some(entry) => Some(&entry.owner),
        None => reg.projection(tagma).map(|p| &p.owner),
    };
    match owner {
        Some(o) if o == user => Ok(()),
        Some(_) => Err((StatusCode::FORBIDDEN, "not your tagma").into_response()),
        None => Err((StatusCode::NOT_FOUND, "no projection").into_response()),
    }
}

/// The upstream channel's projection fan: accept into the projection store
/// (generation logic inside) + dirty fan. One shared implementation so
/// wire paths cannot drift.
pub(super) async fn accept_projection(
    state: &SharedConvState,
    tagma_id: TagmaId,
    snapshot: ProjectionSnapshot,
) -> Response {
    let mut registry = state.registry.write().expect("registry lock");
    let (generation, owner) = match registry.presence.get(&ParticipantId::for_tagma(&tagma_id)) {
        Some(entry) => (entry.id.clone(), entry.owner.clone()),
        None => {
            return (
                StatusCode::NOT_FOUND,
                "tagma not online (push needs a live tunnel session)",
            )
                .into_response();
        }
    };
    match registry.accept_projection(
        &tagma_id,
        &generation,
        snapshot.push_seq,
        owner.clone(),
        snapshot,
    ) {
        Some(seq) => {
            registry.fan_projection_dirty(
                &owner,
                &tagma_id,
                ProjectionDirty {
                    tagma_id: tagma_id.clone(),
                    seq,
                },
            );
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({ "seq": seq })),
            )
                .into_response()
        }
        None => (
            StatusCode::OK,
            axum::Json(serde_json::json!({ "ignored": "stale push_seq" })),
        )
            .into_response(),
    }
}

/// Shared read core: owner gate, then the stored entry and its staleness (a tagma
/// with no live presence serves its projection as `stale`).
#[allow(clippy::result_large_err)] // sibling helper of require_owner
fn read_entry(
    principal: &Principal,
    state: &SharedConvState,
    tagma: &TagmaId,
) -> Result<(crate::state::ProjectionEntry, bool), Response> {
    require_owner(principal, state, tagma)?;
    let registry = state.registry.read().expect("registry lock");
    let entry = registry
        .projection(tagma)
        .cloned()
        .ok_or_else(|| (StatusCode::NOT_FOUND, "no projection").into_response())?;
    let stale = !registry
        .presence
        .contains_key(&ParticipantId::for_tagma(tagma));
    Ok((entry, stale))
}

async fn read_agents(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<String>,
) -> Response {
    let tagma_id = TagmaId::from(id);
    let (entry, stale) = match read_entry(&principal, &state, &tagma_id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    axum::Json(serde_json::json!({
        "stale": stale,
        "seq": entry.seq,
        "updated_at": entry.updated_at,
        "agents": entry.snapshot.agents,
        "status": entry.snapshot.status,
    }))
    .into_response()
}

/// `GET /tagmata/{id}/status` -- the status face's on-demand full fetch:
/// the presence cache's latest relayed snapshot, no tagma round-trip.
/// Owner-gated like the other read planes; an offline tagma falls back
/// to its stored projection (marked `stale`), mirroring `/agents`.
async fn read_status(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<String>,
) -> Response {
    let tagma_id = TagmaId::from(id);
    if let Err(resp) = require_owner(&principal, &state, &tagma_id) {
        return resp;
    }
    let registry = state.registry.read().expect("registry lock");
    if let Some(entry) = registry.presence.get(&ParticipantId::for_tagma(&tagma_id)) {
        return axum::Json(serde_json::json!({
            "stale": false,
            "status": entry.latest_status,
        }))
        .into_response();
    }
    let entry = match registry.projection(&tagma_id) {
        Some(p) => p,
        None => return (StatusCode::NOT_FOUND, "no projection").into_response(),
    };
    axum::Json(serde_json::json!({
        "stale": true,
        "status": entry.snapshot.status,
    }))
    .into_response()
}

async fn read_budget(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<String>,
) -> Response {
    let tagma_id = TagmaId::from(id);
    let (entry, stale) = match read_entry(&principal, &state, &tagma_id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    axum::Json(serde_json::json!({
        "stale": stale,
        "seq": entry.seq,
        "updated_at": entry.updated_at,
        "budget": entry.snapshot.status.token_budget,
        "consumed": entry.snapshot.status.token_consumed,
    }))
    .into_response()
}

async fn read_work_schedule(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<String>,
) -> Response {
    let tagma_id = TagmaId::from(id);
    let (entry, stale) = match read_entry(&principal, &state, &tagma_id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    axum::Json(serde_json::json!({
        "stale": stale,
        "seq": entry.seq,
        "updated_at": entry.updated_at,
        "work_schedule": entry.snapshot.work_schedule,
    }))
    .into_response()
}

/// The per-tagma change stream: `ProjectionDirty { tagma_id, seq }` frames on
/// an independent broadcast (never the `me/events` app stream). The
/// 0 -> 1 subscribe edge fans `OwnerSub { face: Projection, active: true }` down the
/// tagma's tunnel; the last unsubscribe fans `false` after the lag window.
async fn state_events(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<String>,
) -> Result<Sse<axum::response::sse::KeepAliveStream<OnDrop>>, StatusCode> {
    // The SSE plane is owner-gated like the read plane -- a
    // cross-tenant subscriber must not be able to flip the tagma's
    // subscription hint (remote pump start) or probe existence.
    let tagma_id = TagmaId::from(id);
    let user_id = match require_owner(&principal, &state, &tagma_id) {
        Ok(()) => match require_user(&principal) {
            Ok(u) => u.clone(),
            Err(_) => return Err(StatusCode::UNAUTHORIZED),
        },
        Err(resp) => return Err(resp.status()),
    };
    let handle = tokio::runtime::Handle::try_current().ok();
    let (tx, rx, _was_first, unsub_lag) = {
        let mut registry = state.registry.write().expect("registry lock");
        let stream = registry.open_projection_stream(&user_id, &tagma_id);
        let lag = std::time::Duration::from_millis(registry.projection_unsub_lag_ms());
        (stream.0, stream.1, stream.2, lag)
    };
    let stream: BoxEventStream = Box::pin(
        BroadcastStream::new(rx)
            .filter_map(|r| match r {
                Ok(ev) => Some(ev),
                Err(BroadcastStreamRecvError::Lagged(n)) => {
                    tracing::warn!(lag = n, "projection SSE lagged; dirty frames dropped");
                    None
                }
            })
            .map(|dirty| {
                Ok::<Event, std::convert::Infallible>(
                    Event::default().json_data(dirty).expect("event serializes"),
                )
            }),
    );
    // tx is the Sender cloned from the map. By the time this closure runs,
    // the stream (and our rx inside it) is already dropped, so
    // `receiver_count() > 1` == "another subscriber still live". The lag
    // window delays the teardown so a quick reconnect does not flap the hint.
    let cleanup_state = state.clone();
    let cleanup_user = user_id.clone();
    let cleanup_tx = tx.clone();
    let cleaned = OnDrop::new(stream, move || {
        if cleanup_tx.receiver_count() > 1 {
            return;
        }
        let st = cleanup_state.clone();
        let user = cleanup_user.clone();
        let tagma = tagma_id.clone();
        let tx = cleanup_tx.clone();
        let Some(h) = handle.as_ref() else {
            return;
        };
        h.spawn(async move {
            tokio::time::sleep(unsub_lag).await;
            // remove_if_last fans the `OwnerSub { Projection, false }`
            // itself on the 1 -> 0 edge -- no duplicate send here.
            let Ok(mut registry) = st.write() else {
                return;
            };
            registry.remove_projection_stream_if_last(&user, &tagma, &tx);
        });
    });
    // `: ping` comment every 5s: dirty frames only flow on changes, so a
    // quiet tagma would otherwise stream zero bytes and get reaped by an
    // idle timeout (proxy or browser) -- the client would see the body
    // die mid-stream (fetch TypeError) and back off reconnecting. The
    // front-end SSE parser ignores comment lines, so the ping cannot be
    // mistaken for a dirty frame. Same contract as the tunnel SSE.
    Ok(Sse::new(cleaned).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(5))
            .text("ping"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::test_support::db_state;
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use kallip_archeion_common::ids::UserId;
    use kallip_lesche_common::tunnel::TunnelInbound;
    use std::sync::Arc;

    pub(super) fn uid(s: &str) -> UserId {
        UserId::from(s.to_string())
    }

    pub(super) fn tagma_of(s: &str) -> TagmaId {
        TagmaId::from(s.to_string())
    }

    fn snapshot_with(push_seq: u64) -> ProjectionSnapshot {
        serde_json::from_value(serde_json::json!({
            "agents": [],
            "status": {
                "root_state": "idle",
                "subagents_total": 0,
                "subagents_active": 0,
                "token_budget": 100,
                "token_consumed": 3,
            },
            "push_seq": push_seq,
            "work_schedule": null,
        }))
        .expect("snapshot parses")
    }

    pub(super) async fn push(
        state: &SharedConvState,
        _principal: AuthPrincipal,
        agent: &str,
        push_seq: u64,
    ) -> Response {
        accept_projection(
            state,
            TagmaId::from(agent.to_string()),
            snapshot_with(push_seq),
        )
        .await
    }

    pub(super) fn owner_principal(user: &UserId) -> AuthPrincipal {
        AuthPrincipal(Principal::User(user.clone()))
    }

    pub(super) async fn enroll(
        state: &SharedConvState,
        tagma: &TagmaId,
        owner: &UserId,
    ) -> tokio::sync::broadcast::Receiver<TunnelInbound> {
        let mut reg = state.registry.write().unwrap();
        let (tx, rx) = tokio::sync::broadcast::channel(8);
        reg.register_presence(tagma, owner.clone(), tx, Arc::new(()));
        rx
    }

    /// Cross-tenant reads are 403 and an unknown tagma is 404.
    #[tokio::test]
    async fn cross_tenant_read_is_forbidden() {
        let (state, _control) = db_state().await;
        let tagma = tagma_of("t-a");
        let owner = uid("alice");
        let other = uid("mallory");
        let _rx = enroll(&state, &tagma, &owner).await;
        let resp = push(
            &state,
            AuthPrincipal(Principal::Tagma(tagma.clone())),
            "t-a",
            1,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let ok = read_agents(
            State(state.clone()),
            owner_principal(&owner),
            Path("t-a".to_string()),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);
        let denied = read_agents(
            State(state.clone()),
            owner_principal(&other),
            Path("t-a".to_string()),
        )
        .await;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    }

    /// Accepted pushes bump the store seq and serve the stored snapshot; a
    /// same-generation replay (stale push_seq) is ignored without bumping.
    #[tokio::test]
    async fn push_read_roundtrip_and_replay_rejection() {
        let (state, _control) = db_state().await;
        let tagma = tagma_of("t-a");
        let owner = uid("alice");
        let _rx = enroll(&state, &tagma, &owner).await;
        let first = push(
            &state,
            AuthPrincipal(Principal::Tagma(tagma.clone())),
            "t-a",
            1,
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let replay = push(
            &state,
            AuthPrincipal(Principal::Tagma(tagma.clone())),
            "t-a",
            1,
        )
        .await;
        assert_eq!(replay.status(), StatusCode::OK); // idempotent ignore
        let body = read_agents(
            State(state.clone()),
            owner_principal(&owner),
            Path("t-a".to_string()),
        )
        .await;
        let bytes = axum::body::to_bytes(body.into_body(), 1 << 20)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["seq"], 1, "replay must not bump the store seq");
        assert_eq!(json["stale"], false, "tagma online: not stale");
        assert!(json["agents"].is_array());
    }
}

#[cfg(test)]
mod generation_tests {
    use super::tests::{enroll, owner_principal, push, tagma_of, uid};
    use super::*;
    use crate::routes::test_support::db_state;
    use kallip_lesche_common::tunnel::TunnelInbound;

    /// A reconnecting tagma's push counter restarted, so the first push
    /// of the new generation is accepted unconditionally even though its
    /// push_seq is lower than the previous generation's last one.
    #[tokio::test]
    async fn reconnect_generation_accepts_reset_seq() {
        let (state, _control) = db_state().await;
        let tagma = tagma_of("t-a");
        let owner = uid("alice");
        let _rx = enroll(&state, &tagma, &owner).await;
        let principal = AuthPrincipal(Principal::Tagma(tagma.clone()));
        let high = push(&state, principal.clone(), "t-a", 9).await;
        assert_eq!(high.status(), StatusCode::OK);
        // The tunnel drops and re-establishes: a fresh presence generation.
        let _rx2 = enroll(&state, &tagma, &owner).await;
        let after = push(&state, principal, "t-a", 1).await;
        assert_eq!(after.status(), StatusCode::OK);
        let body = read_agents(
            State(state.clone()),
            owner_principal(&owner),
            Path("t-a".to_string()),
        )
        .await;
        let bytes = axum::body::to_bytes(body.into_body(), 1 << 20)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            json["seq"], 2,
            "new-generation push is accepted and seq bumps"
        );
    }

    /// The subscription flip fans OwnerSub both ways over the tagma's
    /// tunnel: first subscriber -> true, (post-lag-window teardown of) the
    /// last subscriber -> false. The lag window itself lives in the SSE
    /// handler; this pins the registry edges the handler drives.
    #[tokio::test]
    async fn subscription_flip_fans_hint_both_ways() {
        let (state, _control) = db_state().await;
        let tagma = tagma_of("t-a");
        let owner = uid("alice");
        let mut hint_rx = enroll(&state, &tagma, &owner).await;
        let (tx, _rx, was_first) = {
            let mut reg = state.registry.write().unwrap();
            reg.open_projection_stream(&owner, &tagma)
        };
        assert!(was_first, "first subscriber is the 0 -> 1 edge");
        hint_rx
            .try_recv()
            .expect("hint true fanned on the open edge");
        assert!(matches!(
            hint_rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        // OnDrop semantics: the dying stream's own rx is dropped before the
        // cleanup runs, which is what makes receiver_count() hit zero.
        drop(_rx);
        let removed = {
            let mut reg = state.registry.write().unwrap();
            reg.remove_projection_stream_if_last(&owner, &tagma, &tx)
        };
        assert!(removed, "last unsubscribe is the 1 -> 0 edge");
        // The false half of the flip actually reaches the tunnel.
        assert!(matches!(
            hint_rx.try_recv(),
            Ok(TunnelInbound::OwnerSub {
                face: kallip_lesche_common::tunnel::Face::Projection,
                active: false,
            })
        ));
    }

    /// The teardown lag gates the false flip. A subscriber that
    /// re-subscribes inside the window re-arms the hint (its open is the
    /// new 0 -> 1 edge) and the pending teardown no-ops; only the true
    /// last departure fans `false` and removes the channel.
    #[tokio::test]
    async fn unsub_lag_window_gates_the_false_flip() {
        let (state, _control) = db_state().await;
        let tagma = tagma_of("t-a");
        let owner = uid("alice");
        let mut hint_rx = enroll(&state, &tagma, &owner).await;
        let (tx, rx, was_first) = {
            let mut reg = state.registry.write().unwrap();
            reg.open_projection_stream(&owner, &tagma)
        };
        assert!(was_first);
        hint_rx
            .try_recv()
            .expect("hint true fanned on the open edge");
        drop(rx);
        // A new subscriber inside the lag window: re-arms the hint.
        let (tx2, rx2, was_first_again) = {
            let mut reg = state.registry.write().unwrap();
            reg.open_projection_stream(&owner, &tagma)
        };
        assert!(was_first_again, "re-subscribe re-arms the hint");
        let removed = {
            let mut reg = state.registry.write().unwrap();
            reg.remove_projection_stream_if_last(&owner, &tagma, &tx)
        };
        assert!(!removed, "a live subscriber survives the teardown");
        assert!(matches!(
            hint_rx.try_recv(),
            Ok(TunnelInbound::OwnerSub {
                face: kallip_lesche_common::tunnel::Face::Projection,
                active: true,
            })
        ));
        // After the true departure: teardown + false hint.
        drop(rx2);
        let removed = {
            let mut reg = state.registry.write().unwrap();
            reg.remove_projection_stream_if_last(&owner, &tagma, &tx2)
        };
        assert!(removed, "the last unsubscribe tears the channel down");
        assert!(matches!(
            hint_rx.try_recv(),
            Ok(TunnelInbound::OwnerSub {
                face: kallip_lesche_common::tunnel::Face::Projection,
                active: false,
            })
        ));
    }

    /// The status-face edges: the first `open_app_stream` create
    /// fans `OwnerSub { Status, true }` to every live tagma of the owner
    /// (owner-scoped: one edge, N sends), a second tab fans nothing, and
    /// the last departure fans `false` to both. A foreign owner's tagma
    /// hears nothing (presence-gated fan).
    #[tokio::test]
    async fn app_stream_edges_fan_status_owner_subs() {
        let (state, _control) = db_state().await;
        let owner = uid("alice");
        let t1 = tagma_of("t-a");
        let t2 = tagma_of("t-b");
        let stranger = tagma_of("t-c");
        let other = uid("mallory");
        let mut rx1 = enroll(&state, &t1, &owner).await;
        let mut rx2 = enroll(&state, &t2, &owner).await;
        let mut rx_stranger = enroll(&state, &stranger, &other).await;

        let stream = {
            let mut reg = state.registry.write().unwrap();
            reg.open_app_stream(&owner)
        };
        for (name, rx) in [("t1", &mut rx1), ("t2", &mut rx2)] {
            assert!(
                matches!(
                    rx.try_recv(),
                    Ok(TunnelInbound::OwnerSub {
                        face: kallip_lesche_common::tunnel::Face::Status,
                        active: true
                    })
                ),
                "{name} hears the open edge"
            );
        }
        assert!(matches!(
            rx_stranger.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));

        // A second tab joins an existing stream: no edge, no fan.
        {
            let mut reg = state.registry.write().unwrap();
            reg.open_app_stream(&owner);
        }
        assert!(matches!(
            rx1.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));

        // The last departure fans `false` to both of the owner's tagmata.
        let _rx_dying = stream.subscribe();
        let removed = {
            let mut reg = state.registry.write().unwrap();
            reg.remove_app_stream_if_last(&owner, &stream)
        };
        assert!(removed, "last subscriber removal is the 1 -> 0 edge");
        for (name, rx) in [("t1", &mut rx1), ("t2", &mut rx2)] {
            assert!(
                matches!(
                    rx.try_recv(),
                    Ok(TunnelInbound::OwnerSub {
                        face: kallip_lesche_common::tunnel::Face::Status,
                        active: false
                    })
                ),
                "{name} hears the close edge"
            );
        }
    }

    /// The removal window: a tab arriving before the last
    /// departure keeps the stream open -- the join pushes the stream's
    /// receiver count past the departing tab's own, so that tab's
    /// `remove_app_stream_if_last` is refused (no close edge, registry
    /// still live), and only the genuinely-last departure fires the
    /// 1 -> 0 fan.
    #[tokio::test]
    async fn removal_window_arrival_keeps_the_stream_open() {
        let (state, _control) = db_state().await;
        let owner = uid("boa");
        let t1 = tagma_of("t-w");
        let mut rx = enroll(&state, &t1, &owner).await;

        let s = {
            let mut reg = state.registry.write().unwrap();
            reg.open_app_stream(&owner)
        };
        let r1 = s.subscribe(); // tab 1's SSE receiver
        assert!(
            matches!(
                rx.try_recv(),
                Ok(TunnelInbound::OwnerSub {
                    face: kallip_lesche_common::tunnel::Face::Status,
                    active: true
                })
            ),
            "open edge fans true"
        );

        // A second tab joins inside the removal window: same stream, no
        // fan (the 0 -> 1 edge already happened).
        let s2 = {
            let mut reg = state.registry.write().unwrap();
            reg.open_app_stream(&owner)
        };
        let r2 = s2.subscribe();
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "the join is not an edge"
        );

        // Tab 1 departs while tab 2 is live: the refusal IS the window
        // protection (no close edge may fire over a live tab).
        let removed = {
            let mut reg = state.registry.write().unwrap();
            reg.remove_app_stream_if_last(&owner, &s)
        };
        assert!(!removed, "a live second tab keeps the stream open");
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "no close edge while a tab remains"
        );
        assert!(
            state.registry.read().unwrap().has_app_stream(&owner),
            "the stream is still open"
        );

        // The genuinely-last departure fires the 1 -> 0 fan.
        drop(r1);
        let removed = {
            let mut reg = state.registry.write().unwrap();
            reg.remove_app_stream_if_last(&owner, &s2)
        };
        assert!(removed, "the last departure fires the close edge");
        assert!(
            matches!(
                rx.try_recv(),
                Ok(TunnelInbound::OwnerSub {
                    face: kallip_lesche_common::tunnel::Face::Status,
                    active: false
                })
            ),
            "close edge fans false"
        );
        drop(r2);
    }

    /// The offline arm of the read plane -- with the tagma's
    /// presence gone, the stored projection still serves the owner,
    /// flagged `stale`, at the same seq the online read showed.
    #[tokio::test]
    async fn offline_tagma_serves_stale_projection() {
        let (state, _control) = db_state().await;
        let tagma = tagma_of("t-a");
        let owner = uid("alice");
        let generation = {
            let mut reg = state.registry.write().unwrap();
            let (tx, _rx) = tokio::sync::broadcast::channel(8);
            let id = std::sync::Arc::new(());
            reg.register_presence(&tagma, owner.clone(), tx, id.clone());
            id
        };
        let first = push(
            &state,
            AuthPrincipal(Principal::Tagma(tagma.clone())),
            "t-a",
            1,
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        // The tunnel drops: presence is gone, the projection table stays.
        {
            let mut reg = state.registry.write().unwrap();
            assert!(reg.take_presence_if_owned(&tagma, &generation));
        }
        let body = read_agents(
            State(state.clone()),
            owner_principal(&owner),
            Path("t-a".to_string()),
        )
        .await;
        assert_eq!(body.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(body.into_body(), 1 << 20)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["seq"], 1);
        assert_eq!(json["stale"], true, "offline tagma reads as stale");
    }

    /// The SSE plane is owner-gated like the read plane -- a
    /// cross-tenant subscriber gets 403 and can neither start the tagma's
    /// pump remotely nor probe the tagma's existence.
    #[tokio::test]
    async fn cross_tenant_events_subscription_is_forbidden() {
        let (state, _control) = db_state().await;
        let tagma = tagma_of("t-a");
        let owner = uid("alice");
        let other = uid("mallory");
        let _rx = enroll(&state, &tagma, &owner).await;
        let err = state_events(
            State(state.clone()),
            owner_principal(&other),
            Path("t-a".to_string()),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::FORBIDDEN);
    }

    /// End-to-end: the real OnDrop path -- drop the SSE response,
    /// the injected lag elapses, then the false hint reaches the tunnel
    /// and the channel is torn down (this is what consumes the setter).
    #[tokio::test]
    async fn on_drop_teardown_fans_false_after_injected_lag() {
        let (state, _control) = db_state().await;
        let tagma = tagma_of("t-a");
        let owner = uid("alice");
        let mut hint_rx = enroll(&state, &tagma, &owner).await;
        {
            let reg = state.registry.read().unwrap();
            reg.set_projection_unsub_lag_ms(50);
        }
        let sse = state_events(
            State(state.clone()),
            owner_principal(&owner),
            Path("t-a".to_string()),
        )
        .await
        .expect("owner subscribes");
        assert!(matches!(
            hint_rx.try_recv(),
            Ok(TunnelInbound::OwnerSub {
                face: kallip_lesche_common::tunnel::Face::Projection,
                active: true,
            })
        ));
        drop(sse); // OnDrop fires: lag, then teardown + false hint
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(matches!(
            hint_rx.try_recv(),
            Ok(TunnelInbound::OwnerSub {
                face: kallip_lesche_common::tunnel::Face::Projection,
                active: false,
            })
        ));
        let live = {
            let reg = state.registry.read().unwrap();
            reg.projection_stream_live_for_tagma(&tagma)
        };
        assert!(!live, "the channel is gone after the lag window");
    }
    /// Keep-alive nail: a stream with no dirty traffic still emits the
    /// `: ping` comment frame within the keep-alive interval, and the
    /// frame carries no `data:`/`event:` lines -- an SSE parser ignores
    /// comment lines, so the idle ping can never be mistaken for a
    /// dirty frame (no seq, no onFrame). The paused clock lets the 5s
    /// interval elapse without any real waiting.
    #[tokio::test]
    async fn idle_stream_emits_comment_ping_not_data() {
        let (state, _control) = db_state().await;
        // Real clock for DB setup, then freeze: the pending stream read
        // auto-advances past the 5s keep-alive with zero real waiting.
        tokio::time::pause();
        let tagma = tagma_of("t-a");
        let owner = uid("alice");
        let _hint_rx = enroll(&state, &tagma, &owner).await;
        let sse = state_events(
            State(state.clone()),
            owner_principal(&owner),
            Path("t-a".to_string()),
        )
        .await
        .expect("owner subscribes");
        let mut stream = sse.into_response().into_body().into_data_stream();
        // Pending read on the paused clock: time auto-advances past the
        // keep-alive interval; the timeout makes a keep_alive regression
        // (no frame ever) fail red instead of hanging the test.
        let frame = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            futures_util::StreamExt::next(&mut stream),
        )
        .await
        .expect("keep-alive frame must arrive within its interval")
        .expect("stream live")
        .expect("frame bytes");
        let text = String::from_utf8(frame.to_vec()).expect("utf8 frame");
        assert!(text.starts_with(':'), "comment line expected: {text:?}");
        assert!(text.contains(": ping"), "ping comment expected: {text:?}");
        assert!(!text.contains("data:"), "no dirty payload: {text:?}");
        assert!(!text.contains("event:"), "no event type: {text:?}");
    }

    /// `GET /tagmata/{id}/status`: a live tunnel serves the presence cache
    /// (stale=false, cache value verbatim); with the tunnel gone and no
    /// projection stored, the route answers 404.
    #[tokio::test]
    async fn read_status_serves_cache_then_404_without_projection() {
        use crate::test_support::{make_state, seed_presence};
        use kallip_archeion_common::ids::ParticipantId;
        use kallip_archeion_common::principal::Principal;
        use kallip_common::protocol::AgentState;
        use kallip_lesche_common::event::TagmaStatusPayload;
        let (state, _control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = uid("owner");
        let tagma = TagmaId::from("tagma-1".to_string());
        let (_t_tx, _id) = seed_presence(&state, &tagma, owner.clone());
        let payload = TagmaStatusPayload {
            root_state: AgentState::Busy,
            subagents_total: 1,
            subagents_active: 1,
            token_budget: 50_000,
            token_consumed: 7,
            token_budget_unlimited: false,
        };
        {
            let mut reg = state.write().unwrap();
            let pid = ParticipantId::for_tagma(&tagma);
            reg.presence.get_mut(&pid).unwrap().latest_status = Some(payload);
        }
        let response = read_status(
            State(state.clone()),
            AuthPrincipal(Principal::User(owner.clone())),
            Path("tagma-1".to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        let text = String::from_utf8(bytes.to_vec()).expect("utf8");
        assert!(text.contains("\"stale\":false"), "{text:?}");
        assert!(text.contains("\"token_consumed\":7"), "{text:?}");
        // Tunnel gone and no projection stored: 404.
        {
            let mut reg = state.write().unwrap();
            let pid = ParticipantId::for_tagma(&tagma);
            reg.presence.remove(&pid);
        }
        let response = read_status(
            State(state),
            AuthPrincipal(Principal::User(owner)),
            Path("tagma-1".to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// `GET /tagmata/{id}/status`: with the tunnel gone, the stored
    /// projection still answers -- `stale: true` with the projection's
    /// status counters (the GET contract's offline half).
    #[tokio::test]
    async fn read_status_serves_stale_projection_after_tunnel_loss() {
        use crate::test_support::{make_state, seed_presence};
        use kallip_archeion_common::principal::Principal;
        use kallip_common::protocol::AgentState;
        use kallip_lesche_common::event::TagmaStatusPayload;
        use kallip_lesche_common::projection::ProjectionSnapshot;
        let (state, _control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = uid("owner");
        let tagma = TagmaId::from("tagma-1".to_string());
        let (_t_tx, generation) = seed_presence(&state, &tagma, owner.clone());
        let payload = TagmaStatusPayload {
            root_state: AgentState::Idle,
            subagents_total: 2,
            subagents_active: 0,
            token_budget: 90_000,
            token_consumed: 11,
            token_budget_unlimited: false,
        };
        {
            let mut reg = state.write().unwrap();
            reg.accept_projection(
                &tagma,
                &generation,
                1,
                owner.clone(),
                ProjectionSnapshot {
                    agents: Vec::new(),
                    status: payload,
                    push_seq: 1,
                    work_schedule: None,
                },
            );
        }
        // The tunnel drops: presence is gone, the projection stays.
        {
            let mut reg = state.write().unwrap();
            assert!(reg.take_presence_if_owned(&tagma, &generation));
        }
        let response = read_status(
            State(state.clone()),
            AuthPrincipal(Principal::User(owner.clone())),
            Path("tagma-1".to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        let text = String::from_utf8(bytes.to_vec()).expect("utf8");
        assert!(text.contains("\"stale\":true"), "{text:?}");
        assert!(text.contains("\"token_consumed\":11"), "{text:?}");
        assert!(text.contains("\"root_state\":\"idle\""), "{text:?}");
    }
}
