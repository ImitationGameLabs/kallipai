//! `kallip-cron-daemon` — the timer/notification daemon.
//!
//! Owns the scheduler (advances due schedules to `Triggered`) and the deliverer
//! (injects fired schedules into agent conversations via the tagma HTTP API),
//! plus a small management HTTP API for the CLI/operator. SQLite-backed
//! (sea-orm). See `AGENTS.md` and `docs/reference/cron-api.md`.

mod args;
mod auth;
mod deliver;
mod migration;
mod routes;
mod scheduler;
mod state;
mod store;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use kallip_client::TagmaClient;
use tokio_util::sync::CancellationToken;
use tracing::info;

use args::Args;
use deliver::Deliverer;
use scheduler::Scheduler;
use state::AppState;
use store::ScheduleStore;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    anyhow::ensure!(
        args.tick_interval_ms >= 1000,
        "KALLIP_CRON_TICK_MS must be >= 1000 (second-precision scheduler), got {}",
        args.tick_interval_ms
    );

    // Loopback is the sole network boundary (there is no cron-specific token
    // anymore; the management API is gated by per-request agent-token
    // verification via the tagma). Refuse any non-loopback bind so the daemon
    // is never accidentally exposed.
    anyhow::ensure!(
        args::is_loopback(&args.listen_addr),
        "KALLIP_CRON_ADDR must be a loopback address ({} is not); cron is an internal tagma-side service",
        args.listen_addr
    );

    let data_dir = args.data_dir.clone().unwrap_or_else(args::default_data_dir);
    let db_path = data_dir.join("cron.sqlite");
    info!(db = %db_path.display(), "opening schedule store");
    let store = ScheduleStore::open(&db_path).await?;

    // The tagma client for delivery: reads KALLIP_TAGMA_URL + KALLIP_AUTH_TOKEN.
    // KALLIP_AUTH_TOKEN must hold the tagma's operator secret so injected
    // messages render `[From: operator]`.
    let tagma = TagmaClient::from_env()?;
    // The tagma base URL for the per-request verify (HTTP goes through
    // `kallip-client`, which owns the shared pool + the verify timeout).
    let tagma_url =
        std::env::var("KALLIP_TAGMA_URL").unwrap_or_else(|_| "http://127.0.0.1:3000".to_string());
    let shutdown = CancellationToken::new();

    // Liveness thresholds: each loop may go quiet for ~5 of its own intervals
    // before `/health` reports it stale. The deliverer gets a 30s floor so one
    // slow serial sweep (each post inherits the tagma client's no-blanket
    // timeout) does not falsely trip staleness.
    let tick_interval = Duration::from_millis(args.tick_interval_ms);
    let deliver_interval = Duration::from_millis(args.deliver_interval_ms);
    let liveness = state::Liveness::new(
        tick_interval * 5,
        std::cmp::max(deliver_interval * 5, Duration::from_secs(30)),
    );

    let state = Arc::new(AppState {
        store: store.clone(),
        tagma_url,
        liveness: liveness.clone(),
    });

    // Spawn the scheduler + deliverer; both observe the shared shutdown token.
    let scheduler = Scheduler::new(store.clone(), tick_interval, liveness.clone());
    let deliverer = Deliverer::new(
        store.clone(),
        tagma.clone(),
        deliver_interval,
        shutdown.clone(),
        liveness,
    );
    let sched_handle = tokio::spawn(scheduler.run(shutdown.clone()));
    let deliver_handle = tokio::spawn(deliverer.run());

    let app = routes::router()
        .with_state(state)
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .layer(routes::cors_layer())
        .layer(tower_http::trace::TraceLayer::new_for_http());

    info!(addr = %args.listen_addr, "kallip-cron-daemon listening (loopback only)");
    let listener = tokio::net::TcpListener::bind(&args.listen_addr).await?;

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown.clone()))
        .await?;

    // The graceful-shutdown signal cancels the token. The scheduler returns at
    // the next select! tick; the deliverer stops starting new posts mid-sweep
    // and finishes at most the one in flight. Bound the join so a hung tagma
    // never pins shutdown indefinitely (a killed in-flight post leaves the row
    // Triggered for redelivery on restart — at-least-once, by contract).
    let drain = Duration::from_secs(30);
    let _ = tokio::time::timeout(drain, sched_handle).await;
    let _ = tokio::time::timeout(drain, deliver_handle).await;
    Ok(())
}

async fn shutdown_signal(token: CancellationToken) {
    let ctrl_c = tokio::signal::ctrl_c();
    let sigterm = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    tokio::select! {
        _ = ctrl_c => {}
        _ = sigterm => {}
    }
    info!("received shutdown signal, initiating graceful shutdown");
    token.cancel();
}
