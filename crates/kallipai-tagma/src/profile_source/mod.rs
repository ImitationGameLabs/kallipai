//! Where model profiles come from (profile-proxy design, decision 9).
//!
//! [`ProfileSource`] is the data-acquisition face: every implementation
//! returns the same [`ProfileConfig`] shape, so the tagma-side construction
//! chain (`backend::build_backends` → `ProfileRegistry`) stays one shared
//! code path. Two implementations:
//!
//! - [`LocalProfileSource`]: read the on-disk
//!   `profiles.toml` (or the implicit env profile). A thin wrapper around
//!   `kallipai_adk::profile::load()`; zero behavior drift is the acceptance
//!   bar (existing tests keep their assertions).
//! - `ProxyProfileSource`: fetch
//!   sets/parking/default from the model gateway's distribution face and
//!   adapt them into the same shape; the local profiles.toml belongs to
//!   the local source alone and is never written in this mode.
//!
//! Distinct from [`kallipai_adk::profile::BackendSource`], which builds HTTP
//! backends per provider — this trait decides *which config* feeds that
//! chain, not how requests are sent. Acquisition and refresh are one
//! operation: a call on an already-loaded source re-fetches its current view.

use std::sync::Arc;

use anyhow::Result;
use kallipai_adk::profile::ProfileConfig;

mod proxy;

pub use proxy::ProxyProfileSource;
/// Which profile source serves the tagma. The choice lives in
/// settings.toml `[profiles.source].mode` (the boot reads it; the
/// management face can switch it at runtime through PUT /profiles).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProfileSourceKind {
    /// Read the local `profiles.toml` / implicit env profile (the mode's
    /// default when settings.toml carries no [profiles.source] entry).
    #[default]
    #[serde(rename = "local")]
    Local,
    /// Fetch from the model gateway's distribution face; the local
    /// profiles.toml stays untouched (memory is the only cache).
    #[serde(rename = "model-gateway")]
    Proxy,
}

/// The wire/settings spelling for each variant (`local` / `model-gateway`):
/// serde, settings.toml, and the management-face health block all read
/// through these, so the spellings cannot drift apart across domains.
impl ProfileSourceKind {
    /// The spelling used in settings.toml and JSON responses.
    pub fn as_wire_str(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Proxy => "model-gateway",
        }
    }

    /// Parse the settings/wire spelling; every other word is `None` and
    /// the caller decides whether that is a default or a hard error.
    pub fn from_wire_str(word: &str) -> Option<Self> {
        match word {
            "local" => Some(Self::Local),
            "model-gateway" => Some(Self::Proxy),
            _ => None,
        }
    }
}

/// Profile/sets data acquisition, refreshable: [`Self::load`] returns the
/// source's current view of the world in the stock [`ProfileConfig`] shape.
#[async_trait::async_trait]
pub trait ProfileSource: Send + Sync {
    /// Which kind this source is. The management face consults it to guard
    /// the mutating routes: under the gateway source the in-memory
    /// snapshot mirrors the gateway, and local writes would be silently
    /// overwritten by the next refresh.
    fn kind(&self) -> ProfileSourceKind;

    async fn load(&self) -> Result<ProfileConfig>;
    /// The boot-shaped acquisition: same fetch as [`Self::load`], except
    /// that the cold leg of a proxy source (a refresh-class failure with
    /// no snapshot yet) degrades to an empty config instead of failing
    /// the boot — the tagma starts unconfigured, the health block flags
    /// `degraded`, and a background repull task retries. A dead
    /// enrollment token stays a hard error, and runtime consumers keep
    /// calling [`Self::load`], whose strict semantics are unchanged.
    /// Default: identical to `load` (a local source cannot be
    /// unreachable).
    async fn boot_load(&self) -> Result<ProfileConfig> {
        self.load().await
    }
    /// The source's latest known-good snapshot, when it keeps one (proxy:
    /// the last successful refresh; local sources hold nothing). The
    /// repull task compares it against the live profile bundle to decide
    /// whether a fetch that already succeeded still has to land: a
    /// refresh that cannot build into a registry clears the degraded
    /// flag while the bundle stays stale, and only this comparison
    /// keeps the retry task armed.
    async fn snapshot(&self) -> Option<ProfileConfig> {
        None
    }
    /// Refuse new work when the source is known-poisoned (proxy: a refresh
    /// confirmed the enrollment token dead). Consulted before a spawn
    /// resolves its profile and before a profile-set bind commits.
    /// Default: always usable — local sources hold no enrollment token.
    async fn ensure_usable(&self) -> Result<()> {
        Ok(())
    }

    /// The management-face health view (the GET /profiles source block).
    /// Default: the local shape — nothing poisoned, no token.
    async fn health(&self) -> SourceHealth {
        SourceHealth::default()
    }

    /// React to one gateway auth signal (a provider 401/403 that survived
    /// a whole failover chain). Default: ignore — a local source has no
    /// enrollment token, so the rejection belongs to the upstream
    /// provider.
    async fn handle_gateway_signal(&self, _signal: &kallipai_adk::GatewaySignal) {}
}

/// Management-face health view of a profile source (GET /profiles):
/// additive JSON members on the existing response shape.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SourceHealth {
    /// A refresh confirmed the enrollment token dead: new spawns and
    /// profile-set binds are refused until a successful refresh clears
    /// the flag.
    pub poisoned: bool,
    /// A boot-time fetch failed with no snapshot: the source serves an
    /// empty config while the repull task retries in the background;
    /// the first successful refresh clears it. Always false for a
    /// local source.
    pub degraded: bool,
    /// Last observed enrollment-token state.
    pub token_state: TokenState,
    /// When the last successful refresh landed (process clock),
    /// serialized as RFC 3339 for the management face.
    #[serde(serialize_with = "serialize_rfc3339_opt")]
    pub last_refresh: Option<std::time::SystemTime>,
    /// Consecutive failed refreshes of any kind.
    #[serde(rename = "refresh_failure_count")]
    pub refresh_failures: u64,
}
fn serialize_rfc3339_opt<S>(
    value: &Option<std::time::SystemTime>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match value {
        None => serializer.serialize_none(),
        Some(t) => {
            let text = time::OffsetDateTime::from(*t)
                .format(&time::format_description::well_known::Rfc3339)
                .map_err(serde::ser::Error::custom)?;
            serializer.serialize_str(&text)
        }
    }
}

/// Last observed enrollment-token health (the gateway lifecycle codes,
/// folded).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenState {
    /// No known token problem (also the local-mode default: no token
    /// at all).
    #[default]
    Ok,
    /// The face rejected the token as expired.
    Expired,
    /// The face rejected the token as revoked.
    Revoked,
    /// The face rejected the token without a parseable lifecycle code.
    Unknown,
}

/// Consume gateway auth signals: one refresh per signal, the poison
/// decision riding on the refresh outcome — never on the signal alone
/// (a rejection may predate the refresh that already fixed it). Runs
/// for the process lifetime; the sender half lives on AppState. The
/// slot is loaded per signal so a source switch (PUT /profiles)
/// redirects later signals to the new source.
pub fn spawn_gateway_signal_task(
    source: Arc<arc_swap::ArcSwap<SourceSlot>>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<kallipai_adk::GatewaySignal>,
) {
    tokio::spawn(async move {
        while let Some(signal) = rx.recv().await {
            let current = source.load_full().source();
            current.handle_gateway_signal(&signal).await;
        }
    });
}

/// The gateway connection parameters a source switch needs. The
/// base is derived from the active relay entry's platform origin
/// (`{origin}/v1/model-gateway`); the token half is that entry's
/// stored enrollment token (relay enrollment; no separate key
/// material). Held on the AppState exactly when the live source
/// is the model gateway; a switch re-resolves them from the
/// platform table when the request carries no polis, so the slot
/// never gates the switchability alone. The origin rides along
/// so the management face can name the active platform without
/// re-walking the relay entries.
#[derive(Clone)]
pub struct GatewayParams {
    pub base: String,
    pub origin: String,
    pub token: String,
}

/// The token is platform credential material: `Debug` prints it as
/// `[REDACTED]` (the TagmaIdentity precedent on the gateway side).
impl std::fmt::Debug for GatewayParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayParams")
            .field("base", &self.base)
            .field("origin", &self.origin)
            .field("token", &"[REDACTED]")
            .finish()
    }
}
/// Sized wrapper around the boxed source: ArcSwap's refcount bound
/// needs a Sized pointee, and `dyn ProfileSource` is not. The slot
/// stores this instead and hands out the inner Arc.
pub struct SourceSlot(Arc<dyn ProfileSource>);

impl SourceSlot {
    pub fn new(inner: Arc<dyn ProfileSource>) -> Self {
        Self(inner)
    }

    pub fn source(&self) -> Arc<dyn ProfileSource> {
        self.0.clone()
    }
}

/// The local source: exactly what the tagma booted with before the source
/// abstraction existed, wrapped so the boot path has one shape.
#[derive(Debug, Default)]
pub struct LocalProfileSource;

impl LocalProfileSource {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl ProfileSource for LocalProfileSource {
    fn kind(&self) -> ProfileSourceKind {
        ProfileSourceKind::Local
    }

    async fn load(&self) -> Result<ProfileConfig> {
        kallipai_adk::profile::load()
    }
}

/// Resolve the source for `kind`. The mode comes from settings.toml
/// (read by the caller at boot, or chosen by a PUT /profiles switch);
/// the gateway source needs the boot-derived connection parameters
/// (`None` when no relay entry carries a stored enrollment token),
/// and the constructor
/// validates them so a misconfigured boot fails fast with an
/// actionable message.
pub fn build(
    kind: ProfileSourceKind,
    gateway: Option<&GatewayParams>,
) -> Result<Arc<dyn ProfileSource>> {
    match kind {
        ProfileSourceKind::Local => Ok(Arc::new(LocalProfileSource::new())),
        ProfileSourceKind::Proxy => {
            let params = gateway.ok_or_else(|| {
                anyhow::anyhow!(
                    "settings mode model-gateway needs a relay entry with a stored enrollment token (relay enrollment on the platform; see KALLIPAI_POLIS_URL / polis.toml); the gateway base is derived from that entry's platform origin"
                )
            })?;
            Ok(Arc::new(ProxyProfileSource::new(
                &params.base,
                &params.token,
            )?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `unwrap_err` needs the Ok side to be `Debug`; the boxed source is not,
    /// so tests pull the error out with a match instead.
    fn build_err(kind: ProfileSourceKind, gateway: Option<&GatewayParams>) -> anyhow::Error {
        match build(kind, gateway) {
            Ok(_) => panic!("expected build to fail"),
            Err(err) => err,
        }
    }

    /// The serde rename and the as_wire_str table must agree: one
    /// spelling per variant across serde, settings.toml, and the
    /// health block.
    #[test]
    fn wire_spellings_agree_with_serde() {
        for kind in [ProfileSourceKind::Local, ProfileSourceKind::Proxy] {
            let serde_name = serde_json::to_string(&kind).unwrap();
            assert_eq!(serde_name, format!("\"{}\"", kind.as_wire_str()));
            let parsed = serde_json::from_str::<ProfileSourceKind>(&serde_name).unwrap();
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn health_serializes_last_refresh_as_rfc3339() {
        let health = SourceHealth {
            last_refresh: Some(std::time::UNIX_EPOCH),
            ..Default::default()
        };
        let text = serde_json::to_string(&health).unwrap();
        assert!(
            text.contains("\"last_refresh\":\"1970-01-01T00:00:00Z\""),
            "{text}"
        );
        let none = serde_json::to_string(&SourceHealth::default()).unwrap();
        assert!(none.contains("\"last_refresh\":null"), "{none}");
    }

    #[test]
    fn build_local_succeeds() {
        build(ProfileSourceKind::Local, None).expect("local source builds");
    }

    #[test]
    fn proxy_without_params_names_the_enrollment_requirement() {
        let err = build_err(ProfileSourceKind::Proxy, None);
        let text = format!("{err:#}");
        assert!(text.contains("KALLIPAI_POLIS_URL"), "got: {text}");
        assert!(text.contains("enrollment token"), "got: {text}");
    }

    #[test]
    fn proxy_with_a_schemeless_origin_names_the_platform_config() {
        let params = GatewayParams {
            base: "api.example.com/v1/model-gateway".to_owned(),
            origin: "api.example.com".to_owned(),
            token: "tagma-token-1".to_owned(),
        };
        let err = build_err(ProfileSourceKind::Proxy, Some(&params));
        let text = format!("{err:#}");
        assert!(text.contains("KALLIPAI_POLIS_URL"), "got: {text}");
    }

    #[test]
    fn local_source_passes_the_env_profile_through() {
        // Zero-drift pin: the wrapper returns exactly what the historical
        // `kallipai_adk::profile::load()` resolves — whichever path the
        // process ends up on. The env vars alone do not guarantee the env
        // path (another test's instance-roots injection + a written
        // profiles.toml would win), so the stable contract is equivalence;
        // the env-profile content is additionally pinned only when the
        // roots are demonstrably absent (deterministic env fallback).
        temp_env::with_vars(
            [
                ("KALLIPAI_TAGMA_SLUG", None::<&str>),
                ("KALLIPAI_LLM_PROVIDER", Some("deepseek")),
                ("KALLIPAI_LLM_MODEL", Some("deepseek-test")),
                ("KALLIPAI_LLM_DEEPSEEK_API_KEY", Some("fake")),
                ("KALLIPAI_CONTEXT_WINDOW_TOKENS", Some("200000")),
            ],
            || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("test runtime");
                let via_wrapper = runtime
                    .block_on(LocalProfileSource::new().load())
                    .expect("profile loads");
                let direct = kallipai_adk::profile::load().expect("direct load");
                assert_eq!(format!("{via_wrapper:?}"), format!("{direct:?}"));
                if kallipai_adk::persistence::data_dir_root().is_err() {
                    // No instance roots: the config-file path cannot resolve,
                    // so both loads deterministically took the env profile.
                    let profile = &direct.sets["default"].profiles[0];
                    assert_eq!(profile.model, "deepseek-test");
                    assert_eq!(direct.default, "default");
                }
            },
        );
    }

    #[test]
    fn build_proxy_constructs_the_real_source() {
        let params = GatewayParams {
            base: "http://gw.test:7501/v1/model-gateway".to_owned(),
            origin: "http://gw.test:7501".to_owned(),
            token: "tagma-token-1".to_owned(),
        };
        build(ProfileSourceKind::Proxy, Some(&params))
            .expect("the proxy source constructs from a stored token");
    }
}
