//! Liveness reconciliation: a slow background sweep that announces the moment
//! a running instance dies.
//!
//! The wire answers (`list`/`health`) already re-scan the record area on
//! every call, so the panel's view is always fresh; what the record area
//! cannot do is *speak* — nothing notices a Running→Dead transition
//! unless someone happens to poll. This loop is that watcher: it keeps
//! its own previous-tick snapshot (the daemon proper stays stateless —
//! the record area remains the only truth), and on
//! one scan cycle's Running→Dead edge it emits a warn line carrying slug and
//! pid. Restarting the daemon resets the snapshot: the first tick after boot
//! observes without alerting, so pre-existing corpses don't fire.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use kallipai_daemon_common::wire::InstanceState;

use crate::scan;

/// How often the reconcile loop re-scans the tree. The panel polls `list`
/// every few seconds anyway; this cadence bounds only how long an unwatched
/// death goes unannounced in the daemon log.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

/// Run the reconciliation sweep until the process exits: scan, diff against
/// the previous tick, log the dead edges, sleep, repeat. The snapshot lives
/// in this task alone; nothing else in the daemon reads or writes it.
pub async fn run(record_root: PathBuf) {
    let mut seen: HashMap<String, InstanceState> = HashMap::new();
    loop {
        // Derive states here (the live /proc checks) so `edge_reports` stays a
        // pure diff over (slug, state, pid) tuples -- unit-testable without
        // depending on the host's pid space.
        let now: Vec<(String, InstanceState, Option<u32>)> = scan::scan_instances(&record_root)
            .into_iter()
            .map(|i| (i.slug.clone(), i.state(), i.runtime_pid()))
            .collect();
        for (slug, pid) in edge_reports(&mut seen, now) {
            // The log tree belongs to the instance's state root, which
            // the record carries: re-read the record on this (rare) edge
            // rather than assemble a path from a home directory. The
            // scan tuples stay (slug, state, pid) so the diff keeps its
            // pure shape. A record that vanished between ticks, or one
            // the boot migration could not fill, renders the failure
            // explicitly — never a plausible-looking wrong path.
            let log_field = match crate::records::read_record(&record_root, &slug) {
                Some(record) => match record.logs_dir(&slug) {
                    Ok(dir) => dir.display().to_string(),
                    Err(error) => format!("<unresolved: {error}>"),
                },
                None => "<unresolved: record gone>".to_string(),
            };
            tracing::warn!(
                slug = %slug,
                pid = ?pid,
                log = %log_field,
                "instance died: the incarnation it named is no longer running"
            );
        }
        tokio::time::sleep(RECONCILE_INTERVAL).await;
    }
}

/// Diff the current tick's `(slug, state, pid)` tuples against `seen`,
/// returning a `(slug, pid)` pair for each instance whose recorded pid
/// just died; `seen` is left holding this tick's states for the next
/// call. Slugs whose directory vanished between ticks are forgotten, so
/// a re-created directory starts fresh instead of inheriting a ghost
/// history.
fn edge_reports(
    seen: &mut HashMap<String, InstanceState>,
    now: Vec<(String, InstanceState, Option<u32>)>,
) -> Vec<(String, Option<u32>)> {
    let mut reports = Vec::new();
    let mut live_slugs = HashSet::new();
    for (slug, state, pid) in now {
        live_slugs.insert(slug.clone());
        let was = seen.insert(slug.clone(), state);
        if matches!(was, Some(InstanceState::Running)) && state == InstanceState::Dead {
            reports.push((slug, pid));
        }
    }
    seen.retain(|slug, _| live_slugs.contains(slug));
    reports
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallipai_daemon_common::wire::InstanceState::*;

    fn recorded(state_dir: Option<PathBuf>) -> crate::records::InstanceRecord {
        crate::records::InstanceRecord {
            instance_id: "instance-1".into(),
            owner_uid: 1000,
            target_uid: 1000,
            target_username: None,
            workspace: Some("/ws".into()),
            env: Vec::new(),
            identity: None,
            data_dir: PathBuf::from("/srv/data/kallipai/tagmata/e2e"),
            config_dir: Some(PathBuf::from("/srv/config/kallipai/tagmata/e2e")),
            state_dir,
        }
    }

    #[test]
    fn the_log_tree_is_the_recorded_state_root_plus_the_leaf() {
        let dir = recorded(Some(PathBuf::from("/srv/state/kallipai")))
            .logs_dir("e2e")
            .expect("the record names its state root");
        assert_eq!(dir, PathBuf::from("/srv/state/kallipai/tagmata/e2e/logs"));
    }

    #[test]
    fn a_record_without_a_state_root_is_an_error_not_a_guess() {
        let error = recorded(None).logs_dir("e2e").unwrap_err();
        assert_eq!(error.slug, "e2e");
    }

    /// One observed instance at one tick.
    fn tick(
        slug: &str,
        state: InstanceState,
        pid: Option<u32>,
    ) -> (String, InstanceState, Option<u32>) {
        (slug.to_string(), state, pid)
    }

    /// A death between ticks reports exactly once; further Dead ticks stay
    /// silent (one corpse, one line).
    #[test]
    fn running_to_dead_reports_once() {
        let mut seen = HashMap::new();
        let up = vec![tick("a", Running, Some(7))];
        assert!(edge_reports(&mut seen, up).is_empty());
        let down = vec![tick("a", Dead, Some(7))];
        let reports = edge_reports(&mut seen, down);
        assert_eq!(reports, vec![("a".to_string(), Some(7))]);
        let again = vec![tick("a", Dead, Some(7))];
        assert!(edge_reports(&mut seen, again).is_empty());
    }

    /// A corpse already present at the first tick after boot is observed,
    /// not announced (no previous Running to die).
    #[test]
    fn first_tick_dead_is_silent() {
        let mut seen = HashMap::new();
        let corpse = vec![tick("a", Dead, Some(7))];
        assert!(edge_reports(&mut seen, corpse).is_empty());
    }

    /// Stopped never alerts, even across re-scans.
    #[test]
    fn stopped_never_alerts() {
        let mut seen = HashMap::new();
        for _ in 0..3 {
            let halt = vec![tick("a", Stopped, None)];
            assert!(edge_reports(&mut seen, halt).is_empty());
        }
    }

    /// A directory removed and re-added between ticks forgets its history:
    /// the re-created instance's first observed state never alerts.
    #[test]
    fn removed_slug_is_forgotten() {
        let mut seen = HashMap::new();
        let up = vec![tick("a", Running, Some(7))];
        let _ = edge_reports(&mut seen, up);
        let down = vec![tick("a", Dead, Some(7))];
        let _ = edge_reports(&mut seen, down);
        // Dir deleted: absent from this tick's list.
        let _ = edge_reports(&mut seen, vec![]);
        // Re-created already Dead (a corpse re-encountered): first sight -- silent.
        let reborn = vec![tick("a", Dead, Some(7))];
        assert!(edge_reports(&mut seen, reborn).is_empty());
    }

    /// Two independent instances die in the same tick: both report.
    #[test]
    fn simultaneous_deaths_all_report() {
        let mut seen = HashMap::new();
        let up = vec![tick("a", Running, Some(1)), tick("b", Running, Some(2))];
        let _ = edge_reports(&mut seen, up);
        let down = vec![tick("a", Dead, Some(1)), tick("b", Dead, Some(2))];
        let mut reports = edge_reports(&mut seen, down);
        reports.sort();
        assert_eq!(
            reports,
            vec![("a".to_string(), Some(1)), ("b".to_string(), Some(2))]
        );
    }
}
