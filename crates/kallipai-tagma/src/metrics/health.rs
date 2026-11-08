//! `/health`: one answer to "is this tagma able to serve, and is anything
//! degraded", from in-memory facts only.
//!
//! The endpoint is deliberately not a second metrics face: booleans, one
//! three-valued relay state, and the names of the checks that are late. No
//! counters, no durations -- a monitor scraping for counters wants `/metrics`,
//! and a load balancer wants this.
//!
//! **Readiness vs degradation.** Readiness is what the process cannot serve
//! without: the chat-history store and a usable profile bundle. Missing either
//! is `not_ready` (503), because a tagma that cannot resolve a profile cannot
//! answer a single agent turn. Degradation is "serving, but something needs
//! attention": a configured relay that is down, a periodic loop that stopped
//! ticking. It answers 200 -- pulling this instance out of service would not
//! fix a dead relay, and the endpoint exists to make the fault visible.
//!
//! Every check reads memory: the store handle, the swapped profile bundle, two
//! atomics, the heartbeat table. No network call, no SQL, no wait -- the handler
//! is meant to stay under a millisecond.
//!
//! That is the limit of the `store` check as well: it asks whether the handle
//! is there and open, never whether the file behind it still answers. A store
//! whose disk went away keeps reading ready here, and the failure surfaces on
//! the first query that needs it. A probe that proved the file would be a
//! second query on a health path, which is the wait this endpoint exists to
//! avoid.
//!
//! Background loops report two separate facts: how long ago they last beat, and
//! whether that pass failed. Late and failing both degrade, but they name
//! different faults, so the report carries them apart.

use axum::http::StatusCode;
use serde::Serialize;

use super::{Beat, metrics};
use crate::state::SharedState;

/// The three states `/health` reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// Everything checked is healthy.
    Ok,
    /// Serving, with something that needs attention.
    Degraded,
    /// Cannot serve: a readiness check failed.
    NotReady,
}

impl HealthStatus {
    /// `not_ready` is the only state that answers 5xx: a degraded tagma is
    /// still serving, and taking it out of rotation would not fix the fault.
    pub fn status_code(self) -> StatusCode {
        match self {
            HealthStatus::Ok | HealthStatus::Degraded => StatusCode::OK,
            HealthStatus::NotReady => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

/// The relay's state, as the health face reports it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayState {
    /// At least one configured relay's tunnel is open.
    Connected,
    /// No relay is configured: a local-only tagma, which is a complete
    /// deployment, not a fault.
    LocalOnly,
    /// A relay is configured and none is connected.
    Down,
}

#[derive(Debug, Serialize)]
pub struct HealthReport {
    status: HealthStatus,
    checks: Checks,
}

#[derive(Debug, Serialize)]
struct Checks {
    store: Presencer,
    profiles: Presencer,
    relay: RelayCheck,
    engine: Presencer,
    background: BackgroundCheck,
}

#[derive(Debug, Serialize)]
struct Presencer {
    ok: bool,
}

#[derive(Debug, Serialize)]
struct RelayCheck {
    state: RelayState,
}

#[derive(Debug, Serialize)]
struct BackgroundCheck {
    ok: bool,
    /// The loops that are late, by their `/metrics` label value; empty when
    /// none is.
    stale: Vec<&'static str>,
    /// The loops whose last pass failed; empty when none did. A loop here is on
    /// time and not working, which freshness alone cannot show.
    failed: Vec<&'static str>,
}

impl HealthReport {
    pub fn status(&self) -> HealthStatus {
        self.status
    }
}

/// Judge every check and assemble the report.
pub fn evaluate(state: &SharedState) -> HealthReport {
    let metrics = metrics();
    let store_ok = state.external.get().is_some_and(|p| p.history_ready());
    let profiles_ok = {
        let bundle = state.profiles.load();
        !bundle.registry.sets().is_empty() && !bundle.config.default.is_empty()
    };
    let relay = relay_state(metrics.relays_configured(), metrics.relays_connected());
    let engine_ok = metrics.beat_fresh(Beat::Engine) && !metrics.beat_failed(Beat::Engine);
    let background: Vec<Beat> = Beat::ALL
        .iter()
        .copied()
        .filter(|beat| *beat != Beat::Engine)
        .collect();
    let stale: Vec<&'static str> = background
        .iter()
        .filter(|beat| !metrics.beat_fresh(**beat))
        .map(|beat| beat.as_str())
        .collect();
    let failed: Vec<&'static str> = background
        .iter()
        .filter(|beat| metrics.beat_failed(**beat))
        .map(|beat| beat.as_str())
        .collect();

    let status = judge(store_ok, profiles_ok, relay, engine_ok, &stale, &failed);

    HealthReport {
        status,
        checks: Checks {
            store: Presencer { ok: store_ok },
            profiles: Presencer { ok: profiles_ok },
            relay: RelayCheck { state: relay },
            engine: Presencer { ok: engine_ok },
            background: BackgroundCheck {
                ok: stale.is_empty() && failed.is_empty(),
                stale,
                failed,
            },
        },
    }
}

fn relay_state(configured: i64, connected: i64) -> RelayState {
    if configured == 0 {
        RelayState::LocalOnly
    } else if connected > 0 {
        RelayState::Connected
    } else {
        RelayState::Down
    }
}

/// The judgement itself, split from the fact-gathering so the matrix can be
/// tested without a live state or a live clock.
fn judge(
    store_ok: bool,
    profiles_ok: bool,
    relay: RelayState,
    engine_ok: bool,
    stale: &[&'static str],
    failed: &[&'static str],
) -> HealthStatus {
    if !store_ok || !profiles_ok {
        HealthStatus::NotReady
    } else if relay == RelayState::Down || !engine_ok || !stale.is_empty() || !failed.is_empty() {
        HealthStatus::Degraded
    } else {
        HealthStatus::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_outranks_degradation() {
        assert_eq!(
            judge(false, true, RelayState::Down, false, &["history_gc"], &[]),
            HealthStatus::NotReady
        );
        assert_eq!(
            judge(true, false, RelayState::LocalOnly, true, &[], &[]),
            HealthStatus::NotReady
        );
    }

    #[test]
    fn each_degradation_cause_degrades_on_its_own() {
        assert_eq!(
            judge(true, true, RelayState::Down, true, &[], &[]),
            HealthStatus::Degraded
        );
        assert_eq!(
            judge(true, true, RelayState::Connected, false, &[], &[]),
            HealthStatus::Degraded
        );
        assert_eq!(
            judge(
                true,
                true,
                RelayState::Connected,
                true,
                &["history_gc"],
                &[]
            ),
            HealthStatus::Degraded
        );
        // On time and not working is a degradation of its own: freshness
        // cannot tell a loop that is busy from one whose pass keeps failing.
        assert_eq!(
            judge(
                true,
                true,
                RelayState::LocalOnly,
                true,
                &[],
                &["history_gc"]
            ),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn a_local_only_tagma_with_live_loops_is_healthy() {
        assert_eq!(
            judge(true, true, RelayState::LocalOnly, true, &[], &[]),
            HealthStatus::Ok
        );
    }

    #[test]
    fn only_not_ready_answers_a_5xx() {
        assert_eq!(HealthStatus::Ok.status_code(), StatusCode::OK);
        assert_eq!(HealthStatus::Degraded.status_code(), StatusCode::OK);
        assert_eq!(
            HealthStatus::NotReady.status_code(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn a_configured_relay_with_no_open_tunnel_is_down() {
        assert_eq!(relay_state(0, 0), RelayState::LocalOnly);
        assert_eq!(relay_state(1, 0), RelayState::Down);
        assert_eq!(relay_state(1, 1), RelayState::Connected);
    }

    #[test]
    fn the_report_serialises_the_documented_shape() {
        // A monitor scraping for numbers wants `/metrics`; this face is
        // booleans and one three-valued state, so its JSON stays small enough
        // to read in a log line.
        let report = HealthReport {
            status: HealthStatus::Degraded,
            checks: Checks {
                store: Presencer { ok: true },
                profiles: Presencer { ok: true },
                relay: RelayCheck {
                    state: RelayState::Down,
                },
                engine: Presencer { ok: true },
                background: BackgroundCheck {
                    ok: false,
                    stale: vec!["history_gc"],
                    failed: Vec::new(),
                },
            },
        };
        let rendered = serde_json::to_string(&report).expect("the report serialises");
        assert_eq!(
            rendered,
            "{\"status\":\"degraded\",\"checks\":{\"store\":{\"ok\":true},\"profiles\":{\"ok\":true},\"relay\":{\"state\":\"down\"},\"engine\":{\"ok\":true},\"background\":{\"ok\":false,\"stale\":[\"history_gc\"],\"failed\":[]}}}"
        );
        let ready = serde_json::to_string(&HealthStatus::Ok).expect("the status serialises");
        assert_eq!(ready, "\"ok\"", "a ready report says ok");
    }

    #[test]
    fn a_state_without_the_history_store_is_not_ready() {
        let state = crate::test_helpers::make_state();
        assert_eq!(evaluate(&state).status(), HealthStatus::NotReady);
    }
    /// The three links that make one failure visible: a pass records it, the
    /// beat table remembers it, and the report a monitor reads names it. This
    /// uses the process-wide table and puts the beat back before it returns.
    #[test]
    fn a_failed_pass_reaches_the_health_report() {
        let state = crate::test_helpers::make_state();
        let metrics = metrics();
        metrics.beat(Beat::HistoryGc, crate::metrics::PassOutcome::Failed);
        let report = evaluate(&state);
        assert!(
            report.checks.background.failed.contains(&"history_gc"),
            "the failed pass is named: {:?}",
            report.checks.background.failed
        );
        assert!(!report.checks.background.ok);

        metrics.beat(Beat::HistoryGc, crate::metrics::PassOutcome::Completed);
        assert!(
            !evaluate(&state)
                .checks
                .background
                .failed
                .contains(&"history_gc")
        );
    }
}
