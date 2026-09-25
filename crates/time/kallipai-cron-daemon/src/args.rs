//! CLI arguments for `kallipai-cron-daemon`.
//!
//! Env prefix is `KALLIPAI_CRON_*` for own knobs. The daemon's tagma client
//! (delivery) reuses the cross-cutting `KALLIPAI_TAGMA_URL` + `KALLIPAI_AUTH_TOKEN`
//! that `TagmaClient::from_env` reads — those are NOT repeated here.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "kallipai-cron-daemon",
    version,
    about = "Timer/notification daemon; fires schedules and injects them into agent conversations"
)]
pub struct Args {
    /// Address to listen on. Must be loopback — cron is an internal tagma-side
    /// service and the management API is not exposed externally.
    #[arg(long, env = "KALLIPAI_CRON_ADDR", default_value = "127.0.0.1:3010")]
    pub listen_addr: String,

    /// Directory holding `cron.sqlite`. Unset = platform data dir + `kallipai/cron`.
    #[arg(long, env = "KALLIPAI_CRON_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Scheduler tick interval in milliseconds (>= 1000; second-precision).
    #[arg(long, env = "KALLIPAI_CRON_TICK_MS", default_value = "1000")]
    pub tick_interval_ms: u64,

    /// Deliverer poll interval in milliseconds.
    #[arg(long, env = "KALLIPAI_CRON_DELIVER_MS", default_value = "500")]
    pub deliver_interval_ms: u64,
}

/// Default data directory: platform data dir + `kallipai/cron`.
///
/// Fails instead of guessing: the schedule store is durable state, and
/// the old current_dir() fallback could silently land it wherever the
/// daemon happened to start from — a refused boot beats a lost store.
pub fn default_data_dir() -> anyhow::Result<PathBuf> {
    default_data_dir_in(dirs::data_dir().as_deref())
}

/// Pure core of [`default_data_dir`] so the fail-fast shape is testable
/// without touching process environment state.
fn default_data_dir_in(data_home: Option<&Path>) -> anyhow::Result<PathBuf> {
    data_home
        .map(|home| home.join("kallipai").join("cron"))
        .context("cannot determine the platform data directory; pass --data-dir or set KALLIPAI_CRON_DATA_DIR")
}

/// Whether `addr` binds to a loopback interface — the sole network boundary
/// (there is no cron-specific token; the management API is gated by per-request
/// agent-token verification via the tagma).
///
/// Parses as a `SocketAddr` so the entire IPv4 `127/8` range and IPv6 `::1` are
/// recognized; falls back to a hostname string match for `localhost` (SocketAddr
/// parsing does not resolve DNS). The fallback is deliberately lenient toward
/// loopback intent — e.g. `::1:3010` (bracketless) extracts host `::1` and is
/// accepted — so a malformed bracket never *exposes* a non-loopback address.
/// `Ipv6Addr::is_loopback()` only treats `::1` as loopback (not IPv4-mapped
/// `::ffff:127.x`); `[::1]:3010` is the canonical IPv6 form.
pub fn is_loopback(addr: &str) -> bool {
    if let Ok(socket) = addr.parse::<SocketAddr>() {
        return socket.ip().is_loopback();
    }
    let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr);
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_data_dir_sits_under_the_kallipai_namespace() {
        let dir = default_data_dir_in(Some(Path::new("/xdg/data"))).expect("resolves");
        assert_eq!(dir, PathBuf::from("/xdg/data/kallipai/cron"));
    }

    #[test]
    fn default_data_dir_fails_fast_without_a_platform_data_dir() {
        let err = default_data_dir_in(None).expect_err("refuses to guess");
        assert!(err.to_string().contains("KALLIPAI_CRON_DATA_DIR"), "{err}");
    }

    #[test]
    fn accepts_loopback_forms() {
        assert!(is_loopback("127.0.0.1:3010"));
        // Entire 127/8 is loopback; the old string-match wrongly rejected these.
        assert!(is_loopback("127.0.0.2:3010"));
        assert!(is_loopback("127.255.255.254:3010"));
        assert!(is_loopback("[::1]:3010"));
        assert!(is_loopback("localhost:3010"));
    }

    #[test]
    fn rejects_non_loopback() {
        assert!(!is_loopback("0.0.0.0:3010"));
        assert!(!is_loopback("192.168.1.5:3010"));
        assert!(!is_loopback("10.0.0.1:3010"));
    }

    #[test]
    fn bracketless_ipv6_loopback_still_accepted() {
        // `::1:3010` does not parse as a SocketAddr, but the string fallback
        // extracts host `::1` and accepts it — lenient toward loopback intent,
        // never exposing a non-loopback address.
        assert!(is_loopback("::1:3010"));
    }
}
