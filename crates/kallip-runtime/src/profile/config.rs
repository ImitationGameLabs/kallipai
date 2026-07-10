//! Profile configuration: a TOML file (multi-tier) or an implicit single profile from
//! `KALLIP_LLM_*` env (the no-config-file path).
//!
//! Progressive disclosure — Harbor / `kallip-run` set only env vars and ship no config
//! file, so they get the implicit single profile with zero overhead. A `profiles.toml`
//! unlocks multi-tier / multi-profile failover. Both paths carry a declared `max_context_window`
//! (the implicit profile derives it from `KALLIP_CONTEXT_WINDOW_TOKENS`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use just_llm_client::family;
use serde::Deserialize;

use super::model::{Endpoint, Profile, Tier};

/// Env override for the profiles config file path.
pub(crate) const PROFILES_FILE_ENV: &str = "KALLIP_PROFILES_FILE";

/// Parsed + validated profile configuration: the data the daemon assembles into a
/// [`super::registry::ProfileRegistry`] after building backends. Pure data — no reqwest, no
/// backends. The daemon owns construction (see `kallip_runtime::profile`).
#[derive(Debug)]
pub struct ProfileConfig {
    /// Ordered capability tiers (selection reads `tiers[depth]`).
    pub tiers: Vec<Tier>,
    /// Named provider instances keyed by [`Endpoint::id`].
    pub endpoints: HashMap<String, Endpoint>,
}

/// Load profile configuration: from `KALLIP_PROFILES_FILE` (or a default path) if present,
/// else an implicit single profile built from `KALLIP_LLM_*` env.
pub fn load() -> Result<ProfileConfig> {
    match resolve_config_path()? {
        Some(path) => load_file(&path),
        None => from_env(),
    }
}

/// Build the implicit single-profile registry from `KALLIP_LLM_*` env (the env path).
///
/// The profile's `max_context_window` is derived from `KALLIP_CONTEXT_WINDOW_TOKENS`
/// (default `128_000`), so the env path and the config-file path both carry an authoritative
/// window installed via `set_context_window` at spawn.
pub fn from_env() -> Result<ProfileConfig> {
    let provider = env_str("KALLIP_LLM_PROVIDER")?;
    let model = env_str("KALLIP_LLM_MODEL")?;
    let (family_id, api_key, base_url) = match provider.as_str() {
        family::DEEPSEEK => {
            let key = env_str("KALLIP_LLM_DEEPSEEK_API_KEY")?;
            let base = std::env::var("KALLIP_LLM_DEEPSEEK_BASE_URL").ok();
            (family::DEEPSEEK, key, base)
        }
        family::OPENAI_COMPATIBLE => {
            let key = env_str("KALLIP_LLM_OPENAI_COMPAT_API_KEY")?;
            let base = std::env::var("KALLIP_LLM_OPENAI_COMPAT_BASE_URL").ok();
            (family::OPENAI_COMPATIBLE, key, base)
        }
        other => bail!("unsupported KALLIP_LLM_PROVIDER: {other}"),
    };
    let endpoint = Endpoint {
        id: provider.clone(),
        family: family_id.into(),
        api_key,
        base_url,
    };
    // The implicit profile's window comes from the same env var `AgentConfig::load` uses as its
    // budget-shape validation anchor — single source, no drift under static daemon env.
    let max_context_window = crate::env_util::parse_env::<usize>("KALLIP_CONTEXT_WINDOW_TOKENS")?
        .unwrap_or(crate::env_util::DEFAULT_CONTEXT_WINDOW_TOKENS);
    let profile = Profile {
        id: format!("{provider}/{model}"),
        endpoint: provider.clone(),
        model,
        max_context_window,
    };
    let mut endpoints = HashMap::new();
    endpoints.insert(provider, endpoint);
    Ok(ProfileConfig {
        tiers: vec![Tier {
            profiles: vec![profile],
        }],
        endpoints,
    })
}

fn load_file(path: &Path) -> Result<ProfileConfig> {
    check_file_mode(path);
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read profiles config {}", path.display()))?;
    let file: ConfigFile = toml::from_str(&raw)
        .with_context(|| format!("failed to parse profiles config {}", path.display()))?;
    validate(&file)?;

    let endpoints: HashMap<String, Endpoint> = file
        .endpoints
        .into_iter()
        .map(|(id, body)| {
            let api_key = expand_vars(&body.api_key)?;
            // Re-check post-expansion: `${VAR}` that resolves to empty must not slip through
            // (the pre-expansion check in `validate` only sees the literal).
            if api_key.trim().is_empty() {
                bail!("endpoint '{id}': api_key is required");
            }
            let endpoint = Endpoint {
                id: id.clone(),
                family: body.family,
                api_key,
                base_url: body.base_url.map(|s| expand_vars(&s)).transpose()?,
            };
            Ok::<_, anyhow::Error>((id, endpoint))
        })
        .collect::<Result<_>>()?;

    let tiers = file
        .tiers
        .into_iter()
        .map(|t| Tier {
            profiles: t.profiles.into_iter().map(Profile::from).collect(),
        })
        .collect();

    Ok(ProfileConfig { tiers, endpoints })
}

/// Resolve the config file path: explicit env, else a default under `$XDG_CONFIG_HOME`.
fn resolve_config_path() -> Result<Option<PathBuf>> {
    if let Some(p) = std::env::var_os(PROFILES_FILE_ENV) {
        let path = PathBuf::from(p);
        return Ok(if path.exists() { Some(path) } else { None });
    }
    let Some(dir) = config_dir() else {
        return Ok(None);
    };
    let path = dir.join("kallip").join("profiles.toml");
    Ok(if path.exists() { Some(path) } else { None })
}

fn config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

/// The directory holding `profiles.toml` (and thus potentially API keys) — the
/// path a sandbox hide-hole should overlay so a broad-read agent cannot read
/// credentials. Mirrors the loader's path resolution: honors
/// `KALLIP_PROFILES_FILE` (hides its parent, covering custom locations) else
/// the default `<config_dir>/kallip`. Returns `None` only when neither
/// `XDG_CONFIG_HOME` nor `HOME` is set.
pub fn profiles_config_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(PROFILES_FILE_ENV) {
        // Hide the directory containing the explicit file (covers custom locations
        // a Guest could otherwise `cat`).
        return PathBuf::from(p).parent().map(Path::to_path_buf);
    }
    config_dir().map(|d| d.join("kallip"))
}

/// Warn (non-fatal) if the config file is readable by group/other — it holds API keys.
fn check_file_mode(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode();
            if mode & 0o077 != 0 {
                tracing::warn!(
                    path = %path.display(),
                    mode = format!("{mode:o}"),
                    "profiles config is group/other-accessible but may contain API keys; \
                     recommend chmod 600"
                );
            }
        }
    }
}

/// Expand `${VAR}` references against the process environment.
///
/// Intentionally minimal: literal `${VAR}` substitution only, applied to operator-controlled config
/// values (`api_key`, `base_url` in `profiles.toml`) — no `$$` escaping, no default values, no
/// nested/recursive expansion, and an unset or unterminated `${` is a hard error (config validation,
/// not silent substitution). There is no injection surface: the inputs are operator config, never
/// agent/LLM/user content. Pulling a full templating crate would trade this simple, fail-loud
/// contract for incidental features.
fn expand_vars(s: &str) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find("${") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find('}') else {
            bail!("unterminated ${{ in config value: {s:?}");
        };
        let name = &after[..close];
        let val = std::env::var(name)
            .with_context(|| format!("config references unset env var ${{{name}}}"))?;
        out.push_str(&val);
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn env_str(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} must be set"))
}

/// Validate the parsed file: non-empty `api_key`, unique profile ids. Profile→endpoint
/// references and backend coverage are validated when the daemon constructs `ProfileRegistry`.
fn validate(file: &ConfigFile) -> Result<()> {
    for (id, body) in &file.endpoints {
        if body.api_key.trim().is_empty() {
            bail!("endpoint '{id}': api_key is required");
        }
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for tier in &file.tiers {
        for p in &tier.profiles {
            if !seen.insert(p.id.as_str()) {
                bail!("duplicate profile id '{}'", p.id);
            }
        }
    }
    Ok(())
}

// --- serde-facing types (TOML schema) ---

#[derive(Deserialize)]
struct ConfigFile {
    #[serde(default)]
    endpoints: HashMap<String, EndpointBody>,
    #[serde(default)]
    tiers: Vec<TierBody>,
}

#[derive(Deserialize)]
struct EndpointBody {
    family: String,
    api_key: String,
    #[serde(default)]
    base_url: Option<String>,
}

#[derive(Deserialize)]
struct TierBody {
    profiles: Vec<ProfileBody>,
}

#[derive(Deserialize)]
struct ProfileBody {
    id: String,
    endpoint: String,
    model: String,
    max_context_window: usize,
}

impl From<ProfileBody> for Profile {
    fn from(p: ProfileBody) -> Self {
        Profile {
            id: p.id,
            endpoint: p.endpoint,
            model: p.model,
            max_context_window: p.max_context_window,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ds_env() -> [(&'static str, Option<&'static str>); 4] {
        [
            ("KALLIP_LLM_PROVIDER", Some("deepseek")),
            ("KALLIP_LLM_MODEL", Some("deepseek-test")),
            ("KALLIP_LLM_DEEPSEEK_API_KEY", Some("fake")),
            ("KALLIP_CONTEXT_WINDOW_TOKENS", Some("200000")),
        ]
    }

    #[test]
    fn from_env_builds_implicit_single_profile() {
        temp_env::with_vars(ds_env(), || {
            let cfg = from_env().unwrap();
            // Env path yields a single implicit tier.
            let p = &cfg.tiers[0].profiles[0];
            assert_eq!(p.model, "deepseek-test");
            assert_eq!(p.max_context_window, 200_000); // implicit env profile derives the window from the env var
        });
    }

    #[test]
    fn from_env_rejects_unknown_provider() {
        temp_env::with_vars(
            [
                ("KALLIP_LLM_PROVIDER", Some("anthropic")),
                ("KALLIP_LLM_MODEL", Some("m")),
            ],
            || {
                assert!(from_env().is_err());
            },
        );
    }

    #[test]
    fn parse_valid_toml() {
        let toml = r#"
[endpoints.ds]
family = "deepseek"
api_key = "fake"

[[tiers]]

  [[tiers.profiles]]
  id = "pro"
  endpoint = "ds"
  model = "deepseek-pro"
  max_context_window = 500000
"#;
        let file: ConfigFile = toml::from_str(toml).unwrap();
        validate(&file).unwrap();
    }

    #[test]
    fn parse_rejects_duplicate_profile_id() {
        let toml = r#"
[endpoints.ds]
family = "deepseek"
api_key = "fake"
[[tiers]]

  [[tiers.profiles]]
  id = "dup"
  endpoint = "ds"
  model = "m"
  max_context_window = 1000
  [[tiers.profiles]]
  id = "dup"
  endpoint = "ds"
  model = "m2"
  max_context_window = 1000
"#;
        let file: ConfigFile = toml::from_str(toml).unwrap();
        assert!(validate(&file).is_err());
    }

    #[test]
    fn expand_vars_substitutes_env() {
        temp_env::with_vars([("MY_SECRET", Some("shhh"))], || {
            assert_eq!(
                expand_vars("prefix-${MY_SECRET}-suffix").unwrap(),
                "prefix-shhh-suffix"
            );
        });
    }

    #[test]
    fn expand_vars_errors_on_unset() {
        temp_env::with_vars_unset(["DEFINITELY_UNSET_VAR_X9Q"], || {
            assert!(expand_vars("${DEFINITELY_UNSET_VAR_X9Q}").is_err());
        });
    }
}
