//! Schedule model + trigger specs for `kallip-cron`.
//!
//! A fired timer is injected into a conversation via
//! `POST /agents/{id}/message`; this module carries the concrete `agent_id` +
//! `message` delivery target and the precision/UTC contract used throughout the
//! daemon.

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

/// Minimum recurrence interval (3 minutes). Guards against event-flood timers
/// that would overwhelm an agent's processing loop with no practical benefit
/// for an LLM consumer.
pub const MIN_RECURRENCE_SECONDS: u64 = 3 * 60;

/// Upper bound on any duration field (~10 years), shared by `In` and `Every`.
/// Bounds the `u64 -> i64` cast in the fire-time math so an absurd value can't
/// wrap negative and fire immediately.
const MAX_DURATION_SECONDS: u64 = 10 * 365 * 24 * 3600;

/// A validation or parse error in a schedule trigger.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ScheduleError {
    #[error("duration_seconds must be >= 1 (second-precision scheduler)")]
    DurationTooSmall,
    #[error("recurrence interval must be >= 3 minutes (event-flood guard)")]
    RecurrenceTooSmall,
    #[error("duration_seconds exceeds the ~10-year v1 ceiling")]
    DurationTooLarge,
}

/// Trigger specification for a schedule.
///
/// Recurring timers (`Every`) are pure rolling intervals — `next_fire` is
/// advanced by `duration_seconds` from each fire time. Calendar-anchored
/// recurrence ("daily at 09:00") is intentionally not modeled; a one-shot
/// `Once` covers an absolute fire time, and a recurring cadence is expressed as
/// a plain interval. Cron expressions are a planned v2 addition — the daemon is
/// named `kallip-cron` because full cron-expression support is the natural next
/// step, not because v1 parses it.
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
    /// Recurring interval, in whole seconds. Must be >=
    /// [`MIN_RECURRENCE_SECONDS`] (the event-flood guard).
    Every { duration_seconds: u64 },
}

impl TriggerSpec {
    /// Whether this trigger recurs (fires more than once).
    pub fn is_recurring(&self) -> bool {
        matches!(self, TriggerSpec::Every { .. })
    }

    /// Validate the trigger against the precision/duration contract. Called at
    /// create time so a malformed trigger is rejected with a 400 rather than
    /// surfacing on the first scheduler tick.
    pub fn validate(&self) -> Result<(), ScheduleError> {
        match self {
            TriggerSpec::Once { .. } => Ok(()),
            TriggerSpec::In { duration_seconds } => {
                check_duration(*duration_seconds, 1, ScheduleError::DurationTooSmall)
            }
            TriggerSpec::Every { duration_seconds } => check_duration(
                *duration_seconds,
                MIN_RECURRENCE_SECONDS,
                ScheduleError::RecurrenceTooSmall,
            ),
        }
    }
}

/// Range-check a duration field: `secs` must be in `[min, MAX_DURATION_SECONDS]`.
/// `too_small` is the variant returned below the floor — `In` and `Every` have
/// different floors (1 s vs 3 min) but share the ceiling.
fn check_duration(secs: u64, min: u64, too_small: ScheduleError) -> Result<(), ScheduleError> {
    if secs < min {
        Err(too_small)
    } else if secs > MAX_DURATION_SECONDS {
        Err(ScheduleError::DurationTooLarge)
    } else {
        Ok(())
    }
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
    fn validate_rejects_subsecond_and_zero_duration() {
        // `In` floor is 1 second.
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
    fn validate_enforces_recurrence_floor() {
        // Below the 3-minute floor is rejected; exactly at it is accepted.
        assert!(matches!(
            TriggerSpec::Every {
                duration_seconds: MIN_RECURRENCE_SECONDS - 1
            }
            .validate(),
            Err(ScheduleError::RecurrenceTooSmall)
        ));
        assert!(
            TriggerSpec::Every {
                duration_seconds: MIN_RECURRENCE_SECONDS
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn validate_rejects_every_above_ceiling() {
        assert!(matches!(
            TriggerSpec::Every {
                duration_seconds: u64::MAX
            }
            .validate(),
            Err(ScheduleError::DurationTooLarge)
        ));
    }

    #[test]
    fn validate_accepts_duration_at_ceiling() {
        // The ceiling is inclusive (`secs > MAX` rejects): exactly MAX is the
        // largest accepted value for both `In` and `Every`. Guards against a
        // `>=` regression in `check_duration`.
        assert!(
            TriggerSpec::In {
                duration_seconds: MAX_DURATION_SECONDS
            }
            .validate()
            .is_ok()
        );
        assert!(
            TriggerSpec::Every {
                duration_seconds: MAX_DURATION_SECONDS
            }
            .validate()
            .is_ok()
        );
        assert!(matches!(
            TriggerSpec::In {
                duration_seconds: MAX_DURATION_SECONDS + 1
            }
            .validate(),
            Err(ScheduleError::DurationTooLarge)
        ));
    }

    #[test]
    fn once_in_the_past_validates() {
        // Past `Once` is valid (fire-ASAP); the scheduler decides to fire it.
        let past = datetime!(2020-01-01 00:00 UTC);
        assert!(TriggerSpec::Once { at: past }.validate().is_ok());
    }
}
