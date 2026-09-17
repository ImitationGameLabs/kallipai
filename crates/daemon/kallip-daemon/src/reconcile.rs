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
use std::path::{Path, PathBuf};
use std::time::Duration;

use kallip_daemon_common::wire::InstanceState;

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
            .map(|i| (i.slug.clone(), i.state(), i.pid))
            .collect();
        for (slug, pid) in edge_reports(&mut seen, now) {
            // The log tree lives in the target user's state home, not
            // the daemon's: re-read the record on this (rare) edge to
            // name the right tree; the scan tuples stay (slug, state,
            // pid) so the diff keeps its pure shape. A record that
            // vanished between ticks, or a uid the passwd database
            // does not know, renders the failure explicitly — never
            // a plausible-looking wrong path.
            let log_field = match crate::records::read_record(&record_root, &slug) {
                Some(record) => match instance_logs_dir(record.target_uid, &slug, passwd_home) {
                    Ok(dir) => dir.display().to_string(),
                    Err(error) => format!("<unresolved: {error}>"),
                },
                None => "<unresolved: record gone>".to_string(),
            };
            tracing::warn!(
                slug = %slug,
                pid = ?pid,
                log = %log_field,
                "instance died: recorded pid is no longer a live kallip-tagma"
            );
        }
        tokio::time::sleep(RECONCILE_INTERVAL).await;
    }
}

/// Where an instance's log files live: the TARGET USER's state tree
/// (`<home>/.local/state/kallipai/tagmata/<slug>/logs`) — mirroring the
/// tagma's own placement, since logs are pure output residue kept outside
/// the instance data tree and the slug names the tree on both sides.
/// The instance resolves the same tree from its own process (its `$HOME`
/// is the passwd home the daemon dropped to), so the two sides meet
/// without sharing a pointer. The daemon may run as root while instances
/// run as their own users — the daemon's own state home would name the
/// wrong tree — so the log verb resolves through [`instance_logs_dir`],
/// the single owner-aware resolver, and the tagma side mirrors the shape
/// with `logs_target` in kallip-tagma; the three shapes move together
/// by hand.
pub(crate) fn logs_pointer(user_home: &Path, slug: &str) -> PathBuf {
    user_home
        .join(".local")
        .join("state")
        .join("kallipai")
        .join("tagmata")
        .join(slug)
        .join("logs")
}

/// The passwd home of a uid, with a broken entry (an empty home field)
/// treated as no answer: joining a relative tree would quietly place the
/// logs in whatever directory the daemon happens to run in.
pub(crate) fn passwd_home(uid: u32) -> Option<PathBuf> {
    crate::spawn::passwd_by_uid(uid)
        .map(|user| user.home)
        .filter(|home| !home.as_os_str().is_empty())
}

/// Why an instance's log directory cannot be placed: the record names a
/// target uid the passwd database does not know. The daemon refuses to
/// guess — falling back to its own state home would read (or point at)
/// another user's tree.
#[derive(Debug, thiserror::Error)]
#[error("uid {uid} has no passwd entry; cannot place the instance log directory")]
pub(crate) struct LogsHomeError {
    pub uid: u32,
}

/// The owner-aware log directory: resolve the target user's home through
/// `lookup` (production: [`passwd_home`], the NSS database; tests: a
/// table) and grow the state tree under it.
pub(crate) fn instance_logs_dir(
    target_uid: u32,
    slug: &str,
    lookup: impl Fn(u32) -> Option<PathBuf>,
) -> Result<PathBuf, LogsHomeError> {
    let home = lookup(target_uid).ok_or(LogsHomeError { uid: target_uid })?;
    Ok(logs_pointer(&home, slug))
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
    use kallip_daemon_common::wire::InstanceState::*;

    #[test]
    fn log_pointer_is_the_target_users_state_tree() {
        let dir = logs_pointer(Path::new("/home/alice"), "e2e");
        assert_eq!(
            dir,
            PathBuf::from("/home/alice/.local/state/kallipai/tagmata/e2e/logs")
        );
    }

    #[test]
    fn instance_logs_dir_resolves_through_the_injected_lookup() {
        let dir = instance_logs_dir(1000, "e2e", |uid| {
            (uid == 1000).then(|| PathBuf::from("/home/alice"))
        })
        .expect("the uid resolves");
        assert_eq!(
            dir,
            PathBuf::from("/home/alice/.local/state/kallipai/tagmata/e2e/logs")
        );
    }

    #[test]
    fn a_uid_without_a_passwd_entry_is_an_error_not_a_fallback() {
        let error = instance_logs_dir(4242, "e2e", |_| None).unwrap_err();
        assert_eq!(error.uid, 4242);
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
