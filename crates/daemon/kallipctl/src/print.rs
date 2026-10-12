//! Output shapes: the scripting vocabulary and the machine-readable printer.

use anyhow::Result;
use kallipai_daemon_common::wire::{ErrorCode, InstanceState, OkPayload, Response, ResponseBody};

/// The stable error vocabulary scripts match on, shared by the
/// plain verbs and the logs loop.
pub(crate) fn error_prefix(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::SlugTaken => "instance conflict",
        ErrorCode::Denied => "not authorized",
        ErrorCode::WorkspaceOverlap => "instance path overlap",
        ErrorCode::InvalidSpawnInput => "invalid spawn input",
        ErrorCode::SpawnTimeout => "spawn timed out (rolled back)",
        ErrorCode::NotFound => "no such instance",
        ErrorCode::NotRunning => "not running",
        ErrorCode::BadRequest => "bad request",
        ErrorCode::Internal => "internal error",
        ErrorCode::Unknown => "unknown error code from daemon",
    }
}

pub(crate) fn print(response: Response, started: bool) -> Result<()> {
    match response.body {
        ResponseBody::Ok { payload } => {
            match payload {
                OkPayload::Spawn { slug, pid, port } => {
                    let verb = if started { "started" } else { "spawned" };
                    println!("{verb} {slug} (pid {pid}, port {port})");
                }
                OkPayload::Stop { slug } => {
                    println!("stopped {slug}");
                }
                OkPayload::Remove { slug } => {
                    println!("removed {slug}");
                }
                OkPayload::List { instances } => {
                    if instances.is_empty() {
                        println!("no instances");
                        return Ok(());
                    }
                    // One labeled block per instance: every field is a full
                    // greppable line, so agents and scripts read records
                    // without parsing column alignment.
                    for (n, i) in instances.iter().enumerate() {
                        if n > 0 {
                            println!();
                        }
                        println!("{}", i.slug);
                        println!("  state: {}", i.state.as_str());
                        println!("  workspace: {}", i.workspace);
                        println!(
                            "  owner-uid: {}",
                            i.owner.map(|u| u.to_string()).unwrap_or_else(|| "-".into())
                        );
                        println!("  instance-id: {}", i.instance_id);
                    }
                }
                OkPayload::Health { report } => match (&report.slug, report.running) {
                    (None, _) => println!("daemon: healthy"),
                    (Some(slug), true) => println!("{slug}: running"),
                    (Some(slug), false) => {
                        println!(
                            "{slug}: not running ({})",
                            report.detail.unwrap_or_default()
                        );
                    }
                },
                OkPayload::Adopt { slug, state } => {
                    println!("{}", adopt_line(&slug, state));
                }
                OkPayload::EnvGet { slug, env } => {
                    if env.is_empty() {
                        println!("{slug}: no persisted env");
                        return Ok(());
                    }
                    // One KEY=VALUE line per variable, no separator:
                    // each line copies straight into a shell.
                    for pair in &env {
                        println!("{pair}");
                    }
                }
                OkPayload::EnvSet { slug } => {
                    println!("updated {slug} env; takes effect on next start");
                }
                OkPayload::EnvUnset { slug, removed } => {
                    let listed = removed.join(", ");
                    println!("removed {listed} from {slug} env; takes effect on next start");
                }
                OkPayload::Record { data_dir, state } => {
                    println!("{}", data_dir.display());
                    println!("state: {}", state.as_str());
                }
                OkPayload::Log { text, .. } => {
                    if text.is_empty() {
                        println!("(no logs)");
                    } else {
                        print_log_text(&text);
                    }
                }
                // The service-log verbs print their own output (the
                // tail loop and the listing shape), so this payload
                // never routes here; the arm keeps the match
                // exhaustive.
                OkPayload::ServiceLogList { .. } => {}
            }
            Ok(())
        }
        ResponseBody::Err { code, message } => {
            // Errors exit non-zero with a human line; stable codes are the
            // machine interface for scripting.
            let prefix = error_prefix(code);
            anyhow::bail!("{prefix}: {message}");
        }
    }
}

/// The one-line adopt outcome: verb, slug, and the observed state —
/// the registration fact a script greps for.
pub(crate) fn adopt_line(slug: &str, state: InstanceState) -> String {
    format!("adopted {slug} ({})", state.as_str())
}

/// Print a log chunk as-is with exactly one trailing newline.
pub(crate) fn print_log_text(text: &str) {
    if text.ends_with('\n') {
        print!("{text}");
    } else {
        println!("{text}");
    }
}
