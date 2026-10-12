//! The launch pipeline: the record-area checks, the detach-exec through
//! the helper, the identity anchor and the runtime-file cleanup.

use super::authorize::authorized;
use super::env::{
    compose_launch_env, daemon_env_value, ensure_addr_parses, fill_relay_defaults,
    validate_user_env,
};
use super::error::{OverlapInput, SpawnError};
use super::harvest::harvest_base_env;
use super::identity::{
    LaunchIdentity, OwnerInference, infer_owner_decision, path_owner_uid, require_resolved_binary,
    resolve_launch_identity,
};
use crate::{bins, records, scan};
use anyhow::Context as _;
use kallipai_daemon_common::wire::valid_slug;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The instance's data directory: `<data home>/kallipai/tagmata/<slug>`.
/// In-place launches derive it from the daemon's XDG data home; drop-to
/// launches from the target user's passwd home. The exact path is
/// handed to the tagma at launch (`KALLIPAI_TAGMA_DATA_DIR`), replacing
/// the old shared-root assumption with an explicit contract.
pub(crate) fn instance_data_dir(
    identity: &LaunchIdentity,
    slug: &str,
) -> Result<PathBuf, SpawnError> {
    let home = match identity {
        LaunchIdentity::InPlace { .. } => {
            dirs::data_dir().context("could not determine platform data directory")?
        }
        LaunchIdentity::DropTo(user) => user.home.join(".local").join("share"),
    };
    Ok(home.join("kallipai").join("tagmata").join(slug))
}

// --- login-environment harvest -------------------------------------------

/// Register and launch one instance. Blocking — the server runs it on
/// the connection task. The record area is `record_root`; the requesting
/// peer is `owner_uid`.
/// The parameter count is the launch contract laid flat: every field is
/// an independent request dimension, and a params struct would only
/// move the names around.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    record_root: &Path,
    slug: &str,
    workspace: &str,
    user_env: &[String],
    exe: Option<&str>,
    timeout: Duration,
    owner_uid: u32,
    request_user: Option<&str>,
) -> Result<(u32, u16), SpawnError> {
    // Relay-intent default injection (the daemon-side fill): a request
    // env signaling relay intent (a `KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE` or
    // explicit `KALLIPAI_POLIS_URL` entry) gets a missing origin filled from
    // the daemon's own environment; explicit values pass through; an unset
    // -- or empty (the kallipai-adk's persistence.rs `filter(!is_empty)`
    // precedent) -- daemon value means unconfigured: nothing is filled
    // for that origin. This runs before the record snapshot is written,
    // so the record stores the filled env: a restart replays the
    // relay-complete env, and the fill cannot be lost between a
    // successful spawn and the first restart (the local-only detection
    // only covers the request moment).
    let user_env = fill_relay_defaults(user_env, daemon_env_value("KALLIPAI_POLIS_URL").as_deref());
    // --- validate ---------------------------------------------------------
    if !valid_slug(slug) {
        return Err(SpawnError::Invalid(format!(
            "slug {slug:?} does not match [a-z0-9][a-z0-9-]*"
        )));
    }
    // A dev-only explicit exe must be an existing, runnable file -
    // refusing at request time beats a 30s timeout discovering it.
    if let Some(exe) = exe
        && !exe_runnable(exe)
    {
        return Err(SpawnError::Invalid(format!(
            "exe {exe:?} is not an existing executable file"
        )));
    }
    // Resolution precedes authorization (the target must be known to
    // authorize against it), and both precede every state change, the
    // collision probe included: a denied peer must not read the
    // registry's occupancy through error differences.
    let identity = resolve_launch_identity(request_user, owner_uid)?;
    let target_uid = identity.uid();
    if !authorized(owner_uid, target_uid) {
        return Err(SpawnError::Denied {
            peer_uid: owner_uid,
            target_uid,
        });
    }
    let data_dir = instance_data_dir(&identity, slug)?;
    // Early occupancy probe: the common sequential-reuse case fails
    // here, before input validation, preserving the reviewed error
    // precedence. Advisory only — the exclusive publication in
    // create_record below is the authority: a racer that slips past
    // this probe still loses there, with the same code.
    if records::read_record(record_root, slug).is_some() {
        return Err(SpawnError::SlugTaken(slug.to_string()));
    }
    let workspace_path = PathBuf::from(workspace);
    if !workspace_path.is_dir() {
        return Err(SpawnError::Invalid(format!(
            "workspace {workspace:?} is not an existing directory"
        )));
    }
    let workspace_canon = workspace_path
        .canonicalize()
        .map_err(|e| SpawnError::Invalid(format!("canonicalizing workspace: {e}")))?;
    // The default identity runs the instance as the daemon's own
    // user, and the fresh data dir is then daemon-owned too; a
    // workspace owned by someone else would mix the two worlds in
    // one launch. An explicit --user declares the target and needs
    // no guard — the divergence rule fires only for the inferred
    // default.
    if request_user.is_none() {
        let workspace_owner = path_owner_uid(&workspace_canon)
            .map_err(|e| SpawnError::Invalid(format!("reading workspace owner: {e}")))?;
        let daemon_uid = unsafe { libc::geteuid() };
        if matches!(
            infer_owner_decision(workspace_owner, daemon_uid, daemon_uid),
            OwnerInference::Divergent
        ) {
            return Err(SpawnError::Invalid(format!(
                "workspace is owned by uid {workspace_owner} but the data dir would be created as uid {daemon_uid}; pass --user explicitly to pick the launch identity"
            )));
        }
    }

    // Workspace disjointness: against every registered instance's
    // workspace and against the data tree itself (an agent whose
    // workspace is the tree could write another instance's data).
    let data_root = data_dir.parent().context("data directory has no parent")?;
    // Canonical where the tree exists; a fresh host has no tree yet, and
    // the verbatim path is then the honest comparison input.
    let data_root_canon = data_root
        .canonicalize()
        .unwrap_or_else(|_| data_root.to_path_buf());
    if overlaps(&workspace_canon, &data_root_canon) {
        return Err(SpawnError::overlaps_instance_tree(
            OverlapInput::Workspace,
            Path::new(workspace),
            data_root,
        ));
    }
    for instance in scan::scan_instances(record_root) {
        if let Some(existing) = instance.workspace {
            let existing_path = PathBuf::from(&existing);
            if overlaps(&workspace_canon, &existing_path) {
                return Err(SpawnError::overlaps_instance_workspace(
                    OverlapInput::Workspace,
                    Path::new(workspace),
                    &instance.slug,
                    &existing_path,
                ));
            }
        }
        // And against its data dir: a workspace laid over another
        // instance's (necessarily tree-external) data home would
        // write into it — the adopt direction-6 mirror.
        if overlaps(&workspace_canon, &instance.data_dir) {
            return Err(SpawnError::overlaps_instance_data_dir(
                OverlapInput::Workspace,
                Path::new(workspace),
                &instance.slug,
                &instance.data_dir,
            ));
        }
    }

    validate_user_env(&user_env)?;

    // --- register ---------------------------------------------------------
    let record = records::InstanceRecord {
        instance_id: uuid::Uuid::new_v4().to_string(),
        owner_uid,
        target_uid,
        target_username: identity.username(),
        workspace: Some(workspace_canon.display().to_string()),
        env: user_env.to_vec(),
        identity: None,
        data_dir: data_dir.clone(),
    };
    // Authoritative collision gate: create_record publishes
    // exclusively, so a spawn racing a same-slug peer loses here with
    // SlugTaken — after authz, so the loser learns nothing about the
    // winner beyond occupancy itself.
    records::create_record(record_root, slug, &record).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => SpawnError::SlugTaken(slug.to_string()),
        _ => anyhow::anyhow!("registering instance record: {e}").into(),
    })?;

    // --- detach-exec + anchor ----------------------------------------------
    let started = launch(
        record_root,
        &data_dir,
        slug,
        &workspace_canon,
        &user_env,
        exe,
        timeout,
        &identity,
    )
    .inspect_err(|_| {
        // Rollback: the fresh registration goes away on any failure —
        // kill whatever the helper left first (a failed exec leaves
        // nothing; a half-boot leaves a running tagma). The data
        // directory is the tagma's; only the record is ours to remove,
        // and its absence is what unblocks a same-slug retry. start()
        // shares launch but keeps its existing record, so identity and
        // credentials survive a failed relaunch.
        if let Some(pid) = scan::read_runtime(&data_dir).map(|r| r.pid) {
            tracing::warn!(pid, "spawn rollback: killing half-booted instance");
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
        if let Err(error) = records::delete_record(record_root, slug) {
            tracing::warn!(%error, "spawn rollback: removing instance record failed");
        }
    });
    if let Ok((pid, port)) = started {
        tracing::info!(slug = %slug, pid, port, "instance spawned and anchored");
        return Ok((pid, port));
    }
    started
}

pub(crate) fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// A dev-only explicit exe must be an existing file with the exec bit
/// set; the request handler refuses anything else before any tree work.
pub(crate) fn exe_runnable(exe: &str) -> bool {
    let path = Path::new(exe);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    path.is_file()
}

/// Detach-exec one instance's tagma via the spawn helper and wait until
/// the process publishes its own runtime.json. Shared tail of spawn
/// (fresh registration) and start (relaunch of a registered one);
/// callers re-validate user env before reaching here. On failure the
/// record is the caller's policy (spawn deregisters, start keeps) —
/// but a half-booted leftover is SIGKILLed here either way so no
/// orphan outlives the timeout.
/// The shared tail of spawn and start: eight honest inputs, no
/// bundling — grouping them would rename, not reduce.
#[allow(clippy::too_many_arguments)]
pub(crate) fn launch(
    record_root: &Path,
    data_dir: &Path,
    slug: &str,
    workspace_canon: &Path,
    user_env: &[String],
    exe: Option<&str>,
    timeout: Duration,
    identity: &LaunchIdentity,
) -> Result<(u32, u16), SpawnError> {
    // The helper chdirs into the data directory before exec; a missing
    // directory is a launch failure, so ensure it exists. In place the
    // daemon's own mkdir is enough (everything inside belongs to the
    // tagma anyway); a drop-to launch must also hand the created chain
    // to the target user — a root-owned directory inside the user's
    // home would be territory the instance cannot fully use.
    match identity {
        LaunchIdentity::InPlace { .. } => {
            std::fs::create_dir_all(data_dir)
                .map_err(|e| anyhow::anyhow!("creating data dir {}: {e}", data_dir.display()))?;
        }
        LaunchIdentity::DropTo(user) => {
            ensure_target_owned_dir(data_dir, user.uid, user.gid)?;
        }
    }
    // The poll below trusts any runtime.json it sees as belonging to
    // this launch. That trust needs a clean slate: drop a previous
    // incarnation's runtime.json before starting the helper. ENOENT is
    // the common path (fresh spawn); any other failure aborts the
    // launch — an unremovable leftover would leave a foreign pid inside
    // the poll's trust window, and the timeout branch would kill it.
    if let Err(e) = clear_stale_runtime(data_dir) {
        tracing::error!(
            data_dir = %data_dir.display(),
            error = %e,
            "cannot clear stale runtime.json; refusing to launch"
        );
        return Err(
            anyhow::anyhow!("clearing stale runtime.json in {}: {e}", data_dir.display()).into(),
        );
    }
    let helper = bins::resolve("kallipai-daemon-spawn");
    // The daemon spawns the helper itself through Command, which
    // searches PATH — a bare-name helper rides the caller's PATH
    // (the unit's `path` in the nix deployment). The tagma is
    // different: the helper execve's it without a PATH search, so
    // its resolution gets the file gate below.
    // Explicit dev exe wins over the resolved one; the resolver stays
    // the production path (KALLIPAI_BIN_DIR, then CARGO_BIN_EXE_* in
    // tests).
    let tagma = match exe {
        Some(exe) => PathBuf::from(exe),
        None => bins::resolve("kallipai-tagma"),
    };
    require_resolved_binary(
        &tagma,
        "kallipai-tagma",
        "set KALLIPAI_BIN_DIR to the bin directory of the installed kallipai package, or pass --bin with an explicit path",
    )?;
    let base = harvest_base_env(tagma.parent(), identity);
    let (state_home, target_user) = match identity {
        LaunchIdentity::InPlace { .. } => (dirs::state_dir(), None),
        LaunchIdentity::DropTo(user) => (None, Some(user)),
    };
    let env = compose_launch_env(
        &base,
        user_env,
        slug,
        workspace_canon,
        data_dir,
        state_home.as_deref(),
        target_user,
    );
    // Guard every channel at the composed boundary: request pairs were
    // validated, but a harvested base value slips past that check.
    ensure_addr_parses(&env)?;
    // The helper drops privilege in the grandchild (initgroups → setgid
    // → setuid, in that order: initgroups needs privilege, and once
    // setuid has fired there is no way back). Flags are sent only for a
    // drop-to launch — the in-place path stays flag-free and identical
    // to the historical invocation. The daemon runs as root in the
    // drop-to form (checked at resolution), so the helper needs no
    // setuid bit.
    let mut helper_command = std::process::Command::new(&helper);
    if let Some(user) = target_user {
        helper_command
            .arg("--user")
            .arg(&user.username)
            .arg("--uid")
            .arg(user.uid.to_string())
            .arg("--gid")
            .arg(user.gid.to_string());
    }
    let status = helper_command
        .arg(data_dir)
        .arg(&tagma)
        .args(&env)
        .status()
        .map_err(|e| anyhow::anyhow!("running spawn helper: {e}"))?;
    if !status.success() {
        tracing::error!(status = %status, "spawn helper failed");
        return Err(anyhow::anyhow!("spawn helper exited {status}").into());
    }

    // --- wait for the self-written runtime.json --------------------------
    // Invariant: reaching this poll ⇔ the data dir held no leftover
    // runtime.json at launch time (cleared before the helper ran). Any
    // file that appears during the poll belongs to this launch, so the
    // pid it carries is trusted directly. Trust is then made durable:
    // the claim point pins pid+starttime into the instance record (or
    // clears a stale anchor a previous incarnation left), and a failed
    // revalidation keeps polling — a pid that died between its
    // starttime read and the check must not be reported as launched.
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(runtime) = scan::read_runtime(data_dir)
            && scan::pid_is_alive(runtime.pid)
            && anchor_identity(record_root, slug, runtime.pid)
        {
            return Ok((runtime.pid, runtime.port));
        }
        if Instant::now() >= deadline {
            // Kill whatever the helper left; the record is the caller's
            // policy (spawn deregisters, start keeps).
            if let Some(pid) = scan::read_runtime(data_dir).map(|r| r.pid) {
                // Diagnosability before the kill: the recorded comm is
                // what a naming-mismatch investigation needs.
                tracing::warn!(
                    pid,
                    comm = ?scan::pid_comm(pid),
                    "launch timed out; killing the pid that never published"
                );
                unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            }
            return Err(SpawnError::Timeout {
                timeout_secs: timeout.as_secs(),
            });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The launch claim point: pin `pid` into the instance record together
/// with the kernel start time read from `/proc` right now. A pid value
/// alone is not an identity — the kernel recycles pids, so a pid that
/// died and was reused would otherwise inherit the anchor; the pairing
/// with the start time binds the record to the one process that wrote
/// this launch's runtime.json, and a later mismatch makes stop refuse
/// instead of signalling a stranger. Post-condition the poll relies
/// on: the anchor names this pid or there is no anchor at all — a
/// stale anchor from a previous incarnation is cleared, never left
/// lying against a pid it does not name. If the record write itself
/// fails that is beyond reach: a stale anchor may survive and
/// classification falls to the name chain — the instance runs, and stop
/// then verifies by name alone: a mismatch is refused (fail-closed, the
/// daemon never signals a process it cannot vouch for); the warn names
/// the gap. Returns false only when
/// the revalidation race says the pid died under us; the caller keeps
/// polling rather than reporting a launch it cannot vouch for.
fn anchor_identity(record_root: &Path, slug: &str, pid: u32) -> bool {
    let starttime = scan::proc_starttime(pid).filter(|t| *t > 0);
    let Some(mut record) = records::read_record(record_root, slug) else {
        tracing::warn!(
            pid,
            "instance record unreadable at claim; launching unanchored"
        );
        return true;
    };
    match starttime {
        Some(starttime) => {
            record.identity = Some(scan::Identity {
                pid,
                starttime,
                anchored_at: now_unix(),
            });
        }
        None => {
            // Without a starttime there is nothing to pin; make that
            // explicit by clearing any anchor a previous incarnation
            // left, so classification falls to the name chain.
            if record.identity.is_some() {
                tracing::warn!(pid, "cannot read start time; clearing stale anchor");
            }
            record.identity = None;
        }
    }
    if let Err(e) = records::write_record(record_root, slug, &record) {
        tracing::warn!(pid, error = %e, "cannot write identity anchor; stale anchor may remain");
        return true;
    }
    // Revalidate against what the registry now says: if the pid died
    // between the starttime read and this check, its incarnation is
    // gone and the launch must not claim it.
    let Some(record) = records::read_record(record_root, slug) else {
        tracing::warn!(
            pid,
            "instance record unreadable after claim; not claiming the pid"
        );
        return false;
    };
    match scan::identity_matches(&record, slug, pid) {
        scan::Verdict::Match => true,
        other => {
            tracing::warn!(pid, verdict = ?other, "anchor failed revalidation; not claiming the pid");
            false
        }
    }
}

/// Seconds since the Unix epoch, saturating at 0 on clock skew;
/// diagnostic stamp only.
pub(crate) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Create `path` (and any missing ancestors) owned by the target user.
/// Only directories this launch creates are chowned — pre-existing
/// directories keep their owner, because the tree above the slug
/// directory is the user's own territory and the daemon merely carves
/// the slug directory into it. Chowning our own creations is what
/// makes the explicit data-dir handoff real: a root-owned directory
/// under the user's home would be territory the instance cannot use.
fn ensure_target_owned_dir(path: &Path, uid: u32, gid: u32) -> Result<(), SpawnError> {
    let mut missing = Vec::new();
    let mut cursor = path;
    // lstat, never stat: exists() follows symlinks, and a symlink
    // planted in the target user's own territory would steer root's
    // create+chown below into a foreign tree — every pre-existing
    // component must be a real directory, and anything else (a
    // symlink included) fails the launch closed.
    loop {
        match std::fs::symlink_metadata(cursor) {
            Ok(meta) if meta.is_dir() => break,
            Ok(_) => {
                return Err(anyhow::anyhow!(
                    "data dir {}: an existing component is not a real directory",
                    cursor.display()
                )
                .into());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                missing.push(cursor);
                cursor = cursor.parent().context("data directory has no parent")?;
            }
            Err(e) => {
                return Err(anyhow::anyhow!("data dir {}: {e}", cursor.display()).into());
            }
        }
    }
    for dir in missing.iter().rev() {
        std::fs::create_dir(dir).map_err(|e| anyhow::anyhow!("creating {}: {e}", dir.display()))?;
        // SAFETY: chown(2) on a directory created two lines above;
        // failure is a real error — the launch must not hand out a
        // wrongly-owned directory as the tagma's territory.
        let c_path = CString::new(dir.as_os_str().as_bytes())
            .map_err(|e| anyhow::anyhow!("data dir path with NUL byte: {e}"))?;
        let rc = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
        if rc != 0 {
            return Err(anyhow::anyhow!(
                "chown {}: {}",
                dir.display(),
                std::io::Error::last_os_error()
            )
            .into());
        }
    }
    Ok(())
}

/// Drop a leftover `runtime.json` from a previous incarnation. ENOENT is
/// success (the target state — no leftover — already holds); any other
/// error surfaces to the caller, which aborts the launch.
pub(super) fn clear_stale_runtime(data_dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(data_dir.join("runtime.json")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
