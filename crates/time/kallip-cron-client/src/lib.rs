//! HTTP client for the `kallip-cron` timer daemon.
//!
//! Used by the management CLI (`kallip-cron`) and, in the future, by the
//! platform-side time-record service to submit schedules. Mirrors the
//! `kallip-client` (`TagmaClient`) shape: a clonable handle around an `Arc`'d
//! inner, a builder, an env-based constructor, and one method per management
//! endpoint. Errors preserve the HTTP status so callers (notably the daemon's
//! own deliverer, when it talks to tagma, and the CLI for diagnostics) can
//! branch on it.

mod client;

pub use client::{CronClient, CronClientBuilder, CronClientError};
pub use kallip_cron_common::{
    CreateScheduleRequest, Period, Priority, Schedule, ScheduleError, ScheduleId, ScheduleStatus,
    SchedulesListResponse, StatusResponse, TriggerSpec, UpdateScheduleRequest,
};
