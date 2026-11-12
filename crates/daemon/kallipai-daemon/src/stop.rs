//! Stop semantics: one resolution names which incarnation of this instance
//! is alive (the record's launch anchor first, the runtime self-report
//! second; see `scan::resolve`), and the stop signals that pid, then polls
//! for exit within the grace period before SIGKILL. A pid the kernel does
//! not attribute to the user the instance runs as is refused, never signalled;
//! runtime.json stays (a daemon restart rebuilds its view from the record
//! area).
use std::path::Path;
use std::time::{Duration, Instant};

use crate::spawn::deny_guidance;
use kallipai_daemon_common::wire::{ErrorCode, valid_slug};

#[derive(Debug, thiserror::Error)]
pub enum StopError {
    #[error("no instance named {0}")]
    NotFound(String),
    #[error("stop denied: slug {slug} runs as uid {target_uid}; {}", deny_guidance(.target_uid))]
    Denied { slug: String, target_uid: u32 },
    #[error("instance {0} is not running (stale or missing pid)")]
    NotRunning(String),
    #[error("{slug}: pid {pid} in runtime.json is alive and is not pinned to this instance")]
    IdentityMismatch { slug: String, pid: u32 },
    /// The pid is alive, but it does not belong to the user this instance
    /// runs as: runtime.json is writable by that user, so naming a pid is
    /// not enough to aim a signal at it.
    #[error(
        "{slug} names pid {pid}: {}",
        foreign_uid_note(.pid_uid, .target_uid)
    )]
    ForeignUid {
        slug: String,
        pid: u32,
        target_uid: u32,
        pid_uid: Option<u32>,
    },
    #[error("stop of {slug} failed: {message}")]
    Internal { slug: String, message: String },
}

impl From<&StopError> for ErrorCode {
    fn from(error: &StopError) -> Self {
        match error {
            StopError::NotFound(_) => ErrorCode::NotFound,
            StopError::Denied { .. } => ErrorCode::Denied,
            StopError::NotRunning(_) => ErrorCode::NotRunning,
            // Same wire code as NotRunning: the CLI's restart treats a
            // stop refusal as "already stopped" and relaunches, and the REST
            // mapping keeps answering 409. Only the sentence differs.
            StopError::IdentityMismatch { .. } => ErrorCode::NotRunning,
            StopError::ForeignUid { .. } => ErrorCode::NotRunning,
            StopError::Internal { .. } => ErrorCode::Internal,
        }
    }
}

const GRACE: Duration = Duration::from_secs(10);

/// Blocking stop. The incarnation is chosen by `scan::resolve` — the
/// record's launch anchor first, the instance's own runtime.json second — so
/// a recycled pid is refused. A live pid neither claim verifies is reported
/// (`IdentityMismatch`), never signalled; an alive pid with no start time is
/// stopped, because an older tagma cannot always publish one. Before any
/// signal the pid has to belong to the user this instance runs as
/// (`ForeignUid` otherwise): naming a pid is not the same as proving it is
/// ours.
pub fn stop(record_root: &Path, slug: &str, peer_uid: u32) -> Result<(), StopError> {
    // Same grammar gate as spawn/start: an invalid slug is not an
    // instance name, so it cannot exist — NotFound without a
    // record-area read (the slug must not become a path probe).
    if !valid_slug(slug) {
        return Err(StopError::NotFound(slug.to_string()));
    }
    let Some(record) = crate::records::read_record(record_root, slug) else {
        return Err(StopError::NotFound(slug.to_string()));
    };
    // Authorization precedes every state change. The target is the
    // uid the instance runs as (the platform-hosting access rule):
    // that user or root may stop. A foreign peer is refused with
    // Denied, naming the slug and that uid: spawn's SlugTaken already
    // makes slug existence public, so the refusal states its terms
    // instead.
    if !crate::spawn::authorized(peer_uid, record.target_uid) {
        tracing::warn!(
            slug = %slug,
            peer_uid,
            target_uid = record.target_uid,
            "stop denied: foreign peer"
        );
        return Err(StopError::Denied {
            slug: slug.to_string(),
            target_uid: record.target_uid,
        });
    }
    let data_dir = record.data_dir.clone();
    let runtime = crate::scan::read_runtime(&data_dir);
    // One resolution decides: anchor first, self-report second. `Refuted`
    // (dead, or a pin naming another incarnation) leaves nothing to stop.
    let resolved = crate::scan::resolve(
        record.identity.as_ref(),
        runtime.as_ref(),
        &crate::scan::incarnation_liveness,
    );
    let live = match resolved {
        Some(resolution) if resolution.verdict != crate::scan::Liveness::Refuted => resolution,
        _ => {
            // Nothing this instance pins is alive. A pid the runtime file
            // names may still be alive — a stranger, or an incarnation the
            // record does not vouch for — and that is a different answer
            // from "nothing is running": it names the pid to look at.
            if let Some(runtime) = runtime
                && crate::scan::pid_is_alive(runtime.pid)
            {
                tracing::warn!(
                    slug = %slug,
                    pid = runtime.pid,
                    comm = ?crate::scan::pid_comm(runtime.pid),
                    exe = ?crate::scan::pid_exe(runtime.pid),
                    "stop refused: runtime.json names a live pid this instance does not pin"
                );
                return Err(StopError::IdentityMismatch {
                    slug: slug.to_string(),
                    pid: runtime.pid,
                });
            }
            return Err(StopError::NotRunning(slug.to_string()));
        }
    };
    let pid = live.pid;
    // Naming a pid is not proof enough to signal it: runtime.json is
    // writable by the user the instance runs as, and a start time is a
    // public number, so a file could aim the daemon's own privileges at
    // any live process. The process has to be one the user this instance
    // runs as could own. Both claims face this gate: the anchor is the
    // daemon's own word, but the gate is one rule, and a record that
    // disagrees with the kernel is worth refusing loudly.
    let pid_uid = crate::scan::proc_uid(pid);
    if pid_uid != Some(record.target_uid) {
        if pid_uid.is_none() && !crate::scan::pid_is_alive(pid) {
            // No /proc entry to attribute and nothing alive there: the pid
            // exited between the resolution and this gate. That is the
            // answer the caller asked for, not a uid verdict, and
            // it should not leave a misleading warning behind.
            tracing::info!(slug = %slug, pid, "the named pid exited before the uid gate");
            return Err(StopError::NotRunning(slug.to_string()));
        }
        tracing::warn!(
            slug = %slug,
            pid,
            target_uid = record.target_uid,
            pid_uid = ?pid_uid,
            source = ?live.source,
            verdict = ?live.verdict,
            "stop refused: the named pid belongs to another uid than the one this instance runs as"
        );
        return Err(StopError::ForeignUid {
            slug: slug.to_string(),
            pid,
            target_uid: record.target_uid,
            pid_uid,
        });
    }
    // Name the claim this signal rests on: an operator has to be able to
    // tell a hand-booted instance (self-report) from the one the daemon
    // launched (anchor), an unpinned pid from a verified one, and which pid
    // the other side named.
    match (live.verdict, live.source) {
        (crate::scan::Liveness::Unverifiable, _) => tracing::warn!(
            slug = %slug,
            pid,
            source = ?live.source,
            "stopping an alive pid with no pinned start time; the uid gate is its only attribution"
        ),
        (_, crate::scan::LiveSource::SelfReported) => tracing::warn!(
            slug = %slug,
            pid,
            runtime_pid = runtime.as_ref().map(|runtime| runtime.pid),
            "stopping the runtime.json self-report: the launch anchor is gone"
        ),
        (_, crate::scan::LiveSource::Anchored) => {
            if runtime.as_ref().map(|runtime| runtime.pid) != Some(pid) {
                tracing::warn!(
                    slug = %slug,
                    pid,
                    runtime_pid = runtime.as_ref().map(|runtime| runtime.pid),
                    "stopping the launch anchor; runtime.json names another pid"
                );
            }
        }
    }

    crate::scan::signal_owned(pid, record.target_uid, libc::SIGTERM)
        .map_err(|error| signal_refusal(slug, error, record.target_uid))?;
    tracing::info!(slug = %slug, pid, "stopping instance; sent SIGTERM");
    let deadline = Instant::now() + GRACE;
    while Instant::now() < deadline {
        if !crate::scan::pid_is_alive(pid) {
            tracing::info!(slug = %slug, pid, "instance stopped");
            recheck_after_stop(slug, pid, record.identity.as_ref(), runtime.as_ref());
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    tracing::warn!(slug = %slug, pid, "grace period expired; escalating to SIGKILL");
    if let Some(reason) = sigkill_refusal(
        &live,
        &crate::scan::proc_uid,
        &crate::scan::incarnation_liveness,
        record.target_uid,
    ) {
        tracing::warn!(
            slug = %slug,
            pid,
            %reason,
            "the pid this stop attributed is gone; nothing left to kill"
        );
        recheck_after_stop(slug, pid, record.identity.as_ref(), runtime.as_ref());
        return Ok(());
    }
    crate::scan::signal_owned(pid, record.target_uid, libc::SIGKILL)
        .map_err(|error| signal_refusal(slug, error, record.target_uid))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !crate::scan::pid_is_alive(pid) {
            tracing::info!(slug = %slug, pid, "instance stopped after SIGKILL");
            recheck_after_stop(slug, pid, record.identity.as_ref(), runtime.as_ref());
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    tracing::error!(slug = %slug, pid, "instance survived SIGKILL");
    Err(internal(slug, format!("pid {pid} survived SIGKILL")))
}

/// A stop that landed is not always the whole instance: when both claims
/// named a live pid, the other one is still running and `list` will keep
/// reporting Running. Say so, with both pids and their sources, so the next
/// listing does not read as a fresh contradiction.
fn recheck_after_stop(
    slug: &str,
    pid: u32,
    anchor: Option<&crate::scan::Identity>,
    runtime: Option<&crate::scan::RuntimeFile>,
) {
    let Some(other) =
        other_live_incarnation(anchor, runtime, pid, &crate::scan::incarnation_liveness)
    else {
        return;
    };
    tracing::warn!(
        slug = %slug,
        pid,
        other_pid = other.pid,
        other_source = ?other.source,
        other_verdict = ?other.verdict,
        "instance still has a live incarnation: list will report Running"
    );
}

/// The incarnation still alive after `stopped` died, if any: the same
/// resolution `list` reads, filtered to the claims that are not the pid just
/// stopped. Pure so the edge is testable with a fake judgement.
fn other_live_incarnation(
    anchor: Option<&crate::scan::Identity>,
    runtime: Option<&crate::scan::RuntimeFile>,
    stopped: u32,
    judge: &dyn Fn(u32, u64) -> crate::scan::Liveness,
) -> Option<crate::scan::Resolution> {
    let resolution = crate::scan::resolve(anchor, runtime, judge)?;
    (resolution.pid != stopped && resolution.verdict != crate::scan::Liveness::Refuted)
        .then_some(resolution)
}

fn internal(slug: &str, message: String) -> StopError {
    StopError::Internal {
        slug: slug.to_string(),
        message,
    }
}

/// The tail of `ForeignUid`'s sentence: the uid the kernel attributes the
/// pid to, against the uid this instance runs as. Kept apart from the
/// variant so the two readings stay word-for-word distinct.
fn foreign_uid_note(pid_uid: &Option<u32>, target_uid: &u32) -> String {
    match pid_uid {
        Some(uid) => format!(
            "the kernel attributes it to uid {uid}, and this instance runs as uid {target_uid}"
        ),
        None => {
            format!("it has no readable /proc entry, and this instance runs as uid {target_uid}")
        }
    }
}

/// The wire shape of a refused signal. A pid the kernel attributes to
/// another uid is a uid refusal; one that is gone stopped on its own,
/// which is the outcome a stop promises, whether `/proc` had no entry at
/// attribution time or `kill(2)` lost the race with ESRCH. Any other
/// `kill(2)` failure is an internal fault.
fn signal_refusal(slug: &str, error: crate::scan::SignalError, target_uid: u32) -> StopError {
    if error.pid_gone() {
        tracing::warn!(
            slug = %slug,
            %error,
            "the pid is gone or unreadable before the signal landed"
        );
        return StopError::NotRunning(slug.to_string());
    }
    match error {
        crate::scan::SignalError::Foreign { pid, pid_uid, .. } => StopError::ForeignUid {
            slug: slug.to_string(),
            pid,
            target_uid,
            pid_uid,
        },
        crate::scan::SignalError::Kill { pid, sig, error } => {
            internal(slug, format!("kill({pid}, {sig}): {error}"))
        }
    }
}

/// The re-ask before the one signal that cannot be taken back. The pid was
/// attributed before the grace period, and ten seconds is long enough for it
/// to exit and for the kernel to hand its number to another process — a
/// recycled pid answers to the same `kill`. `Some(reason)` means the pid is
/// no longer the incarnation this stop attributed — dead, or wearing that
/// number as someone else's process — so the caller's promise (that one
/// instance no longer runs) already holds and the refusal is reported, not
/// raised. `None` means the pid is still that incarnation: a pinned claim is
/// re-judged against its start time, an unpinned one against the uid the
/// instance runs as.
fn sigkill_refusal(
    live: &crate::scan::Resolution,
    uid_of: &dyn Fn(u32) -> Option<u32>,
    judge: &dyn Fn(u32, u64) -> crate::scan::Liveness,
    target_uid: u32,
) -> Option<String> {
    if live.starttime != 0 {
        return match judge(live.pid, live.starttime) {
            crate::scan::Liveness::Live => None,
            _ => Some(format!(
                "pid {} is no longer the incarnation this stop attributed",
                live.pid
            )),
        };
    }
    match uid_of(live.pid) {
        Some(uid) if uid == target_uid => None,
        Some(uid) => Some(foreign_uid_note(&Some(uid), &target_uid)),
        None => Some(format!(
            "pid {} is no longer the incarnation this stop attributed: /proc holds no readable entry for it",
            live.pid
        )),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// A `sleep` child standing in for an instance. Drop kills and reaps it,
    /// so no assertion failure, early return or error path leaves an orphan
    /// behind; `wait` reads the exit status first when a test wants it.
    struct Decoy(std::process::Child);

    impl Decoy {
        fn spawn() -> Self {
            Self(
                std::process::Command::new("sleep")
                    .arg("30")
                    .spawn()
                    .expect("spawn a decoy"),
            )
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }

        fn wait(&mut self) -> std::process::ExitStatus {
            self.0.wait().expect("reap the decoy")
        }
    }

    impl Drop for Decoy {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn stop_denial_guidance_names_root_for_a_root_owned_record() {
        let error = StopError::Denied {
            slug: "mine".to_string(),
            target_uid: 0,
        };
        assert_eq!(
            error.to_string(),
            "stop denied: slug mine runs as uid 0; run as root"
        );
    }

    #[test]
    fn stop_refuses_an_invalid_slug_before_touching_the_record_area() {
        let error = stop(Path::new("/nonexistent-records"), "../escape", unsafe {
            libc::getuid()
        })
        .unwrap_err();
        assert!(matches!(error, StopError::NotFound(_)), "{error}");
    }

    #[test]
    fn stop_refuses_a_peer_that_is_not_the_uid_it_runs_as() {
        // The authorization verdict is a pure comparison against the
        // record, so the negative is exercisable without a second user:
        // record a foreign target uid, connect as the real one. The
        // refused peer is answered with Denied, which names the
        // slug and that uid instead of the missing-slug shape.
        let root = tempfile::tempdir().expect("record root");
        crate::records::write_record(
            root.path(),
            "mine",
            &crate::records::InstanceRecord {
                instance_id: "instance-1".into(),
                owner_uid: unsafe { libc::getuid() } + 1,
                target_uid: unsafe { libc::getuid() } + 1,
                target_username: None,
                workspace: Some("/ws".into()),
                env: vec![],
                identity: None,
                data_dir: root.path().join("data").join("i1"),
                config_dir: Some(root.path().join("config")),
                state_dir: Some(root.path().join("state").join("kallipai")),
            },
        )
        .expect("write record");
        // A foreign peer, never root: root has the admin exemption, so
        // the negative needs a uid that is neither the target nor 0.
        let error = stop(root.path(), "mine", unsafe { libc::getuid() } + 2).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "stop denied: slug mine runs as uid {}; run as that user or root",
                unsafe { libc::getuid() } + 1
            )
        );
        // Root passes the same gate (the admin exemption), then
        // fails on the missing runtime.json like any authorized
        // caller.
        let error = stop(root.path(), "mine", 0).unwrap_err();
        assert!(matches!(error, StopError::NotRunning(_)), "{error}");
    }

    #[test]
    fn stop_allows_the_uid_it_runs_as_but_only_that_peer() {
        let root = tempfile::tempdir().expect("record root");
        let uid = unsafe { libc::getuid() };
        crate::records::write_record(
            root.path(),
            "mine",
            &crate::records::InstanceRecord {
                instance_id: "instance-1".into(),
                owner_uid: uid,
                target_uid: uid,
                target_username: None,
                workspace: Some("/ws".into()),
                env: vec![],
                identity: None,
                data_dir: root.path().join("data").join("i1"),
                config_dir: Some(root.path().join("config")),
                state_dir: Some(root.path().join("state").join("kallipai")),
            },
        )
        .expect("write record");
        // That uid passes authorization (and then fails on the
        // missing runtime.json — NotRunning). The ghost slug's
        // missing-slug no-op stays NotFound; a foreign peer on a
        // recorded slug is refused with Denied; root passes.
        let error = stop(root.path(), "ghost", uid + 1).unwrap_err();
        assert!(matches!(error, StopError::NotFound(_)), "{error}");
        let error = stop(root.path(), "mine", uid).unwrap_err();
        assert!(matches!(error, StopError::NotRunning(_)), "{error}");
        let error = stop(root.path(), "mine", uid + 1).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "stop denied: slug mine runs as uid {}; {}",
                uid,
                deny_guidance(&uid)
            )
        );
    }

    #[test]
    fn stop_names_a_live_pid_the_record_does_not_pin() {
        // The runtime file names this test process with a start time that
        // is not its own: alive, but not the incarnation the file claims.
        // The refusal names the pid instead of reporting a missing one, and
        // nothing is signalled (the pid below is this very process).
        let root = tempfile::tempdir().expect("record root");
        let uid = unsafe { libc::getuid() };
        let data_dir = root.path().join("data").join("i1");
        std::fs::create_dir_all(&data_dir).expect("data dir");
        crate::records::write_record(
            root.path(),
            "mine",
            &crate::records::InstanceRecord {
                instance_id: "instance-1".into(),
                owner_uid: uid,
                target_uid: uid,
                target_username: None,
                workspace: Some("/ws".into()),
                env: vec![],
                identity: None,
                data_dir: data_dir.clone(),
                config_dir: None,
                state_dir: None,
            },
        )
        .expect("write record");
        let pid = std::process::id();
        std::fs::write(
            data_dir.join("runtime.json"),
            format!(r#"{{"pid":{pid},"port":1,"starttime":1}}"#),
        )
        .expect("write runtime.json");

        let error = stop(root.path(), "mine", uid).unwrap_err();
        assert!(
            matches!(error, StopError::IdentityMismatch { .. }),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            format!("mine: pid {pid} in runtime.json is alive and is not pinned to this instance")
        );
        assert_eq!(ErrorCode::from(&error), ErrorCode::NotRunning);
    }

    #[test]
    fn stop_signals_an_unpinned_pid_when_the_uid_gate_passes() {
        // No start time anywhere: Unverifiable, alive, and attributed to the
        // user the instance runs as, so it is treated as this instance
        // and stopped, with the missing pin recorded as a warning. The pid
        // is a decoy child, never this test process.
        let root = tempfile::tempdir().expect("record root");
        let uid = unsafe { libc::getuid() };
        let data_dir = root.path().join("data").join("i1");
        std::fs::create_dir_all(&data_dir).expect("data dir");
        let mut decoy = Decoy::spawn();
        let pid = decoy.pid();
        crate::records::write_record(
            root.path(),
            "mine",
            &crate::records::InstanceRecord {
                instance_id: "instance-1".into(),
                owner_uid: uid,
                target_uid: uid,
                target_username: None,
                workspace: Some("/ws".into()),
                env: vec![],
                identity: None,
                data_dir: data_dir.clone(),
                config_dir: None,
                state_dir: None,
            },
        )
        .expect("write record");
        std::fs::write(
            data_dir.join("runtime.json"),
            format!(r#"{{"pid":{pid},"port":1}}"#),
        )
        .expect("write runtime.json");

        stop(root.path(), "mine", uid).expect("the unpinned pid is stopped");
        let status = decoy.wait();
        assert!(!status.success(), "the decoy was signalled");
    }

    #[test]
    fn stop_refuses_a_live_pid_attributed_to_another_uid() {
        // The record names this very process but claims another run-as uid:
        // the pid is alive, so the refusal is about that uid, and nothing
        // is signalled.
        let root = tempfile::tempdir().expect("record root");
        let uid = unsafe { libc::getuid() };
        let data_dir = root.path().join("data").join("i1");
        std::fs::create_dir_all(&data_dir).expect("data dir");
        let pid = std::process::id();
        crate::records::write_record(
            root.path(),
            "mine",
            &crate::records::InstanceRecord {
                instance_id: "instance-1".into(),
                owner_uid: uid,
                target_uid: uid + 1,
                target_username: None,
                workspace: Some("/ws".into()),
                env: vec![],
                identity: None,
                data_dir: data_dir.clone(),
                config_dir: None,
                state_dir: None,
            },
        )
        .expect("write record");
        std::fs::write(
            data_dir.join("runtime.json"),
            format!(r#"{{"pid":{pid},"port":1}}"#),
        )
        .expect("write runtime.json");

        let error = stop(root.path(), "mine", 0).unwrap_err();
        assert!(matches!(error, StopError::ForeignUid { .. }), "{error}");
        assert_eq!(
            error.to_string(),
            format!(
                "mine names pid {pid}: the kernel attributes it to uid {uid}, and this instance runs as uid {}",
                uid + 1
            )
        );
        assert_eq!(ErrorCode::from(&error), ErrorCode::NotRunning);
    }

    #[test]
    fn a_stop_that_left_another_incarnation_finds_it() {
        use crate::scan::{Identity, LiveSource, Liveness, RuntimeFile};
        let anchor = Identity {
            pid: 42,
            starttime: 1000,
            anchored_at: 7,
        };
        let runtime = RuntimeFile {
            pid: 43,
            port: 1,
            starttime: 2000,
        };
        // The pid just stopped is gone; the anchor's incarnation is not, so
        // the next listing would still say Running.
        let anchor_only = |pid: u32, _start: u64| {
            if pid == 42 {
                Liveness::Live
            } else {
                Liveness::Refuted
            }
        };
        let other = other_live_incarnation(Some(&anchor), Some(&runtime), 43, &anchor_only)
            .expect("the anchor is still live");
        assert_eq!(
            (other.pid, other.source, other.verdict),
            (42, LiveSource::Anchored, Liveness::Live)
        );
        // The pid just stopped is not a second incarnation.
        assert!(other_live_incarnation(Some(&anchor), None, 42, &anchor_only).is_none());
        // Nothing alive: no warning to raise.
        let nothing = |_pid: u32, _start: u64| Liveness::Refuted;
        assert!(other_live_incarnation(Some(&anchor), Some(&runtime), 43, &nothing).is_none());
    }

    #[test]
    fn the_sigkill_recheck_refuses_a_recycled_pid() {
        use crate::scan::{LiveSource, Liveness, Resolution};
        let live = Resolution {
            verdict: Liveness::Live,
            pid: 42,
            starttime: 1000,
            source: LiveSource::Anchored,
        };
        // The kernel hands the number to another incarnation during the
        // grace period: the pinned start time no longer names it.
        let recycled = |_pid: u32, _start: u64| Liveness::Refuted;
        let refusal = sigkill_refusal(&live, &|_pid| Some(0), &recycled, 0)
            .expect("a recycled pid is refused");
        assert!(refusal.contains("42"), "{refusal}");
        // Still the same incarnation: the SIGKILL goes out.
        let same = |_pid: u32, _start: u64| Liveness::Live;
        assert!(sigkill_refusal(&live, &|_pid| Some(0), &same, 0).is_none());
    }

    #[test]
    fn the_sigkill_recheck_falls_back_to_the_target_uid_without_a_pin() {
        use crate::scan::{LiveSource, Liveness, Resolution};
        let unpinned = Resolution {
            verdict: Liveness::Unverifiable,
            pid: 42,
            starttime: 0,
            source: LiveSource::SelfReported,
        };
        let judge = |_pid: u32, _start: u64| Liveness::Unverifiable;
        assert!(sigkill_refusal(&unpinned, &|_pid| Some(1000), &judge, 1000).is_none());
        assert!(sigkill_refusal(&unpinned, &|_pid| Some(0), &judge, 1000).is_some());
        assert!(sigkill_refusal(&unpinned, &|_pid| None, &judge, 1000).is_some());
    }

    #[test]
    fn a_refused_signal_maps_to_the_answer_it_means() {
        use crate::scan::SignalError;
        // Another uid: the uid refusal, with the kernel's number.
        let foreign = signal_refusal(
            "mine",
            SignalError::Foreign {
                pid: 42,
                expected: 1000,
                pid_uid: Some(0),
            },
            1000,
        );
        assert!(matches!(foreign, StopError::ForeignUid { .. }), "{foreign}");
        // No entry to attribute: the pid is gone, which is a stop that
        // already landed.
        let gone = signal_refusal(
            "mine",
            SignalError::Foreign {
                pid: 42,
                expected: 1000,
                pid_uid: None,
            },
            1000,
        );
        assert!(matches!(gone, StopError::NotRunning(_)), "{gone}");
        assert_eq!(ErrorCode::from(&gone), ErrorCode::NotRunning);
        // A kill(2) that lost the race with ESRCH is the same answer.
        let raced = signal_refusal(
            "mine",
            SignalError::Kill {
                pid: 42,
                sig: libc::SIGTERM,
                error: std::io::Error::from_raw_os_error(libc::ESRCH),
            },
            1000,
        );
        assert!(matches!(raced, StopError::NotRunning(_)), "{raced}");
        assert_eq!(ErrorCode::from(&raced), ErrorCode::NotRunning);
        // Any other kill(2) failure is an internal fault.
        let failed = signal_refusal(
            "mine",
            SignalError::Kill {
                pid: 42,
                sig: libc::SIGTERM,
                error: std::io::Error::from_raw_os_error(libc::EPERM),
            },
            1000,
        );
        assert!(matches!(failed, StopError::Internal { .. }), "{failed}");
        assert_eq!(ErrorCode::from(&failed), ErrorCode::Internal);
    }
}
