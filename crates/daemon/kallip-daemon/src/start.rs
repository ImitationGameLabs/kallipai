//! Start semantics: relaunch a registered instance that is stopped or
//! dead. This is relaunch, not allocation — nothing is created or
//! removed: the record carries the workspace and user env captured at
//! spawn time, credentials/ survive untouched for the fresh process to
//! pick up, and a stale runtime.json is removed before the relaunch so
//! the launch poll only ever sees the new incarnation's self-report.

use std::path::Path;
use std::time::Duration;

use kallip_daemon_common::wire::ErrorCode;

use crate::records::{self, InstanceRecord};
use crate::scan;
use crate::spawn::{SpawnError, deny_guidance, identity_from_record, launch, validate_user_env};

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("no instance named {0}")]
    NotFound(String),
    #[error("start denied: slug {slug} is owned by uid {target_uid}; {}", deny_guidance(.target_uid))]
    Denied { slug: String, target_uid: u32 },
    #[error("{0}")]
    Invalid(String),
    #[error("instance {0} is already running")]
    AlreadyRunning(String),
    #[error(
        "instance did not publish pid/port within {timeout_secs}s; see the \
         instance log files under <state-home>/kallipai/tagmata/<slug>/logs/ and system OOM records"
    )]
    Timeout { timeout_secs: u64 },
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<&StartError> for ErrorCode {
    fn from(error: &StartError) -> Self {
        match error {
            // Existing-code reuse over a new wire code: an already-running
            // slug is the same resource-conflict family SlugTaken names for
            // spawn; the message carries the precise state.
            StartError::AlreadyRunning(_) => ErrorCode::SlugTaken,
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
            SpawnError::Timeout { timeout_secs } => StartError::Timeout { timeout_secs },
            SpawnError::Internal(e) => StartError::Internal(e),
            other => StartError::Internal(anyhow::anyhow!("{other}")),
        }
    }
}

/// Env key holding the one-time relay enrollment code. Consumed once the
/// instance holds stored relay credentials: tagma's Stored boot branch never
/// reads it, and its boot resolution fail-fasts on stored credentials plus
/// code — so replaying it after a completed enrollment bricks every restart.
const CONSUMED_ENROLLMENT_CODE: &str = "KALLIP_TAGMA_RELAY_ENROLLMENT_CODE";

/// Whether the instance already holds stored relay credentials — the exact
/// predicate of tagma's Stored branch (`credentials::load_tagma`: both
/// `tagma.id` and `tagma.token` readable under the entry dir; the daemon's
/// env-sugar relay entry is named "default"). Deliberately read, not
/// `exists`, so an unreadable file counts as absent on both sides.
/// Cross-crate layout mirror — keep in sync with
/// crates/kallip-tagma/src/credentials.rs.
fn stored_credentials_exist(data_dir: &Path) -> bool {
    let entry = data_dir.join("credentials").join("default");
    std::fs::read_to_string(entry.join("tagma.id")).is_ok()
        && std::fs::read_to_string(entry.join("tagma.token")).is_ok()
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

/// Blocking relaunch. `pid_is_alive` is injected so tests can fake the
/// liveness verdict without a real process. Liveness
/// alone decides the AlreadyRunning check — the former comm re-check
/// rejected a healthy instance under a wrapped binary name (a
/// makeWrapper-wrapped tagma's truncated comm is not ours to judge).
/// `env_overrides` is a one-shot overlay validated like spawn's request
/// env and applied for this launch only.
pub fn start(
    record_root: &Path,
    slug: &str,
    env_overrides: &[String],
    exe: Option<&str>,
    timeout: Duration,
    pid_is_alive: &dyn Fn(u32) -> bool,
    peer_uid: u32,
) -> Result<(u32, u16), StartError> {
    if !kallip_daemon_common::wire::valid_slug(slug) {
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
    // recorded instance owner — the same-uid or dedicated user
    // chosen at spawn — so the access rule travels with the
    // instance. A foreign peer is refused with Denied, naming the
    // slug and its owner: spawn's SlugTaken already makes slug
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
    if let Some(runtime) = scan::read_runtime(&data_dir)
        && pid_is_alive(runtime.pid)
    {
        tracing::warn!(
            slug = %slug,
            pid = runtime.pid,
            comm = ?scan::pid_comm(runtime.pid),
            "start refused: recorded pid is alive"
        );
        return Err(StartError::AlreadyRunning(slug.to_string()));
    }
    // The persisted copy is re-validated so hand-edited records cannot
    // smuggle daemon-owned keys into a launch; the replay env then drops
    validate_user_env(&record.env)?;
    // material that is provably spent (see replay_env).
    let replay = replay_env(record_root, slug, &record);
    // One-shot overlay: appended after the replay so compose_launch_env's
    // map composition lets the later pair win. Not written back — the
    // persisted snapshot stays the spawn-time truth.
    let mut launch_env = replay;
    launch_env.extend(env_overrides.iter().cloned());
    match launch(
        record_root,
        &data_dir,
        slug,
        Path::new(&workspace),
        &launch_env,
        exe,
        timeout,
        &identity,
    ) {
        Ok((pid, port)) => {
            tracing::info!(slug = %slug, pid, port, "instance started (relaunch)");
            Ok((pid, port))
        }
        Err(SpawnError::Timeout { timeout_secs }) => Err(StartError::Timeout { timeout_secs }),
        Err(SpawnError::Internal(e)) => Err(StartError::Internal(e)),
        // Unreachable via launch today; kept total so a future variant
        // surfaces as an internal error instead of failing to map.
        Err(other) => Err(StartError::Internal(anyhow::anyhow!("{other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn start_denial_guidance_names_root_for_a_root_owned_record() {
        let error = StartError::Denied {
            slug: "instance-1".to_string(),
            target_uid: 0,
        };
        assert_eq!(
            error.to_string(),
            "start denied: slug instance-1 is owned by uid 0; run as root"
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
    fn start_refuses_a_peer_that_is_not_the_recorded_owner() {
        // Same pure-comparison negative as stop's: a foreign peer is
        // answered before any state change (the stale-runtime cleanup
        // included), with Denied, which names the slug and its owner
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
            &|_| false,
            unsafe { libc::getuid() } + 2,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "start denied: slug instance-1 is owned by uid {}; run as the owner or root",
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
            &|_| false,
            0,
        )
        .unwrap_err();
        assert!(!matches!(error, StartError::NotFound(_)), "{error}");
    }

    #[test]
    fn start_hands_the_recorded_owner_the_relaunch() {
        // The owner passes authorization (then fails later — no
        // workspace on disk — proving the denial did not fire).
        let root = tempfile::tempdir().expect("record root");
        write_record_with_target(root.path(), unsafe { libc::getuid() });
        let error = start(
            root.path(),
            "instance-1",
            &[],
            None,
            std::time::Duration::from_secs(1),
            &|_| false,
            unsafe { libc::getuid() },
        )
        .unwrap_err();
        assert!(!matches!(error, StartError::NotFound(_)), "{error}");
    }

    fn write_stored_credentials(data_dir: &std::path::Path) {
        let entry = data_dir.join("credentials").join("default");
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
                "KALLIP_OPERATOR_TOKEN=t",
                "KALLIP_TAGMA_RELAY_ENROLLMENT_CODE=sk-spent",
            ],
            data.path(),
        );
        write_record_at(root.path(), &value);

        let replay = replay_env(root.path(), "instance-1", &value);

        assert_eq!(replay, ["KALLIP_OPERATOR_TOKEN=t"]);
        assert_eq!(persisted_env(root.path()), ["KALLIP_OPERATOR_TOKEN=t"]);
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
                "KALLIP_OPERATOR_TOKEN=t",
                "KALLIP_TAGMA_RELAY_ENROLLMENT_CODE=sk-spent",
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
                "KALLIP_OPERATOR_TOKEN=t",
                "KALLIP_TAGMA_RELAY_ENROLLMENT_CODE=sk-fresh",
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
        let value = record(&["KALLIP_OPERATOR_TOKEN=t"], data.path());
        write_record_at(root.path(), &value);

        assert_eq!(
            replay_env(root.path(), "instance-1", &value),
            ["KALLIP_OPERATOR_TOKEN=t"]
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
            &|_| false,
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
            &["KALLIP_TAGMA_SLUG=elsewhere".to_string()],
            None,
            Duration::from_secs(1),
            &|_| false,
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
            &["KALLIP_WORKSPACE_ROOT=/elsewhere".to_string()],
            None,
            Duration::from_secs(1),
            &|_| false,
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
        let value = record(&["KALLIP_TAGMA_ADDR=127.0.0.1:4711"], root.path());
        write_record_at(root.path(), &value);
        let stored = records::read_record(root.path(), "instance-1").expect("record persists");
        validate_user_env(&stored.env).expect("the pinned addr re-validates");
        let replay = replay_env(root.path(), "instance-1", &stored);
        assert!(
            replay
                .iter()
                .any(|pair| pair == "KALLIP_TAGMA_ADDR=127.0.0.1:4711"),
            "the pinned listen address survives the start replay"
        );
    }
}
