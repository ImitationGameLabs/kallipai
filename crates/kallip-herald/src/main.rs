//! `kallip-herald`: the host-side relay connector. Runs next to a
//! `kallip-daemon`, enrolls with `kallip-agora`, holds the outbound tunnel,
//! brokers per-conversation E2E keys, and bridges decrypted user messages to
//! the local daemon (re-encrypting the agent's reply back through the agora).

mod e2e;
mod herald;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use kallip_agora_common::bytes::Ed25519PublicKey;
use kallip_agora_common::control::EnrollRequest;
use kallip_agora_common::ids::TeamId;
use kallip_client::DaemonClient;
use tracing::info;

use std::io::Write;

use herald::Herald;

#[derive(Parser)]
#[command(name = "kallip-herald", about = "Host-side relay connector")]
struct Args {
    /// Agora base URL.
    #[arg(
        long,
        env = "KALLIP_HERALD_AGORA_URL",
        default_value = "http://127.0.0.1:7100"
    )]
    agora_url: String,
    /// Single-use enrollment code (first run only; after that the stored team
    /// token is reused).
    #[arg(long, env = "KALLIP_HERALD_ENROLLMENT_CODE")]
    enrollment_code: Option<String>,
    /// Local daemon URL.
    #[arg(
        long,
        env = "KALLIP_DAEMON_URL",
        default_value = "http://127.0.0.1:3000"
    )]
    daemon_url: String,
    /// Daemon auth token (the herald acts as the operator).
    #[arg(long, env = "KALLIP_AUTH_TOKEN")]
    daemon_token: String,
    /// State directory (device key + team token). Defaults to a per-user data dir.
    #[arg(long, env = "KALLIP_HERALD_STATE_DIR")]
    state_dir: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let state_dir = resolve_state_dir(args.state_dir)?;
    std::fs::create_dir_all(&state_dir).context("create state dir")?;
    // Tighten the state-dir leaf to owner-only. Only the leaf is set (the stdlib
    // has no atomic way to set intermediate parents); the `0o600` mode on the
    // secret files themselves is the load-bearing guard.
    let _ = set_owner_only(&state_dir);

    // Device key: load or generate + persist.
    let device = load_or_create_device(&state_dir)?;

    // Team credentials: load or enroll.
    let (team_id, team_token) = match load_team(&state_dir) {
        Some(creds) => {
            info!(team = %creds.0, "loaded stored team credentials");
            creds
        }
        None => {
            let code = args
                .enrollment_code
                .as_deref()
                .context("no stored team token; --enrollment-code required for first run")?;
            let creds = enroll(&args.agora_url, code, &device).await?;
            save_team(&state_dir, &creds);
            info!(team = %creds.0, "enrolled with agora");
            creds
        }
    };

    // The daemon proxy streams long-lived responses (e.g. /agents/{id}/events)
    // with no natural end, so the daemon client must NOT carry a total request
    // timeout — reqwest's `.timeout()` is a whole-request deadline that would
    // kill the stream mid-flight. Build an explicit no-timeout client rather
    // than relying on the DaemonClient default; the property is load-bearing
    // for the tunnel.
    let daemon_http = reqwest::Client::builder()
        .build()
        .context("build daemon http client")?;
    let daemon = DaemonClient::builder(&args.daemon_url)
        .auth_token(&args.daemon_token)
        .http_client(daemon_http)
        .build()?;

    Herald::new(args.agora_url, team_id, team_token, daemon, device)
        .run()
        .await;
    Ok(())
}

fn resolve_state_dir(flag: Option<String>) -> Result<PathBuf> {
    if let Some(p) = flag {
        return Ok(PathBuf::from(p));
    }
    // `KALLIP_DATA_DIR` is the shared "where does kallip keep its data" override
    // (the daemon/tui honor the same convention by agreement, not via shared
    // code). Otherwise fall back to the platform data dir namespaced as
    // `<data_dir>/kallip/herald`.
    if let Ok(dir) = std::env::var("KALLIP_DATA_DIR") {
        return Ok(PathBuf::from(dir).join("herald"));
    }
    let base = dirs::data_dir()
        .context("could not determine platform data directory")?
        .join("kallip")
        .join("herald");
    Ok(base)
}

fn load_or_create_device(state_dir: &Path) -> Result<e2e::DeviceKey> {
    let path = state_dir.join("device.key");
    if let Ok(seed_bytes) = std::fs::read(&path)
        && let Ok(seed) = seed_bytes.as_slice().try_into()
    {
        return Ok(e2e::DeviceKey::from_seed(seed));
    }
    let device = e2e::DeviceKey::generate();
    write_secret(&path, &device.seed())?;
    Ok(device)
}

fn load_team(state_dir: &Path) -> Option<(TeamId, String)> {
    let id = std::fs::read_to_string(state_dir.join("team.id")).ok()?;
    let token = std::fs::read_to_string(state_dir.join("team.token")).ok()?;
    Some((TeamId::from(id), token.trim().to_string()))
}

fn save_team(state_dir: &Path, (team_id, team_token): &(TeamId, String)) {
    let _ = std::fs::write(state_dir.join("team.id"), team_id.to_string());
    let _ = write_secret(&state_dir.join("team.token"), team_token.as_bytes());
}

/// Write a secret (device key, team token) with mode `0o600` so other local
/// users cannot read it. Unix-only: `mode` is masked by the process umask, and
/// `0o600 & !umask` stays `0o600` under the usual `0o022`.
fn write_secret(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(bytes))
        .with_context(|| format!("write secret to {path:?}"))?;
    Ok(())
}

/// Set a directory's permissions to owner-only (`0o700`), Unix-only.
fn set_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("set permissions on {path:?}"))?;
    Ok(())
}

async fn enroll(agora_url: &str, code: &str, device: &e2e::DeviceKey) -> Result<(TeamId, String)> {
    let public = device.public_bytes();
    let signature = device.sign(&kallip_agora_common::proof::enroll_transcript(
        code, &public,
    ));
    let req = EnrollRequest {
        code: code.to_string(),
        device_public_key: Ed25519PublicKey(public.to_vec()),
        signature: kallip_agora_common::bytes::Ed25519Signature(signature.to_vec()),
    };
    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("build reqwest client")?
        .post(format!("{agora_url}/v1/teams"))
        .json(&req)
        .send()
        .await
        .context("enrollment POST failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("enrollment returned {}", resp.status());
    }
    let body: kallip_agora_common::control::EnrollResponse =
        resp.json().await.context("decode enrollment response")?;
    Ok((body.team_id, body.team_token))
}
