//! kallip-daemon: stateless, registry-driven manager for local kallip
//! instances.
//!
//! Configuration is exactly two environment variables — no config file:
//! - `KALLIP_DAEMON_RECORD_DIR`: the record area root (default
//!   `~/.local/state/kallipai/daemon/instances`); each `<slug>.json`
//!   file is a managed instance, and the directory is the inventory.
//! - `KALLIP_DAEMON_SOCKET`: an explicit control-socket path; without it
//!   the socket binds the first resolvable leg of the shared chain (see
//!   `kallip_daemon_common::socket`): the runtime dir, then the state
//!   home default (`~/.local/state/kallipai/daemon`).
//!
//! The control socket is 0600: filesystem permission is the only auth.

mod adopt;
mod bins;
mod delegates;
mod env;
mod log;
mod reconcile;
mod records;
mod remove;
mod scan;
mod server;
mod spawn;
mod start;
mod stop;

use anyhow::{Context as _, Result};
use std::ffi::CString;

fn main() -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // Delegation grants fail at startup, not at first use: a list that
    // cannot be honored (unknown name, root, the daemon itself) means
    // this binary must not serve.
    delegates::init_from_env()
        .map_err(|e| anyhow::anyhow!("parse KALLIP_DAEMON_DELEGATES: {e}"))?;

    let record_root = records::record_root()?;
    // The record area is the only tree this daemon owns. It is created
    // lazily by the first registration; the socket parent below is the
    // only boot-time creation.
    let socket_path = kallip_daemon_common::socket::daemon_bind_path().context(
        "no control-socket candidate: set KALLIP_DAEMON_SOCKET, or make the platform state home determinable",
    )?;

    // Prove-liveness socket takeover: a blind unlink
    // could steal the endpoint of a live daemon — the first daemon keeps
    // running but loses its socket file. Instead, bind; on EADDRINUSE,
    // probe by connecting: a live listener refuses startup ("already
    // running"), a refused connection means the file is stale, unlink and
    // bind once more.
    // Close the umask before anything is created: the socket is chmod'd
    // 0600 only after it exists, and a group/other-connectable window
    // in between is unacceptable for the system-install layout. Every
    // file this daemon creates is private, so the mask stays for life.
    unsafe { libc::umask(0o077) };
    std::fs::create_dir_all(
        socket_path
            .parent()
            .context("control socket path has no parent")?,
    )
    .context("create control-socket parent dir")?;
    // Prove-liveness check runs on std sockets (blocking is fine for a
    // one-shot probe); the serving listener is bound by tokio directly.
    refuse_if_live(&socket_path)?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    let socket_path_for_bind = socket_path.clone();
    let record_root_for_serve = record_root.clone();
    runtime.block_on(async move {
        use std::os::unix::fs::PermissionsExt as _;
        let listener = tokio::net::UnixListener::bind(&socket_path_for_bind)
            .with_context(|| format!("bind {}", socket_path_for_bind.display()))?;
        std::fs::set_permissions(
            &socket_path_for_bind,
            std::fs::Permissions::from_mode(0o600),
        )
        .context("chmod socket 0600")?;
        // Optional widening for the system install: the module sets the
        // group name, the daemon applies it — the ownership change lives
        // on the same side as the umask/chmod above, so no external
        // ExecStartPost can race the mode into a wrong final state.
        if let Some(group) = std::env::var_os("KALLIP_DAEMON_SOCKET_GROUP") {
            apply_socket_group(&socket_path_for_bind, &group.to_string_lossy())
                .context("apply socket group")?;
        }
        tracing::info!(
            record_root = %record_root_for_serve.display(),
            socket = %socket_path_for_bind.display(),
            "kallip-daemon listening"
        );
        // The reconcile sweep keeps its snapshot inside its own task; see
        // the module doc for why the daemon proper stays stateless.
        tokio::spawn(reconcile::run(record_root_for_serve.clone()));
        let daemon = server::Daemon::new(record_root_for_serve);
        daemon.serve(listener).await
    })
}

/// Refuse to take over a live daemon's socket (root 16:31Z mandate): a
/// refused-or-absent socket file is stale — unlink so the async bind
/// starts clean; a successful connect means a live listener owns the
/// endpoint and startup must fail instead of stealing it.
fn refuse_if_live(path: &std::path::Path) -> Result<()> {
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_probe) => {
            tracing::error!(
                socket = %path.display(),
                "another daemon owns this socket; refusing to start"
            );
            anyhow::bail!(
                "another kallip-daemon is listening at {}; refusing to take over a live socket",
                path.display()
            );
        }
        Err(_) => {
            let _ = std::fs::remove_file(path);
            Ok(())
        }
    }
}

/// Hand the socket to `group` (name resolved through the group
/// database) and widen the mode to 0660. Why the daemon does this
/// rather than a unit-side ExecStartPost: the mode is the access
/// policy of the platform-hosting form (the declared group reaches
/// the daemon for self-uid spawns), and only the process that owns
/// the umask-and-chmod sequence above can set the final state
/// without racing itself.
fn apply_socket_group(path: &std::path::Path, group: &str) -> Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    let group_c = CString::new(group).context("socket group name with NUL byte")?;
    // SAFETY: getgrnam returns a libc-owned struct or null; the gid
    // is copied out immediately.
    let gid = unsafe { libc::getgrnam(group_c.as_ptr()) };
    let gid = unsafe { gid.as_ref() }
        .map(|gr| gr.gr_gid)
        .with_context(|| format!("socket group {group:?} does not exist"))?;
    let path_c = CString::new(path.as_os_str().as_bytes()).context("socket path with NUL byte")?;
    // SAFETY: chown(2) on our own freshly bound socket; failure is
    // fatal — a socket the declared group cannot reach breaks the
    // deployment's access model silently otherwise.
    let rc = unsafe { libc::chown(path_c.as_ptr(), u32::MAX, gid) };
    if rc != 0 {
        anyhow::bail!(
            "chown socket to group {group}: {}",
            std::io::Error::last_os_error()
        );
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
        .context("chmod socket 0660")?;
    Ok(())
}
