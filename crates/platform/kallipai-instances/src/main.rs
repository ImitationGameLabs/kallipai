//! Thin binary: parse config, resolve the auth mode, serve.

use anyhow::{Context, Result};
use clap::Parser as _;
use kallipai_daemon_client::DaemonClient;
use kallipai_instances::backend::UdsBackend;
use kallipai_instances::{AppState, Config, build_router, resolve_auth};

#[tokio::main]
async fn main() -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // File logging is opt-in via KALLIPAI_INSTANCES_LOG_DIR (same shape
    // as the other platform services: set, the rolling file is the only
    // event channel; unset keeps stdout-only, so container and dev
    // compose forms are untouched).
    let log_dir =
        kallipai_common::logging::parse_log_dir(std::env::var("KALLIPAI_INSTANCES_LOG_DIR").ok());
    kallipai_common::logging::init_service_logging(&filter, "instances", log_dir.as_deref());

    let mut config = Config::parse();
    // Load the archeion-internal secret from its provisioned file (the
    // archeion generates it on first boot). The bounded wait absorbs the
    // boot-ordering race; expiry refuses to start rather than degrade.
    if let Some(path) = config.archeion_internal_token_file.as_deref() {
        config.archeion_internal_token =
            Some(kallipai_common::secret_file::read_trimmed_with_retry(
                std::path::Path::new(path),
                kallipai_common::secret_file::BOOT_RETRY,
            )?);
    }
    let socket = config.resolve_socket()?;
    let addr = config.addr.clone();

    // Resolve the auth mode from the configuration. Fail-safe rule: a
    // non-loopback bind with neither platform nor standalone credentials
    // refuses to start — the open mode is a loopback-only convenience.
    let auth = resolve_auth(&config, &addr)?;
    // The backend seam: daemon is the only implementation; anything else
    // (a future cloud orchestration source) fails fast at boot rather
    // than serving a half-configured surface.
    let backend = match config.backend.as_str() {
        "daemon" => UdsBackend::arc(DaemonClient::new(socket.clone())),
        other => anyhow::bail!(
            "KALLIPAI_INSTANCES_BACKEND={other} is not implemented; only \"daemon\" exists"
        ),
    };
    let state = AppState {
        backend,
        auth,
        allowed_hosts: config.allowed_hosts(),
        cors_origins: config.cors_origins.clone(),
    };
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(
        addr = %addr,
        socket = %socket.display(),
        "kallipai-instances listening"
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
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
}
