//! Control-socket resolution, shared by the daemon and every client so
//! both sides walk the same legs in the same order.
//!
//! Leg order:
//!
//! 1. an explicit `--socket` (clients only - the daemon has no flag)
//! 2. `KALLIP_DAEMON_SOCKET`
//! 3. `$XDG_RUNTIME_DIR/kallipai/daemon/control.sock`, when the runtime
//!    directory is set and usable
//! 4. the platform state home's `kallipai/daemon/control.sock`
//! 5. `/run/kallipai/daemon.sock` - clients only; the NixOS system
//!    daemon's well-known path
//!
//! The daemon binds the FIRST candidate and never falls through on bind
//! failure - binding leg N while clients probe from the top would split
//! the control plane - and its chain stops at leg 4: a user-process
//! daemon must not grab the system path. Clients probe the candidates
//! in order and connect to the first that answers. Identical ordering
//! plus sequential probing keeps the sides converged: a login-session
//! client probes the RUNTIME leg, finds nothing there, and lands on the
//! daemon's state-home socket. Leg 5 is pure client fallback - on a
//! NixOS host the module exports `KALLIP_DAEMON_SOCKET` (leg 2) already,
//! so the env leg still wins first and leg 5 only serves a bare,
//! zero-config client reaching the system daemon.
use std::path::{Path, PathBuf};

/// One resolution leg's raw inputs, collected so the pure ordering can be
/// tested without touching process state.
pub struct SocketLegs<'a> {
    pub explicit: Option<&'a Path>,
    /// Raw `KALLIP_DAEMON_SOCKET`: set-but-empty is still a set value.
    pub daemon_socket_env: Option<String>,
    /// The runtime-dir leg, already resolved to `None` when
    /// `$XDG_RUNTIME_DIR` is unset, empty, or not an existing directory.
    pub runtime_dir: Option<PathBuf>,
    /// The platform state home's `kallipai/daemon` directory, when the
    /// state home can be determined.
    pub state_default: Option<PathBuf>,
}

/// The NixOS system daemon's well-known socket: the last client leg, so
/// a zero-config client can reach a system-installed daemon. Clients
/// only - `daemon_bind_path` never returns it, because a user-process
/// daemon must not bind the system path. Must stay in sync with the
/// `daemonSocket` binding in `nix/nixos-modules.nix`.
pub const SYSTEM_DAEMON_SOCKET: &str = "/run/kallipai/daemon.sock";

/// Collect the legs from the process environment (and an optional
/// client-side explicit socket).
pub fn legs_from_env(explicit: Option<&Path>) -> SocketLegs<'_> {
    let env = |key: &str| std::env::var(key).ok();
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| !dir.as_os_str().is_empty() && dir.is_dir());
    SocketLegs {
        explicit,
        daemon_socket_env: env("KALLIP_DAEMON_SOCKET"),
        runtime_dir,
        state_default: dirs::state_dir().map(|home| home.join("kallipai").join("daemon")),
    }
}

/// Candidate socket paths in probe order: legs that cannot resolve are
/// skipped, duplicates removed, order preserved.
pub fn candidates(legs: &SocketLegs) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf| {
        if !out.contains(&path) {
            out.push(path);
        }
    };
    if let Some(explicit) = legs.explicit {
        push(explicit.to_path_buf());
    }
    if let Some(socket) = legs.daemon_socket_env.as_deref().filter(|s| !s.is_empty()) {
        push(PathBuf::from(socket));
    }
    if let Some(runtime) = &legs.runtime_dir {
        push(runtime.join("kallipai").join("daemon").join("control.sock"));
    }
    if let Some(state) = &legs.state_default {
        push(state.join("control.sock"));
    }
    out
}

/// The candidates a client probes: environment chain plus the explicit
/// socket first.
pub fn candidates_from_env(explicit: Option<&Path>) -> Vec<PathBuf> {
    let mut out = candidates(&legs_from_env(explicit));
    let system = PathBuf::from(SYSTEM_DAEMON_SOCKET);
    if !out.contains(&system) {
        out.push(system);
    }
    out
}

/// The daemon's bind path: the first candidate of the environment chain
/// (the daemon has no explicit flag). `None` when no leg resolves - the
/// caller turns that into a pointed error.
pub fn daemon_bind_path() -> Option<PathBuf> {
    candidates(&legs_from_env(None)).into_iter().next()
}

/// One candidate's failed connect, kept in probe order so an exhausted
/// chain can be reported candidate by candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeAttempt {
    /// The candidate that was tried.
    pub path: PathBuf,
    /// Why the connect failed.
    pub kind: std::io::ErrorKind,
}

/// Why no candidate answered the probe: every attempt with its failure
/// kind, in probe order. A permission denial is the interesting case -
/// the socket exists but this user may not connect (the daemon
/// socket's group-admission design) - and `describe_probe_failure`
/// turns that into an actionable message instead of a bare "not found".
#[derive(Debug, Clone)]
pub struct ProbeError {
    /// One entry per candidate, in probe order.
    pub attempted: Vec<ProbeAttempt>,
}

/// The first candidate that accepts a connection; every failure is
/// recorded and the chain keeps walking, so a live daemon wins wherever
/// it sits while the report keeps the per-candidate reasons. A local
/// unix-socket connect either answers immediately or fails; no timeout
/// is needed.
pub fn probe(candidates: &[PathBuf]) -> Result<PathBuf, ProbeError> {
    let mut attempted = Vec::new();
    for path in candidates {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => return Ok(path.clone()),
            Err(err) => attempted.push(ProbeAttempt {
                path: path.clone(),
                kind: err.kind(),
            }),
        }
    }
    Err(ProbeError { attempted })
}

/// The user-facing report for an exhausted probe chain: a one-line
/// verdict, then every candidate on its own line with the reason it
/// did not answer. A permission denial changes only the verdict -
/// the socket exists but this user may not connect (the daemon
/// socket's group-admission design); the tried list reads the same.
pub fn describe_probe_failure(err: &ProbeError) -> String {
    let verdict = if err
        .attempted
        .iter()
        .any(|attempt| attempt.kind == std::io::ErrorKind::PermissionDenied)
    {
        "daemon socket exists but access is denied for the current user"
    } else {
        "no reachable daemon socket"
    };
    let mut report = format!("{verdict}\ntried, in order:");
    for attempt in &err.attempted {
        report.push_str(&format!(
            "\n  {} ({})",
            attempt.path.display(),
            failure_phrase(attempt.kind)
        ));
    }
    report
}

/// A short human phrase for one connect failure kind.
fn failure_phrase(kind: std::io::ErrorKind) -> &'static str {
    match kind {
        std::io::ErrorKind::NotFound => "no such file or directory",
        std::io::ErrorKind::PermissionDenied => "permission denied",
        std::io::ErrorKind::ConnectionRefused => "connection refused (stale socket file)",
        _ => "connect failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallip_testkit::{DevDir, with_env};

    fn dev_tempdir(label: &str) -> DevDir {
        DevDir::new(label)
    }

    #[test]
    fn leg_order_is_explicit_env_runtime_default() {
        let legs = SocketLegs {
            explicit: Some(Path::new("/explicit.sock")),
            daemon_socket_env: Some("/env.sock".into()),
            runtime_dir: Some(PathBuf::from("/run/user/1000")),
            state_default: Some(PathBuf::from("/state-home/kallipai/daemon")),
        };
        assert_eq!(
            candidates(&legs),
            vec![
                PathBuf::from("/explicit.sock"),
                PathBuf::from("/env.sock"),
                PathBuf::from("/run/user/1000/kallipai/daemon/control.sock"),
                PathBuf::from("/state-home/kallipai/daemon/control.sock"),
            ]
        );
    }

    #[test]
    fn unresolvable_legs_are_skipped_and_duplicates_removed() {
        let legs = SocketLegs {
            explicit: Some(Path::new("/env.sock")),
            daemon_socket_env: Some("/env.sock".into()),
            runtime_dir: None,
            state_default: Some(PathBuf::from("/state-home/kallipai/daemon")),
        };
        assert_eq!(
            candidates(&legs),
            vec![
                PathBuf::from("/env.sock"),
                PathBuf::from("/state-home/kallipai/daemon/control.sock"),
            ]
        );
    }

    #[test]
    fn set_but_empty_env_legs_are_skipped() {
        let legs = SocketLegs {
            explicit: None,
            daemon_socket_env: Some(String::new()),
            runtime_dir: None,
            state_default: Some(PathBuf::from("/state-home/kallipai/daemon")),
        };
        assert_eq!(
            candidates(&legs),
            vec![PathBuf::from("/state-home/kallipai/daemon/control.sock")]
        );
    }

    #[test]
    fn client_chain_ends_with_the_system_leg() {
        with_env(
            &[("KALLIP_DAEMON_SOCKET", None), ("XDG_RUNTIME_DIR", None)],
            || {
                let chain = candidates_from_env(None);
                assert_eq!(
                    chain.last().map(|p| p.as_path()),
                    Some(Path::new(SYSTEM_DAEMON_SOCKET))
                );
            },
        );
    }

    #[test]
    fn system_leg_appears_once_even_when_env_points_at_it() {
        with_env(
            &[
                ("KALLIP_DAEMON_SOCKET", Some(SYSTEM_DAEMON_SOCKET)),
                ("XDG_RUNTIME_DIR", None),
            ],
            || {
                let chain = candidates_from_env(None);
                let count = chain
                    .iter()
                    .filter(|p| p.as_os_str() == SYSTEM_DAEMON_SOCKET)
                    .count();
                // The env leg already answers at its own position (which
                // probes first); the appended system leg must not repeat it.
                assert_eq!(count, 1, "env leg and system leg collapse to one");
            },
        );
    }

    #[test]
    fn daemon_bind_chain_excludes_the_system_leg() {
        with_env(
            &[("KALLIP_DAEMON_SOCKET", None), ("XDG_RUNTIME_DIR", None)],
            || {
                // The daemon's chain stops at the state-home leg; the
                // system path is client-only, so it must not appear at any
                // position of the bind order.
                let chain = candidates(&legs_from_env(None));
                assert!(!chain.is_empty(), "daemon bind chain unexpectedly empty");
                assert!(
                    !chain.iter().any(|p| p.as_os_str() == SYSTEM_DAEMON_SOCKET),
                    "system path must not appear in the daemon bind chain"
                );
            },
        );
    }

    #[test]
    fn daemon_binds_the_first_env_candidate() {
        with_env(
            &[
                ("KALLIP_DAEMON_SOCKET", Some("/env.sock")),
                ("XDG_RUNTIME_DIR", None),
            ],
            || {
                assert_eq!(daemon_bind_path(), Some(PathBuf::from("/env.sock")));
            },
        );
    }

    #[test]
    fn probe_walks_candidates_in_order_until_one_answers() {
        let dir = dev_tempdir("sock");
        let live = dir.join("live.sock");
        let listener = std::os::unix::net::UnixListener::bind(&live).unwrap();
        let dead = dir.join("dead.sock");

        let found = probe(&[dead.clone(), live.clone()]).ok();
        assert_eq!(found, Some(live), "dead candidate skipped, live answered");
        drop(listener);
    }
    use std::os::unix::fs::PermissionsExt;

    /// Euid 0 bypasses the permission bits the denial tests rely on;
    /// /proc/self carries the euid as its file owner on Linux.
    fn running_as_root() -> bool {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self").is_ok_and(|meta| meta.uid() == 0)
    }

    #[test]
    fn exhausted_chain_without_a_denial_keeps_the_not_found_report() {
        let err = probe(&[PathBuf::from("/kp-probe-absent/altogether.sock")]).unwrap_err();
        assert_eq!(err.attempted.len(), 1);
        assert_eq!(err.attempted[0].kind, std::io::ErrorKind::NotFound);

        let report = describe_probe_failure(&err);
        assert!(
            report.contains("no reachable daemon socket"),
            "absence stays reported as absence: {report}"
        );
        assert!(
            report.contains("/kp-probe-absent/altogether.sock"),
            "the tried path stays in the report: {report}"
        );

        assert!(
            report.contains(
                "\ntried, in order:\n  /kp-probe-absent/altogether.sock (no such file or directory)"
            ),
            "the absence report ends with a line per candidate: {report}"
        );
    }

    #[test]
    fn denied_socket_is_reported_as_denial_with_its_path() {
        if running_as_root() {
            // Root bypasses the permission bits this test is built on.
            return;
        }
        let dir = dev_tempdir("sock-denied");
        let guarded = dir.join("guarded.sock");
        let listener = std::os::unix::net::UnixListener::bind(&guarded).unwrap();
        std::fs::set_permissions(&guarded, std::fs::Permissions::from_mode(0o000)).unwrap();
        let denied_path = guarded.to_str().unwrap().to_owned();

        let err = probe(&[guarded]).unwrap_err();
        assert_eq!(
            err.attempted[0].kind,
            std::io::ErrorKind::PermissionDenied,
            "mode 000 must deny a non-root connect on this kernel"
        );
        let report = describe_probe_failure(&err);
        assert!(
            report.contains(denied_path.as_str()),
            "the denied path is named: {report}"
        );
        assert!(
            report.contains("access is denied"),
            "a denial is not reported as absence: {report}"
        );

        assert!(
            report.contains(
                format!("\ntried, in order:\n  {denied_path} (permission denied)").as_str()
            ),
            "the denial is the bare verdict plus a line per candidate: {report}"
        );

        drop(listener);
    }

    #[test]
    fn probe_keeps_walking_after_a_denial_and_reports_every_attempt() {
        if running_as_root() {
            return;
        }
        let dir = dev_tempdir("sock-mixed");
        let guarded = dir.join("guarded.sock");
        let listener = std::os::unix::net::UnixListener::bind(&guarded).unwrap();
        std::fs::set_permissions(&guarded, std::fs::Permissions::from_mode(0o000)).unwrap();
        let missing = dir.join("absent.sock");

        let err = probe(&[missing.clone(), guarded.clone()]).unwrap_err();
        let kinds = err
            .attempted
            .iter()
            .map(|attempt| attempt.kind)
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                std::io::ErrorKind::NotFound,
                std::io::ErrorKind::PermissionDenied
            ],
            "the chain walks past a denial and records both attempts"
        );
        let report = describe_probe_failure(&err);
        assert!(report.contains(missing.to_str().unwrap()));
        assert!(report.contains(guarded.to_str().unwrap()));
        assert!(report.contains("permission denied"));
        drop(listener);
    }

    #[test]
    fn a_denial_does_not_end_the_chain_and_a_live_candidate_wins() {
        if running_as_root() {
            return;
        }
        let dir = dev_tempdir("sock-denied-live");
        let guarded = dir.join("guarded.sock");
        let denied_listener = std::os::unix::net::UnixListener::bind(&guarded).unwrap();
        std::fs::set_permissions(&guarded, std::fs::Permissions::from_mode(0o000)).unwrap();
        let live = dir.join("live.sock");
        let listener = std::os::unix::net::UnixListener::bind(&live).unwrap();

        let found = probe(&[guarded, live.clone()]).ok();
        assert_eq!(
            found,
            Some(live),
            "the chain walks past the denial; the live candidate answers"
        );
        drop(denied_listener);
        drop(listener);
    }

    #[test]
    fn connection_refused_is_reported_with_the_stale_file_hint() {
        let err = ProbeError {
            attempted: vec![ProbeAttempt {
                path: PathBuf::from("/kp-stale/stale.sock"),
                kind: std::io::ErrorKind::ConnectionRefused,
            }],
        };

        let report = describe_probe_failure(&err);
        assert!(
            report.contains("connection refused"),
            "the refusal is named: {report}"
        );
        assert!(
            report.contains("stale socket file"),
            "the hint points at the likely cause: {report}"
        );
        assert!(
            report.contains("no reachable daemon socket"),
            "no denial keeps the absence phrasing: {report}"
        );
    }
}
