//! `kallip-herald`: the host-side relay connector. Runs next to a
//! `kallip-tagma`, enrolls with `kallip-agora`, holds the outbound tunnel,
//! brokers the conversation E2E key, and exposes the tagma as a single stateful
//! entity to remote apps: it binds to the tagma-owned root agent and
//! translates semantic operations into tagma calls.

mod e2e;
mod herald;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use kallip_agora_common::bytes::Ed25519PublicKey;
use kallip_agora_common::control::EnrollRequest;
use kallip_agora_common::ids::TagmaId;
use kallip_client::TagmaClient;
use tracing::info;

use std::io::Write;

use herald::Herald;

#[derive(Parser)]
#[command(name = "kallip-herald", about = "Host-side relay connector")]
struct Args {
    /// Agora base URL. Used ONLY for enrollment (`POST /v1/tagmata/enroll`) on
    /// the herald's first run; the stored tagma token is reused thereafter.
    #[arg(
        long,
        env = "KALLIP_HERALD_AGORA_URL",
        default_value = "http://127.0.0.1:7100"
    )]
    agora_url: String,
    /// Lesche (data-plane relay) base URL for the herald tunnel, envelope
    /// POSTs, and key-exchange responses. The relay is a separate service from
    /// the agora; in dev both default to the same host on different ports.
    #[arg(
        long,
        env = "KALLIP_HERALD_LESCHE_URL",
        default_value = "http://127.0.0.1:7200"
    )]
    lesche_url: String,
    /// Single-use enrollment code (first run only; after that the stored tagma
    /// token is reused).
    #[arg(long, env = "KALLIP_HERALD_ENROLLMENT_CODE")]
    enrollment_code: Option<String>,
    /// Local tagma URL.
    #[arg(
        long,
        env = "KALLIP_TAGMA_URL",
        default_value = "http://127.0.0.1:3000"
    )]
    tagma_url: String,
    /// Tagma auth token (the herald acts as the operator).
    #[arg(long, env = "KALLIP_AUTH_TOKEN")]
    tagma_token: String,
    /// State directory (device key, tagma token). Defaults to a per-user data dir.
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

    // Tagma credentials: load or enroll.
    let (tagma_id, tagma_token) = match load_tagma(&state_dir) {
        Some(creds) => {
            info!(tagma = %creds.0, "loaded stored tagma credentials");
            creds
        }
        None => {
            let code = args
                .enrollment_code
                .as_deref()
                .context("no stored tagma token; --enrollment-code required for first run")?;
            let creds = enroll(&args.agora_url, code, &device).await?;
            save_tagma(&state_dir, &creds);
            info!(tagma = %creds.0, "enrolled with agora");
            creds
        }
    };

    // The tagma event stream is long-lived with no natural end, so the tagma
    // client must NOT carry a total request timeout — reqwest's `.timeout()` is
    // a whole-request deadline that would kill the stream mid-flight. Build an
    // explicit no-timeout client rather than relying on the TagmaClient
    // default; the property is load-bearing for the event pump.
    let tagma_http = reqwest::Client::builder()
        .build()
        .context("build tagma http client")?;
    let tagma = TagmaClient::builder(&args.tagma_url)
        .auth_token(&args.tagma_token)
        .http_client(tagma_http)
        .build()?;

    // The tagma owns exactly one root agent (eagerly created at startup); the
    // herald binds to it directly. The tagma must be reachable to start.
    let root_agent = tagma.get_root_agent().await?.id;

    Herald::new(
        args.lesche_url,
        tagma_id,
        tagma_token,
        tagma,
        device,
        root_agent,
    )
    .run()
    .await;
    Ok(())
}

fn resolve_state_dir(flag: Option<String>) -> Result<PathBuf> {
    if let Some(p) = flag {
        return Ok(PathBuf::from(p));
    }
    // `KALLIP_DATA_DIR` is the shared "where does kallip keep its data" override
    // (the tagma/tui honor the same convention by agreement, not via shared
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

fn load_tagma(state_dir: &Path) -> Option<(TagmaId, String)> {
    let id = std::fs::read_to_string(state_dir.join("tagma.id")).ok()?;
    let token = std::fs::read_to_string(state_dir.join("tagma.token")).ok()?;
    Some((TagmaId::from(id), token.trim().to_string()))
}

fn save_tagma(state_dir: &Path, (tagma_id, tagma_token): &(TagmaId, String)) {
    let _ = std::fs::write(state_dir.join("tagma.id"), tagma_id.to_string());
    if let Err(e) = write_secret(&state_dir.join("tagma.token"), tagma_token.as_bytes()) {
        tracing::error!(error = %format!("{e:#}"), "failed to persist tagma token; next restart will require re-enrollment");
    }
}

/// Write a secret (device key, tagma token) with mode `0o600` so other local
/// users cannot read it. Unix-only: `mode` is masked by the process
/// umask, and `0o600 & !umask` stays `0o600` under the usual `0o022`.
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

async fn enroll(agora_url: &str, code: &str, device: &e2e::DeviceKey) -> Result<(TagmaId, String)> {
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
        .post(format!("{agora_url}/v1/tagmata/enroll"))
        .json(&req)
        .send()
        .await
        .context("enrollment POST failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("enrollment returned {}", resp.status());
    }
    let body: kallip_agora_common::control::EnrollResponse =
        resp.json().await.context("decode enrollment response")?;
    Ok((body.tagma_id, body.tagma_token))
}
