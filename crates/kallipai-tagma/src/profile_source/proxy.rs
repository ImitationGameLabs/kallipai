//! The gateway-backed source (profile-proxy design, decision 9): fetch
//! the selected collection (its member sets and default anchor)
//! plus the parked profiles from the model gateway's distribution
//! face, and adapt them into the stock [`ProfileConfig`] shape, so
//! backend construction stays the one shared code path.
//!
//! Snapshot-freshness semantics (design :444-459):
//! - Runtime refresh failure degrades without a TTL: the last good
//!   snapshot keeps serving, a failure counter and warn logs observe.
//!   Set-content drift is benign (the gateway re-authorizes every
//!   request); the one fatal staleness is a dead enrollment token,
//!   caught at use. A 401 from the face is that fatal staleness:
//!   it propagates instead of degrading (a hard boot failure at
//!   startup; the forward-path watcher drives poisoning).
//! - At boot with an unreachable gateway the tagma degrades: the boot
//!   load returns an empty config (the health block flags `degraded`)
//!   and a background repull task retries until the gateway answers;
//!   agents restore against the empty registry as unconfigured
//!   placeholders. A dead enrollment token is still a hard boot error
//!   (the token is read once at boot; recovery is re-enroll plus
//!   restart). Runtime `load()` keeps the strict cold leg — a switch
//!   to this source still requires a reachable gateway. The local
//!   profiles.toml is never written in this mode: it belongs to the
//!   local source alone.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use just_llm_client::types::generation::ReasoningEffort;
use kallipai_adk::profile::{Profile, ProfileConfig, ProfileSet, Provider};
use kallipai_common::protocol::Modality;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tracing::{error, info, warn};

use super::{ProfileSource, ProfileSourceKind};

// -- Wire DTOs: mirrors of the distribution face's serialize types. -----
// The face serves no profile ids (sets are the addressable unit), so the
// adapter synthesizes local ids below.

#[derive(Deserialize)]
struct SelectedCollection {
    sets: Vec<SanitizedSet>,
    default_set: Option<String>,
}

#[derive(Deserialize)]
struct SanitizedProfile {
    family: String,
    base_url: String,
    model: String,
    max_context_window: Option<i64>,
    effort: Option<String>,
    store: Option<bool>,
    modalities: Vec<String>,
    api_key: String,
}

#[derive(Deserialize)]
struct SanitizedSet {
    name: String,
    description: String,
    profiles: Vec<SanitizedProfile>,
}

#[derive(Deserialize)]
struct Parking {
    profiles: Vec<SanitizedProfile>,
}

// -- Refresh errors ------------------------------------------------------

/// Why a refresh failed. The split drives the freshness semantics: only a
/// token-lifecycle rejection propagates (never a degraded snapshot); every
/// other failure may serve the last good snapshot.
#[derive(Debug)]
pub(crate) enum FetchError {
    /// 401 from the face: the presenting token no longer resolves. `code`
    /// carries the gateway's `key_expired` / `key_revoked`; `None` is the
    /// unknown-bearer case (same handling, config-error wording).
    KeyLifecycle {
        code: Option<String>,
        message: String,
    },
    /// Unreachable, 5xx, 403/404 on the face, malformed payload: stale
    /// content at worst, the token is not the problem.
    Refresh(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::KeyLifecycle { message, .. } => {
                write!(f, "gateway rejected the enrollment token: {message}")
            }
            Self::Refresh(reason) => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for FetchError {}

/// Lift `{"error":{"message","code"}}` (the wire error envelope) out of a
/// response body; a non-JSON body yields the fallback message.
fn parse_api_error(body: &str) -> (String, Option<String>) {
    let value: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let error = value.get("error");
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("unknown enrollment-token rejection")
        .to_owned();
    let code = error
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .map(str::to_owned);
    (message, code)
}

// -- The source ----------------------------------------------------------

/// The gateway-backed [`ProfileSource`]. Refreshes serialize on one mutex
/// (a refresh holds it across the fetches, so concurrent `load()` calls
/// share one refresh instead of stampeding the face).
pub struct ProxyProfileSource {
    client: reqwest::Client,
    /// The gateway's distribution base, no trailing slash: the
    /// api-edge segment; the `base_url` each
    /// profile carries comes from the gateway's own payload, not from
    /// this setting (clients append their own wire path to that one).
    base: String,
    token: String,
    state: tokio::sync::Mutex<ProxyState>,
}

/// The token is gateway credential material and the cached snapshot
/// carries provider api_keys that echo it: `Debug` redacts the token
/// field and keeps the snapshot out of the output entirely.
impl std::fmt::Debug for ProxyProfileSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyProfileSource")
            .field("client", &self.client)
            .field("base", &self.base)
            .field("token", &"[REDACTED]")
            // The mutex contents (last_good) carry api_keys that echo
            // the token; the field reports presence only.
            .field(
                "state",
                &self.state.try_lock().map(|s| s.last_good.is_some()),
            )
            .finish()
    }
}

#[derive(Debug, Default)]
struct ProxyState {
    last_good: Option<ProfileConfig>,
    refresh_failures: u64,
    /// A refresh confirmed the enrollment token dead (401 again):
    /// new spawns and profile-set binds refuse until a successful
    /// refresh clears it.
    poisoned: bool,
    /// A boot-time fetch failed with no snapshot: the source serves an
    /// empty config while the repull task retries; the first successful
    /// refresh clears it (record_success).
    degraded: bool,
    token_state: super::TokenState,
    last_refresh: Option<std::time::SystemTime>,
}

impl ProxyState {
    /// Record a successful refresh: the snapshot lands, counters
    /// reset, and every failure flag (poison, degraded) clears.
    fn record_success(&mut self, cfg: ProfileConfig) {
        self.last_good = Some(cfg);
        self.refresh_failures = 0;
        self.poisoned = false;
        self.degraded = false;
        self.token_state = super::TokenState::Ok;
        self.last_refresh = Some(std::time::SystemTime::now());
    }

    /// Record a confirmed-dead enrollment token and hand back the hard
    /// error both load paths propagate.
    fn record_key_death(&mut self, code: Option<String>, message: String) -> anyhow::Error {
        self.poisoned = true;
        self.token_state = super::TokenState::from_code(code.as_deref());
        anyhow::anyhow!(
            "gateway rejected the enrollment token ({}): {message}",
            code.as_deref().unwrap_or("unknown token")
        )
    }
}

impl super::TokenState {
    /// Fold a gateway lifecycle code into the health-view state.
    fn from_code(code: Option<&str>) -> Self {
        match code {
            Some("key_expired") => Self::Expired,
            Some("key_revoked") => Self::Revoked,
            _ => Self::Unknown,
        }
    }
}

impl ProxyProfileSource {
    /// Validate the enrollment token and the derived distribution
    /// base. Both failures
    /// are boot errors with actionable text. The token is the relay
    /// enrollment token of the entry the boot resolved — there is no
    /// separate key material (the caller scanned the stored
    /// credentials, the same source the files-service token uses).
    pub fn new(base: &str, token: &str) -> Result<Self> {
        let token = token.trim();
        if token.is_empty() {
            bail!(
                "the resolved relay entry stores an empty enrollment token; re-enroll the tagma on the platform and restart"
            );
        }
        let base = base.trim_end_matches('/');
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            bail!(
                "the derived gateway base {base:?} is not an http(s) URL; check the platform origin (KALLIPAI_POLIS_URL / polis.toml entry)"
            );
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .context("failed to build the gateway HTTP client")?;
        Ok(Self {
            client,
            base: base.to_owned(),
            token: token.to_owned(),
            state: tokio::sync::Mutex::new(ProxyState::default()),
        })
    }

    async fn get_json_opt<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>, FetchError> {
        let url = format!("{}{}", self.base, path);
        let response = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| FetchError::Refresh(format!("GET {path}: {e}")))?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            let body = response.text().await.unwrap_or_default();
            let (message, code) = parse_api_error(&body);
            return Err(FetchError::KeyLifecycle { code, message });
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(FetchError::Refresh(format!(
                "GET {path}: HTTP {status}: {body}"
            )));
        }
        response
            .json::<T>()
            .await
            .map(Some)
            .map_err(|e| FetchError::Refresh(format!("GET {path}: malformed payload: {e}")))
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, FetchError> {
        self.get_json_opt::<T>(path).await.and_then(|v| {
            v.ok_or_else(|| FetchError::Refresh(format!("GET {path}: missing resource")))
        })
    }

    /// Write the selection through to the gateway (PUT /selection):
    /// the tagma pointer is the single store, so the local settings
    /// carry no copy. Ok(false) means the named collection is not
    /// visible to this account (the gateway 404s, no existence
    /// oracle); token lifecycle and transport failures surface as the
    /// matching FetchError.
    pub(crate) async fn put_selection(
        &self,
        owner: Option<&str>,
        collection: &str,
    ) -> Result<bool, FetchError> {
        let url = format!("{}/selection", self.base);
        let response = self
            .client
            .put(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "owner": owner,
                "collection": collection,
            }))
            .send()
            .await
            .map_err(|e| FetchError::Refresh(format!("PUT /selection: {e}")))?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            let body = response.text().await.unwrap_or_default();
            let (message, code) = parse_api_error(&body);
            return Err(FetchError::KeyLifecycle { code, message });
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(FetchError::Refresh(format!(
                "PUT /selection: HTTP {status}: {body}"
            )));
        }
        Ok(true)
    }

    async fn fetch_and_adapt(&self) -> Result<ProfileConfig, FetchError> {
        // One read: the gateway serves the collection this tagma
        // selected, with its member sets already sanitized. No
        // selection answers 404, which reads here as the empty
        // snapshot (a fresh tagma, or a pointer not made yet).
        let selected = self
            .get_json_opt::<SelectedCollection>("/selected-collection")
            .await?;
        // The anchor reads before the member sets move into the
        // loop below.
        let mut default_set = selected
            .as_ref()
            .and_then(|s| s.default_set.clone())
            .unwrap_or_default();
        let mut endpoints: HashMap<String, Provider> = HashMap::new();
        let mut sets = BTreeMap::new();
        if let Some(selected) = selected {
            for set in selected.sets {
                let profiles = adapt_profiles(&set.profiles, &mut endpoints)
                    .map_err(|e| FetchError::Refresh(format!("{e:#}")))?;
                sets.insert(
                    set.name.clone(),
                    ProfileSet {
                        name: set.name,
                        description: (!set.description.is_empty()).then_some(set.description),
                        profiles,
                    },
                );
            }
        }
        let parking = self.get_json::<Parking>("/parking").await?;
        let parked = adapt_profiles(&parking.profiles, &mut endpoints)
            .map_err(|e| FetchError::Refresh(format!("{e:#}")))?;
        // A default anchor the gateway no longer serves among the
        // members would dangle locally, so the marker falls back to
        // the no-default state.
        if !default_set.is_empty() && !sets.contains_key(&default_set) {
            default_set = String::new();
        }
        Ok(ProfileConfig {
            sets,
            default: default_set,
            endpoints,
            parking: parked,
        })
    }
}

#[async_trait::async_trait]
impl ProfileSource for ProxyProfileSource {
    fn kind(&self) -> ProfileSourceKind {
        ProfileSourceKind::Proxy
    }

    async fn load(&self) -> Result<ProfileConfig> {
        let mut state = self.state.lock().await;
        match self.fetch_and_adapt().await {
            Ok(cfg) => {
                state.record_success(cfg.clone());
                Ok(cfg)
            }
            Err(err) => {
                state.refresh_failures += 1;
                match err {
                    FetchError::KeyLifecycle { code, message } => {
                        // Mark the poison before propagating: the signal
                        // consumer and ensure_usable read this flag.
                        Err(state.record_key_death(code, message))
                    }
                    FetchError::Refresh(reason) => {
                        if let Some(cfg) = state.last_good.clone() {
                            warn!(
                                failures = state.refresh_failures,
                                reason,
                                "profile refresh failed; serving the last good snapshot (no TTL — token health is caught at use)"
                            );
                            return Ok(cfg);
                        }
                        // No in-memory snapshot and no fallback: this is
                        // the cold-boot leg of the hard-fail decision —
                        // name the gateway and both manual recovery paths.
                        Err(anyhow::anyhow!(
                            "{reason}; no in-memory snapshot and no fallback: the model gateway at {} cannot be read from this tagma — fix the gateway side and restart, or set settings.toml [profiles.source].mode = \"local\" and restart (no automatic switch happens)",
                            self.base
                        ))
                    }
                }
            }
        }
    }

    /// The boot leg ([`ProfileSource::boot_load`]): identical to `load`
    /// except the cold start — a refresh-class failure with no snapshot
    /// degrades to an empty config (flagged in the health block) so the
    /// tagma still boots; the repull task owns recovery. A dead
    /// enrollment token stays a hard error either way.
    async fn boot_load(&self) -> Result<ProfileConfig> {
        let mut state = self.state.lock().await;
        match self.fetch_and_adapt().await {
            Ok(cfg) => {
                state.record_success(cfg.clone());
                Ok(cfg)
            }
            Err(err) => {
                state.refresh_failures += 1;
                match err {
                    FetchError::KeyLifecycle { code, message } => {
                        Err(state.record_key_death(code, message))
                    }
                    FetchError::Refresh(reason) => {
                        if let Some(cfg) = state.last_good.clone() {
                            warn!(
                                failures = state.refresh_failures,
                                reason, "profile boot load fell back to the last good snapshot"
                            );
                            return Ok(cfg);
                        }
                        state.degraded = true;
                        warn!(
                            failures = state.refresh_failures,
                            reason,
                            base = %self.base,
                            "boot fetch failed with no snapshot: booting with an empty profile set (degraded); the repull task retries in the background"
                        );
                        Ok(ProfileConfig {
                            sets: BTreeMap::new(),
                            default: String::new(),
                            endpoints: HashMap::new(),
                            parking: Vec::new(),
                        })
                    }
                }
            }
        }
    }

    async fn ensure_usable(&self) -> Result<()> {
        let state = self.state.lock().await;
        if state.poisoned {
            bail!(
                "the gateway enrollment token is dead ({}): re-enroll the tagma on the platform and restart (the token is read once at boot); spawns and profile-set binds stay refused until then",
                match state.token_state {
                    super::TokenState::Expired => "token expired",
                    super::TokenState::Revoked => "token revoked",
                    super::TokenState::Unknown => "token rejected",
                    super::TokenState::Ok => "token marked dead",
                }
            );
        }
        Ok(())
    }

    async fn snapshot(&self) -> Option<ProfileConfig> {
        self.state.lock().await.last_good.clone()
    }

    async fn health(&self) -> super::SourceHealth {
        let state = self.state.lock().await;
        super::SourceHealth {
            poisoned: state.poisoned,
            degraded: state.degraded,
            token_state: state.token_state,
            last_refresh: state.last_refresh,
            refresh_failures: state.refresh_failures,
        }
    }

    async fn handle_gateway_signal(&self, signal: &kallipai_adk::GatewaySignal) {
        // One refresh per signal; the poison decision rides on the refresh
        // outcome (load() marks it), never on the signal alone. 401 and 403
        // differ only in log framing: 401 says dead token, 403 says scope.
        let outcome = self.load().await;
        let poisoned = self.state.lock().await.poisoned;
        match outcome {
            Ok(_) => info!(
                status = signal.status,
                code = signal.code.as_deref().unwrap_or("none"),
                "gateway auth signal: snapshot refreshed cleanly"
            ),
            Err(error) if poisoned => error!(
                status = signal.status,
                error = %error,
                "gateway auth signal: refresh confirms the enrollment token is dead — spawns and profile-set binds are refused until the tagma is re-enrolled and a refresh succeeds"
            ),
            Err(error) => warn!(
                status = signal.status,
                error = %error,
                "gateway auth signal: refresh failed without token evidence; token health unchanged"
            ),
        }
    }
}

// -- Adaptation: distribution payloads -> ProfileConfig ------------------
//
// The face carries no profile ids, so the adapter synthesizes them from the
// model name (disambiguated -2/-3/... within one list when a set really
// holds two entries for the same model). Endpoints collapse by family: the
// gateway serves every family from one base with the presenting token, so two
// same-family profiles are the same endpoint from the client's side.

fn adapt_profiles(
    source: &[SanitizedProfile],
    endpoints: &mut HashMap<String, Provider>,
) -> Result<Vec<Profile>> {
    let mut used_ids: HashSet<String> = HashSet::new();
    source
        .iter()
        .map(|profile| {
            let mut id = profile.model.clone();
            let mut suffix = 2;
            while !used_ids.insert(id.clone()) {
                id = format!("{}-{suffix}", profile.model);
                suffix += 1;
            }
            adapt_profile(profile, id, endpoints)
        })
        .collect()
}

fn adapt_profile(
    source: &SanitizedProfile,
    id: String,
    endpoints: &mut HashMap<String, Provider>,
) -> Result<Profile> {
    let window = source
        .max_context_window
        .filter(|w| *w > 0)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "profile for model {:?} carries no usable max_context_window; set one on the gateway registry",
                source.model
            )
        })?;
    let endpoint = endpoints
        .entry(source.family.clone())
        .or_insert_with(|| Provider {
            id: source.family.clone(),
            family: source.family.clone(),
            api_key: source.api_key.clone(),
            base_url: Some(source.base_url.clone()),
        });
    let effort = source
        .effort
        .as_deref()
        .map(|raw| {
            serde_json::from_value::<ReasoningEffort>(serde_json::Value::String(raw.to_owned()))
                .with_context(|| {
                    format!(
                        "profile for model {:?} carries unknown effort {raw:?}",
                        source.model
                    )
                })
        })
        .transpose()?;
    // An empty modality list means "not declared" on the face (null column);
    // that maps to the local config's undeclared default: text-only.
    let modalities = if source.modalities.is_empty() {
        Profile::default_modalities()
    } else {
        source
            .modalities
            .iter()
            .map(|m| serde_json::from_value::<Modality>(serde_json::Value::String(m.clone())))
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| {
                format!(
                    "profile for model {:?} carries an unknown modality",
                    source.model
                )
            })?
    };
    Ok(Profile {
        id,
        endpoint: endpoint.id.clone(),
        model: source.model.clone(),
        max_context_window: window as usize,
        store: source.store,
        effort,
        modalities,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use serde_json::json;
    use std::sync::Arc;

    // -- adaptation units -------------------------------------------------

    fn wire_profile(model: &str, family: &str) -> SanitizedProfile {
        SanitizedProfile {
            family: family.to_owned(),
            base_url: "http://gw.test:7501/v1".to_owned(),
            model: model.to_owned(),
            max_context_window: Some(4096),
            effort: None,
            store: None,
            modalities: Vec::new(),
            api_key: "tagma-token".to_owned(),
        }
    }

    #[test]
    fn adapt_dedupes_ids_and_collapses_endpoints() {
        let mut endpoints = HashMap::new();
        let profiles = adapt_profiles(
            &[
                wire_profile("m1", "deepseek"),
                wire_profile("m1", "deepseek"),
                wire_profile("m2", "deepseek"),
            ],
            &mut endpoints,
        )
        .unwrap();
        let ids: Vec<&str> = profiles.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["m1", "m1-2", "m2"]);
        // One endpoint per family; every profile routes through it.
        assert_eq!(endpoints.len(), 1);
        let provider = endpoints.values().next().unwrap();
        assert_eq!(provider.family, "deepseek");
        assert_eq!(provider.api_key, "tagma-token");
        assert!(profiles.iter().all(|p| p.endpoint == provider.id));
    }

    #[test]
    fn adapt_maps_effort_store_and_default_modalities() {
        let mut endpoints = HashMap::new();
        let mut rich = wire_profile("m1", "openai-compatible");
        rich.effort = Some("high".into());
        rich.store = Some(false);
        rich.modalities = vec!["text".into(), "image".into()];
        let profiles = adapt_profiles(&[rich], &mut endpoints).unwrap();
        assert_eq!(profiles[0].effort, Some(ReasoningEffort::High));
        assert_eq!(profiles[0].store, Some(false));
        assert_eq!(profiles[0].modalities.len(), 2);
        // Undeclared modalities fall back to the text-only default.
        let plain =
            adapt_profiles(&[wire_profile("m2", "openai-compatible")], &mut endpoints).unwrap();
        assert_eq!(plain[0].modalities, Profile::default_modalities());
    }

    #[test]
    fn adapt_rejects_a_missing_window() {
        let mut endpoints = HashMap::new();
        let mut blind = wire_profile("m1", "deepseek");
        blind.max_context_window = None;
        let err = adapt_profiles(&[blind], &mut endpoints).unwrap_err();
        assert!(format!("{err:#}").contains("max_context_window"));
    }

    // -- source behaviors against a canned distribution face --------------
    //
    // The canned face echoes a FIXED public base in each profile's
    // `base_url` (the face decides what its public base is); the adapter
    // must copy it verbatim, whatever the mock's own address is.

    async fn spawn_mock_gateway() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route(
                "/selected-collection",
                get(|| async {
                    axum::Json(json!({
                        "owner": "acc-x",
                        "collection": "user-col",
                        "description": "the account's own collection",
                        "sets": [
                            { "name": "alpha",
                              "description": "primary",
                              "profiles": [
                                { "family": "openai-compatible",
                                  "base_url": "http://gw.test:7501/v1",
                                  "model": "gpt-x", "max_context_window": 128000,
                                  "effort": "high", "store": true,
                                  "modalities": ["text", "image"],
                                  "api_key": "tagma-token-1" },
                                { "family": "deepseek",
                                  "base_url": "http://gw.test:7501/v1",
                                  "model": "ds-x", "max_context_window": 64000,
                                  "effort": null, "store": null, "modalities": [],
                                  "api_key": "tagma-token-1" }
                              ] }
                        ],
                        "default_set": "alpha"
                    }))
                }),
            )
            .route(
                "/parking",
                get(|| async {
                    axum::Json(json!({ "profiles": [
                        { "family": "deepseek", "base_url": "http://gw.test:7501/v1",
                          "model": "draft-1", "max_context_window": 32000,
                          "effort": null, "store": null, "modalities": [],
                          "api_key": "tagma-token-1" }
                    ]}))
                }),
            );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// A dead base: a bound-then-dropped listener (the next connect is
    /// refused — nothing serves the port).
    async fn dead_base() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn an_empty_enrollment_token_is_an_actionable_boot_error() {
        let err = ProxyProfileSource::new("http://gw.test:7501", "  ").unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("empty enrollment token"), "got: {text}");
        assert!(text.contains("re-enroll"), "got: {text}");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn full_shape_round_trip_without_a_disk_write() {
        crate::test_helpers::ensure_test_data_dir();
        // Serial-order hygiene: an earlier test's disk fixture must
        // not make the no-write assertion below a false negative.
        let _ = std::fs::remove_file(kallipai_adk::profile::config_path().unwrap());
        let base = spawn_mock_gateway().await;
        let source = ProxyProfileSource::new(&base, "tagma-token-1").unwrap();
        let cfg = source.load().await.unwrap();
        // Set + failover order + synthesized ids + default marker.
        let set = cfg.sets.get("alpha").unwrap();
        assert_eq!(set.description.as_deref(), Some("primary"));
        let ids: Vec<&str> = set.profiles.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["gpt-x", "ds-x"]);
        assert_eq!(set.profiles[0].effort, Some(ReasoningEffort::High));
        assert_eq!(set.profiles[0].store, Some(true));
        // The undeclared-modality profile falls back to text-only.
        assert_eq!(set.profiles[1].modalities, Profile::default_modalities());
        assert_eq!(cfg.default, "alpha");
        // Endpoints collapse by family with the presenting token echoed and
        // the face's own public base copied verbatim.
        assert_eq!(cfg.endpoints.len(), 2);
        assert!(cfg.endpoints.values().all(|p| p.api_key == "tagma-token-1"));
        assert_eq!(
            cfg.endpoints["deepseek"].base_url.as_deref(),
            Some("http://gw.test:7501/v1")
        );
        // Parking adapted into the draft space.
        assert_eq!(cfg.parking.len(), 1);
        assert_eq!(cfg.parking[0].id, "draft-1");
        // A successful refresh lands in the health block (last_refresh
        // moves off the boot value).
        let health = source.health().await;
        assert!(health.last_refresh.is_some());
        // The refresh never writes the local file: profiles.toml
        // belongs to the local source alone (the no-disk decision).
        let cache = kallipai_adk::profile::config_path().unwrap();
        assert!(
            !cache.exists(),
            "a gateway refresh must not write profiles.toml"
        );
    }

    /// The keyed face: /sets scopes its answer by the presenting
    /// bearer (token-wide sees both sets, token-narrow sees one) -- the
    /// adapter consumes whatever the face's visibility filter serves,
    /// never more.
    async fn spawn_keyed_gateway() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route(
                "/selected-collection",
                get(|headers: axum::http::HeaderMap| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    let sets = if auth.ends_with("token-wide") {
                        vec![
                            json!({ "name": "alpha", "description": "primary",
                                    "profiles": [
                                      { "family": "openai-compatible",
                                        "base_url": "http://gw.test:7501/v1",
                                        "model": "gpt-x", "max_context_window": 128000,
                                        "effort": null, "store": null, "modalities": [],
                                        "api_key": "tagma-token-1" } ] }),
                            json!({ "name": "beta", "description": "secondary",
                                    "profiles": [
                                      { "family": "openai-compatible",
                                        "base_url": "http://gw.test:7501/v1",
                                        "model": "beta-model", "max_context_window": 32000,
                                        "effort": null, "store": null, "modalities": [],
                                        "api_key": "tagma-token-1" } ] }),
                        ]
                    } else {
                        vec![json!({ "name": "alpha", "description": "primary",
                                      "profiles": [
                                        { "family": "openai-compatible",
                                          "base_url": "http://gw.test:7501/v1",
                                          "model": "gpt-x", "max_context_window": 128000,
                                          "effort": null, "store": null, "modalities": [],
                                          "api_key": "tagma-token-1" } ] })]
                    };
                    axum::Json(json!({
                        "owner": "acc-x",
                        "collection": "user-col",
                        "description": "the account's own collection",
                        "sets": sets,
                        "default_set": "alpha"
                    }))
                }),
            )
            .route(
                "/parking",
                get(|| async { axum::Json(json!({ "profiles": [] })) }),
            );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// The adapted config mirrors the filtered face: the wide token
    /// adapts both sets, the narrowed token only the set its face
    /// served. The face decides the scope; the adapter decides nothing.
    #[tokio::test]
    #[serial_test::serial]
    async fn adapter_consumes_the_visibility_filtered_face() {
        crate::test_helpers::ensure_test_data_dir();
        let base = spawn_keyed_gateway().await;
        let wide = ProxyProfileSource::new(&base, "token-wide").unwrap();
        let cfg = wide.load().await.unwrap();
        let names: Vec<&str> = cfg.sets.keys().map(|s| s.as_str()).collect();
        assert_eq!(names, ["alpha", "beta"]);
        assert_eq!(cfg.default, "alpha");

        let narrow = ProxyProfileSource::new(&base, "token-narrow").unwrap();
        let cfg = narrow.load().await.unwrap();
        let names: Vec<&str> = cfg.sets.keys().map(|s| s.as_str()).collect();
        assert_eq!(names, ["alpha"]);

        let cache = kallipai_adk::profile::config_path().unwrap();
        let _ = std::fs::remove_file(cache);
    }

    /// The Debug face never echoes the enrollment token: the field is
    /// redacted, and the cached snapshot (whose provider api_keys echo
    /// the token) reports presence only.
    #[tokio::test]
    #[serial_test::serial]
    async fn debug_output_stays_token_free_after_a_refresh() {
        crate::test_helpers::ensure_test_data_dir();
        let base = spawn_keyed_gateway().await;
        let source = ProxyProfileSource::new(&base, "token-wide").unwrap();
        let cfg = source.load().await.unwrap();
        // The refresh populated the snapshot: its providers carry
        // credential material (the gateway-served api_keys), so a
        // snapshot-printing Debug leaks it.
        assert!(cfg.endpoints.values().all(|p| p.api_key == "tagma-token-1"));
        let rendered = format!("{source:?}");
        assert!(
            !rendered.contains("tagma-token-1") && !rendered.contains("token-wide"),
            "got: {rendered}"
        );
        assert!(rendered.contains("[REDACTED]"));
        let cache = kallipai_adk::profile::config_path().unwrap();
        let _ = std::fs::remove_file(cache);
    }

    /// The one-read shape: the gateway serves the selected collection's
    /// members; a stale default anchor (the set it names is not among
    /// them) re-homes to the no-default marker, and a face with no
    /// selection answers the empty snapshot, not an error.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_stale_default_anchor_re_homes_and_no_selection_reads_empty() {
        crate::test_helpers::ensure_test_data_dir();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route(
                "/selected-collection",
                get(|| async {
                    axum::Json(json!({
                        "owner": "acc-x",
                        "collection": "user-col",
                        "description": "the account's own collection",
                        "sets": [
                            { "name": "beta", "description": "second",
                              "profiles": [
                                { "family": "deepseek",
                                  "base_url": "http://gw.test:7501/v1",
                                  "model": "ds-x", "max_context_window": 64000,
                                  "effort": null, "store": null, "modalities": [],
                                  "api_key": "tagma-token-1" }
                              ] }
                        ],
                        "default_set": "alpha"
                    }))
                }),
            )
            .route(
                "/parking",
                get(|| async { axum::Json(json!({ "profiles": [] })) }),
            );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let base = format!("http://{addr}");
        let source = ProxyProfileSource::new(&base, "tagma-token-1").unwrap();
        let cfg = source.load().await.unwrap();
        let names: Vec<&str> = cfg.sets.keys().map(|s| s.as_str()).collect();
        assert_eq!(names, ["beta"]);
        assert_eq!(cfg.default, "");
        // No selection yet: the gateway 404s, which reads as the
        // empty snapshot (a fresh tagma), not an error.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route(
                "/selected-collection",
                get(|| async { StatusCode::NOT_FOUND }),
            )
            .route(
                "/parking",
                get(|| async { axum::Json(json!({ "profiles": [] })) }),
            );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let base = format!("http://{addr}");
        let fresh = ProxyProfileSource::new(&base, "tagma-token-1").unwrap();
        let cfg = fresh.load().await.unwrap();
        assert!(cfg.sets.is_empty());
        assert_eq!(cfg.default, "");
        let cache = kallipai_adk::profile::config_path().unwrap();
        let _ = std::fs::remove_file(cache);
    }

    #[tokio::test]
    async fn token_revoked_propagates_instead_of_degrading() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/selected-collection",
            get(|| async {
                (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(json!({
                        "error": { "message": "revoked", "code": "key_revoked" }
                    })),
                )
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let base = format!("http://{addr}");
        let source = ProxyProfileSource::new(&base, "tagma-token-1").unwrap();
        let err = source.load().await.unwrap_err();
        assert!(format!("{err:#}").contains("key_revoked"), "got: {err:#}");
    }

    fn cached_config() -> ProfileConfig {
        serde_json::from_value(json!({
            "endpoints": {
                "deepseek": { "id": "deepseek", "family": "deepseek", "api_key": "cached" }
            },
            "sets": {
                "cached-set": {
                    "profiles": [
                        { "id": "p", "endpoint": "deepseek", "model": "m",
                          "max_context_window": 8 }
                    ]
                }
            },
            "parking": [],
            "default": "cached-set"
        }))
        .unwrap()
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn unreachable_gateway_hard_fails_even_with_a_disk_file() {
        crate::test_helpers::ensure_test_data_dir();
        let cache = kallipai_adk::profile::config_path().unwrap();
        kallipai_adk::profile::save(&cached_config(), &cache).unwrap();
        let base = dead_base().await;
        let source = ProxyProfileSource::new(&base, "tagma-token-1").unwrap();
        let err = source.load().await.unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("no fallback"), "got: {text}");
        assert!(
            text.contains("settings.toml [profiles.source].mode"),
            "got: {text}"
        );
        let _ = std::fs::remove_file(cache);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn unreachable_gateway_without_cache_hard_fails() {
        crate::test_helpers::ensure_test_data_dir();
        let cache = kallipai_adk::profile::config_path().unwrap();
        let _ = std::fs::remove_file(&cache);
        let base = dead_base().await;
        let source = ProxyProfileSource::new(&base, "tagma-token-1").unwrap();
        let err = source.load().await.unwrap_err();
        assert!(format!("{err:#}").contains("no fallback"), "got: {err:#}");
    }

    // -- gateway auth-signal handling -------------------------------------

    /// A face that rejects the selection read with a fixed status + lifecycle code.
    async fn spawn_rejecting_gateway(status: StatusCode, code: &str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let code = code.to_owned();
        let app = axum::Router::new().route(
            "/selected-collection",
            get(move || {
                let body = json!({ "error": { "message": "nope", "code": code } });
                async move { (status, axum::Json(body)) }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn signal_401_with_a_rejecting_face_poisons_the_source() {
        crate::test_helpers::ensure_test_data_dir();
        let base = spawn_rejecting_gateway(StatusCode::UNAUTHORIZED, "key_revoked").await;
        let source = ProxyProfileSource::new(&base, "tagma-token-1").unwrap();
        assert!(!source.health().await.poisoned);
        source
            .handle_gateway_signal(&kallipai_adk::GatewaySignal {
                status: 401,
                code: Some("key_revoked".into()),
            })
            .await;
        let health = source.health().await;
        assert!(health.poisoned, "a refresh-confirmed 401 poisons");
        assert_eq!(health.token_state, super::super::TokenState::Revoked);
        let err = source.ensure_usable().await.unwrap_err();
        assert!(format!("{err:#}").contains("token revoked"), "got: {err:#}");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn signal_403_refreshes_without_poisoning() {
        crate::test_helpers::ensure_test_data_dir();
        let base = spawn_rejecting_gateway(StatusCode::FORBIDDEN, "quota").await;
        let source = ProxyProfileSource::new(&base, "tagma-token-1").unwrap();
        source
            .handle_gateway_signal(&kallipai_adk::GatewaySignal {
                status: 403,
                code: None,
            })
            .await;
        let health = source.health().await;
        assert!(!health.poisoned, "403 never poisons");
        assert_eq!(health.token_state, super::super::TokenState::Ok);
        assert!(source.ensure_usable().await.is_ok());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn signal_with_a_healthy_face_refreshes_cleanly() {
        crate::test_helpers::ensure_test_data_dir();
        let base = spawn_mock_gateway().await;
        let source = ProxyProfileSource::new(&base, "tagma-token-1").unwrap();
        source
            .handle_gateway_signal(&kallipai_adk::GatewaySignal {
                status: 401,
                code: None,
            })
            .await;
        let health = source.health().await;
        assert!(!health.poisoned, "a clean refresh clears any poison");
        assert_eq!(health.token_state, super::super::TokenState::Ok);
        assert!(health.last_refresh.is_some(), "refresh timestamp recorded");
    }

    // -- pin tests -------------------------------------------------------

    /// A /selected-collection that answers 401 with plain text:
    /// no error envelope, so the lifecycle code parses as None.
    async fn spawn_plain_401_gateway() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/selected-collection",
            get(|| async { (StatusCode::UNAUTHORIZED, "Unauthorized") }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// The full canned face with a switchable /selected-collection:
    /// 0 = healthy, 1 = 401 with a lifecycle envelope, 2 = 500
    async fn spawn_switchable_gateway() -> (String, Arc<std::sync::atomic::AtomicU8>) {
        use std::sync::atomic::{AtomicU8, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mode = Arc::new(AtomicU8::new(0));
        let m = mode.clone();
        let app = axum::Router::new()
            .route(
                "/selected-collection",
                get(move || {
                    let m = m.clone();
                    async move {
                        match m.load(Ordering::Relaxed) {
                            1 => (
                                StatusCode::UNAUTHORIZED,
                                axum::Json(json!({ "error": { "message": "revoked", "code": "key_revoked" } })),
                            )
                                .into_response(),
                            2 => (StatusCode::INTERNAL_SERVER_ERROR, "down").into_response(),
                            _ => axum::Json(json!({
                                "owner": "acc-x",
                                "collection": "user-col",
                                "description": "the account's own collection",
                                "sets": [
                                    { "name": "alpha", "description": "primary",
                                      "profiles": [
                                        { "family": "deepseek", "base_url": "http://gw.test:7501/v1",
                                          "model": "ds-x", "max_context_window": 64000,
                                          "effort": null, "store": null, "modalities": [],
                                          "api_key": "tagma-token-1" }
                                      ] }
                                ],
                                "default_set": "alpha"
                            })).into_response(),
                        }
                    }
                }),
            )
            .route(
                "/parking",
                get(|| async { axum::Json(json!({ "profiles": [] })) }),
            );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), mode)
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn bare_401_without_envelope_poisons_with_unknown_state() {
        crate::test_helpers::ensure_test_data_dir();
        let base = spawn_plain_401_gateway().await;
        let source = ProxyProfileSource::new(&base, "tagma-token-1").unwrap();
        source
            .handle_gateway_signal(&kallipai_adk::GatewaySignal {
                status: 401,
                code: None,
            })
            .await;
        let health = source.health().await;
        assert!(health.poisoned, "a bare 401 refresh poisons too");
        assert_eq!(health.token_state, super::super::TokenState::Unknown);
        let err = source.ensure_usable().await.unwrap_err();
        assert!(
            format!("{err:#}").contains("token rejected"),
            "got: {err:#}"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn poison_clears_only_through_a_successful_refresh() {
        use std::sync::atomic::Ordering;
        crate::test_helpers::ensure_test_data_dir();
        let (base, mode) = spawn_switchable_gateway().await;
        let source = ProxyProfileSource::new(&base, "tagma-token-1").unwrap();
        mode.store(1, Ordering::Relaxed);
        source
            .handle_gateway_signal(&kallipai_adk::GatewaySignal {
                status: 401,
                code: Some("key_revoked".into()),
            })
            .await;
        assert!(source.health().await.poisoned, "poisoned at the start");
        mode.store(0, Ordering::Relaxed);
        source
            .handle_gateway_signal(&kallipai_adk::GatewaySignal {
                status: 401,
                code: None,
            })
            .await;
        let health = source.health().await;
        assert!(!health.poisoned, "the clean refresh cleared the poison");
        assert_eq!(health.token_state, super::super::TokenState::Ok);
        assert!(source.ensure_usable().await.is_ok());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn runtime_refresh_failure_serves_the_last_good_snapshot() {
        use std::sync::atomic::Ordering;
        crate::test_helpers::ensure_test_data_dir();
        let (base, mode) = spawn_switchable_gateway().await;
        let source = ProxyProfileSource::new(&base, "tagma-token-1").unwrap();
        let first = source.load().await.unwrap();
        mode.store(2, Ordering::Relaxed);
        let second = source.load().await.unwrap();
        assert_eq!(
            format!("{second:?}"),
            format!("{first:?}"),
            "serves the in-memory snapshot"
        );
        let health = source.health().await;
        assert_eq!(health.refresh_failures, 1);
        assert!(!health.poisoned, "a network-side failure never poisons");
    }

    /// The degraded boot: an erroring gateway (500s) with no snapshot
    /// yields an empty config plus the health flag instead of failing the
    /// boot, while a runtime load() on the same cold source keeps the
    /// strict manual-recovery error (switch validation stays reachability-
    /// gated).
    #[tokio::test]
    #[serial_test::serial]
    async fn boot_load_degrades_when_the_gateway_returns_errors() {
        use std::sync::atomic::Ordering;
        crate::test_helpers::ensure_test_data_dir();
        let (base, mode) = spawn_switchable_gateway().await;
        mode.store(2, Ordering::Relaxed);
        let source = ProxyProfileSource::new(&base, "tagma-token-1").unwrap();
        let cfg = source.boot_load().await.unwrap();
        assert!(
            cfg.sets.is_empty(),
            "the degraded boot serves an empty config"
        );
        assert!(cfg.endpoints.is_empty());
        assert!(cfg.parking.is_empty());
        let health = source.health().await;
        assert!(health.degraded, "the health block flags the degraded boot");
        let err = source.load().await.unwrap_err();
        assert!(
            format!("{err:#}").contains("cannot be read from this tagma"),
            "got: {err:#}"
        );
    }

    /// Recovery: once the gateway answers again, the next strict load
    /// lands the real snapshot and clears the degraded flag.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_degraded_boot_recovers_on_the_next_successful_load() {
        use std::sync::atomic::Ordering;
        crate::test_helpers::ensure_test_data_dir();
        let (base, mode) = spawn_switchable_gateway().await;
        mode.store(2, Ordering::Relaxed);
        let source = ProxyProfileSource::new(&base, "tagma-token-1").unwrap();
        source.boot_load().await.unwrap();
        assert!(source.health().await.degraded);
        mode.store(0, Ordering::Relaxed);
        let recovered = source.load().await.unwrap();
        assert!(
            recovered.sets.contains_key("alpha"),
            "the real snapshot lands"
        );
        let health = source.health().await;
        assert!(!health.degraded, "the successful refresh cleared the flag");
        assert!(!health.poisoned);
    }

    /// Token death is not a degraded boot: the boot leg propagates the
    /// hard error and the poison flag, exactly like the strict face.
    #[tokio::test]
    #[serial_test::serial]
    async fn boot_load_still_fails_hard_on_a_dead_enrollment_token() {
        crate::test_helpers::ensure_test_data_dir();
        let base = spawn_rejecting_gateway(StatusCode::UNAUTHORIZED, "key_revoked").await;
        let source = ProxyProfileSource::new(&base, "tagma-token-1").unwrap();
        let err = source.boot_load().await.unwrap_err();
        assert!(
            format!("{err:#}").contains("rejected the enrollment token"),
            "got: {err:#}"
        );
        let health = source.health().await;
        assert!(!health.degraded, "token death is not a degraded boot");
        assert!(health.poisoned);
    }
}
