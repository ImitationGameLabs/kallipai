//! Schedule model + trigger specs for `kallip-cron`.
//!
//! Ported from Ephemera's `kairos-common::schedule`, adapted for kallipai:
//! the free-form `payload: serde_json::Value` is replaced by a concrete
//! `agent_id` + `message` delivery target (a fired timer is injected into a
//! conversation via `POST /agents/{id}/message`), the dead `Cron` trigger
//! variant is dropped (planned v2), and the model carries the precision/UTC
//! contract used throughout the daemon.

use kallip_common::agentid::AgentId;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;

/// Unique identifier for a schedule (UUID v4 string minted by the daemon).
pub type ScheduleId = String;

/// Schedule priority levels.
///
/// Metadata-only in v1 (the deliverer does not reorder by priority); kept for
/// future UI filtering and a v2 priority-aware delivery order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
    Urgent,
}

impl std::fmt::Display for Priority {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let s = match self {
            Priority::Low => "low",
            Priority::Normal => "normal",
            Priority::High => "high",
            Priority::Urgent => "urgent",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for Priority {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Strict lowercase, matching the serde canonical form (`rename_all =
        // "lowercase"`). `"HIGH"` / `"Norm"` parse-fail, just as they would on
        // the JSON wire.
        match s {
            "low" => Ok(Priority::Low),
            "normal" | "norm" => Ok(Priority::Normal),
            "high" => Ok(Priority::High),
            "urgent" => Ok(Priority::Urgent),
            _ => Err(format!("unknown priority: {s}")),
        }
    }
}

/// Schedule lifecycle state.
///
/// The fire/ack state machine: a due `Active` schedule is flipped to
/// `Triggered` with `next_fire` pre-advanced; once the deliverer injects the
/// message it acks — recurring → back to `Active`, one-time → `Completed`.
/// `Paused` holds firing without losing the schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleStatus {
    #[default]
    Active,
    Paused,
    Completed,
    /// Fired but not yet consumed/acknowledged by the deliverer.
    Triggered,
}

impl std::fmt::Display for ScheduleStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let s = match self {
            ScheduleStatus::Active => "active",
            ScheduleStatus::Paused => "paused",
            ScheduleStatus::Completed => "completed",
            ScheduleStatus::Triggered => "triggered",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for ScheduleStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Strict lowercase, matching the serde canonical form.
        match s {
            "active" => Ok(ScheduleStatus::Active),
            "paused" => Ok(ScheduleStatus::Paused),
            "completed" => Ok(ScheduleStatus::Completed),
            "triggered" => Ok(ScheduleStatus::Triggered),
            _ => Err(format!("unknown status: {s}")),
        }
    }
}

/// Recurrence period for `Every` triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    Minutely,
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

impl std::fmt::Display for Period {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let s = match self {
            Period::Minutely => "minutely",
            Period::Hourly => "hourly",
            Period::Daily => "daily",
            Period::Weekly => "weekly",
            Period::Monthly => "monthly",
            Period::Yearly => "yearly",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for Period {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Strict lowercase, matching the serde canonical form.
        match s {
            "minutely" | "minute" => Ok(Period::Minutely),
            "hourly" | "hour" => Ok(Period::Hourly),
            "daily" | "day" => Ok(Period::Daily),
            "weekly" | "week" => Ok(Period::Weekly),
            "monthly" | "month" => Ok(Period::Monthly),
            "yearly" | "year" => Ok(Period::Yearly),
            _ => Err(format!("unknown period: {s}")),
        }
    }
}

/// A validation or parse error in a schedule trigger.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ScheduleError {
    #[error("invalid at_time: {0}")]
    InvalidAtTime(String),
    #[error("at_time is only valid for daily/monthly/yearly periods, not for {0}")]
    AtTimeUnsupported(&'static str),
    #[error("duration_seconds must be >= 1 (second-precision scheduler)")]
    DurationTooSmall,
    #[error("duration_seconds exceeds the ~10-year v1 ceiling")]
    DurationTooLarge,
}

/// Trigger specification for a schedule.
///
/// v1 ships the simple-trigger subset. Cron expressions (`Cron { expression }`)
/// are a planned v2 addition — the daemon is named `kallip-cron` because full
/// cron-expression support is the natural next step, not because v1 parses it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TriggerSpec {
    /// One-time event at an absolute time. A `Once` trigger in the past fires
    /// on the next tick (fire-ASAP for a missed reminder).
    Once {
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
    /// Relative delay from creation, in whole seconds.
    In { duration_seconds: u64 },
    /// Recurring event. `at_time` ("HH:MM" UTC) is only meaningful — and only
    /// accepted — for `Daily`/`Monthly`/`Yearly`; `Minutely`/`Hourly` fire on a
    /// rolling cadence from each fire time, and `Weekly` repeats same
    /// weekday/time-of-day.
    Every {
        period: Period,
        at_time: Option<String>,
    },
}

impl TriggerSpec {
    /// Whether this trigger recurs (fires more than once).
    pub fn is_recurring(&self) -> bool {
        matches!(self, TriggerSpec::Every { .. })
    }

    /// Validate the trigger against the v1 precision/`at_time` contract.
    /// Called at create time so a malformed trigger is rejected with a 400
    /// rather than surfacing on the first scheduler tick.
    pub fn validate(&self) -> Result<(), ScheduleError> {
        match self {
            TriggerSpec::Once { .. } => Ok(()),
            TriggerSpec::In { duration_seconds } => {
                // >= 1 (second-precision) and <= ~10 years (bounds the u64 -> i64
                // cast in `calculate_initial_next_fire` so an absurd value can't
                // wrap negative and fire immediately).
                const MAX_IN_SECONDS: u64 = 10 * 365 * 24 * 3600;
                if *duration_seconds < 1 {
                    Err(ScheduleError::DurationTooSmall)
                } else if *duration_seconds > MAX_IN_SECONDS {
                    Err(ScheduleError::DurationTooLarge)
                } else {
                    Ok(())
                }
            }
            TriggerSpec::Every { period, at_time } => match period {
                // `at_time` is meaningful only here; if present, validate format.
                Period::Daily | Period::Monthly | Period::Yearly => {
                    if let Some(t) = at_time {
                        parse_at_time(t)?;
                    }
                    Ok(())
                }
                // Minutely/Hourly fire on a rolling cadence (next_fire is
                // `now + period`) and Weekly repeats same weekday/time-of-day;
                // none consume `at_time`. kairos's math silently drops it for
                // Minutely/Hourly, so reject it here to avoid a surprising no-op.
                Period::Minutely | Period::Hourly | Period::Weekly => {
                    if at_time.is_some() {
                        Err(ScheduleError::AtTimeUnsupported(period_label(*period)))
                    } else {
                        Ok(())
                    }
                }
            },
        }
    }
}

/// Lowercase label for an unsupported-`at_time` period, for error messages.
fn period_label(p: Period) -> &'static str {
    match p {
        Period::Minutely => "minutely",
        Period::Hourly => "hourly",
        Period::Weekly => "weekly",
        Period::Daily => "daily",
        Period::Monthly => "monthly",
        Period::Yearly => "yearly",
    }
}

/// Parse an `at_time` of the form `"HH:MM"` (UTC, no seconds) into `(hour,
/// minute)`. Shared by `TriggerSpec::validate` (create-time) and the daemon's
/// `calculate_next_fire` (fire-time) so the two cannot drift on the format.
pub fn parse_at_time(s: &str) -> Result<(u8, u8), ScheduleError> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        return Err(ScheduleError::InvalidAtTime(format!(
            "expected HH:MM, got {s:?}"
        )));
    }
    let hour: u8 = parts[0]
        .parse()
        .map_err(|_| ScheduleError::InvalidAtTime(format!("invalid hour in {s:?}")))?;
    let minute: u8 = parts[1]
        .parse()
        .map_err(|_| ScheduleError::InvalidAtTime(format!("invalid minute in {s:?}")))?;
    if hour > 23 || minute > 59 {
        return Err(ScheduleError::InvalidAtTime(format!(
            "out of range in {s:?}"
        )));
    }
    Ok((hour, minute))
}

/// A scheduled timer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    /// Server-generated UUID v4.
    pub id: ScheduleId,
    /// Human-readable name/description.
    pub name: String,
    /// Trigger specification.
    pub trigger: TriggerSpec,
    /// Target conversation (root agent or any agent id) to inject into on fire.
    pub agent_id: AgentId,
    /// Exact text posted to `POST /agents/{id}/message` on fire. The tagma
    /// prepends a `[From: operator]` header server-side; do not double-tag.
    pub message: String,
    /// Tags for filtering (JSON array at rest).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Schedule priority (metadata-only in v1).
    #[serde(default)]
    pub priority: Priority,
    /// Current lifecycle state.
    #[serde(default)]
    pub status: ScheduleStatus,
    /// Creation timestamp (UTC, second-precision).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Next scheduled fire time. `None` after a one-time trigger fires.
    #[serde(with = "time::serde::rfc3339::option")]
    pub next_fire: Option<OffsetDateTime>,
    /// Last fire time; also the deliverer's trigger timestamp for ordering.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_fire: Option<OffsetDateTime>,
}

/// Request to create a new schedule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateScheduleRequest {
    pub name: String,
    pub trigger: TriggerSpec,
    pub agent_id: AgentId,
    pub message: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub priority: Priority,
}

/// Request to update a schedule. Status-only: `next_fire`/`last_fire` are not
/// mutable through this endpoint, which is what lets the no-rearm invariant
/// (a fired one-timer can never be re-armed by a manual reset) hold.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateScheduleRequest {
    pub status: Option<ScheduleStatus>,
}

/// Response for `GET /schedules`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulesListResponse {
    pub schedules: Vec<Schedule>,
    pub total: usize,
}

/// Response for `GET /status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub healthy: bool,
    pub active_schedules: usize,
    pub pending_triggered: usize,
    #[serde(with = "time::serde::rfc3339::option")]
    pub next_fire: Option<OffsetDateTime>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn priority_round_trips() {
        for p in [
            Priority::Low,
            Priority::Normal,
            Priority::High,
            Priority::Urgent,
        ] {
            let s = p.to_string();
            assert_eq!(s.parse::<Priority>().unwrap(), p);
        }
        // Strict lowercase: wrong case and unknown values reject.
        assert!("HIGH".parse::<Priority>().is_err());
        assert!("Norm".parse::<Priority>().is_err());
        assert!("garbage".parse::<Priority>().is_err());
        // Lowercase aliases accepted.
        assert_eq!("norm".parse::<Priority>().unwrap(), Priority::Normal);
    }

    #[test]
    fn period_round_trips() {
        for p in [
            Period::Minutely,
            Period::Hourly,
            Period::Daily,
            Period::Weekly,
            Period::Monthly,
            Period::Yearly,
        ] {
            let s = p.to_string();
            assert_eq!(s.parse::<Period>().unwrap(), p);
        }
        // Strict lowercase: wrong case and unknown values reject.
        assert!("DAILY".parse::<Period>().is_err());
        assert!("Daily".parse::<Period>().is_err());
        assert!("garbage".parse::<Period>().is_err());
        // Lowercase aliases accepted.
        assert_eq!("day".parse::<Period>().unwrap(), Period::Daily);
    }

    #[test]
    fn status_round_trips() {
        for s in [
            ScheduleStatus::Active,
            ScheduleStatus::Paused,
            ScheduleStatus::Completed,
            ScheduleStatus::Triggered,
        ] {
            let t = s.to_string();
            assert_eq!(t.parse::<ScheduleStatus>().unwrap(), s);
        }
        // Strict lowercase: wrong case and unknown values reject (matches the
        // serde wire form, which is case-sensitive lowercase).
        assert!("ACTIVE".parse::<ScheduleStatus>().is_err());
        assert!("Active".parse::<ScheduleStatus>().is_err());
        assert!("garbage".parse::<ScheduleStatus>().is_err());
    }

    #[test]
    fn parse_at_time_valid() {
        assert_eq!(parse_at_time("09:00").unwrap(), (9, 0));
        assert_eq!(parse_at_time("23:59").unwrap(), (23, 59));
        assert_eq!(parse_at_time("00:00").unwrap(), (0, 0));
    }

    #[test]
    fn parse_at_time_rejects_bad() {
        // Single-digit hour is accepted ("9:00" -> 9,0); only format/range fails.
        assert!(parse_at_time("24:00").is_err());
        assert!(parse_at_time("09:60").is_err());
        assert!(parse_at_time("0900").is_err());
        assert!(parse_at_time("09:00:00").is_err());
        assert!(parse_at_time("").is_err());
    }

    #[test]
    fn validate_rejects_at_time_for_minutely_hourly_weekly() {
        assert!(
            TriggerSpec::Every {
                period: Period::Minutely,
                at_time: Some("09:00".into()),
            }
            .validate()
            .is_err()
        );
        assert!(
            TriggerSpec::Every {
                period: Period::Hourly,
                at_time: Some("09:00".into()),
            }
            .validate()
            .is_err()
        );
        assert!(
            TriggerSpec::Every {
                period: Period::Weekly,
                at_time: Some("09:00".into()),
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn validate_accepts_at_time_for_daily_monthly_yearly() {
        for p in [Period::Daily, Period::Monthly, Period::Yearly] {
            assert!(
                TriggerSpec::Every {
                    period: p,
                    at_time: Some("09:00".into()),
                }
                .validate()
                .is_ok()
            );
        }
    }

    #[test]
    fn validate_accepts_no_at_time_for_any_period() {
        for p in [
            Period::Minutely,
            Period::Hourly,
            Period::Daily,
            Period::Weekly,
            Period::Monthly,
            Period::Yearly,
        ] {
            assert!(
                TriggerSpec::Every {
                    period: p,
                    at_time: None,
                }
                .validate()
                .is_ok()
            );
        }
    }

    #[test]
    fn validate_rejects_subsecond_and_zero_duration() {
        assert!(
            TriggerSpec::In {
                duration_seconds: 0
            }
            .validate()
            .is_err()
        );
        assert!(
            TriggerSpec::In {
                duration_seconds: 1
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn once_in_the_past_validates() {
        // Past `Once` is valid (fire-ASAP); the scheduler decides to fire it.
        let past = datetime!(2020-01-01 00:00 UTC);
        assert!(TriggerSpec::Once { at: past }.validate().is_ok());
    }
}
