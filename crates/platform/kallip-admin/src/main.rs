//! `kallip-admin`: a headless operator CLI for the archeion relay. It is an HTTP
//! client authenticated with the `sk-admin-` bearer, driving the `/admin/*`
//! surface (enrollment codes, users, passkeys, admin-token lifecycle).
//!
//! The admin token is read from the `KALLIP_ARCHEION_ADMIN_TOKEN` environment
//! variable only (no `--admin-token` flag): a flag would leak the secret into
//! `ps`, `/proc/<pid>/cmdline`, and shell history, while an env var does not.
//! The archeion URL may be passed as `--archeion-url` or `KALLIP_ARCHEION_URL`.
//!
//! Example:
//!   `KALLIP_ARCHEION_ADMIN_TOKEN=sk-admin-... kallip-admin users list`
//!
//! Rotate the minted admin token (prints the fresh value once):
//!   KALLIP_ARCHEION_ADMIN_TOKEN=$(kallip-admin admin-token reset)

use anyhow::Result;
use clap::{Parser, Subcommand};
use comfy_table::{ContentArrangement, Table};
use kallip_archeion_client::{ApiError, ArcheionClient};
use kallip_archeion_common::admin::{
    CreateEnrollmentCodeRequest, Page, PageQuery, PasskeySummary, UpdateUserRequest, UserSummary,
};

#[derive(Parser)]
#[command(
    name = "kallip-admin",
    version,
    about = "Headless admin CLI for kallip-archeion (HTTP client)",
    after_help = "The admin token (sk-admin-...) is read from the KALLIP_ARCHEION_ADMIN_TOKEN \
                  environment variable. It is deliberately not a CLI flag: a flag leaks into \
                  ps, /proc/<pid>/cmdline, and shell history, while an env var does not."
)]
struct Args {
    /// Archeion control-plane base URL (e.g. http://127.0.0.1:7100
    /// for the standalone server).
    #[arg(
        long,
        env = "KALLIP_ARCHEION_URL",
        default_value = "http://127.0.0.1:7100"
    )]
    archeion_url: String,
    /// Emit raw JSON instead of human-readable tables.
    #[arg(long, default_value_t = false)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Reachability + admin-token probe (GET /healthz, then GET /admin).
    Ping,
    /// User account management.
    #[command(subcommand)]
    Users(UsersCmd),
    /// Passkey management.
    #[command(subcommand)]
    Passkeys(PasskeysCmd),
    /// Admin token lifecycle (minted tokens).
    #[command(subcommand)]
    AdminToken(AdminTokenCmd),
    /// Mint an enrollment code (sk-enroll) on a user's behalf.
    NewEnrollment {
        /// User id (UUID) to mint the enrollment code for.
        user_id: String,
    },
}

#[derive(Subcommand)]
enum UsersCmd {
    /// List users (paginated).
    List {
        #[arg(long, help = "fetch every page (ignores --limit/--cursor)")]
        all: bool,
        #[arg(long)]
        limit: Option<u64>,
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Ban a user (disables the account; takes effect on every auth path).
    Ban {
        /// User id (UUID), as shown by `users list`.
        user_id: String,
    },
    /// Re-enable a banned user.
    Enable {
        /// User id (UUID), as shown by `users list`.
        user_id: String,
    },
}

#[derive(Subcommand)]
enum PasskeysCmd {
    /// List a user's passkeys.
    List {
        /// User id (UUID) whose passkeys to list.
        user_id: String,
    },
    /// Revoke a passkey by id (hard-delete + audit row).
    Revoke {
        /// Passkey id (NOT the user id), as shown by `passkeys list`.
        id: String,
    },
}

#[derive(Subcommand)]
enum AdminTokenCmd {
    /// Print the minted admin token's bare value from the token file (a
    /// local read: no env var needed, no server round-trip).
    Show,
    /// Rotate the minted admin token (authenticates with the current
    /// token). Prints the fresh sk-admin- value once; the old one stops
    /// working. Typical use:
    /// KALLIP_ARCHEION_ADMIN_TOKEN=$(kallip-admin admin-token reset)
    Reset,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    // `admin-token show` is a pure local file read: it cannot require the
    // KALLIP_ARCHEION_ADMIN_TOKEN env var (that is what it recovers) and
    // must not build an HTTP client. Dispatch before the env read.
    if let Cmd::AdminToken(AdminTokenCmd::Show) = args.cmd {
        if let Err(e) = admin_token_show() {
            exit_err(&e);
        }
        return;
    }
    // The admin token is env-only (no flag) so it never lands in ps, cmdline,
    // or shell history. See the Args `after_help` for the rationale.
    let admin_token = match std::env::var("KALLIP_ARCHEION_ADMIN_TOKEN") {
        Ok(t) => t,
        Err(_) => exit_err(&anyhow::anyhow!(
            "KALLIP_ARCHEION_ADMIN_TOKEN required (the sk-admin- token)"
        )),
    };
    let client = ArcheionClient::builder(&args.archeion_url)
        .admin_token(admin_token)
        .build();
    // Builder only fails on reqwest client construction; surface it like any
    // runtime error rather than panicking.
    let client = match client {
        Ok(c) => c,
        Err(e) => exit_err(&anyhow::anyhow!(e)),
    };
    if let Err(e) = run(&client, args.json, args.cmd).await {
        exit_err(&e);
    }
}

fn exit_err(e: &anyhow::Error) -> ! {
    let msg = match e.downcast_ref::<ApiError>() {
        Some(api) => format!("{}: {}", api.status, api.message),
        None => format!("{e:#}"),
    };
    eprintln!("error: {msg}");
    std::process::exit(1);
}

/// Read and parse the minted admin token file: the bare secret from the
/// `KALLIP_ARCHEION_ADMIN_TOKEN=` line. A pure local read: no env var
/// needed (this is the recovery path for when the env var is not set),
/// no HTTP client. Error messages are operator-facing (they land on the
/// CLI's `error:` line); the ENOENT one names the pinned possibility.
fn read_admin_token_file(path: &str) -> anyhow::Result<String> {
    let raw = std::fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => anyhow::anyhow!(
            "no minted admin token file at {path}; the archeion may not have run yet, or the token is pinned"
        ),
        std::io::ErrorKind::PermissionDenied => anyhow::anyhow!(
            "permission denied reading {path}; run as root (sudo) or the kallip-archeion user"
        ),
        _ => anyhow::anyhow!("reading {path}: {e}"),
    })?;
    raw.lines()
        .find_map(|l| l.strip_prefix("KALLIP_ARCHEION_ADMIN_TOKEN="))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("{path} carries no KALLIP_ARCHEION_ADMIN_TOKEN value"))
}

/// `admin-token show`: print the minted token's bare value to stdout.
fn admin_token_show() -> anyhow::Result<()> {
    let path = std::env::var("KALLIP_ARCHEION_ADMIN_TOKEN_OUT_FILE")
        .unwrap_or_else(|_| "/var/lib/kallipai/archeion/admin-token.env".to_string());
    let secret = read_admin_token_file(&path)?;
    println!("{secret}");
    Ok(())
}

async fn run(client: &ArcheionClient, json: bool, cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Ping => {
            client.healthz().await?;
            client.admin_verify_token().await?;
            if json {
                println!("{{\"ok\":true}}");
            } else {
                println!("ok: archeion reachable, admin token valid");
            }
        }
        Cmd::NewEnrollment { user_id } => {
            let resp = client
                .admin_create_enrollment_code(CreateEnrollmentCodeRequest { user_id })
                .await?;
            println!("{}", resp.code);
        }
        Cmd::Users(sub) => match sub {
            UsersCmd::List { all, limit, cursor } => {
                let page = if all {
                    let items = fetch_all(limit, cursor, |q| {
                        let client = client.clone();
                        async move {
                            client
                                .admin_list_users(&q)
                                .await
                                .map(|p| (p.items, p.next_cursor))
                        }
                    })
                    .await?;
                    Page {
                        items,
                        next_cursor: None,
                    }
                } else {
                    client
                        .admin_list_users(&PageQuery { limit, cursor })
                        .await?
                };
                if json {
                    print_json(&page)?;
                } else {
                    print_users(&page.items);
                }
            }
            UsersCmd::Ban { user_id } => {
                let user = client
                    .admin_update_user(&user_id, UpdateUserRequest { disabled: true })
                    .await?;
                print_user_result(json, &user)?;
            }
            UsersCmd::Enable { user_id } => {
                let user = client
                    .admin_update_user(&user_id, UpdateUserRequest { disabled: false })
                    .await?;
                print_user_result(json, &user)?;
            }
        },
        Cmd::Passkeys(sub) => match sub {
            PasskeysCmd::List { user_id } => {
                let items: Vec<PasskeySummary> = client.admin_list_user_passkeys(&user_id).await?;
                if json {
                    print_json(&items)?;
                } else {
                    print_passkeys(&items);
                }
            }
            PasskeysCmd::Revoke { id } => {
                client.admin_revoke_passkey(&id).await?;
                println!("revoked");
            }
        },
        Cmd::AdminToken(AdminTokenCmd::Reset) => {
            let resp = client.admin_rotate_token().await?;
            println!("{}", resp.token);
        }
        // `admin-token show` never reaches `run`: main dispatches it before
        // the env read (it cannot require the token). Exhaustiveness only.
        Cmd::AdminToken(AdminTokenCmd::Show) => {
            unreachable!("admin-token show is handled in main")
        }
    }
    Ok(())
}

/// Fetch every page of a paginated endpoint until `next_cursor` is `None`.
async fn fetch_all<T, F, Fut>(
    limit: Option<u64>,
    cursor: Option<String>,
    mut fetch: F,
) -> Result<Vec<T>>
where
    F: FnMut(PageQuery) -> Fut,
    Fut: std::future::Future<Output = Result<(Vec<T>, Option<String>)>>,
{
    let mut items = Vec::new();
    let mut cursor = cursor;
    loop {
        let (page_items, next) = fetch(PageQuery {
            limit,
            cursor: cursor.take(),
        })
        .await?;
        items.extend(page_items);
        match next {
            Some(c) => cursor = Some(c),
            None => return Ok(items),
        }
    }
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn print_user_result(json: bool, user: &UserSummary) -> Result<()> {
    if json {
        print_json(user)?;
    } else {
        let state = if user.disabled_at.is_some() {
            "disabled"
        } else {
            "active"
        };
        println!("{}: {state}", user.id);
    }
    Ok(())
}

fn print_users(items: &[UserSummary]) {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec!["ID", "USERNAME", "EMAIL", "STATE"]);
    for u in items {
        let state = if u.disabled_at.is_some() {
            "disabled"
        } else {
            "active"
        };
        let email = u.primary_email.as_deref().unwrap_or("-");
        table.add_row(vec![&u.id, &u.username, email, state]);
    }
    println!("{table}");
}

fn print_passkeys(items: &[PasskeySummary]) {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec!["ID", "LABEL", "CREATED"]);
    for p in items {
        // The list endpoint returns only live passkeys; revoked history lives
        // in a separate audit table, so there is no per-row state here.
        let label = if p.label.is_empty() {
            "(unnamed)"
        } else {
            p.label.as_str()
        };
        table.add_row(vec![&p.id, label, &fmt_ts(p.created_at)]);
    }
    println!("{table}");
}

fn fmt_ts(ts: time::OffsetDateTime) -> String {
    // Compact ISO-8601 (UTC) is enough for an operator scan.
    ts.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "?".to_string())
}

#[cfg(test)]
mod tests {
    //! The `admin-token show` file-read/error-mapping surface, as a pure
    //! function of the file contents and the io error kind.

    use super::read_admin_token_file;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn show_reads_the_bare_value_from_key_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("admin-token.env");
        fs::write(&path, "OTHER=x\nKALLIP_ARCHEION_ADMIN_TOKEN=sk-admin-t\n").expect("write");
        let got = read_admin_token_file(path.to_str().expect("utf8")).expect("read");
        assert_eq!(got, "sk-admin-t");
    }

    #[test]
    fn show_missing_file_names_the_pinned_possibility() {
        let err = read_admin_token_file("/nonexistent-kallipai-admin/env")
            .expect_err("missing file must fail");
        let msg = err.to_string();
        assert!(msg.contains("no minted admin token file"), "{msg}");
        assert!(msg.contains("pinned"), "{msg}");
    }

    #[test]
    fn show_permission_denied_names_the_reader_fix() {
        // Root ignores permission bits, so this path is only exercisable
        // as a non-root user; under root the skip keeps the test honest.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("admin-token.env");
        fs::write(&path, "KALLIP_ARCHEION_ADMIN_TOKEN=sk-admin-t\n").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("chmod");
        let err = read_admin_token_file(path.to_str().expect("utf8"))
            .expect_err("unreadable file must fail");
        assert!(err.to_string().contains("permission denied"), "{err}");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("restore");
    }

    #[test]
    fn show_empty_value_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("admin-token.env");
        fs::write(&path, "KALLIP_ARCHEION_ADMIN_TOKEN=\n").expect("write");
        let err =
            read_admin_token_file(path.to_str().expect("utf8")).expect_err("empty value must fail");
        assert!(
            err.to_string()
                .contains("carries no KALLIP_ARCHEION_ADMIN_TOKEN"),
            "{err}"
        );
    }
}
