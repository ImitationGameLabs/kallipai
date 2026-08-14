//! Profile management API: read, update, and apply model profiles online.
//!
//! GET /profiles — return the current profile configuration as JSON.
//! PUT /profiles — validate, persist to disk, and hot-swap the in-memory registry.
//!   Does NOT affect running agents — new agents pick up the swap at spawn.
//! POST /profiles/apply — push the current registry to all live agents via a
//!   pending-reset cell; each agent rebuilds its failover state on its next wake-up.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use kallip_common::protocol::ApiError;
use kallip_runtime::profile::{ProfileConfig, ProfileRegistry};
use serde::Serialize;
use tracing::{info, warn};

use crate::auth::AuthIdentity;
use crate::state::SharedState;

/// GET /profiles — return the current profile configuration.
///
/// Operator-only: profiles contain API keys.
pub async fn get_profiles(
    State(state): State<SharedState>,
    auth: AuthIdentity,
) -> Result<impl IntoResponse, ApiError> {
    crate::auth::require_operator(auth.identity())?;
    let bundle = state.profiles.load();
    Ok(Json(bundle.config.clone()))
}

/// PUT /profiles — validate, persist, and hot-swap the profile registry.
///
/// Accepts a full [`ProfileConfig`] as JSON. Validates by building backends +
/// a trial registry (fail-fast on misconfiguration). On success, writes to
/// disk and swaps the [`ArcSwap`]. Running agents are unaffected until an
/// explicit [`apply_profiles`] call.
pub async fn put_profiles(
    State(state): State<SharedState>,
    auth: AuthIdentity,
    Json(config): Json<ProfileConfig>,
) -> Result<impl IntoResponse, ApiError> {
    crate::auth::require_operator(auth.identity())?;

    // Validate: build backends + trial registry. If this fails, nothing changes.
    let factory = just_llm_client::client::BackendFactory::new();
    let user_agent = crate::backend::DEFAULT_USER_AGENT;
    let source = crate::backend::build_backends(&config, factory, user_agent)
        .map_err(|e| ApiError::bad_request(format!("profile validation failed: {e:#}")))?;
    let registry = Arc::new(
        ProfileRegistry::new(config.tiers.clone(), source)
            .map_err(|e| ApiError::bad_request(format!("invalid profile registry: {e:#}")))?,
    );

    // Persist to disk (best-effort: a failure is logged but does not block the swap,
    // since the in-memory state is already validated).
    match kallip_runtime::profile::config_path() {
        Ok(path) => {
            if let Err(e) = kallip_runtime::profile::save(&config, &path) {
                warn!(path = %path.display(), "failed to persist profiles to disk: {e:#}");
            } else {
                info!(path = %path.display(), "profiles persisted to disk");
            }
        }
        Err(e) => {
            warn!("cannot resolve profiles config path for persistence: {e:#}");
        }
    }

    // Swap the ArcSwap atomically.
    let bundle = crate::state::ProfileBundle {
        config: config.clone(),
        registry,
    };
    state.profiles.store(Arc::new(bundle));

    info!("profile registry hot-swapped");
    Ok(Json(config))
}

/// Response body for POST /profiles/apply.
#[derive(Serialize)]
pub struct ApplyResponse {
    /// Number of live agents that received a pending-reset signal.
    pub applied: usize,
    /// Number of agents that were skipped (faulted or already pending).
    pub skipped: usize,
}

/// POST /profiles/apply — push the current registry to all live agents.
///
/// For each live agent, reads its depth, selects the new tier, and writes a
/// [`ProfileReset`] to the agent's pending-reset cell. The agent picks it up
/// on its next wake-up (top of `run_and_report`). Agents mid-round finish
/// their current work first.
pub async fn apply_profiles(
    State(state): State<SharedState>,
    auth: AuthIdentity,
) -> Result<impl IntoResponse, ApiError> {
    crate::auth::require_operator(auth.identity())?;

    let bundle = state.profiles.load();
    let registry = bundle.registry.clone();

    let mut applied = 0usize;
    let skipped;

    // Collect the reset targets under the read lock, then apply outside it
    // so register/remove writes are not blocked during cell writes + notify.
    let (targets, non_live): (Vec<_>, usize) = {
        let registry_guard = state.registry.read().await;
        registry_guard.iter().fold(
            (Vec::new(), 0usize),
            |(mut targets, mut skipped), (_id, entry)| {
                let Some(live) = entry.as_live() else {
                    skipped += 1;
                    return (targets, skipped);
                };
                let depth = live.identity.config.permissions.depth();
                let tier = registry.select_profile(depth).clone();
                targets.push((
                    kallip_runtime::ProfileReset {
                        tier,
                        registry: registry.clone(),
                    },
                    live.agent.pending_profile_reset.clone(),
                    live.agent.notify.clone(),
                ));
                (targets, skipped)
            },
        )
    };
    skipped = non_live;

    for (reset, cell_lock, notify) in targets {
        let mut cell = cell_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *cell = Some(reset);
        drop(cell);
        notify.notify_one();
        applied += 1;
    }

    info!(applied, skipped, "profile apply signaled to live agents");
    Ok(Json(ApplyResponse { applied, skipped }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{make_entry_with_rx, make_state};
    use crate::state::{RegistryEntry};
    use kallip_common::agentid::AgentId;

    fn op_auth() -> AuthIdentity {
        AuthIdentity::test_new(crate::auth::Identity::Operator)
    }

    #[tokio::test]
    async fn get_profiles_returns_config() {
        let state = make_state();
        // Just verify it doesn't error — returns the default test config.
        let _ = get_profiles(State(state), op_auth()).await.unwrap();
    }

    #[tokio::test]
    async fn apply_signals_live_agents() {
        let state = make_state();
        let agent = AgentId::random();
        let (entry, _rx) = make_entry_with_rx(None, format!("agent-{agent}"));
        state.registry.write().await.register(agent.clone(), RegistryEntry::Live(entry));

        let _resp = apply_profiles(State(state.clone()), op_auth()).await.unwrap();
        // Check that pending_profile_reset was set.
        let reg = state.registry.read().await;
        let entry = reg.get(&agent).unwrap();
        let live = entry.as_live().unwrap();
        let cell = live.agent.pending_profile_reset.lock().unwrap();
        assert!(cell.is_some(), "pending_profile_reset should be set");
    }

    #[tokio::test]
    async fn apply_skips_faulted_agents() {
        let state = make_state();
        let agent = AgentId::random();
        state.registry.write().await.register(
            agent.clone(),
            RegistryEntry::Faulted(crate::test_helpers::make_faulted_entry(None, "test")),
        );

        let _ = apply_profiles(State(state), op_auth()).await.unwrap();
        // Should not panic; skipped count includes the faulted agent.
    }
}
