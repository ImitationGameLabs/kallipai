//! `kallip-archeion`: the registry / control plane (identity, WebAuthn, tagma
//! lifecycle) for the kallip relay.
//!
//! The archeion owns the durable Postgres store and exposes a narrow `ControlPlane`
//! trait (in `kallip-archeion-common`). The data-plane relay (`kallip-lesche`) is a
//! separate process that consumes that trait over the `/internal/*` HTTP API
//! served here (each handler wraps the DB-backed `DbControlPlane`, guarded by a
//! shared-secret bearer). If no internal-token file is configured, the
//! `/internal` nest is not mounted and the archeion runs standalone.

mod args;
mod auth;
mod boot_secrets;
mod clientip;
mod code;
mod control_plane;
mod db;
mod email;
#[cfg(test)]
mod integration;
mod middleware;
mod notify;
mod oauth;
mod ratelimit;
mod routes;
mod session;
mod state;
#[cfg(test)]
mod test_helpers;
mod token;
mod username;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use kallip_common::authtoken::{MintedToken, TokenHash};
use tracing::{info, warn};

use args::Args;
use state::{AppState, Limits};

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // File logging is opt-in via KALLIP_ARCHEION_LOG_DIR: set, events are
    // double-written to a rolling file and stdout; unset keeps the historical
    // stdout-only behavior, so container and dev forms are untouched.
    let log_dir =
        kallip_common::logging::parse_log_dir(std::env::var("KALLIP_ARCHEION_LOG_DIR").ok());
    kallip_common::logging::init_service_logging(&filter, "archeion", log_dir.as_deref());

    // Admin token lifecycle: an operator-provided token is a pinned asset and
    // stays where the operator put it; a generated one is provisioned into
    // persistent state on first boot and then only read -- it outlives
    // restarts, and rotation is a deliberate act (`kallip-admin admin-token`
    // reset`). Either way the plaintext never reaches the journal -- the
    // banner names the source, not the value, and only the hash is retained
    // beyond this boot.
    let admin_pinned = args.admin_token.is_some();
    let (admin, admin_source) = match args.admin_token.clone() {
        Some(s) => (
            MintedToken::from_secret(s),
            "KALLIP_ARCHEION_ADMIN_TOKEN / --admin-token".to_owned(),
        ),
        None => {
            let out = args.admin_token_out_file.as_deref().context(concat!(
                "no admin token configured: pin one via --admin-token ",
                "(or KALLIP_ARCHEION_ADMIN_TOKEN), or name an output file ",
                "via --admin-token-out-file (or ",
                "KALLIP_ARCHEION_ADMIN_TOKEN_OUT_FILE) to have one generated on ",
                "first boot; the NixOS module configures the output file automatically",
            ))?;
            let secret = boot_secrets::provision_admin_token(out)?;
            let generated = MintedToken::from_secret(secret);
            info!(
                "admin token file at {} (0600); generated on first boot, read as-is after -- rotate via kallip-admin admin-token reset",
                out.display()
            );
            (
                generated,
                format!("minted at {} (stable across restarts)", out.display()),
            )
        }
    };
    println!("==================================================");
    println!("  kallip-archeion {}", env!("CARGO_PKG_VERSION"));
    println!("  Admin token active (source: {admin_source})");
    println!("==================================================");

    // The local-platform admin-login turns the admin token into a User-session
    // mint, so when it is enabled a weak operator-chosen token is not just an
    // admin CLI credential anymore: refuse to boot with one. The generated
    // token is 256-bit and exempt from the length floor.
    const MIN_ADMIN_TOKEN_LEN: usize = 32;
    if args.admin_user_login
        && let Some(s) = args.admin_token.as_deref()
        && s.len() < MIN_ADMIN_TOKEN_LEN
    {
        anyhow::bail!(
            "KALLIP_ARCHEION_ADMIN_USER_LOGIN is enabled but KALLIP_ARCHEION_ADMIN_TOKEN is \
                 shorter than {MIN_ADMIN_TOKEN_LEN} chars; set a strong token or unset the flag"
        );
    }

    let limits = Limits {
        max_body_size_bytes: body_size_bytes(args.max_body_size_kb),
        enrollment_code_ttl: Duration::from_secs(args.enrollment_code_ttl_secs),
    };

    // Connect to Postgres (retrying with a capped backoff) and apply pending
    // migrations before serving a single request.
    let db = crate::db::connect_and_migrate(&args.database_url).await?;

    // Build the WebAuthn relying party via the high-level wrapper's safe
    // builder (validates rp_id is an effective domain of rp_origin) AND a bare
    // core from the same config (discoverable registration needs the core, since
    // the wrapper hardcodes require_resident_key=false). The session-cookie
    // config and per-IP auth rate limiter come from the boot args.
    let rp_origin = url::Url::parse(&args.webauthn_rp_origin)
        .map_err(|e| anyhow::anyhow!("invalid KALLIP_ARCHEION_WEBAUTHN_RP_ORIGIN: {e}"))?;
    let (webauthn, webauthn_core) = crate::state::build_webauthn_pair(
        &args.webauthn_rp_name,
        &args.webauthn_rp_id,
        &rp_origin,
        args.webauthn_allow_any_port,
        false,
    )
    .map_err(|e| anyhow::anyhow!("WebAuthn RP config invalid: {e}"))?;
    let session_cfg = session::SessionCfg {
        ttl: Duration::from_secs(args.session_ttl_secs),
        cookie_secure: args.cookie_secure,
        cookie_domain: args.cookie_domain.clone(),
    };
    let auth_rate_limiter =
        ratelimit::IpRateLimiter::new(args.auth_rate_capacity, args.auth_rate_refill_per_sec);
    let pair_rate_limiter =
        ratelimit::GlobalRateLimiter::new(args.pair_rate_capacity, args.pair_rate_refill_per_sec);

    // Shared HTTP client for outbound OAuth round-trips. Request-bounded (each
    // call is a token exchange + userinfo fetch with a natural end); rustls via
    // the workspace reqwest features; no cookie jar.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| anyhow::anyhow!("build reqwest client: {e}"))?;

    // Build the OAuth provider registry from config. A provider is enabled iff
    // BOTH its client id and secret are non-empty. redirect_base is required
    // only when at least one provider is configured (the canonical redirect_uri
    // is derived from it); an empty registry runs the archeion with OAuth disabled.
    let oauth_providers = build_oauth_registry(&args)?;

    // Parse the trusted-proxy CIDRs. The default trusts loopback (correct for
    // the default same-box reverse-proxy deploy). When the archeion binds a
    // non-loopback address and the operator left the default in place, force
    // the set empty: trusting loopback XFF on a publicly-bound socket would let
    // any co-resident process forge XFF and evade per-client limiting. An
    // operator behind a loopback proxy on a public bind must set
    // KALLIP_ARCHEION_TRUSTED_PROXIES explicitly. Compare parsed CIDR sets (not
    // raw strings) so a semantically-identical default spelled differently
    // (whitespace, order) is still treated as "left at the default".
    let mut trusted_proxies = parse_trusted_proxies(&args.trusted_proxies);
    let explicit_trusted = trusted_proxies != parse_trusted_proxies(args::DEFAULT_TRUSTED_PROXIES);
    if !explicit_trusted && !is_loopback_bind(&args.listen_addr) && !trusted_proxies.is_empty() {
        warn!(
            "listen_addr {addr} is publicly bound but trusted_proxies is the loopback default; \
             clearing it to avoid XFF spoofing. Set KALLIP_ARCHEION_TRUSTED_PROXIES explicitly to \
             trust a reverse proxy on this bind.",
            addr = args.listen_addr
        );
        trusted_proxies.clear();
    }
    info!(
        trusted_proxies = ?trusted_proxies,
        "resolved trusted proxy CIDRs for X-Forwarded-For"
    );

    let state: Arc<AppState> = Arc::new(AppState::new(
        admin.hash().clone(),
        admin_pinned,
        args.admin_token_out_file.clone(),
        limits,
        db,
        webauthn,
        webauthn_core,
        session_cfg,
        auth_rate_limiter,
        pair_rate_limiter,
        trusted_proxies,
        http,
        oauth_providers,
        args.signup_enabled,
    ));

    // The data-plane services (lesche, files, instances) call the archeion's
    // `/internal/*` ControlPlane API with a shared secret. That secret is
    // machine-internal alignment material — the four services only need to
    // agree, the value is regenerable, and it never leaves this host — so the
    // archeion owns its lifecycle instead of asking the operator to ship it:
    // first boot generates it into the state directory, later boots read the
    // existing value. Never overwriting is the load-bearing rule: consumers
    // serve with the value they read at their own boot, so a rewrite would
    // silently split the platform into token generations. An unset path runs
    // the archeion standalone (no /internal nest mounted).
    let internal_hash = args
        .internal_token_file
        .as_deref()
        .map(boot_secrets::provision_internal_token)
        .transpose()?
        .map(|secret| TokenHash::of(&secret));

    let app = routes::router(state.clone(), internal_hash, args.admin_user_login);

    // Background sweep of expired WebAuthn ceremonies. Decoupled from the
    // request path so the DELETE never adds latency to a ceremony begin.
    // Shutdown is honoured: the select is on the sleep, not the query, so an
    // in-flight DELETE still completes.
    {
        let sweep_db = state.db.clone();
        let shutdown = state.shutdown.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            // `interval` fires its first tick immediately; consume it so the
            // sweep does not run once at boot (before anything could expire).
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        crate::db::gc_expired_challenges(&sweep_db).await;
                        crate::db::gc_expired_pairing_codes(&sweep_db).await;
                        crate::db::gc_expired_oauth_states(&sweep_db).await;
                    }
                    _ = shutdown.cancelled() => break,
                }
            }
        });
    }

    // Outermost layers: body limit, then CORS (explicit allowlist, never Any),
    // then request tracing.
    let app = app
        .layer(axum::extract::DefaultBodyLimit::max(
            state.limits.max_body_size_bytes,
        ))
        .layer(routes::cors_layer(&args.cors_origins))
        .layer(tower_http::trace::TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&args.listen_addr)
        .await
        .with_context(|| format!("binding listen addr {}", args.listen_addr))?;
    info!(addr = %args.listen_addr, "archeion listening");
    let shutdown_token = state.shutdown.clone();
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(shutdown_token))
    .await?;

    Ok(())
}

/// Resolve the body-size limit in bytes. `0` means "use axum's default" (2 MB);
/// any other value is kilobytes.
fn body_size_bytes(max_body_size_kb: usize) -> usize {
    if max_body_size_kb > 0 {
        max_body_size_kb * 1024
    } else {
        2 * 1024 * 1024
    }
}

/// Build the OAuth [`crate::oauth::ProviderRegistry`] from boot config. A
/// provider is enabled iff BOTH its client id and secret are non-empty.
/// `redirect_base` is required only when at least one provider is configured
/// (it derives the canonical redirect_uri); an empty registry runs the archeion
/// with OAuth disabled.
fn build_oauth_registry(args: &Args) -> Result<crate::oauth::ProviderRegistry> {
    use crate::oauth::{GitHubProvider, GoogleProvider, OAuthConfig, ProviderRegistry};

    let github = configured(
        &args.oauth_github_client_id,
        &args.oauth_github_client_secret,
    );
    let google = configured(
        &args.oauth_google_client_id,
        &args.oauth_google_client_secret,
    );
    let any = github || google;
    let redirect_base = match &args.oauth_redirect_base {
        Some(s) if any => Some(
            url::Url::parse(s)
                .map_err(|e| anyhow::anyhow!("invalid KALLIP_ARCHEION_OAUTH_REDIRECT_BASE: {e}"))?,
        ),
        Some(_) => {
            warn!(
                "KALLIP_ARCHEION_OAUTH_REDIRECT_BASE is set but no OAuth provider is configured; ignoring"
            );
            None
        }
        None if any => {
            anyhow::bail!(
                "KALLIP_ARCHEION_OAUTH_REDIRECT_BASE is required when an OAuth provider is configured"
            )
        }
        None => None,
    };

    let mut providers: Vec<Box<dyn crate::oauth::OAuthProvider>> = Vec::new();
    // `redirect_base` is `Some` iff at least one provider is configured (above),
    // so the `expect` is invariant-satisfied, not a real panic surface.
    if github {
        let (id, secret) = pair(
            &args.oauth_github_client_id,
            &args.oauth_github_client_secret,
        );
        let base = redirect_base
            .as_ref()
            .expect("github configured => redirect_base set")
            .clone();
        info!("OAuth provider enabled: github");
        providers.push(Box::new(GitHubProvider::new(OAuthConfig {
            client_id: id,
            client_secret: secret,
            redirect_base: base,
        })));
    }
    if google {
        let (id, secret) = pair(
            &args.oauth_google_client_id,
            &args.oauth_google_client_secret,
        );
        let base = redirect_base
            .as_ref()
            .expect("google configured => redirect_base set")
            .clone();
        info!("OAuth provider enabled: google");
        providers.push(Box::new(GoogleProvider::new(OAuthConfig {
            client_id: id,
            client_secret: secret,
            redirect_base: base,
        })));
    }
    Ok(ProviderRegistry::new(providers))
}

/// Whether a provider's client id + secret are BOTH set (non-empty).
fn configured(id: &Option<String>, secret: &Option<String>) -> bool {
    matches!(id.as_deref(), Some(s) if !s.trim().is_empty())
        && matches!(secret.as_deref(), Some(s) if !s.trim().is_empty())
}

/// Unwrap a configured id/secret pair (caller checked [`configured`]).
fn pair(id: &Option<String>, secret: &Option<String>) -> (String, String) {
    (
        id.as_ref().expect("checked").trim().to_string(),
        secret.as_ref().expect("checked").trim().to_string(),
    )
}

/// Parse a comma-separated CIDR list into a sorted, de-duplicated vector of
/// `IpNet`. Unparseable entries are warned-and-skipped (a misconfiguration does
/// not abort boot). Sorting makes the result order-independent so two strings
/// naming the same set compare equal.
fn parse_trusted_proxies(raw: &str) -> Vec<ipnet::IpNet> {
    let mut nets: Vec<ipnet::IpNet> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| match s.parse() {
            Ok(net) => Some(net),
            Err(_) => {
                warn!(value = %s, "ignoring unparseable trusted-proxy CIDR");
                None
            }
        })
        .collect();
    nets.sort();
    nets.dedup();
    nets
}

/// Whether `listen_addr` binds a loopback address. Used by the trusted-proxy
/// footgun guard: trusting loopback XFF is only safe when the socket is itself
/// loopbound (so no external peer can reach it). A parse failure is treated as
/// non-loopback (fail-safe: clear trust).
fn is_loopback_bind(listen_addr: &str) -> bool {
    // Take the host portion before the port.
    let host = listen_addr.rsplit_once(':').map(|(h, _)| h).unwrap_or("");
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_loopback(),
        Ok(std::net::IpAddr::V6(v6)) => v6.is_loopback(),
        Err(_) => false,
    }
}

async fn shutdown_signal(token: tokio_util::sync::CancellationToken) {
    let ctrl_c = tokio::signal::ctrl_c();
    let sigterm = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = sigterm => {},
    }
    info!("received shutdown signal, initiating graceful shutdown");
    token.cancel();
}
