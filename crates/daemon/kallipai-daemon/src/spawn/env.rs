//! Environment assembly: the launch environment, the address check, the
//! relay defaults and the user environment validation.

use super::authorize::RESERVED_KEYS;
use super::error::SpawnError;
use super::identity::ResolvedUser;
use std::collections::BTreeMap;
use std::path::Path;

/// Compose the instance env from a map, one value per key:
///
/// * the base (harvest, or fallback PATH) supplies every key it has;
/// * explicit request pairs win per key over the base;
/// * the daemon-owned keys win over everything — a polluted profile
///   cannot smuggle them in;
/// * `RUST_LOG=info` appears only when neither base nor explicit pair
///   supplied one (map composition ends duplicate-key shadowing);
/// * `KALLIPAI_TAGMA_ADDR` defaults to `127.0.0.1:0` when neither base nor
///   explicit pair supplies one — the one user-set listen knob;
/// * the data-dir handoff and the state anchor land last — daemon-owned
///   like the slug, they pin where the instance's data root and logs
///   resolve. The dedicated form additionally pins the standard identity
///   set (HOME, USER, LOGNAME, SHELL from the passwd entry, and the XDG
///   config/data/state/cache tree, plus the runtime dir when it exists)
///   to the target user: daemon-owned like the rest, so neither a
///   polluted profile nor a request pair can describe a foreign home
///   into the instance.
pub(super) fn compose_launch_env(
    base: &[(String, String)],
    user_env: &[String],
    slug: &str,
    workspace_canon: &Path,
    data_dir: &Path,
    state_home: Option<&Path>,
    target: Option<&ResolvedUser>,
) -> Vec<String> {
    let mut env: BTreeMap<&str, String> = BTreeMap::new();
    for (key, value) in base {
        env.insert(key, value.clone());
    }
    for pair in user_env {
        // Shape, emptiness and key ownership are validated before launch
        // (spawn validates the request; start re-validates the persisted
        // copy) — a pair without `=` here would be a bug elsewhere.
        if let Some((key, value)) = pair.split_once('=') {
            env.insert(key, value.to_owned());
        }
    }
    env.insert("KALLIPAI_TAGMA_SLUG", slug.to_owned());
    env.insert(
        "KALLIPAI_WORKSPACE_ROOT",
        workspace_canon.display().to_string(),
    );
    env.entry("KALLIPAI_TAGMA_ADDR")
        .or_insert_with(|| "127.0.0.1:0".to_owned());
    env.insert("KALLIPAI_TAGMA_DATA_DIR", data_dir.display().to_string());
    if let Some(state) = state_home {
        env.insert("XDG_STATE_HOME", state.display().to_string());
    }
    if let Some(user) = target {
        env.insert("HOME", user.home.display().to_string());
        env.insert("USER", user.username.clone());
        env.insert("LOGNAME", user.username.clone());
        if !user.shell.is_empty() {
            env.insert("SHELL", user.shell.clone());
        }
        env.insert(
            "XDG_CONFIG_HOME",
            user.home.join(".config").display().to_string(),
        );
        env.insert(
            "XDG_DATA_HOME",
            user.home.join(".local/share").display().to_string(),
        );
        env.insert(
            "XDG_STATE_HOME",
            user.home.join(".local/state").display().to_string(),
        );
        env.insert(
            "XDG_CACHE_HOME",
            user.home.join(".cache").display().to_string(),
        );
        // linger (declared in the system deployment) is what guarantees
        // this directory; a dev host without it just omits the key —
        // the instance binds TCP and never reads XDG_RUNTIME_DIR.
        let runtime = Path::new("/run/user").join(user.uid.to_string());
        if runtime.is_dir() {
            env.insert("XDG_RUNTIME_DIR", runtime.display().to_string());
        }
    }
    env.entry("RUST_LOG").or_insert_with(|| "info".to_owned());
    env.into_iter().map(|(k, v)| format!("{k}={v}")).collect()
}

/// The addr key is user-set, and the base (harvest) channel can carry it
/// past request validation (`validate_user_env` never sees the base): the
/// composed launch env is the last choke point, so a malformed pin from
/// any channel dies here instead of at tagma bind time.
pub(super) fn ensure_addr_parses(env: &[String]) -> Result<(), SpawnError> {
    let Some(pair) = env.iter().find(|p| p.starts_with("KALLIPAI_TAGMA_ADDR=")) else {
        return Ok(());
    };
    let value = pair.split_once('=').map(|(_, v)| v).unwrap_or_default();
    if value.parse::<std::net::SocketAddr>().is_err() {
        return Err(SpawnError::Invalid(format!(
            "env key \"KALLIPAI_TAGMA_ADDR\" must be a SocketAddr (got {value:?})"
        )));
    }
    Ok(())
}

/// One path contains the other (ancestor/descendant), including equality.
/// Non-canonical `b` still compares correctly when it is a prefix/suffix
/// match of the canonical `a` only in pathological trees; existing
/// workspaces were canonicalized when written.
/// The daemon-side relay-URL defaults (read once per spawn from this
/// process's environment; the unit env carries them). An unset -- or
/// empty, aligned with the kallipai-adk's persistence.rs
/// `filter(!is_empty)` precedent
/// so a stray empty value cannot silently switch the fill off -- value
/// counts as unconfigured.
pub(super) fn daemon_env_value(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Fill the relay-origin entry for a relay-intent request env.
///
/// Fires only when the env signals relay intent (a
/// `KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE` or an explicit `KALLIPAI_POLIS_URL`
/// entry): the origin key survives exactly once, with its explicit value
/// or -- when absent and the daemon has one configured -- the default. An
/// origin the daemon has not configured is left untouched (an explicit
/// empty entry then dies in `validate_user_env`, the same fail-loud end
/// an empty default reached before the fill moved here); no relay
/// signal at all returns the env unchanged -- local-only spawns must not
/// carry an origin that would flip them online.
pub(super) fn fill_relay_defaults(env: &[String], polis: Option<&str>) -> Vec<String> {
    let mut env = env.to_vec();
    if !env
        .iter()
        .any(|e| e.starts_with("KALLIPAI_TAGMA_RELAY_") || e.starts_with("KALLIPAI_POLIS_URL="))
    {
        return env;
    }
    fill_one(&mut env, "KALLIPAI_POLIS_URL=", polis);
    env
}

/// One entry per key survives: the first explicit value wins
/// outright, an empty or absent key is filled with the configured
/// default (None: left as-is), and every duplicate is dropped.
/// Ordering must stay irrelevant downstream -- the spawn helper passes
/// duplicate pairs straight to execve, and the daemon's env validation
/// rejects empty values -- so collapsing here is the one place that
/// keeps both properties airtight.
fn fill_one(env: &mut Vec<String>, prefix: &str, default: Option<&str>) {
    let explicit = env
        .iter()
        .any(|e| e.strip_prefix(prefix).is_some_and(|v| !v.is_empty()));
    let mut kept = false;
    env.retain(|e| {
        let Some(v) = e.strip_prefix(prefix) else {
            return true;
        };
        if kept || (explicit && v.is_empty()) {
            return false;
        }
        kept = true;
        true
    });
    if explicit {
        return;
    }
    let Some(default) = default else {
        return;
    };
    match env.iter_mut().find(|e| e.starts_with(prefix)) {
        Some(slot) => *slot = format!("{prefix}{default}"),
        None => env.push(format!("{prefix}{default}")),
    }
}

/// Request env pairs: KEY=VALUE shape, KALLIPAI_* (plus RUST_LOG and PATH —
/// both normally arrive via the login harvest; an explicit pair wins),
/// none of the daemon-owned keys. Shared by spawn (fresh request
/// env) and start (re-validating the persisted copy against hand-edited
/// records).
pub(crate) fn validate_user_env(user_env: &[String]) -> Result<(), SpawnError> {
    for pair in user_env {
        let Some((key, value)) = pair.split_once('=') else {
            return Err(SpawnError::Invalid(format!(
                "env arg {pair:?} is not KEY=VALUE"
            )));
        };
        if RESERVED_KEYS.contains(&key) {
            return Err(SpawnError::Invalid(format!(
                "env key {key} is set by the daemon and cannot be overridden"
            )));
        }
        if !(key.starts_with("KALLIPAI_") || key == "RUST_LOG" || key == "PATH") {
            return Err(SpawnError::Invalid(format!(
                "env key {key:?} is not allowlisted (KALLIPAI_*, RUST_LOG, or PATH)"
            )));
        }
        // The addr key is user-set: shape-check it here
        // so a bad value dies at request time, not at tagma bind time.
        if key == "KALLIPAI_TAGMA_ADDR" && value.parse::<std::net::SocketAddr>().is_err() {
            return Err(SpawnError::Invalid(format!(
                "env key {key:?} must be a SocketAddr (got {value:?})"
            )));
        }
        if value.is_empty() {
            return Err(SpawnError::Invalid(format!(
                "env arg {pair:?} has an empty value"
            )));
        }
    }
    Ok(())
}
