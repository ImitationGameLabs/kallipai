//! Start semantics: relaunch a registered instance that is stopped or
//! dead. This is relaunch, not allocation — nothing is created or
//! removed: the record carries the workspace and the request env captured
//! at spawn time, credentials/ survive untouched for the fresh process to
//! pick up, and a stale runtime.json is removed before the relaunch so the
//! launch poll only ever sees the new incarnation's self-report. The rest
//! of the environment — identity seed, harvest, deployment roots — is
//! recomposed here, so a restart follows the host as it is now.
//! The record also carries the three roots its spawn resolved; a relaunch
//! refuses a data root the environment no longer resolves, and records the
//! config and state roots the launch handed over, so the log pointer names
//! the tree the instance writes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use kallipai_daemon_common::roots;
use kallipai_daemon_common::wire::ErrorCode;

use crate::records::{self, InstanceRecord};
use crate::scan;
use crate::spawn::{
    SpawnError, deny_guidance, identity_from_record, launch, note_roots, prepare_launch, same_tree,
    validate_user_env,
};

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("no instance named {0}")]
    NotFound(String),
    #[error("start denied: slug {slug} runs as uid {target_uid}; {}", deny_guidance(.target_uid))]
    Denied { slug: String, target_uid: u32 },
    #[error("{0}")]
    Invalid(String),
    #[error("instance {slug} is already running (pid {pid})")]
    AlreadyRunning { slug: String, pid: u32 },
    /// Alive, but nothing pins it: a live pid with no start time could be
    /// this instance (a self-report from an older tagma writes `0` when it
    /// cannot read `/proc/self/stat`, and a hand-edited record can pin `0`),
    /// so a relaunch is refused rather than racing it.
    #[error(
        "instance {slug} is already running (pid {pid}); no start time pins it to this instance"
    )]
    UnpinnedRunning { slug: String, pid: u32 },
    #[error(
        "instance did not publish pid/port within {timeout_secs}s; see the instance log files \
         under {logs} and system OOM records"
    )]
    Timeout {
        timeout_secs: u64,
        /// The log tree this launch resolved: the recorded state root
        /// plus the log leaf, so the message names the tree the instance
        /// writes rather than a shape assembled here.
        logs: PathBuf,
    },
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<&StartError> for ErrorCode {
    fn from(error: &StartError) -> Self {
        match error {
            // Existing-code reuse over a new wire code: an already-running
            // slug is the same resource-conflict family SlugTaken names for
            // spawn; the message carries the precise state.
            StartError::AlreadyRunning { .. } => ErrorCode::SlugTaken,
            StartError::UnpinnedRunning { .. } => ErrorCode::SlugTaken,
            StartError::NotFound(_) => ErrorCode::NotFound,
            StartError::Denied { .. } => ErrorCode::Denied,
            StartError::Invalid(_) => ErrorCode::InvalidSpawnInput,
            StartError::Timeout { .. } => ErrorCode::SpawnTimeout,
            StartError::Internal(_) => ErrorCode::Internal,
        }
    }
}

impl From<SpawnError> for StartError {
    fn from(error: SpawnError) -> Self {
        match error {
            // launch()'s shared failure shapes carry over verbatim; the
            // spawn-only variants cannot occur through start's path but
            // stay total.
            SpawnError::Invalid(m) => StartError::Invalid(m),
            SpawnError::Timeout { timeout_secs } => {
                StartError::Internal(anyhow::anyhow!("launch timed out after {timeout_secs}s"))
            }
            SpawnError::Internal(e) => StartError::Internal(e),
            other => StartError::Internal(anyhow::anyhow!("{other}")),
        }
    }
}

/// Env key holding the one-time relay enrollment code. Consumed once the
/// instance holds stored relay credentials: tagma's Stored boot branch never
/// reads it, and its boot resolution fail-fasts on stored credentials plus
/// code — so replaying it after a completed enrollment bricks every restart.
const CONSUMED_ENROLLMENT_CODE: &str = "KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE";

/// Whether the instance already holds stored relay credentials — the shared
/// enrollment predicate
/// (`kallipai_daemon_common::credentials::read_enrolled_id`) applied to the
/// daemon's env-sugar relay entry, named `default`. Deliberately read, not
/// `exists`: an unreadable file counts as absent, and so does a blank one
/// left behind by an interrupted enrollment.
fn stored_credentials_exist(data_dir: &Path) -> bool {
    let entry = kallipai_daemon_common::credentials::entry_dir(data_dir, "default");
    kallipai_daemon_common::credentials::read_enrolled_id(&entry).is_some()
}

/// Build the replay env for a restart. Two filters apply:
///
/// * the enrollment code is dropped once it is provably spent (stored
///   credentials exist) — single-use secret material must not sit on disk
///   forever. Conditional on the probe: an instance whose first enrollment
///   failed (that boot degrades the entry to local-only, it does not fail)
///   still needs the code on restart to retry, so an unconditional strip
///   would make such instances unbootable.
///
/// The scrub writes the filtered list back to the instance record in the
/// same stroke so the record stops carrying the stale key. A failed scrub
/// write never blocks the relaunch — the in-memory filter is the
/// functional fix, the persisted copy is hygiene.
fn replay_env(record_root: &Path, slug: &str, record: &InstanceRecord) -> Vec<String> {
    let spent = format!("{CONSUMED_ENROLLMENT_CODE}=");
    let code_is_spent = has_spent_code(record, record.data_dir.as_path());
    let replay: Vec<String> = record
        .env
        .iter()
        .filter(|pair| {
            let drop_code = code_is_spent && pair.starts_with(&spent);
            !drop_code
        })
        .cloned()
        .collect();
    if replay.len() == record.env.len() {
        return record.env.clone();
    }
    let scrubbed = InstanceRecord {
        env: replay.clone(),
        ..record.clone()
    };
    if let Err(error) = records::write_record(record_root, slug, &scrubbed) {
        tracing::warn!(%error, "could not persist the enrollment-code scrub");
    }
    replay
}

/// Whether the instance carries an enrollment code that is provably spent:
/// the code is present in the persisted env and stored relay credentials
/// already exist (see [`stored_credentials_exist`]).
fn has_spent_code(record: &InstanceRecord, data_dir: &Path) -> bool {
    let spent = format!("{CONSUMED_ENROLLMENT_CODE}=");
    record.env.iter().any(|pair| pair.starts_with(&spent)) && stored_credentials_exist(data_dir)
}

/// Blocking relaunch. `judge` is the injected three-valued liveness
/// judgement (a pid plus the kernel start time that pins it), so tests can
/// fake it without a real process. `scan::resolve` decides the refusal: the
/// record's launch anchor first, the runtime self-report second — the same
/// answer `list` and `stop` read, and a claim still alive blocks the
/// relaunch whether or not its start time is pinned. A runtime.json naming
/// a dead or recycled pid does not block, and a missing one does not let a
/// second tagma start next to a live anchor.
/// `env_overrides` is a one-shot overlay validated like spawn's request
/// env and applied for this launch only.
pub fn start(
    record_root: &Path,
    slug: &str,
    env_overrides: &[String],
    exe: Option<&str>,
    timeout: Duration,
    judge: &dyn Fn(u32, u64) -> scan::Liveness,
    peer_uid: u32,
) -> Result<(u32, u16), StartError> {
    if !kallipai_daemon_common::wire::valid_slug(slug) {
        return Err(StartError::Invalid(format!(
            "slug {slug:?} does not match [a-z0-9][a-z0-9-]*"
        )));
    }
    // Same dev-only gate as spawn: a bad explicit exe is a request
    // rejection, not a 30s timeout.
    if let Some(exe) = exe
        && !crate::spawn::exe_runnable(exe)
    {
        return Err(StartError::Invalid(format!(
            "exe {exe:?} is not an existing executable file"
        )));
    }
    // Same rules as spawn's request env, checked before any filesystem
    // work: what could not be sent to spawn cannot overlay a replay.
    validate_user_env(env_overrides)?;
    let Some(record) = records::read_record(record_root, slug) else {
        return Err(StartError::NotFound(slug.to_string()));
    };
    // Authorization precedes every state change (the stale-runtime
    // removal and the relaunch both follow). The target is the
    // uid the instance runs as — the same-uid or dedicated user
    // chosen at spawn — so the access rule travels with the
    // instance. A foreign peer is refused with Denied, naming the
    // slug and that uid: spawn's SlugTaken already makes slug
    // existence public, so the refusal states its terms instead.
    if !crate::spawn::authorized(peer_uid, record.target_uid) {
        tracing::warn!(
            slug = %slug,
            peer_uid,
            target_uid = record.target_uid,
            "start denied: foreign peer"
        );
        return Err(StartError::Denied {
            slug: slug.to_string(),
            target_uid: record.target_uid,
        });
    }
    let identity = identity_from_record(record.target_uid, record.target_username.as_deref())?;
    let data_dir = record.data_dir.clone();
    let Some(workspace) = record.workspace.as_ref().filter(|w| !w.is_empty()) else {
        return Err(StartError::Invalid(format!(
            "instance {slug} has no recorded workspace"
        )));
    };
    let runtime = scan::read_runtime(&data_dir);
    let resolved = scan::resolve(record.identity.as_ref(), runtime.as_ref(), judge);
    if let Some(resolution) = resolved
        && resolution.verdict != scan::Liveness::Refuted
    {
        tracing::warn!(
            slug = %slug,
            pid = resolution.pid,
            source = ?resolution.source,
            verdict = ?resolution.verdict,
            comm = ?scan::pid_comm(resolution.pid),
            "start refused: a pid this instance's claims name is alive"
        );
        return Err(match resolution.verdict {
            scan::Liveness::Unverifiable => StartError::UnpinnedRunning {
                slug: slug.to_string(),
                pid: resolution.pid,
            },
            _ => StartError::AlreadyRunning {
                slug: slug.to_string(),
                pid: resolution.pid,
            },
        });
    }
    // The persisted request pairs are re-validated so hand-edited records
    // cannot smuggle daemon-owned keys into a launch; the replay env then
    // drops material that is provably spent (see replay_env), and the whole
    // set stays re-validated before it reaches the composition.
    validate_user_env(&record.env)?;
    let replay = replay_env(record_root, slug, &record);
    // One-shot overlay: appended after the replay so the later pair wins in
    // the composition. Not written back — the persisted request pairs stay
    // the spawn-time truth; the rest of the environment is recomposed below.
    let mut launch_env = replay;
    launch_env.extend(env_overrides.iter().cloned());
    // Compose the layers spawn shares and resolve the roots from exactly
    // that pair list; the record's roots are what this launch must land on.
    let (env, resolved) = prepare_launch(slug, Path::new(workspace), &launch_env, &identity)?;
    note_roots(slug, &resolved);
    require_recorded_data_root(&record, slug, &resolved.data)?;
    // A record whose startup migration could not persist the roots it
    // resolved names no config or state root. This relaunch resolves both from
    // the same environment and completes the record once the instance is up
    // (see `record_moved_roots`), so one failed write does not lock the
    // instance out for good.
    if let Err(error) = record.recorded_roots(slug) {
        tracing::warn!(
            slug = %slug,
            cause = %error,
            "instance record names no config or state root yet; this relaunch completes it"
        );
    }
    match launch(record_root, &data_dir, slug, &env, exe, timeout, &identity) {
        Ok((pid, port)) => {
            // Only a launch that landed may teach the record its roots: a
            // failed one hands the instance nothing, and a record claiming
            // trees this run never used would send the log pointer to a tree
            // with no log in it.
            record_moved_roots(record_root, slug, &record, &resolved);
            tracing::info!(slug = %slug, pid, port, "instance started (relaunch)");
            Ok((pid, port))
        }
        Err(SpawnError::Timeout { timeout_secs }) => Err(StartError::Timeout {
            timeout_secs,
            logs: roots::log_tree(&resolved.state, slug),
        }),
        Err(SpawnError::Internal(e)) => Err(StartError::Internal(e)),
        // Unreachable via launch today; kept total so a future variant
        // surfaces as an internal error instead of failing to map.
        Err(other) => Err(StartError::Internal(anyhow::anyhow!("{other}"))),
    }
}
/// Refuse a relaunch whose env resolves a data root other than the recorded
/// one: the instance reads its trees from this env, so a mismatch would boot
/// it into a tree the daemon does not record, scan or kill under. Both sides
/// are canonicalized (`same_tree`, the spelling adopt records), so a difference
/// is a real move and not a second spelling or a symlinked path to one
/// location.
fn require_recorded_data_root(
    record: &InstanceRecord,
    slug: &str,
    resolved: &Path,
) -> Result<(), StartError> {
    if same_tree(&record.data_dir, resolved) {
        return Ok(());
    }
    Err(StartError::Invalid(format!(
        "instance {slug} was recorded with data root {} but this launch resolves {}; the \
         root keys ({} or {}) are read from the daemon's own environment, so set them on \
         the daemon's unit and restart the daemon; a record written while a login \
         profile's root key was honored names a tree this launch no longer resolves; \
         restore whatever moved it, or adopt the resolved tree so the record points at \
         it (a fresh spawn of the same slug hits the existing record)",
        record.data_dir.display(),
        resolved.display(),
        roots::RootKind::Data.own_key(),
        roots::RootKind::Data.xdg_var(),
    )))
}

/// A relaunch recomposes the environment, so a moved deployment can hand
/// the instance a different config or state root. The record follows the
/// launch here: those two roots are writable state, and the log pointer
/// has to name the tree the instance actually writes. The data root is
/// the hard case above; these two only report.
fn record_moved_roots(
    record_root: &Path,
    slug: &str,
    record: &InstanceRecord,
    resolved: &roots::Roots,
) {
    let moved = [
        ("config", record.config_dir.as_deref(), &resolved.config),
        ("state", record.state_dir.as_deref(), &resolved.state),
    ];
    let differs =
        |recorded: Option<&Path>, now: &Path| recorded.is_none_or(|path| !same_tree(path, now));
    if !moved
        .iter()
        .any(|(_, recorded, now)| differs(*recorded, now))
    {
        return;
    }
    let mut updated = record.clone();
    updated.config_dir = Some(resolved.config.clone());
    updated.state_dir = Some(resolved.state.clone());
    for (root, recorded, now) in moved {
        if !differs(recorded, now) {
            continue;
        }
        tracing::warn!(
            slug = %slug,
            root,
            recorded = %recorded.map(|path| path.display().to_string()).unwrap_or_default(),
            resolved = %now.display(),
            "the instance root moved since the record was written; recording the new one"
        );
    }
    if let Err(error) = records::write_record(record_root, slug, &updated) {
        tracing::warn!(slug = %slug, error = %error, "recording the moved roots failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn::{Harvest, LaunchIdentity, prepare_launch_with};
    #[test]
    fn start_denial_guidance_names_root_for_a_root_owned_record() {
        let error = StartError::Denied {
            slug: "instance-1".to_string(),
            target_uid: 0,
        };
        assert_eq!(
            error.to_string(),
            "start denied: slug instance-1 runs as uid 0; run as root"
        );
    }

    fn record(env: &[&str], data_dir: &std::path::Path) -> InstanceRecord {
        InstanceRecord {
            instance_id: "instance-1".into(),
            owner_uid: 1000,
            target_uid: 1000,
            target_username: None,
            workspace: Some("/ws".into()),
            env: env.iter().map(|pair| pair.to_string()).collect(),
            identity: None,
            data_dir: data_dir.to_path_buf(),
            config_dir: None,
            state_dir: None,
        }
    }

    fn write_record_at(root: &std::path::Path, value: &InstanceRecord) {
        records::write_record(root, "instance-1", value).expect("write record");
    }

    fn persisted_env(root: &std::path::Path) -> Vec<String> {
        records::read_record(root, "instance-1")
            .expect("record persists")
            .env
    }

    fn write_record_with_target(root: &std::path::Path, target_uid: u32) {
        let mut value = record(&[], std::path::Path::new("/data/i1"));
        value.target_uid = target_uid;
        write_record_at(root, &value);
    }

    #[test]
    fn start_refuses_a_peer_that_is_not_the_uid_it_runs_as() {
        // Same pure-comparison negative as stop's: a foreign peer is
        // answered before any state change (the stale-runtime cleanup
        // included), with Denied, which names the slug and that uid
        // instead of the missing-slug shape.
        let root = tempfile::tempdir().expect("record root");
        write_record_with_target(root.path(), unsafe { libc::getuid() } + 1);
        // A foreign peer, never root: root has the admin exemption.
        let error = start(
            root.path(),
            "instance-1",
            &[],
            None,
            std::time::Duration::from_secs(1),
            &|_, _| scan::Liveness::Refuted,
            unsafe { libc::getuid() } + 2,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "start denied: slug instance-1 runs as uid {}; run as that user or root",
                unsafe { libc::getuid() } + 1
            )
        );
        // Root passes the same gate (the admin exemption).
        let error = start(
            root.path(),
            "instance-1",
            &[],
            None,
            std::time::Duration::from_secs(1),
            &|_, _| scan::Liveness::Refuted,
            0,
        )
        .unwrap_err();
        assert!(!matches!(error, StartError::NotFound(_)), "{error}");
    }

    #[test]
    fn start_hands_the_relaunch_to_the_uid_it_runs_as() {
        // That uid passes authorization (then fails later — no
        // workspace on disk — proving the denial did not fire).
        let root = tempfile::tempdir().expect("record root");
        write_record_with_target(root.path(), unsafe { libc::getuid() });
        let error = start(
            root.path(),
            "instance-1",
            &[],
            None,
            std::time::Duration::from_secs(1),
            &|_, _| scan::Liveness::Refuted,
            unsafe { libc::getuid() },
        )
        .unwrap_err();
        assert!(!matches!(error, StartError::NotFound(_)), "{error}");
    }

    #[test]
    fn start_refuses_while_the_launch_anchor_is_alive() {
        // The anchor names a live incarnation while runtime.json names
        // another pid: the anchor is the daemon's own claim, so the instance
        // is running and the refusal names the pid to stop.
        let root = tempfile::tempdir().expect("record root");
        let data = tempfile::tempdir().expect("data tempdir");
        let mut value = record(&[], data.path());
        value.identity = Some(scan::Identity {
            pid: 42,
            starttime: 99,
            anchored_at: 7,
        });
        write_record_at(root.path(), &value);
        std::fs::write(
            data.path().join("runtime.json"),
            r#"{"pid":7,"port":1,"starttime":5}"#,
        )
        .expect("write runtime.json");

        let error = start(
            root.path(),
            "instance-1",
            &[],
            None,
            Duration::from_secs(1),
            &|pid, starttime| {
                if pid == 42 && starttime == 99 {
                    scan::Liveness::Live
                } else {
                    scan::Liveness::Refuted
                }
            },
            unsafe { libc::getuid() },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "instance instance-1 is already running (pid 42)"
        );
    }

    #[test]
    fn start_refuses_an_unpinned_live_pid() {
        // The file names an alive pid and no start time: Unverifiable, and a
        // second launch is refused rather than racing an instance it cannot
        // rule out.
        let root = tempfile::tempdir().expect("record root");
        let data = tempfile::tempdir().expect("data tempdir");
        write_record_at(root.path(), &record(&[], data.path()));
        std::fs::write(data.path().join("runtime.json"), r#"{"pid":7,"port":1}"#)
            .expect("write runtime.json");

        let error = start(
            root.path(),
            "instance-1",
            &[],
            None,
            Duration::from_secs(1),
            &|_, _| scan::Liveness::Unverifiable,
            unsafe { libc::getuid() },
        )
        .unwrap_err();
        assert!(
            matches!(error, StartError::UnpinnedRunning { .. }),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            "instance instance-1 is already running (pid 7); no start time pins it to this instance"
        );
    }

    #[test]
    fn start_refuses_while_the_anchor_is_alive_without_a_runtime_file() {
        // The anchor alone is enough: runtime.json is the fallback claim, not
        // a requirement, so its absence does not make the anchor stale.
        let root = tempfile::tempdir().expect("record root");
        let data = tempfile::tempdir().expect("data tempdir");
        let mut value = record(&[], data.path());
        value.identity = Some(scan::Identity {
            pid: 42,
            starttime: 99,
            anchored_at: 7,
        });
        write_record_at(root.path(), &value);

        let error = start(
            root.path(),
            "instance-1",
            &[],
            None,
            Duration::from_secs(1),
            &|pid, starttime| {
                if pid == 42 && starttime == 99 {
                    scan::Liveness::Live
                } else {
                    scan::Liveness::Refuted
                }
            },
            unsafe { libc::getuid() },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "instance instance-1 is already running (pid 42)"
        );
    }

    #[test]
    fn start_relaunches_over_a_recycled_pid_in_runtime_json() {
        // The record pins nothing, and the pid runtime.json names does not
        // verify (the injected judgement says no): nothing is running, so
        // the relaunch gets past the check and fails later, on the
        // workspace this record does not have on disk.
        let root = tempfile::tempdir().expect("record root");
        let data = tempfile::tempdir().expect("data tempdir");
        write_record_at(root.path(), &record(&[], data.path()));
        std::fs::write(
            data.path().join("runtime.json"),
            r#"{"pid":7,"port":1,"starttime":5}"#,
        )
        .expect("write runtime.json");

        let error = start(
            root.path(),
            "instance-1",
            &[],
            None,
            Duration::from_secs(1),
            &|_, _| scan::Liveness::Refuted,
            unsafe { libc::getuid() },
        )
        .unwrap_err();
        assert!(
            !matches!(error, StartError::AlreadyRunning { .. }),
            "{error}"
        );
    }

    fn write_stored_credentials(data_dir: &std::path::Path) {
        let entry = kallipai_daemon_common::credentials::entry_dir(data_dir, "default");
        std::fs::create_dir_all(&entry).expect("create entry dir");
        std::fs::write(entry.join("tagma.id"), "tagma-1").expect("write id");
        std::fs::write(entry.join("tagma.token"), "token").expect("write token");
    }

    #[test]
    fn replay_drops_spent_code_and_scrubs_the_record() {
        let root = tempfile::tempdir().expect("record tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        write_stored_credentials(data.path());
        let value = record(
            &[
                "KALLIPAI_OPERATOR_TOKEN=t",
                "KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE=sk-spent",
            ],
            data.path(),
        );
        write_record_at(root.path(), &value);

        let replay = replay_env(root.path(), "instance-1", &value);

        assert_eq!(replay, ["KALLIPAI_OPERATOR_TOKEN=t"]);
        assert_eq!(persisted_env(root.path()), ["KALLIPAI_OPERATOR_TOKEN=t"]);
    }

    #[test]
    fn replay_scrub_preserves_the_identity_anchor() {
        // The scrub rewrites the record wholesale; dropping the anchor
        // here would silently demote every enrolled instance to
        // name-chain classification after its first code scrub.
        let root = tempfile::tempdir().expect("record tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        write_stored_credentials(data.path());
        let mut value = record(
            &[
                "KALLIPAI_OPERATOR_TOKEN=t",
                "KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE=sk-spent",
            ],
            data.path(),
        );
        value.identity = Some(scan::Identity {
            pid: 7,
            starttime: 99,
            anchored_at: 5,
        });
        write_record_at(root.path(), &value);

        replay_env(root.path(), "instance-1", &value);

        let parsed = records::read_record(root.path(), "instance-1").expect("record");
        let identity = parsed.identity.expect("anchor survives the scrub");
        assert_eq!((identity.pid, identity.starttime), (7, 99));
    }

    #[test]
    fn replay_keeps_code_while_enrollment_is_incomplete() {
        // No stored credentials: a first enrollment that degraded to
        // local-only still needs the code on restart to retry.
        let root = tempfile::tempdir().expect("record tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        let value = record(
            &[
                "KALLIPAI_OPERATOR_TOKEN=t",
                "KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE=sk-fresh",
            ],
            data.path(),
        );
        write_record_at(root.path(), &value);

        let replay = replay_env(root.path(), "instance-1", &value);

        assert_eq!(replay, value.env);
        assert_eq!(persisted_env(root.path()), value.env);
    }

    #[test]
    fn a_blank_credential_pair_does_not_count_as_spent() {
        let root = tempfile::tempdir().expect("record tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        let entry = kallipai_daemon_common::credentials::entry_dir(data.path(), "default");
        std::fs::create_dir_all(&entry).expect("create entry dir");
        // An interrupted enrollment leaves a pair of blank files behind; that
        // is not an enrollment, so the code is not spent and stays for the
        // retry.
        std::fs::write(entry.join("tagma.id"), " \n").expect("write blank id");
        std::fs::write(entry.join("tagma.token"), "").expect("write blank token");
        let value = record(
            &[
                "KALLIPAI_OPERATOR_TOKEN=t",
                "KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE=sk-pending",
            ],
            data.path(),
        );
        write_record_at(root.path(), &value);

        let replay = replay_env(root.path(), "instance-1", &value);

        assert_eq!(replay, value.env);
        assert_eq!(persisted_env(root.path()), value.env);
    }

    #[test]
    fn replay_without_code_is_identity() {
        let root = tempfile::tempdir().expect("record tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        write_stored_credentials(data.path());
        let value = record(&["KALLIPAI_OPERATOR_TOKEN=t"], data.path());
        write_record_at(root.path(), &value);

        assert_eq!(
            replay_env(root.path(), "instance-1", &value),
            ["KALLIPAI_OPERATOR_TOKEN=t"]
        );
    }

    #[test]
    fn start_rejects_overlay_key_outside_the_allowlist() {
        // Validation fires before any filesystem work, so the path is
        // never read and no record is needed.
        let error = start(
            std::path::Path::new("."),
            "instance-1",
            &["SOME_OTHER_KEY=v".to_string()],
            None,
            Duration::from_secs(1),
            &|_, _| scan::Liveness::Refuted,
            unsafe { libc::getuid() },
        )
        .expect_err("non-allowlisted overlay key");
        assert!(error.to_string().contains("allowlisted"), "{error}");
    }

    #[test]
    fn start_rejects_overlay_on_daemon_owned_key() {
        let error = start(
            std::path::Path::new("."),
            "instance-1",
            &["KALLIPAI_TAGMA_SLUG=elsewhere".to_string()],
            None,
            Duration::from_secs(1),
            &|_, _| scan::Liveness::Refuted,
            unsafe { libc::getuid() },
        )
        .expect_err("reserved overlay key");
        assert!(
            error.to_string().contains("cannot be overridden"),
            "{error}"
        );
        let error = start(
            std::path::Path::new("."),
            "instance-1",
            &["KALLIPAI_WORKSPACE_ROOT=/elsewhere".to_string()],
            None,
            Duration::from_secs(1),
            &|_, _| scan::Liveness::Refuted,
            unsafe { libc::getuid() },
        )
        .expect_err("reserved overlay key");
        assert!(
            error.to_string().contains("cannot be overridden"),
            "{error}"
        );
    }
    #[test]
    fn start_replay_keeps_a_pinned_addr_key() {
        // A pinned listen address is a persisted user key: the
        // stable-listening promise is that the recorded pair survives
        // re-validation and the replay verbatim.
        let root = tempfile::tempdir().expect("record root");
        let value = record(&["KALLIPAI_TAGMA_ADDR=127.0.0.1:4711"], root.path());
        write_record_at(root.path(), &value);
        let stored = records::read_record(root.path(), "instance-1").expect("record persists");
        validate_user_env(&stored.env).expect("the pinned addr re-validates");
        let replay = replay_env(root.path(), "instance-1", &stored);
        assert!(
            replay
                .iter()
                .any(|pair| pair == "KALLIPAI_TAGMA_ADDR=127.0.0.1:4711"),
            "the pinned listen address survives the start replay"
        );
    }

    fn harvest_with_home(home: &str) -> Harvest {
        Harvest {
            seed: vec![("HOME".to_string(), home.to_string())],
            pairs: vec![("PATH".to_string(), "/usr/bin:/bin".to_string())],
        }
    }

    fn in_place_identity() -> LaunchIdentity {
        LaunchIdentity::InPlace {
            uid: 1000,
            username: Some("alice".to_string()),
        }
    }

    /// The composition as spawn and start build it: the caller's request
    /// pairs over one harvested layer and one deployment layer. The tests
    /// below pin the restart's read of it; `prepare_launch` adds only the
    /// harvest that needs a real login shell.
    fn composed(
        request: &[String],
        harvest: &Harvest,
        deployment: &[(String, String)],
    ) -> (Vec<String>, roots::Roots) {
        prepare_launch_with(
            "instance-1",
            Path::new("/ws"),
            request,
            harvest,
            deployment,
            &in_place_identity(),
        )
        .expect("the composition resolves")
    }

    /// A record written before the daemon captured env pairs holds none;
    /// with only HOME in the seed the composition resolves the root spawn
    /// recorded, so such a record restarts unchanged.
    #[test]
    fn a_record_without_env_resolves_the_root_spawn_recorded() {
        let recorded = Path::new("/home/alice/.local/share/kallipai/tagmata/instance-1");
        let (env, resolved) = composed(&[], &harvest_with_home("/home/alice"), &[]);
        assert_eq!(resolved.data, recorded);
        assert!(
            require_recorded_data_root(&record(&[], recorded), "instance-1", &resolved.data)
                .is_ok(),
            "the recorded root is the resolved root"
        );
        assert!(
            env.iter().any(|pair| pair == "HOME=/home/alice"),
            "the seed HOME reaches the instance"
        );
    }

    /// One deployment XDG root: the spawn composition (request carrying the
    /// profile pairs) and the restart composition (request replaying an empty
    /// record) resolve the same root, so a restart never moves the instance.
    #[test]
    fn spawn_and_restart_resolve_one_root_under_a_deployment_xdg_root() {
        let deployment = vec![("XDG_DATA_HOME".to_string(), "/srv/xdg".to_string())];
        let profile = vec!["HOME=/home/alice".to_string(), "LANG=C".to_string()];
        let (_, spawned) = composed(&profile, &harvest_with_home("/home/alice"), &deployment);
        let (_, restarted) = composed(&[], &harvest_with_home("/tmp/elsewhere"), &deployment);
        assert_eq!(
            spawned.data, restarted.data,
            "an XDG root fixed by the deployment follows neither the profile nor the seed"
        );
    }

    /// A record whose root this launch does not resolve is refused, naming
    /// both paths and the exit: the instance would otherwise boot into a tree
    /// the daemon does not record, scan and kill under.
    #[test]
    fn a_restart_refuses_a_resolved_root_other_than_the_recorded_one() {
        let recorded = Path::new("/srv/old/kallipai/tagmata/instance-1");
        let deployment = vec![("XDG_DATA_HOME".to_string(), "/srv/new".to_string())];
        let (_, resolved) = composed(&[], &harvest_with_home("/home/alice"), &deployment);
        let error =
            require_recorded_data_root(&record(&[], recorded), "instance-1", &resolved.data)
                .expect_err("a moved root is refused");
        let StartError::Invalid(message) = &error else {
            panic!("a moved root is an invalid request, got {error}");
        };
        assert!(
            message.contains("/srv/old/kallipai/tagmata/instance-1"),
            "the refusal names the recorded root: {message}"
        );
        assert!(
            message.contains("/srv/new/kallipai/tagmata/instance-1"),
            "the refusal names the resolved root: {message}"
        );
        assert!(
            message.contains("KALLIPAI_TAGMA_DATA_DIR"),
            "the refusal names the exit: {message}"
        );
    }

    /// A second spelling of one location is not a move: the comparison
    /// folds redundant components before it decides.
    #[test]
    fn a_second_spelling_of_the_recorded_root_is_not_a_move() {
        let recorded = Path::new("/srv/x/kallipai/tagmata/instance-1");
        let spelled = Path::new("/srv//x/./kallipai/tagmata/instance-1");
        assert!(
            require_recorded_data_root(&record(&[], recorded), "instance-1", spelled).is_ok(),
            "one location, two spellings"
        );
    }

    /// The config and state roots follow the launch: a moved one is written
    /// back, so the log pointer names the tree the instance writes. The data
    /// root stays exactly as recorded.
    #[test]
    fn a_moved_state_root_is_recorded_and_the_data_root_is_kept() {
        let root = tempfile::tempdir().expect("record root");
        let mut value = record(&[], Path::new("/srv/data/kallipai/tagmata/instance-1"));
        value.config_dir = Some(Path::new("/old/config/kallipai/tagmata/instance-1").to_path_buf());
        value.state_dir = Some(Path::new("/old/state/kallipai").to_path_buf());
        write_record_at(root.path(), &value);
        let resolved = roots::Roots {
            data: value.data_dir.clone(),
            config: Path::new("/new/config/kallipai/tagmata/instance-1").to_path_buf(),
            state: Path::new("/new/state/kallipai").to_path_buf(),
        };
        record_moved_roots(root.path(), "instance-1", &value, &resolved);
        let stored = records::read_record(root.path(), "instance-1").expect("record persists");
        assert_eq!(stored.config_dir, Some(resolved.config.clone()));
        assert_eq!(stored.state_dir, Some(resolved.state.clone()));
        assert_eq!(
            stored.data_dir, value.data_dir,
            "the data root is not rewritten"
        );
    }

    /// A record the startup migration could not complete names no config
    /// or state root: its own reader answers with the keys that supply
    /// one, so the caller learns which keys restore the trees.
    #[test]
    fn a_record_without_roots_answers_with_the_keys_that_supply_one() {
        let error = record(&[], Path::new("/srv/data/kallipai/tagmata/instance-1"))
            .recorded_roots("instance-1")
            .expect_err("an old record has no roots to answer for");
        let message = error.to_string();
        assert!(
            message.contains("instance-1"),
            "the error names the record: {message}"
        );
        for key in roots::RootKind::Config.remedies() {
            assert!(
                message.contains(key),
                "{key} missing from the message: {message:?}"
            );
        }
    }

    /// A relaunch completes a record whose roots the startup migration could
    /// not persist: the write-back fills both missing roots from the trees
    /// this launch resolved, so the record answers for itself afterwards.
    #[test]
    fn a_relaunch_completes_a_record_without_the_recorded_roots() {
        let root = tempfile::tempdir().expect("record root");
        let value = record(&[], Path::new("/srv/data/kallipai/tagmata/instance-1"));
        write_record_at(root.path(), &value);
        let resolved = roots::Roots {
            data: value.data_dir.clone(),
            config: Path::new("/srv/config/kallipai/tagmata/instance-1").to_path_buf(),
            state: Path::new("/srv/state/kallipai").to_path_buf(),
        };
        record_moved_roots(root.path(), "instance-1", &value, &resolved);
        let stored = records::read_record(root.path(), "instance-1").expect("record persists");
        assert_eq!(stored.config_dir, Some(resolved.config.clone()));
        assert_eq!(stored.state_dir, Some(resolved.state.clone()));
        assert!(
            stored.recorded_roots("instance-1").is_ok(),
            "the record answers for its three roots after the completion"
        );
    }

    /// A home that arrives through a symlink is one tree, not a move: the
    /// comparison canonicalizes, the spelling adopt records.
    #[test]
    fn a_symlinked_spelling_of_the_recorded_root_is_not_a_move() {
        let base = tempfile::tempdir().expect("base");
        let real = base.path().join("real").join("kallipai/tagmata/instance-1");
        std::fs::create_dir_all(&real).expect("real tree");
        let home = base.path().join("home");
        std::os::unix::fs::symlink(base.path().join("real"), &home).expect("symlink");
        let spelled = home.join("kallipai/tagmata/instance-1");
        assert!(
            require_recorded_data_root(&record(&[], &real), "instance-1", &spelled).is_ok(),
            "one tree through a symlink is not a move"
        );
    }
}
