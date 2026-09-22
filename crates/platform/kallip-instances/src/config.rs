//! Startup configuration: bind address, daemon socket, and the
//! auth-mode inputs (platform credentials or a standalone token).

use std::path::PathBuf;

use clap::Parser;

/// Local instance management service for the kallip daemon: proxies
/// the bare resource paths to the daemon's UDS socket.
#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Config {
    /// Listen address. Loopback by default; a LAN deployment points this at
    /// the machine's LAN address explicitly.
    #[arg(long, env = "KALLIP_INSTANCES_ADDR", default_value = "127.0.0.1:7300")]
    pub addr: String,

    /// Daemon control socket. Defaults to the same XDG-derived path the
    /// daemon itself uses when KALLIP_DAEMON_SOCKET is unset (deliberate
    /// duplication: the daemon crate is a binary, so this mirrors its
    /// main.rs instead of sharing code).
    #[arg(long, env = "KALLIP_DAEMON_SOCKET")]
    pub daemon_socket: Option<PathBuf>,

    /// Standalone-mode bearer token for the instances API (constant-time
    /// compared; the platform mode uses the archeion credentials below).
    #[arg(long, env = "KALLIP_INSTANCES_TOKEN")]
    pub token: Option<String>,
    /// Instance source: the host daemon (the only implementation today;
    /// a cloud orchestration backend is reserved but not built yet).
    #[arg(long, env = "KALLIP_INSTANCES_BACKEND", default_value = "daemon")]
    pub backend: String,
    /// Archeion internal root for platform mode (e.g. http://127.0.0.1:7100);
    /// together with the internal token this enables archeion-backed auth.
    #[arg(long, env = "KALLIP_INSTANCES_ARCHEION_URL")]
    pub archeion_internal_url: Option<String>,

    /// File holding the shared platform-internal secret — provisioned by
    /// the archeion (0640 in its state directory), read here at boot.
    #[arg(long, env = "KALLIP_POLIS_INTERNAL_TOKEN_FILE")]
    pub archeion_internal_token_file: Option<String>,
    /// The loaded secret value (from the file above); not a CLI arg.
    #[arg(skip)]
    pub archeion_internal_token: Option<String>,
    /// Extra Host values allowed through the host guard (comma separated;
    /// a reverse-proxied deployment names its public domain here).
    #[arg(long, env = "KALLIP_INSTANCES_ALLOWED_HOSTS", default_value = "")]
    pub allowed_hosts_raw: String,
    /// Comma-separated CORS allowed origins (the app's origin(s)). Empty
    /// = no cross-origin allowed. Never use a wildcard on a public-facing
    /// deploy.
    #[arg(long, env = "KALLIP_INSTANCES_CORS_ORIGINS", default_value = "")]
    pub cors_origins: String,
}

impl Config {
    /// Resolve the daemon socket by probing the shared chain in order
    /// (see `kallip_daemon_common::socket`): the configured socket verbatim
    /// first, then the environment legs - the first socket that answers
    /// is wherever the daemon actually bound.
    pub fn resolve_socket(&self) -> anyhow::Result<PathBuf> {
        let candidates = kallip_daemon_common::socket::candidates_from_env(
            self.daemon_socket.as_deref().map(std::path::Path::new),
        );
        Self::probe_candidates(&candidates)
    }

    /// The probe half of resolve_socket, split so tests can drive the
    /// report path with a self-contained candidate list; a seam that
    /// keeps the host system leg out of the unit tests' reach.
    pub(crate) fn probe_candidates(candidates: &[PathBuf]) -> anyhow::Result<PathBuf> {
        kallip_daemon_common::socket::probe(candidates).map_err(|err| {
            anyhow::anyhow!(kallip_daemon_common::socket::describe_probe_failure(&err))
        })
    }

    /// The parsed allowlist from `allowed_hosts_raw` (empty = none).
    pub fn allowed_hosts(&self) -> Vec<String> {
        self.allowed_hosts_raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bind a listener at `path` (parent dirs created); the sandboxed
    /// stand-in for a live daemon socket.
    fn live_socket(path: &std::path::Path) -> std::os::unix::net::UnixListener {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::net::UnixListener::bind(path).unwrap()
    }

    #[test]
    fn probe_candidates_returns_the_first_reachable_candidate() {
        let scratch = tempfile::tempdir().unwrap();
        let live = scratch.path().join("live.sock");
        let listener = live_socket(&live);
        let dead = scratch.path().join("absent").join("control.sock");
        let resolved = Config::probe_candidates(&[dead, live.clone()]).expect("resolve");
        assert_eq!(resolved, live);
        drop(listener);
    }

    #[test]
    fn probe_candidates_names_every_dead_candidate_when_exhausted() {
        let scratch = tempfile::tempdir().unwrap();
        let runtime = scratch.path().join("runtime");
        let state = scratch.path().join("state");
        let dead_runtime = runtime.join("kallipai").join("daemon").join("control.sock");
        let dead_state = state.join("kallipai").join("daemon").join("control.sock");
        let error =
            Config::probe_candidates(&[dead_runtime.clone(), dead_state.clone()]).unwrap_err();
        let message = error.to_string();
        for leg in [&dead_runtime, &dead_state] {
            assert!(
                message.contains(&format!("{} (no such file or directory)", leg.display())),
                "{message}"
            );
        }
        assert!(message.contains("no reachable daemon socket"), "{message}");
    }

    #[test]
    fn allowed_hosts_splits_and_trims() {
        let config = Config {
            addr: "127.0.0.1:7300".into(),
            daemon_socket: None,
            token: None,
            backend: "daemon".into(),
            archeion_internal_url: None,
            archeion_internal_token: None,
            archeion_internal_token_file: None,
            allowed_hosts_raw: " platform.internal , localhost ,".into(),
            cors_origins: String::new(),
        };
        assert_eq!(
            config.allowed_hosts(),
            vec!["platform.internal", "localhost"]
        );
    }
}
