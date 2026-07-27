//! `POST /agents/{id}/lesche/messages` — the root agent's "speak to the user"
//! primitive.
//!
//! The agent invokes `kallip lesche send` (a subcommand of the `kallip` CLI)
//! via `bash_exec`; it authenticates with its own per-agent token and POSTs
//! here. The tagma, holding the E2E key in-process, delivers the text as an
//! `AssistantContent` envelope over the relay. This replaces the former
//! standalone connector's unix-socket reply path.
//!
//! Root-only: the conversation with the user is owned by the single root
//! agent, so delivering a user-facing message is the root's job. A subagent
//! that tries is rejected (it must route outward communication through its
//! supervisor).

use axum::Json;
use axum::extract::{Path, State};
use kallip_common::message::DeliveryResponse;
use kallip_common::protocol::ApiError;
use serde::Deserialize;

use crate::relay::RelayMessageError;
use crate::state::SharedState;
use kallip_common::agentid::AgentId;

#[derive(Debug, Deserialize)]
pub(super) struct LescheMessageRequest {
    pub text: String,
}

/// Deliver a message to the user. Self-only AND root-only — only the root
/// agent itself (authenticated by its own per-agent token) may deliver a
/// user-facing message. The operator is deliberately **not** authorized here: a
/// message is something the end user attributes to the agent, so letting the
/// operator post one would forge the agent's voice. (An operator announcement,
/// if ever needed, is a separate route with its own sender identity, not this
/// one.) Subagents are rejected too: the conversation with the user is owned by
/// the root, and a subagent must route outward communication through its
/// supervisor.
pub async fn post_message(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Json(req): Json<LescheMessageRequest>,
) -> Result<Json<DeliveryResponse>, ApiError> {
    {
        let registry = state.registry.read().await;
        registry.require_self(auth.identity(), &id)?;
        // Root-only: the relay emits a single agent-free `AssistantContent`
        // envelope over the tagma conversation, and that conversation is owned
        // by the root. A subagent has no attributed voice to the user.
        let is_root = registry
            .root_agent()
            .is_some_and(|(root_id, _)| root_id == &id);
        if !is_root {
            return Err(ApiError::forbidden(
                "delivering messages to the user requires the root agent; \
                 subagents must route outward communication through their supervisor",
            ));
        }
    }
    let relay = {
        let slot = state.relay.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref()
            .map(|(handle, _)| handle.clone())
            .ok_or_else(|| {
                ApiError::unavailable(
                    "relay not active (no KALLIP_TAGMA_RELAY_AGORA_URL configured)",
                )
            })?
    };
    match relay.emit_message(req.text).await {
        Ok(()) => Ok(Json(DeliveryResponse {
            ok: true,
            error: None,
        })),
        Err(RelayMessageError::BurstExceeded) => {
            Err(ApiError::too_many_requests("message burst cap exceeded"))
        }
        Err(RelayMessageError::Delivery(e)) => {
            Err(ApiError::bad_gateway(format!("delivery failed: {e:#}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthIdentity, Identity};
    use crate::state::RegistryEntry;
    use crate::test_helpers::{make_entry, make_state};
    use axum::Json;
    use axum::extract::{Path, State};
    use kallip_common::agentid::AgentId;

    /// With no relay active (pure-local tagma, no `KALLIP_TAGMA_RELAY_AGORA_URL`) the
    /// route returns 503 unavailable rather than touching the relay. Authed as
    /// the agent itself (self-only) so the 403 check does not short-circuit.
    #[tokio::test]
    async fn message_unavailable_when_no_relay() {
        let state = make_state();
        let id = AgentId::random();
        let entry = make_entry(None, "tok".to_string());
        state
            .registry
            .write()
            .await
            .register(id.clone(), RegistryEntry::Live(entry));

        let err = post_message(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: id.clone() }),
            Path(id),
            Json(LescheMessageRequest { text: "hi".into() }),
        )
        .await
        .expect_err("no relay -> unavailable");
        assert_eq!(
            err.status, 503,
            "expected 503 unavailable, got {}",
            err.status
        );
    }

    /// The operator may NOT send as an agent — a message is something the end
    /// user attributes to the agent, so an operator posting one would forge the
    /// agent's voice. This is the deliberate narrowing from
    /// `require_self_or_operator` (used by non-impersonating self-writes) to
    /// `require_self`.
    #[tokio::test]
    async fn message_forbidden_for_operator() {
        let state = make_state();
        let id = AgentId::random();
        let entry = make_entry(None, "tok".to_string());
        state
            .registry
            .write()
            .await
            .register(id.clone(), RegistryEntry::Live(entry));

        let err = post_message(
            State(state),
            AuthIdentity::test_new(Identity::Operator),
            Path(id),
            Json(LescheMessageRequest { text: "hi".into() }),
        )
        .await
        .expect_err("operator -> forbidden");
        assert_eq!(
            err.status, 403,
            "expected 403 forbidden, got {}",
            err.status
        );
    }

    /// A peer agent may not send for another agent either (self-only).
    #[tokio::test]
    async fn message_forbidden_for_other_agent() {
        let state = make_state();
        let a = AgentId::random();
        let b = AgentId::random();
        let entry_a = make_entry(None, "a".to_string());
        let entry_b = make_entry(Some(a.clone()), "b".to_string());
        {
            let mut registry = state.registry.write().await;
            registry.register(a.clone(), RegistryEntry::Live(entry_a));
            registry.register(b.clone(), RegistryEntry::Live(entry_b));
        }

        let err = post_message(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: a }),
            Path(b),
            Json(LescheMessageRequest { text: "hi".into() }),
        )
        .await
        .expect_err("peer agent -> forbidden");
        assert_eq!(
            err.status, 403,
            "expected 403 forbidden, got {}",
            err.status
        );
    }

    /// A subagent may not deliver a user-facing message even when posting as
    /// itself (self-only passes) — the conversation with the user is owned by
    /// the root, so a subagent must route outward communication through its
    /// supervisor.
    #[tokio::test]
    async fn message_forbidden_for_subagent() {
        let state = make_state();
        let root = AgentId::random();
        let sub = AgentId::random();
        {
            let mut registry = state.registry.write().await;
            registry.register(
                root.clone(),
                RegistryEntry::Live(make_entry(None, "root".into())),
            );
            registry.register(
                sub.clone(),
                RegistryEntry::Live(make_entry(Some(root), "sub".into())),
            );
        }

        let err = post_message(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: sub.clone() }),
            Path(sub),
            Json(LescheMessageRequest { text: "hi".into() }),
        )
        .await
        .expect_err("subagent -> forbidden");
        assert_eq!(
            err.status, 403,
            "expected 403 forbidden, got {}",
            err.status
        );
    }
}
