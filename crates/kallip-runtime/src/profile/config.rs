//! Profile configuration: a TOML file of named profile sets, or an implicit
//! single set from `KALLIP_LLM_*` env (the no-config-file path).
//!
//! Progressive disclosure — Harbor / `kallip-run` set only env vars and ship no config
//! file, so they get the implicit single set with zero overhead. A `profiles.toml`
//! unlocks multiple named sets / multi-profile failover. Both paths carry a declared
//! `max_context_window` (the implicit profile derives it from
//! `KALLIP_CONTEXT_WINDOW_TOKENS`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use just_llm_client::family;
use serde::{Deserialize, Serialize};

use super::model::{Profile, ProfileSet, Provider};
use kallip_common::protocol::Modality;

/// Parsed + validated profile configuration: the data the tagma assembles into a
/// [`super::registry::ProfileRegistry`] after building backends. Pure data — no reqwest, no
/// backends. The tagma owns construction (see `kallip_runtime::profile`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileConfig {
    /// Named profile sets keyed by set name (`[sets.<name>]` in TOML). The map
    /// iterates in sorted-key order, which is also the serialization order —
    /// set order carries no selection meaning.
    pub sets: BTreeMap<String, ProfileSet>,
    /// The default set's name (used for the root agent's model selection).
    /// Empty iff `sets` is empty: the sentinel is a deliberate trade for a
    /// plain `String` on the TOML/JSON face (absent and empty read the
    /// same), with `normalize_default` holding the invariant.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default: String,
    /// Named provider instances keyed by [`Provider::id`].
    pub endpoints: HashMap<String, Provider>,
    /// Profiles parked out of rotation: draft space the runtime never
    /// reads (selection is sets-only), kept so a parked profile survives
    /// without a set. Empty defaults and omitted from the serialized file
    /// when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parking: Vec<Profile>,
}

/// Load profile configuration: from `<config_dir>/profiles.toml` when
/// present, else an implicit single profile built from `KALLIP_LLM_*` env.
pub fn load() -> Result<ProfileConfig> {
    match resolve_config_path()? {
        Some(path) => load_file(&path),
        None => from_env(),
    }
}

/// Build the implicit single-set config from `KALLIP_LLM_*` env (the env path).
/// With no KALLIP_LLM_PROVIDER set, returns an empty config so the tagma
/// boots profile-less; the management page then adds the first profile.
///
/// The implicit set is named `default` and marked default, so the env path and
/// the config-file path present the same shape (a named, defaulted collection)
/// with zero configuration. The profile's `max_context_window` is derived from
/// `KALLIP_CONTEXT_WINDOW_TOKENS` (default `128_000`), so both paths carry an
/// authoritative window installed via `set_context_window` at spawn.
pub fn from_env() -> Result<ProfileConfig> {
    // An unset provider boots the empty registry; a set-but-incomplete
    // provider spec falls through to the hard errors below (fail loud on
    // half-configuration, not a silent empty profile).
    let Ok(provider) = std::env::var("KALLIP_LLM_PROVIDER") else {
        return Ok(ProfileConfig {
            sets: BTreeMap::new(),
            default: String::new(),
            endpoints: HashMap::new(),
            parking: Vec::new(),
        });
    };
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
    let implicit_provider = Provider {
        id: provider.clone(),
        family: family_id.into(),
        api_key,
        base_url,
    };
    // The implicit profile's window comes from the same env var `AgentConfig::load` uses as its
    // budget-shape validation anchor — single source, no drift under static tagma env.
    let max_context_window = crate::env_util::parse_env::<usize>("KALLIP_CONTEXT_WINDOW_TOKENS")?
        .unwrap_or(crate::env_util::DEFAULT_CONTEXT_WINDOW_TOKENS);
    let profile = Profile {
        id: format!("{provider}/{model}"),
        endpoint: provider.clone(),
        model,
        max_context_window,
        // The implicit env profile keeps both behavior fields unset: the client defaults
        // (store on, no effort) are exactly the unconfigured semantics.
        store: None,
        effort: None,
        // The implicit env profile is text-only by default, like every
        // un-annotated profile.
        modalities: Profile::default_modalities(),
    };
    let mut endpoints = HashMap::new();
    endpoints.insert(provider, implicit_provider);
    Ok(ProfileConfig {
        sets: BTreeMap::from([(
            "default".to_string(),
            ProfileSet {
                name: "default".into(),
                description: None,
                profiles: vec![profile],
            },
        )]),
        default: "default".into(),
        endpoints,
        parking: vec![],
    })
}

fn load_file(path: &Path) -> Result<ProfileConfig> {
    check_file_mode(path);
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read profiles config {}", path.display()))?;
    let value: toml::Value = toml::from_str(&raw)
        .with_context(|| format!("failed to parse profiles config {}", path.display()))?;
    // Reject the legacy positional [[tiers]] format explicitly: serde would
    // otherwise silently drop the unknown key and boot an empty registry.
    if value.get("tiers").is_some() {
        bail!(
            "profiles config {} uses the legacy [[tiers]] format, which is no longer supported; rewrite each tier as a named set ([[tiers]] becomes [sets.<name>], [[tiers.profiles]] becomes [[sets.<name>.profiles]]) and add a top-level `default = \"<name>\"` naming the default set",
            path.display()
        );
    }
    let file: ConfigFile = value
        .clone()
        .try_into()
        .with_context(|| format!("failed to parse profiles config {}", path.display()))?;
    validate(&file)?;
    // Declared-capability shrinkage inside a set is never silent: the
    // intersection governs, and members declaring more are warned at load
    // (the CLI and the management UI surface the same fact).
    for (name, effective) in shadowed_set_notices(&file.sets) {
        tracing::warn!(
            set = %name,
            effective = %effective,
            "profile set has members declaring beyond the effective intersection; the intersection governs"
        );
    }

    let endpoints: HashMap<String, Provider> = file
        .endpoints
        .into_iter()
        .map(|(id, body)| {
            let api_key = expand_vars(&body.api_key)?;
            // Re-check post-expansion: `${VAR}` that resolves to empty must not slip through
            // (the pre-expansion check in `validate` only sees the literal).
            if api_key.trim().is_empty() {
                bail!("endpoint '{id}': api_key is required");
            }
            let endpoint = Provider {
                id: id.clone(),
                family: body.family,
                api_key,
                base_url: body.base_url.map(|s| expand_vars(&s)).transpose()?,
            };
            Ok::<_, anyhow::Error>((id, endpoint))
        })
        .collect::<Result<_>>()?;

    let (default, auto_marked) = normalize_default(&file.sets, &file.default)?;
    if auto_marked {
        tracing::warn!(
            path = %path.display(),
            set = %default,
            "single set with no default marker; marking it default and writing the file back"
        );
        write_back_default(&raw, &default, path)?;
    }

    Ok(ProfileConfig {
        sets: file.sets,
        default,
        endpoints,
        parking: file.parking,
    })
}
/// Distinct temp name per call: two saves of the same config never race
/// on one temp file; the counter makes this clock-independent.
fn toml_tmp_path(path: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    path.with_extension(format!("toml.tmp.{}.{}", std::process::id(), seq))
}

/// Persist the auto-marked default by inserting a `default` key into the
/// original file text, ahead of the first table header. Working on the text
/// keeps comments, key order, and formatting intact; `${VAR}` spellings are
/// copied verbatim, so unexpanded secrets never materialize on disk.
fn write_back_default(raw: &str, default: &str, path: &Path) -> Result<()> {
    let marker = format!("default = \"{default}\"\n");
    // A top-level scalar must precede every table header; insert before the
    // first line that opens one (this schema has no multi-line string
    // values, so a header-looking line is always a header).
    let mut insert_at = raw.len();
    let mut offset = 0;
    for line in raw.split_inclusive('\n') {
        if line.trim_start().starts_with('[') {
            insert_at = offset;
            break;
        }
        offset += line.len();
    }
    // A hand-written explicit `default = ""` reads back as unmarked and
    // re-enters this path; inserting a second key would corrupt the file
    // (TOML rejects duplicate keys on the next parse). Keep the in-memory
    // default and leave the file alone.
    if raw[..insert_at]
        .lines()
        .any(|l| l.trim_start().starts_with("default"))
    {
        tracing::warn!(
            "profiles config already carries an explicit (empty) default key; skipping the write-back"
        );
        return Ok(());
    }
    let mut text = String::with_capacity(raw.len() + marker.len() + 1);
    text.push_str(&raw[..insert_at]);
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&marker);
    if insert_at < raw.len() {
        text.push('\n');
        text.push_str(&raw[insert_at..]);
    }
    let parent = path
        .parent()
        .context("profiles config path has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create config dir {}", parent.display()))?;
    let tmp = toml_tmp_path(path);
    std::fs::write(&tmp, &text)
        .with_context(|| format!("failed to write profiles config to {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename temp config to {}", path.display()))?;
    Ok(())
}

/// Resolve the config file path for reading:
/// `<config root>/profiles.toml` (`persistence::config_dir_root`,
/// installed by the host at boot) — declared configuration is operator
/// intent, so it lives under the config home, not the runtime data
/// tree. Returns `None` both when the resolved file does not exist and
/// when no roots are installed: either
/// way there is no config file to read, and `load()` degrades to the
/// implicit env profile instead of blocking boot.
fn resolve_config_path() -> Result<Option<PathBuf>> {
    let path = match config_dir_profile_path() {
        Ok(p) => p,
        Err(error) => {
            tracing::warn!(
                %error,
                "profiles config unavailable; using the implicit env profile"
            );
            return Ok(None);
        }
    };
    Ok(path.exists().then_some(path))
}

/// Resolve the config file path for writing, same single location as the read
/// side. Unlike the read side (which returns `None` when the file does not
/// exist), this always returns a path — `save()` needs a target even on first
/// write. Errors when no instance roots are installed: a write has nowhere
/// to land.
pub fn config_path() -> Result<PathBuf> {
    config_dir_profile_path()
}
/// The single profiles location shared by both resolve fns:
/// `<config root>/profiles.toml` (the same host-installed root
/// `persistence::config_dir_root` names, so the declared config sits in
/// the config tree while runtime data stays in the data tree). Errors
/// when the config root cannot be derived.
fn config_dir_profile_path() -> Result<PathBuf> {
    Ok(crate::persistence::config_dir_root()?.join("profiles.toml"))
}

/// Serialize a [`ProfileConfig`] to TOML and write it to `path` atomically
/// (temp file + rename), chmod 600. The `id` field inside each endpoint is
/// redundant with the map key but harmless: `load_file` uses an intermediate
/// type that ignores it.
pub fn save(config: &ProfileConfig, path: &Path) -> Result<()> {
    let toml =
        toml::to_string_pretty(config).context("failed to serialize profiles config to TOML")?;
    let parent = path
        .parent()
        .context("profiles config path has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create config dir {}", parent.display()))?;
    // Write to a temp file in the same directory, then rename for atomicity.
    let tmp = toml_tmp_path(path);
    std::fs::write(&tmp, &toml)
        .with_context(|| format!("failed to write profiles config to {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename temp config to {}", path.display()))?;
    Ok(())
}

/// The directory holding `profiles.toml` (and thus potentially API keys) — the
/// path a sandbox hide-hole should overlay so a broad-read agent cannot read
/// credentials. That directory is the config root itself (the file sits at
/// `<config root>/profiles.toml`); `None` when the config root cannot be
/// derived (no derivable root means no profiles file to hide).
pub fn profiles_config_dir() -> Option<PathBuf> {
    // Same single source as the loader (`config_dir_profile_path`): the
    // hide-hole must follow the resolver, or a relocated profiles.toml leaks
    // past the Guest sandbox. The config root is a directory (as the tmpfs
    // overlay contract requires), and hiding it never touches the data root
    // (agents/skills stay Guest-visible).
    config_dir_profile_path()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
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

/// Validate the parsed file: non-empty `api_key`; set names matching
/// `^[A-Za-z0-9_-]+$` (they double as TOML keys and URL path segments); unique
/// profile ids across sets ∪ parking (both hold the same id namespace — a
/// duplicate would break the wire-boundary invariant on the next PUT).
/// Profile→endpoint references and backend coverage are validated when the
/// tagma constructs `ProfileRegistry` (sets only — parked profiles may dangle
/// by design). Default resolution lives in [`normalize_default`].
fn validate(file: &ConfigFile) -> Result<()> {
    for (id, body) in &file.endpoints {
        if body.api_key.trim().is_empty() {
            bail!("endpoint '{id}': api_key is required");
        }
    }
    for name in file.sets.keys() {
        if !is_valid_set_name(name) {
            bail!("set name '{name}' must match ^[A-Za-z0-9_-]+$ (letters, digits, '_', '-')");
        }
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for set in file.sets.values() {
        for p in &set.profiles {
            if !seen.insert(p.id.as_str()) {
                bail!("duplicate profile id '{}'", p.id);
            }
        }
    }
    for p in &file.parking {
        if !seen.insert(p.id.as_str()) {
            bail!("duplicate profile id '{}'", p.id);
        }
    }
    for set in file.sets.values() {
        for p in &set.profiles {
            require_text_member("set", p)?;
        }
    }
    for p in &file.parking {
        require_text_member("parking", p)?;
    }
    Ok(())
}

/// Every profile must accept text: system prompts and tool
/// results are plain text, so a profile that cannot take text can never
/// hold a useful conversation; an explicit declaration lacking `text` is
/// refused at load (absent declarations fall back to text-only and never
/// trip this). Distinct from the set-level uniformity question — this is
/// a per-profile validity rule.
pub fn require_text_member(where_in: &str, p: &Profile) -> Result<()> {
    if p.modalities.contains(&Modality::Text) {
        return Ok(());
    }
    let declared = p
        .modalities
        .iter()
        .map(|m| m.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "{where_in} profile '{}' declares modalities = [{declared}], which lacks the required 'text' member; every profile must accept text",
        p.id
    )
}

/// Per-set shadow notices: (set name, effective-modalities debug text)
/// for every set whose members declare beyond the intersection. Both the
/// load path and the PUT path warn with these; kept pure so the rule is
/// unit-testable without a tracing subscriber.
pub fn shadowed_set_notices(sets: &BTreeMap<String, ProfileSet>) -> Vec<(String, String)> {
    sets.iter()
        .filter(|(_, s)| s.has_shadowed_members())
        .map(|(name, s)| (name.clone(), format!("{:?}", s.effective_modalities())))
        .collect()
}

/// A set name: non-empty, ASCII letters, digits, `_`, `-`. This keeps the name
/// safe as a bare TOML key and as a URL path segment.
pub fn is_valid_set_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Resolve the default set name against the collection:
/// - explicit name → must reference an existing key;
/// - no name, exactly one set → that set is the default (`auto_marked = true`;
///   the caller persists the marker back to the file);
/// - no name, several sets → error with guidance;
/// - empty collection → empty default (profile-less boot).
pub fn normalize_default(
    sets: &BTreeMap<String, ProfileSet>,
    current: &str,
) -> Result<(String, bool)> {
    if sets.is_empty() {
        return Ok((String::new(), false));
    }
    if !current.is_empty() {
        if sets.contains_key(current) {
            return Ok((current.to_string(), false));
        }
        bail!("default set name '{current}' does not match any set");
    }
    match sets.len() {
        1 => {
            let only = sets.keys().next().expect("len checked").clone();
            Ok((only, true))
        }
        _ => bail!(
            "several sets are configured but none is marked default; add a top-level `default = \"<name>\"` naming the default set"
        ),
    }
}

// --- serde-facing types (TOML schema) ---

#[derive(Deserialize)]
struct ConfigFile {
    #[serde(default)]
    endpoints: HashMap<String, ProviderEntry>,
    #[serde(default)]
    sets: BTreeMap<String, ProfileSet>,
    #[serde(default)]
    default: String,
    #[serde(default)]
    parking: Vec<Profile>,
}

#[derive(Deserialize)]
struct ProviderEntry {
    family: String,
    api_key: String,
    #[serde(default)]
    base_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use just_llm_client::types::generation::ReasoningEffort;
    use serial_test::serial;
    use std::collections::BTreeSet;

    #[test]
    fn parse_profile_behavior_fields() {
        // store / effort are optional per-profile behavior knobs; absent = None.
        let toml = r#"
default = "a"

[endpoints.ds]
family = "deepseek"
api_key = "fake"

[sets.a]

  [[sets.a.profiles]]
  id = "tuned"
  endpoint = "ds"
  model = "m"
  max_context_window = 1000
  store = false
  effort = "high"

  [[sets.a.profiles]]
  id = "plain"
  endpoint = "ds"
  model = "m2"
  max_context_window = 1000
"#;
        let file: ConfigFile = toml::from_str(toml).unwrap();
        validate(&file).unwrap();
        let tuned = &file.sets["a"].profiles[0];
        assert_eq!(tuned.store, Some(false));
        assert_eq!(tuned.effort, Some(ReasoningEffort::High));
        let plain = &file.sets["a"].profiles[1];
        assert_eq!(plain.store, None);
        assert_eq!(plain.effort, None);
    }

    #[test]
    fn parse_rejects_unknown_effort_level() {
        // Effort levels are the upstream enum, so a typo dies at deserialization
        // (config load) instead of surfacing as a bad request downstream.
        let toml = r#"
default = "a"

[endpoints.ds]
family = "deepseek"
api_key = "fake"

[sets.a]

  [[sets.a.profiles]]
  id = "p"
  endpoint = "ds"
  model = "m"
  max_context_window = 1000
  effort = "absurd"
"#;
        assert!(toml::from_str::<ConfigFile>(toml).is_err());
    }

    #[test]
    fn behavior_fields_round_trip_without_none_noise() {
        // Unset fields must stay out of the serialized TOML (Option::None has no
        // TOML representation), while set ones round-trip as plain values.
        let tuned = Profile {
            id: "p".into(),
            endpoint: "e".into(),
            model: "m".into(),
            max_context_window: 1,
            store: Some(false),
            effort: Some(ReasoningEffort::Xhigh),
            modalities: Profile::default_modalities(),
        };
        let s = toml::to_string(&tuned).unwrap();
        assert!(s.contains("store = false"));
        assert!(s.contains("effort = \"xhigh\""));

        let plain = Profile {
            store: None,
            effort: None,
            ..tuned
        };
        let s = toml::to_string(&plain).unwrap();
        assert!(!s.contains("store"));
        assert!(!s.contains("effort"));
    }

    fn ds_env() -> [(&'static str, Option<&'static str>); 4] {
        [
            ("KALLIP_LLM_PROVIDER", Some("deepseek")),
            ("KALLIP_LLM_MODEL", Some("deepseek-test")),
            ("KALLIP_LLM_DEEPSEEK_API_KEY", Some("fake")),
            ("KALLIP_CONTEXT_WINDOW_TOKENS", Some("200000")),
        ]
    }

    #[test]
    fn from_env_yields_single_named_default_set() {
        temp_env::with_vars(ds_env(), || {
            let cfg = from_env().unwrap();
            // The env path yields one implicit set named "default", marked default.
            let p = &cfg.sets["default"].profiles[0];
            assert_eq!(p.model, "deepseek-test");
            assert_eq!(p.max_context_window, 200_000); // implicit env profile derives the window from the env var
            assert_eq!(cfg.default, "default");
            assert!(cfg.parking.is_empty()); // env path has no draft space
        });
    }

    #[test]
    fn from_env_without_provider_boots_empty() {
        temp_env::with_vars([("KALLIP_LLM_PROVIDER", None::<&str>)], || {
            let cfg = from_env().unwrap();
            assert!(cfg.sets.is_empty());
            assert!(cfg.default.is_empty());
            assert!(cfg.endpoints.is_empty());
            assert!(cfg.parking.is_empty());
        });
    }

    #[test]
    fn from_env_half_configured_fails_loud() {
        temp_env::with_vars(
            [
                ("KALLIP_LLM_PROVIDER", Some("deepseek")),
                ("KALLIP_LLM_MODEL", None),
            ],
            || {
                let err = from_env().unwrap_err();
                assert!(format!("{err:#}").contains("KALLIP_LLM_MODEL"));
            },
        );
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
default = "thinking"

[endpoints.ds]
family = "deepseek"
api_key = "fake"

[sets.thinking]
description = "long-chain reasoning"

  [[sets.thinking.profiles]]
  id = "pro"
  endpoint = "ds"
  model = "deepseek-pro"
  max_context_window = 500000
"#;
        let file: ConfigFile = toml::from_str(toml).unwrap();
        validate(&file).unwrap();
        assert_eq!(file.default, "thinking");
        assert!(file.sets.contains_key("thinking"));
    }

    #[test]
    fn parse_rejects_duplicate_profile_id() {
        let toml = r#"
default = "a"

[endpoints.ds]
family = "deepseek"
api_key = "fake"

[sets.a]

  [[sets.a.profiles]]
  id = "dup"
  endpoint = "ds"
  model = "m"
  max_context_window = 1000

  [[sets.a.profiles]]
  id = "dup"
  endpoint = "ds"
  model = "m2"
  max_context_window = 1000
"#;
        let file: ConfigFile = toml::from_str(toml).unwrap();
        assert!(validate(&file).is_err());
    }

    #[test]
    fn rejects_invalid_set_name() {
        // A name with a space parses (quoted TOML key) but must fail validation.
        let toml = r#"
[endpoints.ds]
family = "deepseek"
api_key = "fake"

[sets."has space"]

[[sets."has space".profiles]]
id = "p"
endpoint = "ds"
model = "m"
max_context_window = 1000
"#;
        let file: ConfigFile = toml::from_str(toml).unwrap();
        let err = validate(&file).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("has space"), "got: {msg}");
    }

    #[test]
    fn rejects_explicit_modalities_without_text() {
        // A profile that cannot accept text can never hold a useful
        // conversation — an explicit declaration lacking `text` is refused.
        // Absent declarations default to [text] and never trip this.
        let toml = r#"
[endpoints.ds]
family = "deepseek"
api_key = "fake"

[sets.a]

  [[sets.a.profiles]]
  id = "vision-only"
  endpoint = "ds"
  model = "m"
  max_context_window = 1000
  modalities = ["image"]
"#;
        let file: ConfigFile = toml::from_str(toml).unwrap();
        let err = validate(&file).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("vision-only"), "got: {msg}");
        assert!(msg.contains("image"), "got: {msg}");
        assert!(msg.contains("text"), "got: {msg}");
    }

    #[test]
    fn declared_modalities_parse_and_the_intersection_governs() {
        // An explicit multi-modal declaration parses; an absent one defaults
        // to [text]; the mixed set's effective modalities are the
        // intersection, and the shadowed member is flagged.
        let toml = r#"
[endpoints.ds]
family = "deepseek"
api_key = "fake"

[sets.a]

  [[sets.a.profiles]]
  id = "multi"
  endpoint = "ds"
  model = "m"
  max_context_window = 1000
  modalities = ["text", "image"]

  [[sets.a.profiles]]
  id = "quiet"
  endpoint = "ds"
  model = "m2"
  max_context_window = 1000
"#;
        let file: ConfigFile = toml::from_str(toml).unwrap();
        validate(&file).unwrap();
        let set = file.sets.get("a").unwrap();
        assert_eq!(
            set.profiles[0].modalities,
            vec![Modality::Text, Modality::Image]
        );
        assert!(Profile::modalities_is_default(&set.profiles[1].modalities));
        assert_eq!(set.effective_modalities(), BTreeSet::from([Modality::Text]));
        assert!(set.has_shadowed_members());
    }
    #[test]
    fn shadowed_set_notices_reports_only_shadowed_sets() {
        let toml = r#"
[endpoints.ds]
family = "deepseek"
api_key = "fake"

[sets.a]

  [[sets.a.profiles]]
  id = "multi"
  endpoint = "ds"
  model = "m"
  max_context_window = 1000
  modalities = ["text", "image"]

  [[sets.a.profiles]]
  id = "quiet"
  endpoint = "ds"
  model = "m2"
  max_context_window = 1000

[sets.b]

  [[sets.b.profiles]]
  id = "solo"
  endpoint = "ds"
  model = "m3"
  max_context_window = 1000
"#;
        let file: ConfigFile = toml::from_str(toml).unwrap();
        let notices = shadowed_set_notices(&file.sets);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].0, "a");
        assert!(notices[0].1.contains("Text"), "got: {:?}", notices);
    }
    #[test]
    fn rejects_duplicate_set_key() {
        // Duplicate set names are rejected by the TOML parser itself (duplicate key).
        let toml = r#"
[endpoints.ds]
family = "deepseek"
api_key = "fake"

[sets.same]
[[sets.same.profiles]]
id = "p1"
endpoint = "ds"
model = "m"
max_context_window = 1000

[sets.same]
[[sets.same.profiles]]
id = "p2"
endpoint = "ds"
model = "m"
max_context_window = 1000
"#;
        assert!(toml::from_str::<ConfigFile>(toml).is_err());
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

    #[test]
    fn save_load_roundtrip() {
        use std::collections::BTreeMap;

        let mut endpoints = HashMap::new();
        endpoints.insert(
            "ds".into(),
            Provider {
                id: "ds".into(),
                family: "deepseek".into(),
                api_key: "secret-key".into(),
                base_url: None,
            },
        );
        endpoints.insert(
            "oa".into(),
            Provider {
                id: "oa".into(),
                family: "openai-compatible".into(),
                api_key: "other-key".into(),
                base_url: Some("https://api.example.com".into()),
            },
        );
        let original = ProfileConfig {
            sets: BTreeMap::from([
                (
                    "thinking".to_string(),
                    ProfileSet {
                        name: "thinking".into(),
                        description: Some("long-chain reasoning".into()),
                        profiles: vec![
                            Profile {
                                id: "pro".into(),
                                endpoint: "ds".into(),
                                model: "deepseek-pro".into(),
                                max_context_window: 500_000,
                                store: None,
                                effort: None,
                                modalities: Profile::default_modalities(),
                            },
                            Profile {
                                id: "backup".into(),
                                endpoint: "oa".into(),
                                model: "gpt-4".into(),
                                max_context_window: 128_000,
                                store: None,
                                effort: None,
                                modalities: Profile::default_modalities(),
                            },
                        ],
                    },
                ),
                (
                    "mechanical".to_string(),
                    ProfileSet {
                        name: "mechanical".into(),
                        description: None,
                        profiles: vec![Profile {
                            id: "fast".into(),
                            endpoint: "ds".into(),
                            model: "deepseek-flash".into(),
                            max_context_window: 128_000,
                            store: None,
                            effort: None,
                            modalities: Profile::default_modalities(),
                        }],
                    },
                ),
            ]),
            default: "thinking".into(),
            endpoints,
            // A parked profile rides along (out of rotation, may dangle).
            parking: vec![Profile {
                id: "spare".into(),
                endpoint: "oa".into(),
                model: "gpt-4-mini".into(),
                max_context_window: 128_000,
                store: None,
                effort: None,
                modalities: Profile::default_modalities(),
            }],
        };

        let toml_str = toml::to_string_pretty(&original).expect("serialize");
        // The name lives in the map key only — no redundant inner copy.
        assert!(
            !toml_str.contains("name ="),
            "set name must live in the key only, got: {toml_str}"
        );
        // BTreeMap serialization is deterministic: sets appear in sorted-key order. A set
        // with no scalar fields omits its table header (its first output line is the
        // profiles array-of-tables), so match on the shared prefix.
        let mechanical = toml_str.find("[sets.mechanical").expect("mechanical");
        let thinking = toml_str.find("[sets.thinking").expect("thinking");
        assert!(
            mechanical < thinking,
            "sorted-key order expected, got: {toml_str}"
        );

        // Parse back via load_file's internal types (simulates disk round-trip).
        let file: ConfigFile = toml::from_str(&toml_str).expect("parse back");
        validate(&file).expect("validate");

        assert_eq!(file.default, "thinking");
        assert_eq!(file.sets.len(), 2);
        assert_eq!(file.sets["thinking"].profiles.len(), 2);
        assert_eq!(file.sets["thinking"].profiles[0].id, "pro");
        assert_eq!(file.sets["thinking"].profiles[0].endpoint, "ds");
        assert_eq!(
            file.sets["thinking"].description.as_deref(),
            Some("long-chain reasoning")
        );
        assert_eq!(file.endpoints.len(), 2);
        assert_eq!(file.endpoints["ds"].family, "deepseek");
        assert_eq!(file.endpoints["ds"].api_key, "secret-key");
        assert_eq!(
            file.endpoints["oa"].base_url.as_deref(),
            Some("https://api.example.com")
        );
        assert_eq!(file.parking.len(), 1);
        assert_eq!(file.parking[0].id, "spare");
    }

    #[test]
    fn save_empty_parking_writes_no_key() {
        use std::collections::BTreeMap;

        let mut endpoints = HashMap::new();
        endpoints.insert(
            "ds".into(),
            Provider {
                id: "ds".into(),
                family: "deepseek".into(),
                api_key: "k".into(),
                base_url: None,
            },
        );
        let cfg = ProfileConfig {
            sets: BTreeMap::from([(
                "only".to_string(),
                ProfileSet {
                    name: "only".into(),
                    description: None,
                    profiles: vec![Profile {
                        id: "pro".into(),
                        endpoint: "ds".into(),
                        model: "m".into(),
                        max_context_window: 1000,
                        store: None,
                        effort: None,
                        modalities: Profile::default_modalities(),
                    }],
                },
            )]),
            default: "only".into(),
            endpoints,
            parking: vec![],
        };
        let toml_str = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(
            !toml_str.contains("parking"),
            "empty parking must not write a key, got: {toml_str}"
        );
        assert!(
            toml_str.contains("default = \"only\""),
            "non-empty config must persist the default marker, got: {toml_str}"
        );
    }

    fn write_config(dir: &std::path::Path, content: &str) -> std::path::PathBuf {
        let path = dir.join("profiles.toml");
        std::fs::write(&path, content).unwrap();
        path
    }

    const ONE_SET_NO_DEFAULT: &str = r#"
[endpoints.ds]
family = "deepseek"
api_key = "fake"

[sets.only]

[[sets.only.profiles]]
id = "p"
endpoint = "ds"
model = "m"
max_context_window = 1000
"#;

    const SECOND_SET: &str = r#"
[sets.other]

[[sets.other.profiles]]
id = "p2"
endpoint = "ds"
model = "m2"
max_context_window = 1000
"#;

    const DEFAULT_MISSING_PREFIX: &str = r#"default = "missing"
"#;
    const DEFAULT_A_PREFIX: &str = r#"default = "a"
"#;
    const PARKING_DUP: &str = r#"
[[parking]]
id = "p"
endpoint = "ds"
model = "m2"
max_context_window = 1000
"#;

    #[test]
    fn rejects_legacy_tiers_key_with_migration_hint() {
        let legacy = r#"
[[tiers]]

[[tiers.profiles]]
id = "p"
endpoint = "ds"
model = "m"
max_context_window = 1000
"#;
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config(tmp.path(), legacy);
        let err = load_file(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("legacy"), "got: {msg}");
        assert!(
            msg.contains("[sets."),
            "guidance must name the new form, got: {msg}"
        );
        assert!(
            msg.contains("default"),
            "guidance must mention the default key, got: {msg}"
        );
    }

    #[test]
    fn default_must_reference_existing_set() {
        let tmp = tempfile::tempdir().unwrap();
        let content = format!("{DEFAULT_MISSING_PREFIX}{ONE_SET_NO_DEFAULT}");
        let path = write_config(tmp.path(), &content);
        let err = load_file(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("does not match any set"), "got: {msg}");
    }

    #[test]
    fn single_set_without_default_is_auto_marked_and_written_back() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config(tmp.path(), ONE_SET_NO_DEFAULT);
        let cfg = load_file(&path).unwrap();
        assert_eq!(cfg.default, "only");
        // The write-back persists the auto-marked default.
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            on_disk.contains("default = \"only\""),
            "default must be persisted, got: {on_disk}"
        );
    }
    #[test]
    fn auto_mark_write_back_preserves_comments_and_key_order() {
        let tmp = tempfile::tempdir().unwrap();
        // Hand-written file: leading comment, sections in non-alphabetical
        // order, no default marker — exactly the shape the auto-mark path
        // rewrites.
        const HANDWRITTEN: &str = r#"# operator note: sets listed before endpoints here

[sets.solo]

[[sets.solo.profiles]]
id = "p"
endpoint = "ds"
model = "m"
max_context_window = 1000

[endpoints.ds]
family = "deepseek"
api_key = "fake"
"#;
        let path = write_config(tmp.path(), HANDWRITTEN);
        let cfg = load_file(&path).unwrap();
        assert_eq!(cfg.default, "solo");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        let note = on_disk.find("# operator note").expect("comment survives");
        let marker = on_disk.find("default = \"solo\"").expect("marker written");
        let sets = on_disk.find("[sets.solo]").expect("set section kept");
        let endpoints = on_disk.find("[endpoints.ds]").expect("endpoint kept");
        assert!(
            note < marker && marker < sets && sets < endpoints,
            "comment, marker, and original section order must survive, got: {on_disk}"
        );
        // Reloading is stable: the marker is already there, no rewrite runs.
        let cfg = load_file(&path).unwrap();
        assert_eq!(cfg.default, "solo");
    }
    #[test]
    fn explicit_empty_default_skips_the_write_back() {
        let tmp = tempfile::tempdir().unwrap();
        // A hand-written `default = ""` reads as unmarked; the write-back
        // must not insert a second default key (TOML rejects duplicates).
        let content = format!("default = \"\"\n{ONE_SET_NO_DEFAULT}");
        let path = write_config(tmp.path(), &content);
        let cfg = load_file(&path).unwrap();
        assert_eq!(cfg.default, "only");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            on_disk.matches("default =").count(),
            1,
            "the explicit key must stay the only one, got: {on_disk}"
        );
        // The untouched file still parses (idempotent skip).
        let cfg = load_file(&path).unwrap();
        assert_eq!(cfg.default, "only");
    }

    #[test]
    fn multi_set_without_default_errors_with_guidance() {
        let tmp = tempfile::tempdir().unwrap();
        let content = format!("{ONE_SET_NO_DEFAULT}{SECOND_SET}");
        let path = write_config(tmp.path(), &content);
        let err = load_file(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("default"),
            "an unmarked multi-set config must point at the default key, got: {msg}"
        );
    }

    #[test]
    fn parse_rejects_duplicate_id_between_set_and_parking() {
        let toml = format!("{DEFAULT_A_PREFIX}{ONE_SET_NO_DEFAULT}{PARKING_DUP}");
        let file: ConfigFile = toml::from_str(&toml).unwrap();
        assert!(validate(&file).is_err());
    }
    /// Fixture: install instance roots under an isolated temp dir. The
    /// config root is `<tmp>/config`.
    fn with_installed_roots<R>(tmp: &tempfile::TempDir, f: impl FnOnce() -> R) -> R {
        crate::persistence::set_instance_roots_for_tests(Some(crate::persistence::InstanceRoots {
            data: tmp.path().join("data"),
            config: tmp.path().join("config"),
            state: tmp.path().join("state"),
        }));
        let out = f();
        crate::persistence::set_instance_roots_for_tests(None);
        out
    }

    #[test]
    #[serial]
    fn installed_config_root_is_the_only_profiles_location() {
        let tmp = tempfile::tempdir().unwrap();
        let config_profiles = tmp.path().join("config").join("profiles.toml");
        std::fs::create_dir_all(config_profiles.parent().unwrap()).unwrap();
        std::fs::write(&config_profiles, "").unwrap();
        with_installed_roots(&tmp, || {
            assert_eq!(
                resolve_config_path().unwrap().as_deref(),
                Some(config_profiles.as_path())
            );
            assert_eq!(config_path().unwrap(), config_profiles);
        });
    }

    #[test]
    #[serial]
    fn missing_roots_degrade_the_read_and_error_the_write() {
        crate::persistence::set_instance_roots_for_tests(None);
        // The old HOME-level config silently shared one file across instances;
        // a run without installed roots gets no config file: the read side
        // degrades to the env profile (a warning, not a boot gate), while the
        // write side fails loud — nowhere to land.
        assert!(resolve_config_path().unwrap().is_none());
        assert!(config_path().is_err());
        assert!(profiles_config_dir().is_none());
    }

    #[test]
    #[serial]
    fn installed_root_reads_only_when_file_exists() {
        let tmp = tempfile::tempdir().unwrap();
        with_installed_roots(&tmp, || {
            // No profiles.toml in the installed config root: the read side
            // must yield None, not fall through to any other location.
            let resolved = resolve_config_path().unwrap();
            assert!(resolved.is_none());
        });
    }

    #[test]
    #[serial]
    fn profiles_config_dir_follows_the_installed_root() {
        let tmp = tempfile::tempdir().unwrap();
        let profiles_dir = tmp.path().join("config");
        std::fs::create_dir_all(&profiles_dir).unwrap();
        with_installed_roots(&tmp, || {
            // The hide-hole source must track the resolver: it hides the
            // config root itself (declared config is operator intent), not
            // the data root (agents/skills stay Guest-visible).
            assert_eq!(
                profiles_config_dir().as_deref(),
                Some(profiles_dir.as_path())
            );
        });
    }
}
