//! Environment assembly: the launch environment, the address check, the
//! relay defaults and the user environment validation.

use super::authorize::RESERVED_KEYS;
use super::error::SpawnError;
use super::identity::ResolvedUser;
use kallipai_daemon_common::roots::RootKind;
use std::collections::BTreeMap;
use std::path::Path;

/// The layers one launch environment is composed from. Every caller
/// (spawn, start, adopt) supplies the same inputs; only the request
/// pairs differ — replayed and overridden on a restart.
pub(crate) struct LaunchEnv<'a> {
    /// The instance slug, inserted as KALLIPAI_TAGMA_SLUG.
    pub(crate) slug: &'a str,
    /// The canonical workspace, inserted as KALLIPAI_WORKSPACE_ROOT.
    pub(crate) workspace_canon: &'a Path,
    /// The caller's request pairs: spawn's validated request, start's
    /// replay plus overrides, adopt's request.
    pub(crate) request: &'a [String],
    /// The root keys this deployment set on the daemon's own environment.
    pub(crate) deployment: &'a [(String, String)],
    /// The identity seed pairs, always inserted.
    pub(crate) seed: &'a [(String, String)],
    /// The harvested login pairs, or the degraded fallback.
    pub(crate) harvest: &'a [(String, String)],
    /// The drop-to target; None launches as the daemon's own user.
    pub(crate) target: Option<&'a ResolvedUser>,
}

/// Compose one launch environment from its layers, in this order:
///
/// 1. the identity seed pairs (HOME, USER, LOGNAME, XDG_RUNTIME_DIR) —
///    always present, so no launch depends on the harvest having run;
/// 2. the harvested login pairs, or the degraded fallback (root keys are
///    dropped: a profile file the target user controls must not name
///    where an instance lands);
/// 3. the caller's request pairs, which win per key;
/// 4. the deployment's root variables, verbatim: the three own keys only
///    for an in-place launch (a unit-level own key must not become every
///    drop-to user's root) and the three XDG names for both forms (the
///    identity pin overrides them on a drop-to launch);
/// 5. the daemon-owned keys: slug, workspace, and the 127.0.0.1:0
///    address default;
/// 6. the drop-to identity pin (HOME, USER, LOGNAME, SHELL and the XDG
///    tree from the passwd entry), last, so no earlier layer can describe
///    a foreign home.
///
/// RUST_LOG and KALLIPAI_TAGMA_ADDR default only when no earlier layer
/// supplied them; KALLIPAI_TAGMA_ADDR stays the one user-set listen knob.
/// The resolver reads these same pairs, so the roots this env names are the
/// roots the daemon creates, records and polls.
pub(crate) fn compose_launch(input: LaunchEnv<'_>) -> Vec<String> {
    let mut env: BTreeMap<&str, String> = BTreeMap::new();
    for (key, value) in input.seed {
        env.insert(key.as_str(), value.clone());
    }
    // A profile file the target user controls must not decide where
    // an instance lands: with the own keys skipped in the deployment
    // layer for a drop-to launch and absent from the identity pin, a
    // harvested root key would resolve verbatim, and the daemon would
    // create that tree (as root) from the target user's own file.
    let harvested = input
        .harvest
        .iter()
        .filter(|(key, _)| !RootKind::names_root(key));
    for (key, value) in harvested {
        env.insert(key.as_str(), value.clone());
    }
    for pair in input.request {
        // Shape, emptiness and key ownership are validated before launch
        // (spawn validates the request; start re-validates the persisted
        // copy) — a pair without an equals sign would be a bug elsewhere.
        if let Some((key, value)) = pair.split_once('=') {
            env.insert(key, value.to_owned());
        }
    }
    for kind in RootKind::all() {
        // The own keys are the in-place escape hatch: a drop-to launch
        // takes only the XDG names from the deployment, because a root
        // the unit names is the daemon's own while the identity pin
        // below names the target user's tree.
        if input.target.is_none()
            && let Some((_, value)) = input
                .deployment
                .iter()
                .find(|(name, _)| name == kind.own_key())
        {
            env.insert(kind.own_key(), value.clone());
        }
        if let Some((_, value)) = input
            .deployment
            .iter()
            .find(|(name, _)| name == kind.xdg_var())
        {
            env.insert(kind.xdg_var(), value.clone());
        }
    }
    env.insert("KALLIPAI_TAGMA_SLUG", input.slug.to_owned());
    env.insert(
        "KALLIPAI_WORKSPACE_ROOT",
        input.workspace_canon.display().to_string(),
    );
    env.entry("KALLIPAI_TAGMA_ADDR")
        .or_insert_with(|| "127.0.0.1:0".to_owned());
    if let Some(user) = input.target {
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

/// The root keys this deployment set on the daemon's own environment,
/// verbatim and non-empty: the three own keys and the three XDG names.
/// They form layer 4 of the composition, so the resolver answers exactly
/// what the instance will answer from the same pairs.
pub(crate) fn deployment_root_vars() -> Vec<(String, String)> {
    let mut vars = Vec::new();
    for kind in RootKind::all() {
        for key in kind.keys() {
            if let Some(value) = daemon_env_value(key) {
                vars.push((key.to_owned(), value));
            }
        }
    }
    vars
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
/// rejects empty values -- so this function collapses the keys it fills,
/// and `fill_tracing_env` collapses the keys the daemon owns. One place
/// per policy: an explicit value wins here, the daemon's own value wins
/// there.
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

// --- tracing handoff ------------------------------------------------------

/// The OTLP endpoint the daemon exports to, when its own environment names
/// one. The three names mirror `kallipai_common::telemetry`, where the tagma
/// reads them; the daemon does not depend on that crate (it would pull the
/// exporter stack into the CLI), so a test reads the defining file and fails
/// when either side drifts.
pub(super) const ENDPOINT_VAR: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
/// The sampler override, forwarded verbatim.
pub(super) const SAMPLER_VAR: &str = "OTEL_TRACES_SAMPLER";
/// The argument the ratio samplers read.
pub(super) const SAMPLER_ARG_VAR: &str = "OTEL_TRACES_SAMPLER_ARG";
/// The SDK's environment detector folds these attributes into the resource.
const RESOURCE_ATTRIBUTES_VAR: &str = "OTEL_RESOURCE_ATTRIBUTES";

/// The values one launch may forward, read once from the daemon's own
/// environment (the NixOS module sets them on the daemon unit).
#[derive(Debug, Clone, Default)]
pub(super) struct TracingSource {
    endpoint: Option<String>,
    sampler: Option<String>,
    sampler_arg: Option<String>,
}

impl TracingSource {
    pub(super) fn from_daemon_env() -> Self {
        Self {
            endpoint: daemon_env_value(ENDPOINT_VAR),
            sampler: daemon_env_value(SAMPLER_VAR),
            sampler_arg: daemon_env_value(SAMPLER_ARG_VAR),
        }
    }

    #[cfg(test)]
    pub(super) fn new(
        endpoint: Option<&str>,
        sampler: Option<&str>,
        sampler_arg: Option<&str>,
    ) -> Self {
        Self {
            endpoint: endpoint.map(str::to_owned),
            sampler: sampler.map(str::to_owned),
            sampler_arg: sampler_arg.map(str::to_owned),
        }
    }
}

/// Hand the trace configuration to one instance.
///
/// `env` is the composed vector this launch will exec with, the harvested
/// login environment and the request pairs already merged, and the call sits
/// before the exec boundary: the endpoint test below has to see what the
/// instance will actually receive. A request cannot name these keys (the
/// allowlist admits `KALLIPAI_*`, `RUST_LOG` and `PATH` only), though the
/// composed login environment can carry an endpoint of its own. Pairs already
/// in `env` under a key the daemon sets are replaced rather than duplicated:
/// `execve` leaves the winner of a duplicate pair to the libc.
///
/// `service.instance.id` is added whenever the environment this launch
/// hands over carries the generic `OTEL_EXPORTER_OTLP_ENDPOINT`, whichever
/// channel supplied it (the per-signal endpoint keys are not consulted): several
/// instances of one build reporting to one collector are exactly the case
/// where the slug has to tell their traces apart, and a login shell that
/// exports the endpoint produces that case too. An instance with no
/// endpoint gets no attribute, because nothing would read it. An
/// attributes value already present is kept and appended to, so an
/// operator's `deployment.environment` survives.
pub(super) fn fill_tracing_env(env: &[String], tracing: &TracingSource, slug: &str) -> Vec<String> {
    let endpoint_prefix = format!("{ENDPOINT_VAR}=");
    let carries_endpoint = |pairs: &[String]| {
        pairs.iter().any(|pair| {
            pair.strip_prefix(&endpoint_prefix)
                .is_some_and(|value| !value.is_empty())
        })
    };
    if tracing.endpoint.is_none()
        && tracing.sampler.is_none()
        && tracing.sampler_arg.is_none()
        && !carries_endpoint(env)
    {
        return env.to_vec();
    }
    // Only the keys this launch carries are replaced: a value the daemon does
    // not set stays whatever the caller passed.
    let owned: Vec<&str> = [
        tracing.endpoint.as_ref().map(|_| ENDPOINT_VAR),
        tracing.sampler.as_ref().map(|_| SAMPLER_VAR),
        tracing.sampler_arg.as_ref().map(|_| SAMPLER_ARG_VAR),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut out: Vec<String> = env
        .iter()
        .filter(|pair| {
            !owned.iter().any(|key| {
                pair.strip_prefix(key)
                    .is_some_and(|rest| rest.starts_with('='))
            })
        })
        .cloned()
        .collect();
    if let Some(endpoint) = &tracing.endpoint {
        out.push(format!("{ENDPOINT_VAR}={endpoint}"));
    }
    if let Some(sampler) = &tracing.sampler {
        out.push(format!("{SAMPLER_VAR}={sampler}"));
    }
    if let Some(sampler_arg) = &tracing.sampler_arg {
        out.push(format!("{SAMPLER_ARG_VAR}={sampler_arg}"));
    }
    if carries_endpoint(&out) {
        let attributes = format!("service.instance.id={slug}");
        let prefix = format!("{RESOURCE_ATTRIBUTES_VAR}=");
        let merged = match env
            .iter()
            .find_map(|pair| pair.strip_prefix(&prefix))
            .filter(|value| !value.is_empty())
        {
            Some(existing) => format!("{existing},{attributes}"),
            None => attributes,
        };
        out.retain(|pair| !pair.starts_with(&prefix));
        out.push(format!("{prefix}{merged}"));
    }
    out
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
