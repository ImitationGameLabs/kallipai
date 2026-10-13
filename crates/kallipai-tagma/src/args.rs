use clap::Parser;

/// Upper bound for max_agents.
pub(crate) const MAX_AGENTS_LIMIT: usize = 1000;
/// Upper bound for max_subagents.
pub(crate) const MAX_SUBAGENTS_LIMIT: usize = 100;

/// CLI arguments for kallipai-tagma.
#[derive(Parser)]
#[command(
    name = "kallipai-tagma",
    version,
    about = "HTTP API server hosting multiple agent instances"
)]
pub struct Args {
    /// Address to listen on.
    #[arg(long, env = "KALLIPAI_TAGMA_ADDR", default_value = "127.0.0.1:3000")]
    pub listen_addr: String,
    /// URL that agents use to reach this tagma (injected into the agent
    /// shell env). When unset it is derived from the actually-bound listen
    /// address after the socket is up: scheme http, host from the listen
    /// address (an unspecified host becomes 127.0.0.1) and the real bound
    /// port -- so a daemon-spawned instance on an ephemeral port never
    /// advertises a stale default.
    #[arg(long, env = "KALLIPAI_ADVERTISE_URL")]
    pub advertise_url: Option<String>,
    /// Max queued messages per agent (message channel capacity). Must be >= 1.
    #[arg(long, env = "KALLIPAI_PROMPT_QUEUE_SIZE", default_value = "5")]
    pub prompt_queue_size: usize,
    /// Max concurrent agents. Range: 1..=1000.
    #[arg(long, env = "KALLIPAI_MAX_AGENTS", default_value = "50")]
    pub max_agents: usize,
    /// Max direct subagents per agent. Range: 1..=100.
    #[arg(long, env = "KALLIPAI_MAX_SUBAGENTS", default_value = "20")]
    pub max_subagents: usize,
    /// Max HTTP request body size in kilobytes. 0 = axum default (2 MB).
    #[arg(long, env = "KALLIPAI_MAX_BODY_SIZE_KB", default_value = "1024")]
    pub max_body_size_kb: usize,
    /// User-Agent sent on outbound LLM HTTP calls. Unset = `kallipai/<tagma-version>`.
    #[arg(long, env = "KALLIPAI_LLM_API_USER_AGENT")]
    pub llm_api_user_agent: Option<String>,
    /// Platform API edge origin (`https://api.<domain>`): enrolls there and
    /// reaches every backend service through it. Unset = local-only (no
    /// relay; the lesche message route returns 503).
    #[arg(long, env = "KALLIPAI_POLIS_URL")]
    pub polis_url: Option<String>,
    /// Single-use archeion enrollment code (first run only; thereafter the stored
    /// tagma token is reused).
    #[arg(long, env = "KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE")]
    pub relay_enrollment_code: Option<String>,
    /// Max `kallip lesche send` deliveries per burst window. Bounds a runaway
    /// agent message loop; process-global (today one root agent = one
    /// conversation, so this is effectively per-conversation). Unset = 20.
    #[arg(long, env = "KALLIPAI_TAGMA_RELAY_MESSAGE_BURST_MAX")]
    pub relay_message_burst_max: Option<u32>,
    /// Length in seconds of the message burst window. Unset = 10.
    #[arg(long, env = "KALLIPAI_TAGMA_RELAY_MESSAGE_BURST_WINDOW_SECS")]
    pub relay_message_burst_window_secs: Option<u64>,
    /// Chat-history retention in days. Rows older than this are GC'd; a device
    /// that reconnects after the window only sees what remains. Unset = 30.
    #[arg(long, env = "KALLIPAI_TAGMA_RELAY_HISTORY_TTL_DAYS")]
    pub relay_history_ttl_days: Option<u64>,
    /// Chat-history row cap. When exceeded, the oldest rows are trimmed
    /// regardless of age. Unset = 100000 (a runaway backstop, not a usage
    /// quota; normal use within the TTL window should never reach it).
    #[arg(long, env = "KALLIPAI_TAGMA_RELAY_HISTORY_CAP")]
    pub relay_history_cap: Option<u64>,

    /// Statement duration above which a SQL statement is logged at WARN,
    /// in milliseconds. Statements are logged at DEBUG regardless (quiet
    /// under the default `info` filter); a store that fails to open at boot
    /// is logged at ERROR. A non-numeric value fails boot: clap refuses it
    /// rather than silently falling back to the default.
    #[arg(long, env = "KALLIPAI_TAGMA_SQL_SLOW_MS", default_value_t = 100)]
    pub sql_slow_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::Args;
    use clap::Parser;

    /// The slow-SQL threshold defaults to 100 ms and refuses non-numeric
    /// values instead of silently falling back.
    #[test]
    fn sql_slow_threshold_defaults_and_is_typed() {
        // SAFETY: single-threaded test setup, and nothing else in
        // this crate reads this var, so no concurrent access.
        unsafe { std::env::remove_var("KALLIPAI_TAGMA_SQL_SLOW_MS") };
        let args = Args::parse_from(["kallipai-tagma"]);
        assert_eq!(args.sql_slow_ms, 100);

        let err = Args::try_parse_from(["kallipai-tagma", "--sql-slow-ms", "fast"])
            .err()
            .expect("a non-numeric threshold must be refused");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }
}
