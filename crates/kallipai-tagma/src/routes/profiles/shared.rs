//! Config validation and persistence, plus the shared dangling-binding
//! scan and the profile-reset signal.

use crate::state::SharedState;
use kallipai_adk::profile::{ProfileConfig, ProfileRegistry};
use kallipai_common::protocol::ApiError;
use std::sync::Arc;
use tracing::{info, warn};

/// Refuse memberless sets, then build backends and a trial registry for the
/// candidate config; nothing changes on failure. Shared by every set-
/// management mutation. The refusal is this face's call: the user is
/// present, so the actionable 400 beats a silent partial boot — while the
/// boot path degrades instead (skips serving the set; agents bound to it
/// fail at resolution, see `kallipai_adk::profile::ProfileRegistry`).
pub(super) fn validate_config(config: &ProfileConfig) -> Result<Arc<ProfileRegistry>, ApiError> {
    for (name, set) in &config.sets {
        if set.profiles.is_empty() {
            return Err(ApiError::bad_request(format!(
                "set '{name}' has no profiles; remove the set or add a member",
            )));
        }
    }
    let factory = just_llm_client::client::BackendFactory::new();
    let user_agent = crate::backend::DEFAULT_USER_AGENT;
    let source = crate::backend::build_backends(config, factory, user_agent)
        .map_err(|e| ApiError::bad_request(format!("profile validation failed: {e:#}")))?;
    Ok(Arc::new(
        ProfileRegistry::new(config.sets.clone(), source)
            .map_err(|e| ApiError::bad_request(format!("invalid profile registry: {e:#}")))?,
    ))
}

/// Persist the config to disk (best-effort: a failure is logged but does not
/// block the swap, since the in-memory state is already validated). Shared
/// by every set-management mutation.
pub(super) fn persist_config(config: &ProfileConfig) {
    match kallipai_adk::profile::config_path() {
        Ok(path) => {
            if let Err(e) = kallipai_adk::profile::save(config, &path) {
                warn!(path = %path.display(), "failed to persist profiles to disk: {e:#}");
            } else {
                info!(path = %path.display(), "profiles persisted to disk");
            }
        }
        Err(e) => {
            warn!("cannot resolve profiles config path for persistence: {e:#}");
        }
    }
}

/// Collect the profile-set bindings that the candidate config would strand:
/// each entry renders as `agent-id → 'set'`. Consumed by `put_profiles` to
/// reject (or, with `force`, accept) a wholesale save that drops a set some
/// agent still records.
pub(super) async fn dangling_bindings(state: &SharedState, config: &ProfileConfig) -> Vec<String> {
    let registry = state.registry.read().await;
    registry
        .iter()
        .filter_map(|(id, entry)| {
            let binding = entry.identity().config.profile_set.as_deref()?;
            (!config.sets.contains_key(binding)).then(|| format!("{id} → '{binding}'"))
        })
        .collect()
}

/// Write a ProfileReset into the agent's pending cell and wake it — the
/// shared tail of apply and the explicit per-agent rebind.
pub(crate) fn signal_profile_reset(
    reset: kallipai_adk::ProfileReset,
    cell: &std::sync::Mutex<Option<kallipai_adk::ProfileReset>>,
    notify: &tokio::sync::Notify,
) {
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(reset);
    drop(guard);
    notify.notify_one();
}
