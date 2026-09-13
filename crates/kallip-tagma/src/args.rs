use clap::Parser;

/// Upper bound for max_agents.
pub(crate) const MAX_AGENTS_LIMIT: usize = 1000;
/// Upper bound for max_subagents.
pub(crate) const MAX_SUBAGENTS_LIMIT: usize = 100;

/// CLI arguments for kallip-tagma.
#[derive(Parser)]
#[command(
    name = "kallip-tagma",
    version,
    about = "HTTP API server hosting multiple agent instances"
)]
pub struct Args {
    /// Address to listen on.
    #[arg(long, env = "KALLIP_TAGMA_ADDR", default_value = "127.0.0.1:3000")]
    pub listen_addr: String,
    /// URL that agents use to reach this tagma (injected into the agent
    /// shell env). When unset it is derived from the actually-bound listen
    /// address after the socket is up: scheme http, host from the listen
    /// address (an unspecified host becomes 127.0.0.1) and the real bound
    /// port -- so a daemon-spawned instance on an ephemeral port never
    /// advertises a stale default.
    #[arg(long, env = "KALLIP_ADVERTISE_URL")]
    pub advertise_url: Option<String>,
    /// Max queued messages per agent (message channel capacity). Must be >= 1.
    #[arg(long, env = "KALLIP_PROMPT_QUEUE_SIZE", default_value = "5")]
    pub prompt_queue_size: usize,
    /// Max concurrent agents. Range: 1..=1000.
    #[arg(long, env = "KALLIP_MAX_AGENTS", default_value = "50")]
    pub max_agents: usize,
    /// Max direct subagents per agent. Range: 1..=100.
    #[arg(long, env = "KALLIP_MAX_SUBAGENTS", default_value = "20")]
    pub max_subagents: usize,
    /// Max HTTP request body size in kilobytes. 0 = axum default (2 MB).
    #[arg(long, env = "KALLIP_MAX_BODY_SIZE_KB", default_value = "1024")]
    pub max_body_size_kb: usize,
    /// User-Agent sent on outbound LLM HTTP calls. Unset = `kallip/<tagma-version>`.
    #[arg(long, env = "KALLIP_LLM_API_USER_AGENT")]
    pub llm_api_user_agent: Option<String>,
    /// Platform API edge origin (`https://api.<domain>`): enrolls there and
    /// reaches every backend service through it. Unset = local-only (no
    /// relay; the lesche message route returns 503).
    #[arg(long, env = "KALLIP_POLIS_URL")]
    pub polis_url: Option<String>,
    /// Single-use archeion enrollment code (first run only; thereafter the stored
    /// tagma token is reused). Replaces the former standalone connector's
    /// enrollment-code env var.
    #[arg(long, env = "KALLIP_TAGMA_RELAY_ENROLLMENT_CODE")]
    pub relay_enrollment_code: Option<String>,
    /// Max `kallip lesche send` deliveries per burst window. Bounds a runaway
    /// agent message loop; process-global (today one root agent = one
    /// conversation, so this is effectively per-conversation). Unset = 20.
    #[arg(long, env = "KALLIP_TAGMA_RELAY_MESSAGE_BURST_MAX")]
    pub relay_message_burst_max: Option<u32>,
    /// Length in seconds of the message burst window. Unset = 10.
    #[arg(long, env = "KALLIP_TAGMA_RELAY_MESSAGE_BURST_WINDOW_SECS")]
    pub relay_message_burst_window_secs: Option<u64>,
    /// Chat-history retention in days. Rows older than this are GC'd; a device
    /// that reconnects after the window only sees what remains. Unset = 30.
    #[arg(long, env = "KALLIP_TAGMA_RELAY_HISTORY_TTL_DAYS")]
    pub relay_history_ttl_days: Option<u64>,
    /// Chat-history row cap. When exceeded, the oldest rows are trimmed
    /// regardless of age. Unset = 100000 (a runaway backstop, not a usage
    /// quota; normal use within the TTL window should never reach it).
    #[arg(long, env = "KALLIP_TAGMA_RELAY_HISTORY_CAP")]
    pub relay_history_cap: Option<u64>,
}
