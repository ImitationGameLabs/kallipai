//! `kallip-agora`: the registry / control plane (identity, WebAuthn, tagma
//! lifecycle, invites) for the kallip relay.
//!
//! The agora owns the durable Postgres store and exposes a narrow `ControlPlane`
//! trait (in `kallip-agora-common`). The data-plane relay (`kallip-lesche`) is a
//! separate process that consumes that trait over the `/internal/*` HTTP API
//! served here (each handler wraps the DB-backed `DbControlPlane`, guarded by a
//! shared-secret bearer). If `KALLIP_AGORA_INTERNAL_TOKEN` is unset, the
//! `/internal` nest is not mounted and the agora runs standalone.

mod args;
mod auth;
mod clientip;
mod control_plane;
mod db;
mod email;
#[cfg(test)]
mod integration;
mod middleware;
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

use anyhow::Result;
use clap::Parser;
use kallip_common::authtoken::{MintedToken, TokenHash};
use tracing::{info, warn};
use webauthn_rs::prelude::WebauthnBuilder;

use args::Args;
use state::{AppState, Limits};

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // Mint the admin token: honor KALLIP_AGORA_ADMIN_TOKEN if set, otherwise
    // generate a fresh `sk-admin-...`. Only the hash is retained; the plaintext
    // is printed once below then dropped.
    let admin = match args.admin_token.clone() {
        Some(s) => MintedToken::from_secret(s),
        None => MintedToken::generate(token::ADMIN),
    };
    println!("==================================================");
    println!("  kallip-agora {}", env!("CARGO_PKG_VERSION"));
    println!("  Admin Token:");
    println!("    {}", admin.secret());
    println!("  (retain only this hash; plaintext shown once)");
    println!("==================================================");

    let limits = Limits {
        max_body_size_bytes: body_size_bytes(args.max_body_size_kb),
        enrollment_code_ttl: Duration::from_secs(args.enrollment_code_ttl_secs),
        invite_default_ttl_secs: args.invite_default_ttl_secs,
    };

    // Connect to Postgres (retrying with a capped backoff) and apply pending
    // migrations before serving a single request.
    let db = crate::db::connect_and_migrate(&args.database_url).await?;

    // Build the WebAuthn relying party via the high-level wrapper's safe
    // builder (validates rp_id is an effective domain of rp_origin), the
    // session-cookie config, and the per-IP auth rate limiter from the boot args.
    let rp_origin = url::Url::parse(&args.webauthn_rp_origin)
        .map_err(|e| anyhow::anyhow!("invalid KALLIP_AGORA_WEBAUTHN_RP_ORIGIN: {e}"))?;
    let webauthn = WebauthnBuilder::new(&args.webauthn_rp_id, &rp_origin)
        .map_err(|e| anyhow::anyhow!("invalid WebAuthn RP config: {e}"))?
        .allow_any_port(args.webauthn_allow_any_port)
        .rp_name(&args.webauthn_rp_name)
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| anyhow::anyhow!("WebAuthn build failed: {e}"))?;
    let session_cfg = session::SessionCfg {
        ttl: Duration::from_secs(args.session_ttl_secs),
        cookie_secure: args.cookie_secure,
        cookie_domain: args.cookie_domain,
    };
    let auth_rate_limiter =
        ratelimit::IpRateLimiter::new(args.auth_rate_capacity, args.auth_rate_refill_per_sec);

    // Parse the trusted-proxy CIDRs. The default trusts loopback (correct for
    // the default same-box reverse-proxy deploy). When the agora binds a
    // non-loopback address and the operator left the default in place, force
    // the set empty: trusting loopback XFF on a publicly-bound socket would let
    // any co-resident process forge XFF and evade per-client limiting. An
    // operator behind a loopback proxy on a public bind must set
    // KALLIP_AGORA_TRUSTED_PROXIES explicitly. Compare parsed CIDR sets (not
    // raw strings) so a semantically-identical default spelled differently
    // (whitespace, order) is still treated as "left at the default".
    let mut trusted_proxies = parse_trusted_proxies(&args.trusted_proxies);
    let explicit_trusted = trusted_proxies != parse_trusted_proxies(args::DEFAULT_TRUSTED_PROXIES);
    if !explicit_trusted && !is_loopback_bind(&args.listen_addr) && !trusted_proxies.is_empty() {
        warn!(
            "listen_addr {addr} is publicly bound but trusted_proxies is the loopback default; \
             clearing it to avoid XFF spoofing. Set KALLIP_AGORA_TRUSTED_PROXIES explicitly to \
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
        limits,
        db,
        Arc::new(webauthn),
        session_cfg,
        auth_rate_limiter,
        trusted_proxies,
    ));

    // The data-plane relay (`kallip-lesche`) is a separate process that calls
    // the agora's `/internal/*` ControlPlane API. Mount that surface only when
    // a non-empty shared secret is configured; an unset (or empty) token runs
    // the agora standalone (no relay connected, no internal surface exposed).
    // Treating the empty string as "unset" is load-bearing: an operator who
    // exports `KALLIP_AGORA_INTERNAL_TOKEN=` (intending to disable) must NOT
    // instead enable the surface with a trivially-known empty secret.
    let internal_hash = args
        .internal_token
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(TokenHash::of);
    if matches!(&args.internal_token, Some(s) if s.is_empty()) {
        warn!(
            "KALLIP_AGORA_INTERNAL_TOKEN is set but empty; treating as unset (no /internal surface)"
        );
    }

    let app = routes::router(state.clone(), internal_hash);

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
                    _ = interval.tick() => crate::db::gc_expired_challenges(&sweep_db).await,
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

    let listener = tokio::net::TcpListener::bind(&args.listen_addr).await?;
    info!(addr = %args.listen_addr, "agora listening");
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
