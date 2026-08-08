//! `kallip-admin`: a headless operator CLI for the agora relay. It is an HTTP
//! client authenticated with the `sk-admin-` bearer, driving the `/v1/admin/*`
//! surface (enrollment codes, users, passkeys).
//!
//! The admin token is read from the `KALLIP_AGORA_ADMIN_TOKEN` environment
//! variable only (no `--admin-token` flag): a flag would leak the secret into
//! `ps`, `/proc/<pid>/cmdline`, and shell history, while an env var does not.
//! The agora URL may be passed as `--agora-url` or `KALLIP_AGORA_URL`.
//!
//! Example:
//!   `KALLIP_AGORA_ADMIN_TOKEN=sk-admin-... kallip-admin users list`

use anyhow::Result;
use clap::{Parser, Subcommand};
use comfy_table::{ContentArrangement, Table};
use kallip_agora_client::{AgoraClient, ApiError};
use kallip_agora_common::admin::{
    CreateEnrollmentCodeRequest, Page, PageQuery, PasskeySummary, UpdateUserRequest, UserSummary,
};

#[derive(Parser)]
#[command(
    name = "kallip-admin",
    version,
    about = "Headless admin CLI for kallip-agora (HTTP client)",
    after_help = "The admin token (sk-admin-...) is read from the KALLIP_AGORA_ADMIN_TOKEN \
                  environment variable. It is deliberately not a CLI flag: a flag leaks into \
                  ps, /proc/<pid>/cmdline, and shell history, while an env var does not."
)]
struct Args {
    /// Agora base URL.
    #[arg(
        long,
        env = "KALLIP_AGORA_URL",
        default_value = "http://127.0.0.1:7100"
    )]
    agora_url: String,
    /// Emit raw JSON instead of human-readable tables.
    #[arg(long, default_value_t = false)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Reachability + admin-token probe (GET /healthz, then GET /v1/admin).
    Ping,
    /// User account management.
    #[command(subcommand)]
    Users(UsersCmd),
    /// Passkey management.
    #[command(subcommand)]
    Passkeys(PasskeysCmd),
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

#[tokio::main]
async fn main() {
    let args = Args::parse();
    // The admin token is env-only (no flag) so it never lands in ps, cmdline,
    // or shell history. See the Args `after_help` for the rationale.
    let admin_token = match std::env::var("KALLIP_AGORA_ADMIN_TOKEN") {
        Ok(t) => t,
        Err(_) => exit_err(&anyhow::anyhow!(
            "KALLIP_AGORA_ADMIN_TOKEN required (the sk-admin- token)"
        )),
    };
    let client = AgoraClient::builder(&args.agora_url)
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

async fn run(client: &AgoraClient, json: bool, cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Ping => {
            client.healthz().await?;
            client.admin_verify_token().await?;
            if json {
                println!("{{\"ok\":true}}");
            } else {
                println!("ok: agora reachable, admin token valid");
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
