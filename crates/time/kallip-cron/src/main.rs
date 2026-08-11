//! `kallip-cron` — management CLI for the kallip-cron timer daemon.
//!
//! Env-driven like `kallip`: `CronClient::from_env` reads `KALLIP_CRON_URL` +
//! `KALLIP_AUTH_TOKEN`, and each command resolves the caller's agent id from
//! `KALLIP_ID` (both auto-injected into every agent shell by the tagma). The
//! daemon verifies the `(agent_id, token)` pair against the tagma and scopes
//! every operation to that agent's own schedules.

use std::io::Read;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use kallip_common::agentid::AgentId;
use kallip_cron_client::CronClient;
use kallip_cron_common::{Priority, ScheduleStatus, TriggerSpec};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

#[derive(Parser)]
#[command(
    name = "kallip-cron",
    version,
    about = "Manage your kallip-cron timers and reminders (self-scoped)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new schedule (targets your own conversation)
    Create(CreateArgs),
    /// List your schedules
    List(ListArgs),
    /// Show one of your schedules
    Get { id: String },
    /// Delete one of your schedules
    Delete { id: String },
    /// Pause one of your schedules (hold firing)
    Pause { id: String },
    /// Resume a paused schedule
    Resume { id: String },
    /// Show your next schedule to fire
    Next,
    /// Show your schedule status
    Status,
}

#[derive(Args)]
struct CreateArgs {
    /// Human-readable name.
    #[arg(long)]
    name: String,
    /// Message text. If omitted, reads the full text from stdin (multiline).
    #[arg(long, allow_hyphen_values = true)]
    message: Option<String>,
    /// One-shot at an absolute RFC3339 time (e.g. "2025-12-25T09:00:00Z").
    #[arg(long, value_name = "RFC3339")]
    once: Option<String>,
    /// One-shot after a delay. Bare integer = seconds (`5400`); or unit
    /// segments `s`/`m`/`h`/`d`/`w` (`90m`, `1h30m`, `2d`).
    #[arg(long, value_name = "DURATION")]
    r#in: Option<String>,
    /// Recurring interval. Bare integer = seconds (`180`); or unit segments
    /// `s`/`m`/`h`/`d`/`w` (`5m`, `1h30m`, `2d`). Must be >= 3 minutes.
    #[arg(long, value_name = "DURATION")]
    every: Option<String>,
    /// Tag (repeatable).
    #[arg(long = "tag", value_name = "TAG")]
    tags: Vec<String>,
    /// Priority (low/normal/high/urgent). Default normal.
    #[arg(long, value_name = "PRIORITY")]
    priority: Option<Priority>,
}

impl CreateArgs {
    fn trigger(&self) -> Result<TriggerSpec> {
        let n_specified = [
            self.once.is_some(),
            self.r#in.is_some(),
            self.every.is_some(),
        ]
        .iter()
        .filter(|&&b| b)
        .count();
        if n_specified != 1 {
            bail!("specify exactly one of --once, --in, --every");
        }
        if let Some(s) = &self.once {
            let at = OffsetDateTime::parse(s, &Rfc3339)
                .with_context(|| format!("invalid --once RFC3339 time: {s}"))?;
            return Ok(TriggerSpec::Once { at });
        }
        if let Some(spec) = &self.r#in {
            let secs = parse_duration_seconds(spec)
                .with_context(|| format!("invalid --in duration: {spec}"))?;
            return Ok(TriggerSpec::In {
                duration_seconds: secs,
            });
        }
        let spec = self.every.as_ref().expect("checked above");
        let secs = parse_duration_seconds(spec)
            .with_context(|| format!("invalid --every duration: {spec}"))?;
        Ok(TriggerSpec::Every {
            duration_seconds: secs,
        })
    }
}

/// Parse a `--in` duration into whole seconds. Accepts a bare integer
/// (seconds, back-compat: `5400`) **or** one-or-more `<number><unit>` segments
/// where unit is `s`/`m`/`h`/`d`/`w` (`90m`, `1h30m`, `2d`). Uses checked
/// arithmetic; absurd values are caught downstream by `TriggerSpec::validate`'s
/// ~10-year ceiling anyway.
fn parse_duration_seconds(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty duration");
    }
    // Bare integer = seconds (back-compat with the old u64 field).
    if let Ok(secs) = s.parse::<u64>() {
        return Ok(secs);
    }
    let mut total: u64 = 0;
    let mut num = String::new();
    let mut saw_unit = false;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
            continue;
        }
        let n: u64 = num.parse().context("duration segment missing a number")?;
        num.clear();
        let mult: u64 = match ch {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            'w' => 604_800,
            other => bail!("unknown duration unit '{other}' (use s/m/h/d/w)"),
        };
        total = n
            .checked_mul(mult)
            .and_then(|added| total.checked_add(added))
            .context("duration overflow")?;
        saw_unit = true;
    }
    if !saw_unit {
        bail!("duration has no unit (use s/m/h/d/w, or a bare integer for seconds)");
    }
    if !num.is_empty() {
        bail!("trailing number without a unit");
    }
    Ok(total)
}

#[derive(Args)]
struct ListArgs {
    /// Filter by status (active/paused/completed/triggered).
    #[arg(long)]
    status: Option<String>,
    /// Filter by tag.
    #[arg(long)]
    tag: Option<String>,
}

/// Read the caller's agent id from `KALLIP_ID` (mirrors `kallip`'s helper).
fn agent_id_from_env() -> Result<AgentId> {
    std::env::var("KALLIP_ID")
        .map_err(|_| anyhow::anyhow!("KALLIP_ID env var not set"))
        .and_then(|s| s.parse::<AgentId>().map_err(Into::into))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = CronClient::from_env().context("build cron client (set KALLIP_CRON_URL)")?;

    match cli.command {
        Command::Create(a) => {
            let agent = agent_id_from_env()?;
            let trigger = a.trigger()?;
            trigger.validate().map_err(|e| anyhow::anyhow!(e))?;
            let message = match a.message {
                Some(m) => m,
                None => {
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf)?;
                    buf
                }
            };
            let req = kallip_cron_common::CreateScheduleRequest {
                name: a.name,
                trigger,
                agent_id: agent,
                message,
                tags: a.tags,
                priority: a.priority.unwrap_or_default(),
            };
            let sched = client.create(req).await?;
            println!("{}", sched.id);
        }
        Command::List(a) => {
            let agent = agent_id_from_env()?;
            let status = a
                .status
                .as_deref()
                .map(|s| s.parse::<ScheduleStatus>())
                .transpose()
                .map_err(|e| anyhow::anyhow!("invalid status: {e}"))?;
            let scheds = client.list(&agent, status, a.tag.as_deref()).await?;
            if scheds.is_empty() {
                println!("(no schedules)");
            } else {
                for s in &scheds {
                    println!("{}", format_schedule(s));
                }
            }
        }
        Command::Get { id } => {
            let agent = agent_id_from_env()?;
            match client.get(&agent, &id).await? {
                Some(s) => println!("{}", format_schedule(&s)),
                None => bail!("schedule {id} not found"),
            }
        }
        Command::Delete { id } => {
            let agent = agent_id_from_env()?;
            if client.delete(&agent, &id).await? {
                println!("Deleted {id}.");
            } else {
                bail!("schedule {id} not found");
            }
        }
        Command::Pause { id } => {
            let agent = agent_id_from_env()?;
            client
                .update(
                    &agent,
                    &id,
                    kallip_cron_common::UpdateScheduleRequest {
                        status: Some(ScheduleStatus::Paused),
                    },
                )
                .await?;
            println!("Paused {id}.");
        }
        Command::Resume { id } => {
            let agent = agent_id_from_env()?;
            client
                .update(
                    &agent,
                    &id,
                    kallip_cron_common::UpdateScheduleRequest {
                        status: Some(ScheduleStatus::Active),
                    },
                )
                .await?;
            println!("Resumed {id}.");
        }
        Command::Next => {
            let agent = agent_id_from_env()?;
            match client.next(&agent).await? {
                Some(s) => println!("{}", format_schedule(&s)),
                None => println!("(no active schedule)"),
            }
        }
        Command::Status => {
            let agent = agent_id_from_env()?;
            let st = client.status(&agent).await?;
            println!("healthy: {}", st.healthy);
            println!("active_schedules: {}", st.active_schedules);
            println!("pending_triggered: {}", st.pending_triggered);
            if let Some(nf) = st.next_fire {
                println!("next_fire: {nf}");
            }
        }
    }
    Ok(())
}

/// One-line summary of a schedule for list/get/next output.
fn format_schedule(s: &kallip_cron_common::Schedule) -> String {
    let nf = s
        .next_fire
        .map(|t| t.to_string())
        .unwrap_or_else(|| "-".into());
    format!(
        "{}  [{}]  {}  trigger={}  next={}",
        s.id,
        s.status,
        s.name,
        trigger_label(&s.trigger),
        nf,
    )
}

fn trigger_label(t: &TriggerSpec) -> String {
    match t {
        TriggerSpec::Once { at } => format!("once@{at}"),
        TriggerSpec::In { duration_seconds } => {
            format!("in {}", format_duration_seconds(*duration_seconds))
        }
        TriggerSpec::Every { duration_seconds } => {
            format!("every {}", format_duration_seconds(*duration_seconds))
        }
    }
}

/// Format a whole-second duration as the compact segment form (`3h28m`,
/// `5m`, `1d`, `1w`), largest unit first, omitting zero segments. The inverse
/// of [`parse_duration_seconds`]: `parse_duration_seconds(&format_duration_seconds(n)) == Ok(n)`.
fn format_duration_seconds(total: u64) -> String {
    const UNITS: &[(u64, &str)] = &[
        (604_800, "w"),
        (86_400, "d"),
        (3_600, "h"),
        (60, "m"),
        (1, "s"),
    ];
    let mut remaining = total;
    let mut out = String::new();
    for (mult, label) in UNITS {
        let n = remaining / mult;
        remaining %= mult;
        if n > 0 {
            out.push_str(&format!("{n}{label}"));
        }
    }
    if out.is_empty() {
        // A zero (or sub-second) duration renders as 0s rather than the empty
        // string; this branch is unreachable for validated triggers but keeps
        // the function total.
        out.push_str("0s");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_integer_is_seconds() {
        assert_eq!(parse_duration_seconds("5400").unwrap(), 5400);
        assert_eq!(parse_duration_seconds("0").unwrap(), 0);
    }

    #[test]
    fn single_unit() {
        assert_eq!(parse_duration_seconds("90m").unwrap(), 5400);
        assert_eq!(parse_duration_seconds("1h").unwrap(), 3600);
        assert_eq!(parse_duration_seconds("2d").unwrap(), 172_800);
        assert_eq!(parse_duration_seconds("1w").unwrap(), 604_800);
        assert_eq!(parse_duration_seconds("45s").unwrap(), 45);
    }

    #[test]
    fn compound_units() {
        assert_eq!(parse_duration_seconds("1h30m").unwrap(), 5400);
        assert_eq!(parse_duration_seconds("2d4h").unwrap(), 187_200);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse_duration_seconds("").is_err());
        assert!(parse_duration_seconds("1x").is_err()); // unknown unit
        assert!(parse_duration_seconds("1h30").is_err()); // trailing number
        assert!(parse_duration_seconds("h").is_err()); // missing number
        assert!(parse_duration_seconds("123").is_ok()); // bare int still ok
        // No unit on a multi-char non-numeric string.
        assert!(parse_duration_seconds("abc").is_err());
    }

    #[test]
    fn overflow_does_not_panic() {
        // Would overflow u64 seconds; must error, not panic.
        assert!(parse_duration_seconds("999999999999999w").is_err());
    }

    #[test]
    fn format_covers_each_unit() {
        assert_eq!(format_duration_seconds(45), "45s");
        assert_eq!(format_duration_seconds(180), "3m");
        assert_eq!(format_duration_seconds(3600), "1h");
        assert_eq!(format_duration_seconds(5400), "1h30m");
        assert_eq!(format_duration_seconds(12_480), "3h28m");
        assert_eq!(format_duration_seconds(86_400), "1d");
        assert_eq!(format_duration_seconds(604_800), "1w");
        assert_eq!(format_duration_seconds(0), "0s");
    }

    #[test]
    fn format_is_inverse_of_parse() {
        // Every value formatted then parsed must round-trip exactly, across all
        // unit boundaries and a compound value.
        for n in [
            1u64,
            45,
            60,
            180,            // the recurrence floor
            5400,           // 1h30m
            12_480,         // 3h28m
            86_400,         // 1d
            604_800,        // 1w
            604_800 + 3600, // 1w1h
        ] {
            let rendered = format_duration_seconds(n);
            assert_eq!(parse_duration_seconds(&rendered).unwrap(), n, "{rendered}");
        }
    }
}
