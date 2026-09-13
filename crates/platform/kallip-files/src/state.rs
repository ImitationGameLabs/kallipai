//! Service state and runtime assembly: configuration, the shared
//! [`AppState`], the router, and the boot sequence ([`run`]).

use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

use crate::metadata::{self, Db};
use axum::Router;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::api;
use crate::auth::FilesControlPlane;
use crate::gc::GcConfig;
use kallip_archeion_common::control_plane::ControlPlane;
use kallip_blob_store::{BlobStore, LocalBackend};

/// Static service configuration, resolved once at boot.
#[derive(Debug, Clone)]
pub struct FilesConfig {
    /// Maximum accepted upload body, in bytes; a longer stream is cut off
    /// with 413. The value is a size ceiling only (default 100 MB; the
    /// operator may tune it), never a chunking boundary.
    pub max_body_bytes: u64,
    /// Comma-separated CORS allowed origins (the app's origin). Empty = no
    /// cross-origin allowed (the allowlist is `AllowOrigin::list`, never
    /// `Any`); see `Args::cors_origins`.
    pub cors_origins: String,
    /// Archeion degrade posture (seventh approved default). `false` (default)
    /// is fail-closed: a registry that cannot answer produces 503 and no
    /// decision. `true` is fail-soft: the enrollment lookup degrading to an
    /// empty fact set turns tagma decisions into denials (403) instead of
    /// 503s. It never weakens verification -- with the registry down, no
    /// request authenticates either way.
    pub degrade_fail_soft: bool,
    /// Garbage collection cadence (sweep + reconcile per tick; the first
    /// tick fires immediately, which is the startup audit).
    pub gc: GcConfig,
}

/// Shared state handed to every handler and extractor.
#[derive(Clone)]
pub struct AppState {
    /// Metadata database (migrated at boot).
    pub db: Db,
    /// The blob store behind the object-safe seam.
    pub blob: Arc<dyn BlobStore>,
    /// Blob root path, for the reconcile walk (the store trait has no
    /// directory listing; reconciliation is root-aware by design).
    pub blob_root: PathBuf,
    /// The archeion control-plane client.
    pub control: Arc<dyn ControlPlane>,
    /// Static configuration.
    pub config: Arc<FilesConfig>,
    /// Best-effort lesche event-push client; `None` disables the push
    /// (an unset notify URL/token is the documented safe posture).
    pub notify: Option<Arc<dyn crate::notify::NotifyPusher>>,
}

/// Everything the service needs at boot. `main` fills it from CLI/env; the
/// integration smoke test fills it directly against a test Postgres.
pub struct BootConfig {
    /// Address to bind (behind a TLS-terminating reverse proxy).
    pub listen_addr: String,
    /// Postgres URL for the metadata store.
    pub database_url: String,
    /// Archeion internal base URL for `/internal/*` calls.
    pub archeion_internal_url: String,
    /// Shared secret bearer for the archeion internal API.
    pub archeion_internal_token: String,
    /// Lesche internal base URL + shared secret for the file-delivered
    /// event push; an empty URL disables the push.
    pub notify_url: String,
    pub notify_token: String,
    /// Root directory of the blob store (the reconciler walks it; the
    /// store trait itself has no directory listing).
    pub blob_root: std::path::PathBuf,
    /// The files-specific statics.
    pub files: FilesConfig,
}

/// The authenticated API surface. `/health` is a deliberate no-auth route
/// (compose healthcheck / Caddy and baseline acceptance; the instances'
/// health route is the precedent).
pub fn router(state: AppState) -> Router {
    let cors = cors_layer(&state.config.cors_origins);
    Router::new()
        .route(
            "/",
            axum::routing::put(api::put::put_file).get(api::list::list_files),
        )
        .route(
            "/{id}",
            axum::routing::get(api::get::get_file)
                .head(api::get::head_file)
                .delete(api::delete_file),
        )
        .route("/{id}/send", axum::routing::post(api::send::send_file))
        .route(
            "/admin/delivery-events",
            axum::routing::get(api::admin::list_events),
        )
        .route("/health", axum::routing::get(api::health))
        .with_state(state)
        .layer(cors)
}

/// The CORS gate for browser callers: the web app lives on a different
/// origin than this service (e.g. `files.<domain>` vs `app.<domain>`), so
/// uploads/downloads from the browser are cross-origin and preflighted.
/// Same shape as the archeion's layer (the twin implementation): origins are
/// an explicit comma-separated allowlist (an empty config yields an empty
/// allowlist, never a wildcard), methods are enumerated because
/// `allow_credentials(true)` + `Any` is forbidden by the Fetch spec, and
/// the headers list carries the ones the web client actually sends.
fn cors_layer(origins: &str) -> CorsLayer {
    let allowed: Vec<axum::http::HeaderValue> = origins
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    let origin = if allowed.is_empty() {
        AllowOrigin::list(Vec::new())
    } else {
        AllowOrigin::list(allowed)
    };
    CorsLayer::new()
        .allow_origin(origin)
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::PUT,
            axum::http::Method::POST,
            axum::http::Method::DELETE,
            axum::http::Method::HEAD,
        ])
        .allow_credentials(true)
        .allow_headers([
            axum::http::header::AUTHORIZATION,
            axum::http::header::CONTENT_TYPE,
            axum::http::header::ACCEPT,
            axum::http::HeaderName::from_static("x-requested-with"),
        ])
}

/// Boot sequence: connect and migrate the metadata store, build the state,
/// start the GC driver (its first tick is the startup audit), and serve.
/// Migration failure is fatal (fail fast: connect, migrate, or exit) --
/// the caller sees the error and the process exits nonzero.
pub async fn run(boot: BootConfig) -> Result<(), Box<dyn Error + Send + Sync>> {
    let db = metadata::connect_and_migrate(&boot.database_url).await?;
    let state = AppState {
        db: db.clone(),
        blob: LocalBackend::arc(&boot.blob_root),
        blob_root: boot.blob_root.clone(),
        control: Arc::new(FilesControlPlane::new(
            boot.archeion_internal_url,
            boot.archeion_internal_token,
        )),
        notify: crate::notify::LescheNotifyClient::new(boot.notify_url, boot.notify_token)
            .map(|c| std::sync::Arc::new(c) as _),
        config: Arc::new(boot.files),
    };
    spawn_gc_driver(state.clone());
    let listener = tokio::net::TcpListener::bind(&boot.listen_addr).await?;
    axum::serve(listener, router(state)).await?;
    Ok(())
}

/// Start the background GC driver: sweep + reconcile each interval tick.
/// The first `interval` tick fires immediately, so the driver doubles as
/// the startup audit, then the periodic self-check. Drift
/// found by reconcile is a warning, never an error: reconciliation is a
/// detector, not a repairer (the GC module owns that distinction).
pub fn spawn_gc_driver(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(state.config.gc.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            match crate::gc::sweep(&state.db, state.blob.as_ref(), &state.config.gc).await {
                Ok(report) => {
                    if report.catalog_reclaimed > 0 {
                        tracing::info!(reclaimed = report.catalog_reclaimed, "gc sweep");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "gc sweep failed"),
            }
            match crate::gc::reconcile(&state.db, &state.blob_root).await {
                Ok(report) => {
                    if !report.missing_blobs.is_empty() || !report.orphan_files.is_empty() {
                        tracing::warn!(
                            missing_blobs = report.missing_blobs.len(),
                            orphan_files = report.orphan_files.len(),
                            "catalog/store drift detected"
                        );
                    }
                }
                Err(e) => tracing::warn!(error = %e, "gc reconcile failed"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::header::{
        ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_REQUEST_METHOD,
        ORIGIN,
    };
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*;

    /// The preflight surface: an allowed origin gets the method list back,
    /// mirroring the archeion's pinned preflight test (the twin layer).
    #[tokio::test]
    async fn preflight_from_an_allowed_origin_advertises_methods() {
        let app = Router::new()
            .route("/health", get(api::health))
            .layer(cors_layer("https://app.example"));
        let request = Request::builder()
            .method(axum::http::Method::OPTIONS)
            .header(ORIGIN, "https://app.example")
            .header(ACCESS_CONTROL_REQUEST_METHOD, "PUT")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let advertised = response
            .headers()
            .get(ACCESS_CONTROL_ALLOW_METHODS)
            .expect("allowed preflight advertises the method list")
            .to_str()
            .unwrap();
        let mut advertised: Vec<&str> = advertised.split(',').map(str::trim).collect();
        advertised.sort_unstable();
        assert_eq!(advertised, vec!["DELETE", "GET", "HEAD", "POST", "PUT"]);
    }

    /// An empty allowlist is the safe default: a request from any origin
    /// gets no ACAO header, so the browser rejects the response. This pins
    /// the misconfigured-`*`-yields-empty behavior rather than an open
    /// cross-origin hole.
    #[tokio::test]
    async fn an_empty_allowlist_denies_every_origin() {
        let app = Router::new()
            .route("/health", get(api::health))
            .layer(cors_layer(""));
        let request = Request::builder()
            .method(axum::http::Method::OPTIONS)
            .header(ORIGIN, "https://app.example")
            .header(ACCESS_CONTROL_REQUEST_METHOD, "PUT")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert!(
            response
                .headers()
                .get(ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );
    }
}
