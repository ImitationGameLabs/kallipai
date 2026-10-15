//! Operator token handling: path resolution, prompt, store, read and rotate.

use crate::cli::OperatorTokenCommand;
use anyhow::Context as _;
use anyhow::Result;
use kallipai_common::authtoken::{OPERATOR_TOKEN_ENV_KEY, OPERATOR_TOKEN_FILE};
use kallipai_daemon_client::DaemonClient;
use kallipai_daemon_common::wire::{OkPayload, RequestBody, ResponseBody};

/// The instance's token file path: one daemon query supplies the data
/// dir the daemon computed at launch (in-place and drop-to launches
/// differ, so a local guess would miss drop-to instances). Only the
/// path crosses the wire; the secret is read and written locally.
async fn operator_token_path(client: &DaemonClient, slug: &str) -> Result<std::path::PathBuf> {
    let response = client
        .call(RequestBody::Record {
            slug: slug.to_owned(),
        })
        .await
        .context("talking to the kallipai daemon")?;
    let data_dir = match response.body {
        ResponseBody::Ok {
            payload: OkPayload::Record { data_dir, .. },
        } => data_dir,
        ResponseBody::Err { message, .. } => {
            anyhow::bail!("cannot resolve {slug}: {message}");
        }
        _ => anyhow::bail!("unexpected daemon response while resolving {slug}"),
    };
    let credentials = kallipai_daemon_common::credentials::credentials_root(&data_dir);
    Ok(credentials.join(kallipai_common::authtoken::OPERATOR_TOKEN_FILE))
}

/// Read and parse the token file: the bare secret from the
/// `KALLIPAI_OPERATOR_TOKEN=` line. Error messages are operator-facing
/// (they land on the CLI's `error:` line); the ENOENT one names the
/// pinned possibility, the EACCES one the owner-or-sudo requirement.
pub(crate) fn read_operator_token_file(path: &std::path::Path) -> Result<String> {
    let raw = std::fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => anyhow::anyhow!("no operator token file at {}; the instance may not have run yet, or the token is pinned via {OPERATOR_TOKEN_ENV_KEY}", path.display()),
        std::io::ErrorKind::PermissionDenied => anyhow::anyhow!("permission denied reading {}; run as root (sudo) or the instance's owner user", path.display()),
        _ => anyhow::anyhow!("reading {}: {e}", path.display()),
    })?;
    let prefix = format!("{OPERATOR_TOKEN_ENV_KEY}=");
    raw.lines()
        .find_map(|l| l.strip_prefix(prefix.as_str()))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{} carries no {OPERATOR_TOKEN_ENV_KEY} value",
                path.display()
            )
        })
}

/// Strip exactly one trailing newline (LF, or CRLF) — what a pipe, a
/// redirect, or an Enter keypress adds. Every other byte stays verbatim,
/// matching how the env pin keeps the operator's value untouched (the
/// tool never silently rewrites the value).
pub(crate) fn strip_trailing_newline(mut raw: String) -> String {
    if raw.ends_with('\n') {
        raw.pop();
    }
    if raw.ends_with('\r') {
        raw.pop();
    }
    raw
}

/// Terminal echo disabled for the enclosing scope (the hidden-input
/// prompt). The original termios is parked in [`SAVED_TERMIOS`] so the
/// signal watcher can restore it even though Ctrl-C bypasses `Drop`;
/// `Drop` still covers every ordinary exit, including error unwinding.
struct EchoOff(libc::termios);

/// The terminal state captured while a hidden read is in flight.
static SAVED_TERMIOS: std::sync::Mutex<Option<libc::termios>> = std::sync::Mutex::new(None);

impl EchoOff {
    fn new() -> Result<Self> {
        // SAFETY: tcgetattr on our own stdin fd; libc fills the struct
        // completely on success.
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut term) } != 0 {
            anyhow::bail!("could not read terminal settings");
        }
        let original = term;
        term.c_lflag &= !libc::ECHO;
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &term) } != 0 {
            anyhow::bail!("could not disable terminal echo");
        }
        if let Ok(mut slot) = SAVED_TERMIOS.lock() {
            *slot = Some(original);
        }
        Ok(EchoOff(original))
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        // SAFETY: restoring the settings captured at construction.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) };
        if let Ok(mut slot) = SAVED_TERMIOS.lock() {
            *slot = None;
        }
    }
}

/// Parked beside the hidden read: SIGINT/SIGTERM bypass `Drop`, so the
/// watcher restores the terminal echo first and only then exits with the
/// signal's conventional status. Runs as a normal task, so the plain
/// Mutex access here is sound.
async fn restore_echo_on_signal() -> ! {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM watcher");
    let mut interrupt = signal(SignalKind::interrupt()).expect("SIGINT watcher");
    tokio::select! {
        _ = terminate.recv() => restore_echo_and_exit(143),
        _ = interrupt.recv() => restore_echo_and_exit(130),
    }
}

fn restore_echo_and_exit(status: i32) -> ! {
    if let Ok(Some(term)) = SAVED_TERMIOS.lock().map(|mut slot| slot.take()) {
        // SAFETY: restoring the terminal state captured at prompt start.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &term) };
    }
    std::process::exit(status)
}

/// Read the token value from stdin: a hidden prompt when stdin is a
/// terminal (the operator types it; echo stays off), a plain line when
/// piped or redirected (automation). Empty or whitespace-only input is
/// refused before anything is written.
async fn read_token_value() -> Result<String> {
    let hidden = unsafe { libc::isatty(0) } == 1;
    let guard = if hidden {
        eprint!("Enter operator token (input hidden): ");
        use std::io::Write;
        std::io::stderr().flush()?;
        Some(EchoOff::new()?)
    } else {
        None
    };
    // The watcher loses the race only when the read returns; while the
    // read blocks it is the one that answers a signal.
    let watcher = tokio::spawn(restore_echo_on_signal());
    let raw = tokio::task::spawn_blocking(move || {
        let mut raw = String::new();
        std::io::stdin()
            .read_line(&mut raw)
            .context("reading from stdin")?;
        Ok::<String, anyhow::Error>(raw)
    })
    .await
    .context("stdin read task")??;
    watcher.abort();
    drop(guard);
    if hidden {
        eprintln!();
    }
    let value = strip_trailing_newline(raw);
    if value.trim().is_empty() {
        anyhow::bail!("no token value on stdin (empty input refused; nothing written)");
    }
    Ok(value)
}

/// Persist the value: credentials dir created owner-only when missing,
/// then temp-write + chmod 0600 + rename (atomic; the secret is never
/// world-readable). Same mechanics as the tagma's boot-time writer.
pub(crate) fn write_operator_token_file(
    credentials_dir: &std::path::Path,
    secret: &str,
) -> Result<()> {
    std::fs::create_dir_all(credentials_dir)
        .with_context(|| format!("creating {}", credentials_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(credentials_dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod {}", credentials_dir.display()))?;
    }
    let path = credentials_dir.join(OPERATOR_TOKEN_FILE);
    let body = format!("{OPERATOR_TOKEN_ENV_KEY}={secret}\n");
    let tmp = credentials_dir.join(format!(".{OPERATOR_TOKEN_FILE}.tmp"));
    std::fs::write(&tmp, body).context("write operator token temp file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .context("chmod operator token file")?;
    }
    std::fs::rename(&tmp, &path).context("rename operator token file into place")?;
    Ok(())
}

pub(crate) async fn run_operator_token(
    command: OperatorTokenCommand,
    client: &DaemonClient,
) -> Result<()> {
    match command {
        OperatorTokenCommand::Show { slug } => {
            let path = operator_token_path(client, &slug).await?;
            println!("{}", read_operator_token_file(&path)?);
        }
        OperatorTokenCommand::Set { slug } => {
            let value = read_token_value().await?;
            write_for_slug(client, &slug, &value).await?;
            eprintln!(
                "operator token written; it takes effect on next start (kallipctl restart {slug})"
            );
            eprintln!(
                "note: an instance started with {OPERATOR_TOKEN_ENV_KEY} set ignores this file"
            );
        }
        OperatorTokenCommand::Reset { slug } => {
            // The operator-prefix literal mirrors kallipai-tagma's
            // `token::OPERATOR` (each crate pins its own prefixes — see
            // kallipai-common::authtoken's module doc); tagma itself is
            // deliberately not a kallipctl dependency.
            const OPERATOR_PREFIX: kallipai_common::authtoken::TokenKind =
                kallipai_common::authtoken::TokenKind("sk-operator-");
            let secret = kallipai_common::authtoken::MintedToken::generate(OPERATOR_PREFIX);
            write_for_slug(client, &slug, secret.secret()).await?;
            // The one print the operator explicitly asked for (the admin
            // token reset contract).
            println!("{}", secret.secret());
            eprintln!(
                "fresh operator token written; it takes effect on next start (kallipctl restart {slug})"
            );
            eprintln!(
                "note: an instance started with {OPERATOR_TOKEN_ENV_KEY} set ignores this file"
            );
        }
    }
    Ok(())
}

/// Resolve the slug's credentials dir and persist the value there.
async fn write_for_slug(client: &DaemonClient, slug: &str, secret: &str) -> Result<()> {
    let path = operator_token_path(client, slug).await?;
    let dir = path
        .parent()
        .expect("the token path always has a credentials parent")
        .to_path_buf();
    write_operator_token_file(&dir, secret)
}
