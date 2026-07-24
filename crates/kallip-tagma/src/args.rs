use clap::Parser;

/// Upper bound for max_agents.
pub(crate) const MAX_AGENTS_LIMIT: usize = 1000;
/// Upper bound for max_subagents.
pub(crate) const MAX_SUBAGENTS_LIMIT: usize = 100;

/// CLI arguments for kallip-tagma.
#[derive(Parser)]
#[command(
    name = "kallip-tagma",
    about = "HTTP API server hosting multiple agent instances"
)]
pub struct Args {
    /// Address to listen on.
    #[arg(long, env = "KALLIP_TAGMA_ADDR", default_value = "127.0.0.1:3000")]
    pub listen_addr: String,
    /// URL that agents use to reach this tagma (injected into the agent shell env).
    #[arg(
        long,
        env = "KALLIP_ADVERTISE_URL",
        default_value = "http://127.0.0.1:3000"
    )]
    pub advertise_url: String,
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
    /// Activate the online-mode relay connector by enrolling with this agora
    /// control-plane URL. Unset = local-only (no relay; the lesche message
    /// route returns 503). Replaces the former standalone connector's
    /// control-plane env var.
    #[arg(long, env = "KALLIP_RELAY_AGORA_URL")]
    pub relay_agora_url: Option<String>,
    /// Lesche (data-plane relay) base URL the relay connector tunnels to. A full
    /// URL. If unset, defaults to the agora URL's origin (scheme + host + port),
    /// which is correct only for same-origin agora/lesche deployments — set it
    /// explicitly otherwise. Replaces the former standalone connector's data-plane env var.
    #[arg(long, env = "KALLIP_RELAY_LESCHE_URL")]
    pub relay_lesche_url: Option<String>,
    /// Single-use agora enrollment code (first run only; thereafter the stored
    /// tagma token is reused). Replaces the former standalone connector's
    /// enrollment-code env var.
    #[arg(long, env = "KALLIP_RELAY_ENROLLMENT_CODE")]
    pub relay_enrollment_code: Option<String>,
    /// Max `kallip lesche send` deliveries per burst window. Bounds a runaway
    /// agent message loop; process-global (today one root agent = one
    /// conversation, so this is effectively per-conversation). Unset = 20.
    #[arg(long, env = "KALLIP_RELAY_MESSAGE_BURST_MAX")]
    pub relay_message_burst_max: Option<u32>,
    /// Length in seconds of the message burst window. Unset = 10.
    #[arg(long, env = "KALLIP_RELAY_MESSAGE_BURST_WINDOW_SECS")]
    pub relay_message_burst_window_secs: Option<u64>,
}
