//! Applying the registry to live agents: the broadcast, the degraded
//! re-pull and its task spawner.

use super::shared::{signal_profile_reset, validate_config};
use crate::auth::AuthIdentity;
use crate::profile_source::ProfileSourceKind;
use crate::state::{ProfileBundle, SharedState};
use axum::Json;
use axum::extract::State;
use kallipai_adk::profile::ProfileConfig;
use kallipai_common::protocol::ApiError;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// Response body for POST /profiles/apply.
#[derive(Serialize)]
pub struct ApplyResponse {
    /// Number of live agents that received a pending-reset signal.
    pub applied: usize,
    /// Number of agents skipped: faulted entries and live agents whose
    /// recorded set no longer resolves.
    pub skipped: usize,
}

/// POST /profiles/apply — push the current registry to all live agents.
///
/// For each live agent, resolves its recorded set name against the new
/// registry and writes a [`ProfileReset`](kallipai_adk::ProfileReset) to the agent's pending-reset
/// cell. An unbound root instead derives its binding from the current
/// default set, and the derived name is written back to the in-memory
/// record (not meta.json — restore re-derives it on the next boot).
/// The agent picks it up on its next wake-up (top of
/// `run_and_report`). Agents mid-round finish their current work first.
pub async fn apply_profiles(
    State(state): State<SharedState>,
    auth: AuthIdentity,
) -> Result<Json<ApplyResponse>, ApiError> {
    crate::auth::require_operator(auth.identity())?;

    let (applied, skipped) = broadcast_current_bundle(&state).await;
    info!(applied, skipped, "profile apply signaled to live agents");
    Ok(Json(ApplyResponse { applied, skipped }))
}

/// Push the current profile bundle to every live agent: resolve each
/// recorded set through the bundle's default-set fallback, write the
/// in-memory rebinds, and signal a pending reset. The shared tail of
/// POST /profiles/apply and the degraded-repull recovery (a repull that
/// lands a fresh bundle broadcasts it, so bound agents re-resolve
/// without a restart).
pub(super) async fn broadcast_current_bundle(state: &SharedState) -> (usize, usize) {
    let bundle = state.profiles.load();
    let registry = bundle.registry.clone();

    // Collect the reset targets under the read lock, then apply outside it
    // so register/remove writes are not blocked during cell writes + notify.
    let (targets, rebinds, non_live): (Vec<_>, Vec<_>, usize) = {
        let registry_guard = state.registry.read().await;
        registry_guard.iter().fold(
            (Vec::new(), Vec::new(), 0usize),
            |(mut targets, mut rebinds, mut skipped), (id, entry)| {
                let Some(live) = entry.as_live() else {
                    skipped += 1;
                    return (targets, rebinds, skipped);
                };
                // Resolve through the bundle's default-set fallback (the one
                // resolution rule for every consumer): an unbound record
                // resolves to the default and is rebound in memory; a
                // dangling root binding resolves through the fallback
                // without rebinding (its recorded name is kept). A still-
                // dangling binding is skipped, not guessed at. The rebind
                // never writes meta.json (restore re-derives it on the
                // next boot).
                let recorded = live.identity.config.profile_set.clone();
                let Ok(set) = bundle
                    .resolve_with_fallback(recorded.as_deref(), live.identity.config.is_root())
                else {
                    skipped += 1;
                    return (targets, rebinds, skipped);
                };
                if recorded.is_none() {
                    rebinds.push((id.clone(), Some(set.name.clone())));
                }
                targets.push((
                    kallipai_adk::ProfileReset {
                        set: set.clone(),
                        registry: registry.clone(),
                    },
                    live.agent.pending_profile_reset.clone(),
                    live.agent.notify.clone(),
                ));
                (targets, rebinds, skipped)
            },
        )
    };
    let skipped = non_live;

    // Write the derived root bindings back to the in-memory records so
    // the delivery gate sees them without a restart.
    if !rebinds.is_empty() {
        let mut registry_guard = state.registry.write().await;
        for (id, name) in rebinds {
            if let Some(entry) = registry_guard.get_mut(&id)
                && let Some(live) = entry.as_live_mut()
            {
                live.identity.config.profile_set = name;
            }
        }
    }

    let mut applied = 0usize;
    for (reset, cell_lock, notify) in targets {
        signal_profile_reset(reset, &cell_lock, &notify);
        applied += 1;
    }
    (applied, skipped)
}

// -- degraded-boot repull ------------------------------------------------

/// First retry delay after a degraded boot, doubling per failure.
const REPULL_RETRY_BASE: Duration = Duration::from_secs(30);
/// Backoff ceiling: a down gateway is retried at most every 5 minutes.
const REPULL_RETRY_MAX: Duration = Duration::from_secs(5 * 60);

/// One round of the degraded-repull task, shared by the task loop and
/// the tests. Reads the live source through the slot, so a runtime
/// switch between rounds redirects the next attempt.
#[derive(Debug)]
pub(super) enum RepullOutcome {
    /// The live source is not the gateway: nothing to repull.
    NotProxy,
    /// The gateway source carries no degraded flag: nothing to do.
    Healthy,
    /// The enrollment token is confirmed dead: retrying cannot help
    /// (the token is read once at boot); recovery is re-enroll plus
    /// restart. The task idles at the backoff ceiling.
    Poisoned,
    /// The fetch still fails, or the result was dropped because a
    /// switch replaced the source mid-fetch; retry later.
    Retrying(String),
    /// A fresh config landed: the bundle was rebuilt, stored, and
    /// broadcast to live agents.
    Recovered { applied: usize, skipped: usize },
}

/// Value-level config equality. ProfileConfig derives no PartialEq; the
/// repull gate only asks "is the live bundle the source's current
/// snapshot", so a JSON round-trip comparison is exact and cheap at the
/// task's cadence.
fn config_matches(live: &ProfileConfig, snapshot: &ProfileConfig) -> bool {
    serde_json::to_value(live).ok() == serde_json::to_value(snapshot).ok()
}

/// One repull attempt against the live gateway source. The slot is
/// loaded per attempt, and a source swapped mid-fetch means the result
/// belongs to a source no longer live — it is dropped instead of
/// stored (the switch that replaced it owns the state from there).
pub(super) async fn repull_once(state: &SharedState) -> RepullOutcome {
    let source = state.profile_source.load_full().source();
    if source.kind() != ProfileSourceKind::Proxy {
        return RepullOutcome::NotProxy;
    }
    let health = source.health().await;
    if health.poisoned {
        return RepullOutcome::Poisoned;
    }
    // The gate stays armed until the live bundle reflects the source's
    // latest known-good snapshot. The degraded flag alone is not
    // enough: a fetch that succeeds with a config that cannot build
    // (an unknown family, say) clears the flag while the bundle
    // stays stale — without the snapshot comparison the task would
    // idle on an empty bundle after one retry.
    let snapshot = source.snapshot().await;
    if let Some(snapshot) = snapshot.as_ref() {
        if config_matches(&state.profiles.load().config, snapshot) {
            return RepullOutcome::Healthy;
        }
    } else if !health.degraded {
        return RepullOutcome::Healthy;
    }
    match source.load().await {
        Ok(config) => {
            let current = state.profile_source.load_full().source();
            if !Arc::ptr_eq(&current, &source) {
                return RepullOutcome::Retrying(
                    "a source switch replaced the gateway source mid-fetch; result dropped"
                        .to_owned(),
                );
            }
            let registry = match validate_config(&config) {
                Ok(registry) => registry,
                Err(error) => {
                    return RepullOutcome::Retrying(format!(
                        "the refreshed gateway config does not build: {}",
                        error.message
                    ));
                }
            };
            state
                .profiles
                .store(Arc::new(ProfileBundle { config, registry }));
            let (applied, skipped) = broadcast_current_bundle(state).await;
            RepullOutcome::Recovered { applied, skipped }
        }
        Err(error) => RepullOutcome::Retrying(format!("{error:#}")),
    }
}

/// The boot task for a degraded model-gateway source: while the live
/// source is the gateway and its health flags `degraded` (the boot
/// fetched nothing), retry the fetch on a doubling backoff; the first
/// success rebuilds the profile bundle and broadcasts the reset signal
/// to live agents, so a gateway that comes back is picked up without a
/// restart. A poisoned source stops retrying (a dead enrollment token
/// cannot recover in-process); a later switch re-arms the loop through
/// the per-round slot load.
pub(crate) fn spawn_degraded_repull_task(state: SharedState) {
    tokio::spawn(async move {
        let mut backoff = REPULL_RETRY_BASE;
        let mut poison_noted = false;
        loop {
            tokio::time::sleep(backoff).await;
            match repull_once(&state).await {
                RepullOutcome::NotProxy | RepullOutcome::Healthy => {
                    backoff = REPULL_RETRY_BASE;
                    poison_noted = false;
                }
                RepullOutcome::Poisoned => {
                    if !poison_noted {
                        warn!(
                            "degraded repull paused: the gateway enrollment token is dead; re-enroll the tagma on the platform and restart"
                        );
                        poison_noted = true;
                    }
                    backoff = REPULL_RETRY_MAX;
                }
                RepullOutcome::Retrying(reason) => {
                    backoff = (backoff * 2).min(REPULL_RETRY_MAX);
                    warn!(
                        reason,
                        backoff_secs = backoff.as_secs(),
                        "degraded repull failed; retrying"
                    );
                }
                RepullOutcome::Recovered { applied, skipped } => {
                    info!(
                        applied,
                        skipped, "degraded repull recovered: profile bundle rebuilt and broadcast"
                    );
                    backoff = REPULL_RETRY_BASE;
                    poison_noted = false;
                }
            }
        }
    });
}
