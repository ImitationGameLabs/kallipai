//! Wire types for `kallip-cron` — the timer/notification daemon.
//!
//! Pure data types shared by the daemon (`kallip-cron-daemon`), the HTTP client
//! (`kallip-cron-client`), and the management CLI (`kallip-cron`). The daemon
//! is a host-side scheduler that fires timed events and, on fire, injects a
//! message into an agent conversation via the tagma HTTP API. No behavior lives
//! here — only the `Schedule` model, trigger specs, and request/response DTOs —
//! so the future platform-side time-record service can reuse these without
//! pulling daemon internals.

pub mod schedule;

pub use schedule::{
    CreateScheduleRequest, Period, Priority, Schedule, ScheduleError, ScheduleId, ScheduleStatus,
    SchedulesListResponse, StatusResponse, TriggerSpec, UpdateScheduleRequest, parse_at_time,
};
