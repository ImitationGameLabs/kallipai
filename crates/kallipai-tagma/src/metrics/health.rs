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
//! ticking, an installed exporter whose last attempt failed and has not
//! succeeded since. It answers 200 -- pulling this instance out of service
//! would not fix a dead relay, and the endpoint exists to make the fault
//! visible.
//!
//! Every check reads memory: the store handle, the swapped profile bundle, the
//! relay counters, the heartbeat table, and the export counters' snapshot. No
//! network call, no SQL, no wait -- the handler is meant to stay under a
//! millisecond.
//!
//! That is the limit of the `history_store` check as well: it asks whether the
//! handle is there and open, never whether the file behind it still answers. A
//! store whose disk went away keeps reading ready here, and the failure
//! surfaces on the first query that needs it. A probe that proved the file
//! would be a second query on a health path, which is the wait this endpoint
//! exists to avoid.
//!
//! Background loops report two separate facts: how long ago they last beat, and
//! whether that pass failed. Late and failing both degrade, but they name
//! different faults, so the report carries them apart.

use axum::http::StatusCode;
use serde::Serialize;

use super::{Beat, metrics};
use crate::state::SharedState;
use kallipai_common::telemetry::ExportSnapshot;

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

impl RelayState {
    /// The verdict this state stands for. Only `down` is a fault: a local-only
    /// tagma is a complete deployment. The match names both healthy states
    /// rather than negating the fault, so a state added later does not compile
    /// until it is judged here.
    fn is_ok(self) -> bool {
        match self {
            RelayState::Connected | RelayState::LocalOnly => true,
            RelayState::Down => false,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct HealthReport {
    status: HealthStatus,
    checks: Checks,
}

#[derive(Debug, Serialize)]
struct Checks {
    history_store: Check,
    profile_bundle: Check,
    relay: RelayCheck,
    work_schedule_engine: Check,
    background_loops: BackgroundCheck,
    telemetry_export: Check,
}

/// A check with nothing to add beyond its verdict. Every entry under `checks`
/// is an object carrying `ok`, so a monitor loops over them alike; a check with
/// detail puts it beside `ok`.
#[derive(Debug, Serialize)]
struct Check {
    ok: bool,
}

#[derive(Debug, Serialize)]
struct RelayCheck {
    /// Derived from `state` in one place, so the two cannot disagree.
    ok: bool,
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

    let export = kallipai_common::telemetry::export_stats().snapshot();
    let status = judge(
        store_ok,
        profiles_ok,
        relay,
        engine_ok,
        &export,
        &stale,
        &failed,
    );

    HealthReport {
        status,
        checks: Checks {
            history_store: Check { ok: store_ok },
            profile_bundle: Check { ok: profiles_ok },
            relay: RelayCheck {
                ok: relay.is_ok(),
                state: relay,
            },
            work_schedule_engine: Check { ok: engine_ok },
            background_loops: BackgroundCheck {
                ok: stale.is_empty() && failed.is_empty(),
                stale,
                failed,
            },
            telemetry_export: Check {
                ok: export_ok(&export),
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
    export: &ExportSnapshot,
    stale: &[&'static str],
    failed: &[&'static str],
) -> HealthStatus {
    if !store_ok || !profiles_ok {
        HealthStatus::NotReady
    } else if !relay.is_ok()
        || !engine_ok
        || !export_ok(export)
        || !stale.is_empty()
        || !failed.is_empty()
    {
        HealthStatus::Degraded
    } else {
        HealthStatus::Ok
    }
}

/// Whether the export pipeline is reaching its collector.
///
/// An uninstalled layer is not a fault: the spans stay in this process by
/// choice. For an installed one the test is `last_failure_unix >
/// last_success_unix` -- a failure nothing has recovered from -- and not the
/// age of the last success, because a process with nothing to export exports
/// nothing and would otherwise read as broken on a quiet morning.
fn export_ok(export: &ExportSnapshot) -> bool {
    !export.enabled || export.last_failure_unix <= export.last_success_unix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_outranks_degradation() {
        assert_eq!(
            judge(
                false,
                true,
                RelayState::Down,
                false,
                &ExportSnapshot::default(),
                &["history_gc"],
                &[]
            ),
            HealthStatus::NotReady
        );
        assert_eq!(
            judge(
                true,
                false,
                RelayState::LocalOnly,
                true,
                &ExportSnapshot::default(),
                &[],
                &[]
            ),
            HealthStatus::NotReady
        );
    }

    #[test]
    fn each_degradation_cause_degrades_on_its_own() {
        assert_eq!(
            judge(
                true,
                true,
                RelayState::Down,
                true,
                &ExportSnapshot::default(),
                &[],
                &[]
            ),
            HealthStatus::Degraded
        );
        assert_eq!(
            judge(
                true,
                true,
                RelayState::Connected,
                false,
                &ExportSnapshot::default(),
                &[],
                &[]
            ),
            HealthStatus::Degraded
        );
        assert_eq!(
            judge(
                true,
                true,
                RelayState::Connected,
                true,
                &ExportSnapshot::default(),
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
                &ExportSnapshot::default(),
                &[],
                &["history_gc"]
            ),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn a_local_only_tagma_with_live_loops_is_healthy() {
        assert_eq!(
            judge(
                true,
                true,
                RelayState::LocalOnly,
                true,
                &ExportSnapshot::default(),
                &[],
                &[]
            ),
            HealthStatus::Ok
        );
    }

    /// The export check reads two stamps and no clock: a failure nothing has
    /// recovered from degrades the report, while a quiet process whose last
    /// success is old -- it has nothing to export -- stays ok.
    #[test]
    fn a_failed_export_nothing_recovered_from_degrades_the_report() {
        assert!(
            export_ok(&ExportSnapshot::default()),
            "an uninstalled layer keeps spans in this process by choice"
        );

        let quiet = ExportSnapshot {
            enabled: true,
            exported_spans: 4,
            last_success_unix: 100,
            ..Default::default()
        };
        assert!(export_ok(&quiet), "an old success is not a failure");

        let recovered = ExportSnapshot {
            enabled: true,
            failed_timeout: 1,
            last_failure_unix: 100,
            last_success_unix: 200,
            ..Default::default()
        };
        assert!(export_ok(&recovered), "a later success is the recovery");

        let down = ExportSnapshot {
            enabled: true,
            failed_unavailable: 1,
            last_failure_unix: 200,
            last_success_unix: 100,
            ..Default::default()
        };
        assert!(
            !export_ok(&down),
            "a failure newer than the success is a fault"
        );
        assert_eq!(
            judge(true, true, RelayState::LocalOnly, true, &down, &[], &[]),
            HealthStatus::Degraded
        );

        let never = ExportSnapshot {
            enabled: true,
            failed_unavailable: 3,
            last_failure_unix: 200,
            ..Default::default()
        };
        assert!(
            !export_ok(&never),
            "an exporter that never landed a batch is not serving"
        );
        // The shape a collector that stops answering leaves: the tally books
        // the wrapper's deadline as a timeout, and the judgement reads stamps.
        let hung = ExportSnapshot {
            enabled: true,
            failed_timeout: 1,
            last_failure_unix: 200,
            last_success_unix: 100,
            ..Default::default()
        };
        assert!(!export_ok(&hung), "a hung collector reads as a fault");
        assert_eq!(
            judge(true, true, RelayState::LocalOnly, true, &hung, &[], &[]),
            HealthStatus::Degraded
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
                history_store: Check { ok: true },
                profile_bundle: Check { ok: true },
                relay: RelayCheck {
                    ok: false,
                    state: RelayState::Down,
                },
                work_schedule_engine: Check { ok: true },
                background_loops: BackgroundCheck {
                    ok: false,
                    stale: vec!["history_gc"],
                    failed: Vec::new(),
                },
                telemetry_export: Check { ok: false },
            },
        };
        let rendered = serde_json::to_string(&report).expect("the report serialises");
        assert_eq!(
            rendered,
            "{\"status\":\"degraded\",\"checks\":{\"history_store\":{\"ok\":true},\"profile_bundle\":{\"ok\":true},\"relay\":{\"ok\":false,\"state\":\"down\"},\"work_schedule_engine\":{\"ok\":true},\"background_loops\":{\"ok\":false,\"stale\":[\"history_gc\"],\"failed\":[]},\"telemetry_export\":{\"ok\":false}}}"
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
            report
                .checks
                .background_loops
                .failed
                .contains(&"history_gc"),
            "the failed pass is named: {:?}",
            report.checks.background_loops.failed
        );
        assert!(!report.checks.background_loops.ok);

        metrics.beat(Beat::HistoryGc, crate::metrics::PassOutcome::Completed);
        assert!(
            !evaluate(&state)
                .checks
                .background_loops
                .failed
                .contains(&"history_gc")
        );
    }
    /// The relay's verdict is derived, not picked: a tagma with no relay
    /// must read `local_only` and ok, a relay with no tunnel open must read
    /// `down` and not ok, and an open tunnel must read `connected` and ok.
    /// Both gauges are process-wide, so this snapshots them, drains the
    /// tunnel count to zero, and puts both back in the state it found them.
    #[test]
    fn the_relay_check_follows_its_state() {
        let state = crate::test_helpers::make_state();
        let metrics = metrics();
        let saved_configured = metrics.relays_configured();
        let saved_connected = metrics.relays_connected();
        for _ in 0..saved_connected {
            metrics.relay_down();
        }

        metrics.set_relays_configured(0);
        let local = evaluate(&state);
        metrics.set_relays_configured(1);
        let down = evaluate(&state);
        metrics.relay_up();
        let connected = evaluate(&state);
        metrics.relay_down();

        metrics.set_relays_configured(saved_configured);
        for _ in 0..saved_connected {
            metrics.relay_up();
        }

        assert_eq!(local.checks.relay.state, RelayState::LocalOnly);
        assert!(local.checks.relay.ok, "a local-only tagma is ok");
        assert_eq!(down.checks.relay.state, RelayState::Down);
        assert!(!down.checks.relay.ok, "a down relay is not ok");
        assert_eq!(connected.checks.relay.state, RelayState::Connected);
        assert!(connected.checks.relay.ok, "a connected relay is ok");
    }
}
