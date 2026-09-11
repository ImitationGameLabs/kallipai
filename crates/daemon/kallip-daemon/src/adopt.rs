//! The adopt verb: register an existing instance under this daemon
//! without touching any process.
//!
//! The division of labor: spawn registers and
//! launches a new instance; adopt takes an already-existing one —
//! running or stopped — under management. Adopt is pure registration:
//! the only write surface is the record area. Nothing is launched,
//! killed, chowned, created, or written inside the data directory.

use crate::records::{self, InstanceRecord};
use crate::scan::{self, ScannedInstance};
use crate::spawn::{
    LaunchIdentity, OwnerInference, SpawnError, authorized, cached_passwd_identity,
    ensure_not_root_inplace, infer_owner_decision, instance_data_dir, now_unix, overlaps,
    passwd_by_uid, path_owner_uid, require_root_for_drop, resolve_launch_identity,
    validate_user_env,
};
use anyhow::Context as _;
use kallip_daemon_common::wire::{InstanceState, valid_slug};
use std::path::{Path, PathBuf};

/// The publish-time half of the upsert authorization. The gate above
/// the register section decided against the snapshot read; by rename
/// time a racer may have replaced the record. The fresh owner must
/// satisfy the same rule — unchanged (nothing to re-decide), owned by
/// this peer, or a root peer — or the adopt loses with the usual
/// SlugTaken instead of overwriting a registration it never saw. A
/// vanished record publishes as a fresh registration: nothing stands
/// there to overwrite.
fn publish_authorized(
    previous_owner: u32,
    current: Option<&InstanceRecord>,
    peer_uid: u32,
) -> bool {
    match current {
        Some(record) => {
            record.owner_uid == previous_owner || authorized(peer_uid, record.owner_uid)
        }
        None => true,
    }
}

/// Register an externally-born instance. `workspace` and `data_dir`
/// are absolute paths to existing directories; `user_env` is the
/// persistent env snapshot later starts replay (spawn's env semantics,
/// not start's one-shot overlay). Returns the observed state at
/// adoption time — the daemon never launches anything, so the state is
/// an observation, not an outcome.
#[allow(clippy::too_many_arguments)] // the spawn entry's own shape
pub fn adopt(
    record_root: &Path,
    slug: &str,
    workspace: &str,
    data_dir: &str,
    user_env: &[String],
    owner_uid: u32,
    request_user: Option<&str>,
    accept_local_only: bool,
) -> Result<InstanceState, SpawnError> {
    if !valid_slug(slug) {
        return Err(SpawnError::Invalid(format!(
            "slug {slug:?} does not match [a-z0-9][a-z0-9-]*"
        )));
    }
    // The externally-supplied paths are read (shape and ownership)
    // before identity resolution: the default identity is *inferred*
    // from the data dir's owner, so reading it is an input to
    // resolution, not a probe after it. Record-area probes stay after
    // authorization: a denied requester still learns nothing about
    // slug existence from the error (the discipline spawn's entry
    // comment pins).
    let workspace_path = PathBuf::from(workspace);
    if !workspace_path.is_dir() {
        return Err(SpawnError::Invalid(format!(
            "workspace {workspace:?} is not an existing directory"
        )));
    }
    let workspace_canon = workspace_path
        .canonicalize()
        .map_err(|e| SpawnError::Invalid(format!("canonicalizing workspace: {e}")))?;
    let workspace_owner = path_owner_uid(&workspace_canon)
        .map_err(|e| SpawnError::Invalid(format!("reading workspace owner: {e}")))?;

    // The data directory is adopt's second externally-supplied path.
    // Shape: an existing directory carrying at least one of the
    // daemon's two real read touchpoints — `runtime.json` (the scan's
    // runtime file) and `credentials/` (the enrolled id). A directory
    // with neither would only ever produce a permanently Stopped
    // record with no identity. `relays.toml` is deliberately not a
    // criterion: a local-only instance legitimately lacks it.
    let data_dir_path = PathBuf::from(data_dir);
    if !data_dir_path.is_dir() {
        return Err(SpawnError::Invalid(format!(
            "data dir {data_dir:?} is not an existing directory"
        )));
    }
    let data_dir_canon = data_dir_path
        .canonicalize()
        .map_err(|e| SpawnError::Invalid(format!("canonicalizing data dir: {e}")))?;
    let looks_like_instance =
        data_dir_canon.join("runtime.json").exists() || data_dir_canon.join("credentials").is_dir();
    if !looks_like_instance {
        return Err(SpawnError::Invalid(format!(
            "data dir {data_dir:?} does not look like an instance data directory (no runtime.json, no credentials/)"
        )));
    }
    let data_dir_owner = path_owner_uid(&data_dir_canon)
        .map_err(|e| SpawnError::Invalid(format!("reading data dir owner: {e}")))?;

    // Identity. An explicit --user wins verbatim (the declared
    // dedicated form). Absent, the on-disk owners are the request:
    // diverging owners leave two candidate identities, which refuses
    // instead of picking silently; agreeing owners resolve through
    // the data dir's owner — the daemon's own user collapses to the
    // in-place form, anything else drops to that owner (root-only,
    // like every drop-to).
    // A delegate peer must name its target here too: the inference
    // would hand the adopted instance to the delegate's own service
    // account, and delegates own nothing.
    if request_user.is_none()
        && let Some(err) =
            crate::spawn::delegate_keyless_rejection(crate::delegates::current(), owner_uid)
    {
        return Err(err);
    }
    let identity = match request_user {
        Some(_) => resolve_launch_identity(request_user, owner_uid)?,
        None => {
            let daemon_uid = unsafe { libc::geteuid() };
            match infer_owner_decision(workspace_owner, data_dir_owner, daemon_uid) {
                OwnerInference::Divergent => {
                    return Err(SpawnError::Invalid(format!(
                        "workspace is owned by uid {workspace_owner} but the data dir by uid {data_dir_owner}; pass --user explicitly to pick the launch identity"
                    )));
                }
                OwnerInference::InPlace => {
                    // Same rule as spawn's self-form: an inferred in-place
                    // launch under a root daemon is a real-root instance.
                    // An explicit --user skips this — tagma's boot guard
                    // owns that case.
                    ensure_not_root_inplace(daemon_uid)?;
                    LaunchIdentity::InPlace {
                        uid: daemon_uid,
                        username: cached_passwd_identity()
                            .map(|(name, _)| name.to_string_lossy().into_owned()),
                    }
                }
                OwnerInference::DropTo(uid) => {
                    require_root_for_drop()?;
                    LaunchIdentity::DropTo(passwd_by_uid(uid).ok_or_else(|| {
                        SpawnError::Invalid(format!(
                            "data dir owner uid {uid} has no passwd entry; cannot resolve a home for it"
                        ))
                    })?)
                }
            }
        }
    };
    let target_uid = identity.uid();
    if !authorized(owner_uid, target_uid) {
        return Err(SpawnError::Denied {
            peer_uid: owner_uid,
            target_uid,
        });
    }
    // Re-registration reads its subject up front: a record owned by
    // someone else is not this peer's to replace, and the answer is
    // the same SlugTaken a fresh spawn would say (nothing new leaks —
    // spawn's own probe tells an authorized peer as much). The true
    // cause stays in the log.
    let existing = records::read_record(record_root, slug);
    if let Some(previous) = &existing
        && !authorized(owner_uid, previous.owner_uid)
    {
        tracing::warn!(
            slug = %slug,
            peer_uid = owner_uid,
            record_owner = previous.owner_uid,
            "adopt upsert denied: the record belongs to another owner"
        );
        return Err(SpawnError::SlugTaken(slug.to_string()));
    }

    // Disjointness: adopt introduces a second external path, so every
    // direction is checked (see `check_overlaps`).
    let data_root = instance_data_dir(&identity, slug)?
        .parent()
        .context("instance data tree has no parent")?
        .to_path_buf();
    // Canonical where the tree exists; a fresh host has no tree yet,
    // and the verbatim path is then the honest comparison input (the
    // spawn precedent).
    let data_root_canon = data_root
        .canonicalize()
        .unwrap_or_else(|_| data_root.clone());
    let registered = scan::scan_instances(record_root);

    // A re-adopt replaces its own record, so the slug is not its own
    // overlap rival; everyone else's registrations still are.
    let others: Vec<ScannedInstance> = registered
        .iter()
        .filter(|instance| instance.slug != slug)
        .cloned()
        .collect();
    check_overlaps(&workspace_canon, &data_dir_canon, &data_root_canon, &others)?;

    // Env: allowlist, daemon-owned keys, addr shape — the same
    // validation spawn applies, verbatim.
    validate_user_env(user_env)?;

    // The relay probe (adopt's one relay touchpoint): an enrolled
    // instance that relied on env-sugar relays, adopted without any
    // relay configuration, would silently degrade to local-only on
    // the first start — the record replays a relay-less env and there
    // is no relays.toml to fall back to, and nothing errors. Demand
    // the choice explicitly instead of letting it happen by default.
    if !accept_local_only
        && stored_credentials_in_any_entry(&data_dir_canon)
        && !data_dir_canon.join("relays.toml").exists()
        && !user_env
            .iter()
            .any(|pair| pair.starts_with("KALLIP_TAGMA_RELAY_"))
    {
        return Err(SpawnError::Invalid(
            "the instance has stored credentials but no relay configuration: without a relay entry the next start silently degrades to local-only. Pass -e KALLIP_TAGMA_RELAY_*=... to carry the relay config, or --accept-local-only to adopt it as a local-only instance"
                .to_string(),
        ));
    }

    // --- observe: the identity at adoption time ---------------------------
    // A corrupt or unreadable runtime.json counts as not running (the
    // scan's read_runtime precedent): the instance adopts as Stopped.
    let live_pid = scan::read_runtime(&data_dir_canon)
        .map(|runtime| runtime.pid)
        .filter(|pid| scan::pid_is_alive(*pid));
    let mut anchor = None;
    let mut observed_running = false;
    if let Some(pid) = live_pid {
        // Split-brain guard: a registered record already anchoring this
        // live pid means this data dir (or a copy of it) was adopted
        // once under another slug (the slug's own record is the one a
        // re-adopt replaces, so it is not its own rival). Two records
        // each pass stop's verification and kill the other's process;
        // refuse the second adoption instead.
        if registered.iter().any(|instance| {
            instance.slug != slug && instance.anchored.as_ref().is_some_and(|a| a.pid == pid)
        }) {
            return Err(SpawnError::Invalid(format!(
                "live pid {pid} is already anchored by a registered instance; a data-dir copy must not be adopted twice"
            )));
        }
        let facts = scan::observe_identity(pid);
        let (verdict, _) = scan::classify_identity(None, pid, &facts);
        match verdict {
            scan::Verdict::Match => {
                observed_running = true;
                // Same anchor shape as spawn's claim point: pin the
                // incarnation when the kernel start time is readable,
                // leave the name chain in charge when it is not.
                anchor = scan::proc_starttime(pid)
                    .filter(|t| *t > 0)
                    .map(|starttime| scan::Identity {
                        pid,
                        starttime,
                        anchored_at: now_unix(),
                    });
            }
            other => {
                // A live non-tagma process would adopt into a record
                // that could never be stopped (identity mismatch,
                // fail-closed) nor started (already running) — a
                // permanently stuck entry whose only exit is manual
                // record surgery in a 0700 state directory. Refuse
                // with the /proc observations instead.
                return Err(SpawnError::Invalid(format!(
                    "live pid {pid} in the data dir's runtime.json is not a kallip-tagma process (verdict {other:?}, exe {:?}, comm {:?}); refusing to register an instance that could never be stopped",
                    facts.exe, facts.comm
                )));
            }
        }
    }

    // --- register -----------------------------------------------------------
    // Upsert: a fresh slug publishes exclusively (create_record's
    // hard-link gate elects one winner among racers); a re-adopt
    // overwrites atomically (write_record's rename). The replaced
    // record keeps its instance_id — the same data tree under the
    // same slug is the same instance (the id follows the slug, not
    // the tree), and observers joining on the id keep their key.
    let record = InstanceRecord {
        instance_id: existing
            .as_ref()
            .map(|previous| previous.instance_id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        owner_uid,
        target_uid,
        target_username: identity.username(),
        workspace: Some(workspace_canon.display().to_string()),
        env: user_env.to_vec(),
        identity: anchor,
        data_dir: data_dir_canon,
    };
    let published = match &existing {
        Some(previous) => {
            // The snapshot gate above authorized against what the read
            // saw; the rename would publish over what stands there now.
            let current = records::read_record(record_root, slug);
            if !publish_authorized(previous.owner_uid, current.as_ref(), owner_uid) {
                tracing::warn!(
                    slug = %slug,
                    peer_uid = owner_uid,
                    record_owner = current.as_ref().map(|r| r.owner_uid),
                    "adopt upsert denied: the record changed hands mid-adopt"
                );
                return Err(SpawnError::SlugTaken(slug.to_string()));
            }
            records::write_record(record_root, slug, &record)
        }
        None => records::create_record(record_root, slug, &record),
    };
    published.map_err(|e| match e.kind() {
        // A racer published the slug after the read above; its
        // ownership is unchecked, so this adopt loses exactly as a
        // spawn would and the retry upserts.
        std::io::ErrorKind::AlreadyExists => SpawnError::SlugTaken(slug.to_string()),
        _ => anyhow::anyhow!("registering instance record: {e}").into(),
    })?;
    Ok(if observed_running {
        InstanceState::Running
    } else {
        InstanceState::Stopped
    })
}

/// Every disjointness direction adopt must check — seven in all: the
/// two request paths against each other, each against the shared
/// instance tree, and each against every registered instance's
/// workspace and data dir. `overlaps` is mutual prefix containment,
/// so each comparison covers both nesting directions by itself.
fn check_overlaps(
    workspace: &Path,
    data_dir: &Path,
    tree_root: &Path,
    instances: &[ScannedInstance],
) -> Result<(), SpawnError> {
    if overlaps(workspace, tree_root) {
        return Err(SpawnError::Overlap {
            requested: workspace.display().to_string(),
            existing_slug: "(instance tree)".into(),
            existing_workspace: tree_root.display().to_string(),
        });
    }
    if overlaps(data_dir, workspace) {
        return Err(SpawnError::Overlap {
            requested: data_dir.display().to_string(),
            existing_slug: "(requested workspace)".into(),
            existing_workspace: workspace.display().to_string(),
        });
    }
    if overlaps(data_dir, tree_root) {
        return Err(SpawnError::Overlap {
            requested: data_dir.display().to_string(),
            existing_slug: "(instance tree)".into(),
            existing_workspace: tree_root.display().to_string(),
        });
    }
    for instance in instances {
        if let Some(existing) = &instance.workspace {
            let existing_path = PathBuf::from(existing);
            if overlaps(workspace, &existing_path) {
                return Err(SpawnError::Overlap {
                    requested: workspace.display().to_string(),
                    existing_slug: instance.slug.clone(),
                    existing_workspace: existing.clone(),
                });
            }
            if overlaps(data_dir, &existing_path) {
                return Err(SpawnError::Overlap {
                    requested: data_dir.display().to_string(),
                    existing_slug: instance.slug.clone(),
                    existing_workspace: existing.clone(),
                });
            }
        }
        if overlaps(workspace, &instance.data_dir) {
            return Err(SpawnError::Overlap {
                requested: workspace.display().to_string(),
                existing_slug: instance.slug.clone(),
                existing_workspace: instance.data_dir.display().to_string(),
            });
        }
        if overlaps(data_dir, &instance.data_dir) {
            return Err(SpawnError::Overlap {
                requested: data_dir.display().to_string(),
                existing_slug: instance.slug.clone(),
                existing_workspace: instance.data_dir.display().to_string(),
            });
        }
    }
    Ok(())
}

/// Whether any credentials entry under `data_dir/credentials/` carries
/// stored relay credentials (both `tagma.id` and `tagma.token`
/// readable). Sorted entry traversal — the same shape as the scan's
/// `read_tagma_id`, so the probe and the scan never disagree about
/// what "enrolled" means. Deliberately read, not `exists`: an
/// unreadable file counts as absent.
fn stored_credentials_in_any_entry(data_dir: &Path) -> bool {
    let creds = data_dir.join("credentials");
    let Ok(entries) = std::fs::read_dir(&creds) else {
        return false;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let is_dir = e.file_type().ok()?.is_dir();
            is_dir.then(|| e.file_name().into_string().ok()).flatten()
        })
        .collect();
    names.sort();
    names.iter().any(|name| {
        let entry = creds.join(name);
        std::fs::read_to_string(entry.join("tagma.id")).is_ok()
            && std::fs::read_to_string(entry.join("tagma.token")).is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};
    use std::{fs, io, path::Path, sync::OnceLock, time::Duration};

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, text).expect("write");
    }

    /// A data dir that satisfies the shape check via an empty
    /// `credentials/` dir: probe-neutral (no enrolled entry) and
    /// runtime-less (adopts as Stopped).
    fn mk_stopped_data_dir(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(dir.join("credentials")).expect("create data dir");
        dir
    }

    /// An enrolled data dir: credentials with both files readable —
    /// the probe's trigger shape.
    fn mk_enrolled_data_dir(root: &Path, name: &str) -> PathBuf {
        let dir = mk_stopped_data_dir(root, name);
        write(&dir.join("credentials/default/tagma.id"), "tid-1\n");
        write(&dir.join("credentials/default/tagma.token"), "tok-1\n");
        dir
    }

    fn runtime_json(dir: &Path, pid: u32) {
        write(
            &dir.join("runtime.json"),
            &format!(r#"{{"pid":{pid},"port":4711}}"#),
        );
    }

    fn mk_record(data_dir: &Path) -> InstanceRecord {
        InstanceRecord {
            instance_id: uuid::Uuid::new_v4().to_string(),
            owner_uid: unsafe { libc::geteuid() },
            target_uid: unsafe { libc::geteuid() },
            target_username: None,
            workspace: None,
            env: Vec::new(),
            identity: None,
            data_dir: data_dir.to_path_buf(),
        }
    }

    fn peer() -> u32 {
        unsafe { libc::geteuid() }
    }

    fn daemon_user() -> String {
        // The explicit self user: the guard refuses an *inferred*
        // in-place identity for a root daemon, so the mechanics tests
        // name the user explicitly (works on any host, root or not).
        cached_passwd_identity()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .expect("test host has a passwd entry")
    }

    fn adopt_at(
        root: &Path,
        slug: &str,
        workspace: &Path,
        data_dir: &Path,
        env: &[String],
    ) -> Result<InstanceState, SpawnError> {
        adopt(
            root,
            slug,
            workspace.to_str().expect("utf8 workspace"),
            data_dir.to_str().expect("utf8 data dir"),
            env,
            peer(),
            Some(&daemon_user()),
            false,
        )
    }

    /// The test host's sleep binary, probed by exec (the sandbox may
    /// hide binaries from stat but not from exec — the spawn tests'
    /// bash() precedent).
    ///
    /// Resolved once per process and cached: a transient exec failure
    /// must not flip later tests from run to skip mid-session. A
    /// missing candidate is told apart from a transient spawn failure:
    /// `NotFound` abandons the candidate at once, any other spawn
    /// error is retried before the candidate is given up on.
    fn sleep_exe() -> Option<&'static str> {
        static SLEEP: OnceLock<Option<&'static str>> = OnceLock::new();
        *SLEEP.get_or_init(|| resolve_exec_candidate(SLEEP_CANDIDATES, probe_exec))
    }

    /// One exec probe attempt against one candidate binary.
    #[derive(Debug, PartialEq, Eq)]
    enum ProbeAttempt {
        /// The candidate ran and exited cleanly.
        Ran,
        /// The candidate is unusable here (absent, or present but
        /// broken) — retrying will not mend it.
        Missing,
        /// A transient spawn failure — worth retrying.
        Transient,
    }

    const SLEEP_CANDIDATES: &[&str] = &["/bin/sleep", "/usr/bin/sleep"];

    /// Total tries before a transiently failing candidate is abandoned.
    const PROBE_ATTEMPTS: usize = 3;

    /// Walk the candidates in order and return the first one that runs.
    /// A missing candidate is skipped for good; a transient spawn
    /// failure gets `PROBE_ATTEMPTS` tries before the candidate is
    /// abandoned.
    fn resolve_exec_candidate<'a>(
        candidates: &[&'a str],
        mut probe: impl FnMut(&str) -> ProbeAttempt,
    ) -> Option<&'a str> {
        for candidate in candidates {
            for attempt in 1..=PROBE_ATTEMPTS {
                match probe(candidate) {
                    ProbeAttempt::Ran => return Some(candidate),
                    ProbeAttempt::Missing => break,
                    ProbeAttempt::Transient if attempt == PROBE_ATTEMPTS => break,
                    ProbeAttempt::Transient => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        }
        None
    }

    /// One real exec probe: existence is decided by running the
    /// candidate with a zero-length sleep.
    fn probe_exec(candidate: &str) -> ProbeAttempt {
        match Command::new(candidate)
            .arg("0")
            .stdin(std::process::Stdio::null())
            .status()
        {
            Ok(status) if status.success() => ProbeAttempt::Ran,
            Ok(_) => ProbeAttempt::Missing,
            Err(err) if err.kind() == io::ErrorKind::NotFound => ProbeAttempt::Missing,
            Err(_) => ProbeAttempt::Transient,
        }
    }

    #[test]
    fn resolve_takes_the_first_candidate_that_runs() {
        let ran = resolve_exec_candidate(&["/first", "/second"], |c| {
            assert_eq!(c, "/first");
            ProbeAttempt::Ran
        });
        assert_eq!(ran, Some("/first"));
    }

    #[test]
    fn resolve_moves_past_a_missing_candidate_without_retrying_it() {
        let mut first_probes = 0;
        let ran = resolve_exec_candidate(&["/first", "/second"], |c| match c {
            "/first" => {
                first_probes += 1;
                ProbeAttempt::Missing
            }
            _ => ProbeAttempt::Ran,
        });
        assert_eq!(ran, Some("/second"));
        assert_eq!(first_probes, 1, "a missing candidate must not be retried");
    }

    #[test]
    fn resolve_exhausts_a_transient_candidate_then_takes_the_next() {
        let mut first_probes = 0;
        let ran = resolve_exec_candidate(&["/first", "/second"], |c| match c {
            "/first" => {
                first_probes += 1;
                ProbeAttempt::Transient
            }
            _ => ProbeAttempt::Ran,
        });
        assert_eq!(ran, Some("/second"));
        assert_eq!(
            first_probes, PROBE_ATTEMPTS,
            "a transient candidate gets exactly PROBE_ATTEMPTS tries"
        );
    }

    #[test]
    fn resolve_recovers_on_a_later_transient_try() {
        let mut tries = 0;
        let ran = resolve_exec_candidate(&["/only"], |_| {
            tries += 1;
            if tries < 2 {
                ProbeAttempt::Transient
            } else {
                ProbeAttempt::Ran
            }
        });
        assert_eq!(ran, Some("/only"));
        assert_eq!(tries, 2);
    }

    #[test]
    fn resolve_yields_none_when_every_candidate_is_missing() {
        let mut probes = 0;
        let ran = resolve_exec_candidate(&["/a", "/b"], |_| {
            probes += 1;
            ProbeAttempt::Missing
        });
        assert_eq!(ran, None);
        assert_eq!(probes, 2);
    }

    #[test]
    fn resolve_yields_none_when_transients_exhaust_everywhere() {
        let ran = resolve_exec_candidate(&["/a", "/b"], |_| ProbeAttempt::Transient);
        assert_eq!(ran, None);
    }

    /// A live process the scan's exe family check classifies as a
    /// tagma, through the PARENT-directory name rule (the store
    /// layout `<hash>-kallip-tagma-<ver>/bin/kallip-tagma`). The
    /// binary keeps its own file name, so a multicall /bin/sleep
    /// (which dispatches on argv[0] and rejects a renamed copy)
    /// still runs. Returns the child plus the family dir (which
    /// must outlive the run).
    fn spawn_fake_tagma(dir: &Path, source: &str) -> (Child, PathBuf) {
        let family = dir.join("kallip-tagma-family");
        fs::create_dir_all(&family).expect("create family dir");
        let exe = family.join("sleep");
        fs::copy(source, &exe).expect("copy sleep");
        // Executing a file the moment after its copy races the
        // kernel's deferred release of the copy's write handle:
        // ETXTBSY with no process holding the path (probe baseline:
        // ~8.6% of spawns under load, btrfs and tmpfs alike; later
        // bounded-retry runs absorbed 166 fires, zero exhaustion).
        // Not a shared-path collision -- each call site owns a
        // fresh tempdir. The 50-attempt bound is this site's own
        // conservative ceiling; harvest shims pick 10 for the same
        // window under their own timing asserts.
        let mut command = Command::new(&exe);
        command.arg("60").stdin(std::process::Stdio::null());
        for _ in 0..50 {
            match command.spawn() {
                Ok(child) => return (child, family),
                Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(error) => panic!("spawn fake tagma: {error}"),
            }
        }
        panic!(
            "spawn fake tagma: still busy (ETXTBSY) after 50 attempts: {}",
            exe.display()
        );
    }

    // --- validation -----------------------------------------------------

    #[test]
    fn adopt_rejects_a_bad_slug() {
        let root = tempdir();
        let ws = tempdir();
        let dd = mk_stopped_data_dir(root.path(), "dd");
        let error = adopt_at(root.path(), "Bad Slug", ws.path(), &dd, &[]).unwrap_err();
        assert!(
            matches!(error, SpawnError::Invalid(_)),
            "grammar is a request-input fault: {error}"
        );
    }

    /// Upsert is the owner's or root's act: a record owned by
    /// another peer refuses with the same SlugTaken a fresh spawn
    /// would say, and the record survives untouched. The caller
    /// reaches the check as a real non-root uid via --user nobody
    /// (skipped on hosts without that passwd entry).
    #[test]
    fn adopt_refuses_to_replace_a_record_owned_by_another_peer() {
        let root = tempdir();

        let Some(nobody) = crate::spawn::passwd_by_name("nobody") else {
            return; // no `nobody` on this host: skip
        };
        let ws = tempdir();

        let dd = mk_stopped_data_dir(root.path(), "dd");
        let mut seeded = mk_record(&dd);
        seeded.owner_uid = 65535;
        records::create_record(root.path(), "taken", &seeded).expect("seed record");
        let error = adopt(
            root.path(),
            "taken",
            ws.path().to_str().expect("utf8 workspace"),
            dd.to_str().expect("utf8 data dir"),
            &[],
            nobody.uid,
            Some("nobody"),
            false,
        )
        .unwrap_err();
        assert!(matches!(error, SpawnError::SlugTaken(_)), "{error}");
        let kept = records::read_record(root.path(), "taken").expect("record");
        assert_eq!(kept.instance_id, seeded.instance_id);
    }

    /// The upsert flow: a same-owner re-adopt succeeds, the
    /// instance_id carries over (the same data tree stays the same
    /// instance), and the record's mutable fields take the new
    /// request's values.
    #[test]
    fn adopt_upserts_a_same_slug_registration_and_keeps_the_instance_id() {
        let root = tempdir();
        let ws = tempdir();
        let dd = mk_stopped_data_dir(root.path(), "dd");
        let mut seeded = mk_record(&dd);
        seeded.workspace = None;
        records::create_record(root.path(), "team-a", &seeded).expect("seed record");
        let state = adopt_at(root.path(), "team-a", ws.path(), &dd, &[]).expect("upsert");
        assert_eq!(state, InstanceState::Stopped);
        let record = records::read_record(root.path(), "team-a").expect("record");
        assert_eq!(record.instance_id, seeded.instance_id);
        assert_eq!(
            record.workspace.as_deref(),
            Some(ws.path().to_str().expect("utf8 workspace"))
        );
    }

    #[test]
    fn adopt_requires_a_workspace_directory() {
        let root = tempdir();
        let dd = mk_stopped_data_dir(root.path(), "dd");
        let error = adopt_at(
            root.path(),
            "team-a",
            root.path().join("nope").as_path(),
            &dd,
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, SpawnError::Invalid(_)));
    }

    #[test]
    fn adopt_requires_an_existing_data_dir() {
        let root = tempdir();
        let ws = tempdir();
        let error = adopt_at(
            root.path(),
            "team-a",
            ws.path(),
            &root.path().join("nope"),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, SpawnError::Invalid(_)));
    }

    #[test]
    fn adopt_rejects_a_contentless_data_dir() {
        // A bare directory adopts into a permanently Stopped, anchor-less
        // record — the ghost-record shape refused by rule.
        let root = tempdir();
        let ws = tempdir();
        let dd = root.path().join("bare");
        fs::create_dir_all(&dd).expect("bare dir");
        let error = adopt_at(root.path(), "team-a", ws.path(), &dd, &[]).unwrap_err();
        let SpawnError::Invalid(message) = error else {
            panic!("expected Invalid, got {error}")
        };
        assert!(message.contains("does not look like"), "{message}");
    }

    #[test]
    fn adopt_accepts_runtime_json_alone_as_the_shape() {
        let root = tempdir();
        let ws = tempdir();
        let dd = root.path().join("rt");
        fs::create_dir_all(&dd).expect("data dir");
        runtime_json(&dd, u32::MAX - 1); // never a live pid
        assert!(matches!(
            adopt_at(root.path(), "team-a", ws.path(), &dd, &[]),
            Ok(InstanceState::Stopped)
        ));
    }

    // --- disjointness (the pure direction matrix) -----------------------

    fn reg(slug: &str, workspace: Option<&str>, data_dir: &Path) -> ScannedInstance {
        ScannedInstance {
            slug: slug.to_string(),
            instance_id: format!("id-{slug}"),
            workspace: workspace.map(str::to_string),
            pid: None,
            port: None,
            owner: None,
            tagma_id: None,
            anchored: None,
            data_dir: data_dir.to_path_buf(),
        }
    }

    fn overlap_slug(error: &SpawnError) -> String {
        match error {
            SpawnError::Overlap { existing_slug, .. } => existing_slug.clone(),
            other => panic!("expected Overlap, got {other}"),
        }
    }

    #[test]
    fn overlaps_are_checked_in_every_direction() {
        let root = tempdir();
        let tree = root.path().join("tree");
        let managed = root.path().join("managed");
        let other_dd = managed.join("other-dd");
        fs::create_dir_all(&tree).expect("tree");
        fs::create_dir_all(&other_dd).expect("other dd");
        let instances = vec![reg(
            "other",
            Some(root.path().join("other-ws").to_str().unwrap()),
            &other_dd,
        )];
        let ws = root.path().join("ws");
        let dd = root.path().join("dd");
        fs::create_dir_all(&ws).expect("ws");
        fs::create_dir_all(&dd).expect("dd");

        // Disjoint everywhere: the pass shape.
        check_overlaps(&ws, &dd, &tree, &instances).expect("disjoint paths pass");

        // workspace vs the shared tree (spawn's rule, still enforced).
        let error = check_overlaps(&tree.join("nested"), &dd, &tree, &instances).unwrap_err();
        assert_eq!(overlap_slug(&error), "(instance tree)");

        // The two request paths against each other (both nestings).
        let error = check_overlaps(&ws, &ws.join("as-data"), &tree, &instances).unwrap_err();
        assert_eq!(overlap_slug(&error), "(requested workspace)");
        let error = check_overlaps(&dd.join("as-workspace"), &dd, &tree, &instances).unwrap_err();
        assert_eq!(overlap_slug(&error), "(requested workspace)");

        // data dir vs the shared tree: two instances never share a tree.
        let error = check_overlaps(&ws, &tree.join("nested"), &tree, &instances).unwrap_err();
        assert_eq!(overlap_slug(&error), "(instance tree)");

        // Both request paths vs a registered workspace.
        let other_ws = root.path().join("other-ws");
        let error = check_overlaps(&other_ws.join("x"), &dd, &tree, &instances).unwrap_err();
        assert_eq!(overlap_slug(&error), "other");
        let error = check_overlaps(&ws, &other_ws.join("x"), &tree, &instances).unwrap_err();
        assert_eq!(overlap_slug(&error), "other");

        // data dir vs a registered data dir (the double-adoption shape).
        let error = check_overlaps(&ws, &other_dd.join("x"), &tree, &instances).unwrap_err();
        assert_eq!(overlap_slug(&error), "other");

        // workspace vs a registered data dir (both nestings): an
        // adopted instance's outside data dir is off the workspace map.
        let error = check_overlaps(&managed, &dd, &tree, &instances).unwrap_err();
        assert_eq!(overlap_slug(&error), "other");
        let error = check_overlaps(&other_dd.join("x"), &dd, &tree, &instances).unwrap_err();
        assert_eq!(overlap_slug(&error), "other");
    }

    // --- env ------------------------------------------------------------

    #[test]
    fn adopt_validates_env_like_spawn() {
        let root = tempdir();
        let ws = tempdir();
        let dd = mk_stopped_data_dir(root.path(), "dd");
        let error = adopt_at(
            root.path(),
            "team-a",
            ws.path(),
            &dd,
            &["FOO=bar".to_string()],
        )
        .unwrap_err();
        assert!(matches!(error, SpawnError::Invalid(_)), "not allowlisted");
        let error = adopt_at(
            root.path(),
            "team-a",
            ws.path(),
            &dd,
            &["KALLIP_TAGMA_SLUG=x".to_string()],
        )
        .unwrap_err();
        assert!(
            matches!(error, SpawnError::Invalid(_)),
            "daemon-owned keys stay reserved"
        );
        let error = adopt_at(
            root.path(),
            "team-a",
            ws.path(),
            &dd,
            &["KALLIP_TAGMA_ADDR=not-an-addr".to_string()],
        )
        .unwrap_err();
        let SpawnError::Invalid(message) = error else {
            panic!("expected Invalid, got {error}")
        };
        assert!(message.contains("SocketAddr"), "{message}");
        // The good pin passes the same path.
        adopt_at(
            root.path(),
            "team-a",
            ws.path(),
            &dd,
            &["KALLIP_TAGMA_ADDR=127.0.0.1:4711".to_string()],
        )
        .expect("a well-formed addr pin is a legal user key");
    }

    #[test]
    fn adopt_denies_a_cross_user_form_without_root() {
        if peer() == 0 {
            // The suite runs as root: the drop is possible, the refusal
            // does not apply (spawn's guard precedent).
            return;
        }
        let root = tempdir();
        let ws = tempdir();
        let dd = mk_stopped_data_dir(root.path(), "dd");
        let error = adopt(
            root.path(),
            "team-a",
            ws.path().to_str().unwrap(),
            dd.to_str().unwrap(),
            &[],
            peer(),
            Some("root"),
            false,
        )
        .unwrap_err();
        assert!(
            matches!(error, SpawnError::Denied { .. }),
            "denied precedes every probe: {error}"
        );
    }

    // --- identity -------------------------------------------------------

    #[test]
    fn adopt_pins_a_live_tagma_and_the_record_stops_it() {
        let Some(sleep) = sleep_exe() else {
            return; // no sleep binary on the host: skip the live-process tests
        };
        let root = tempdir();
        let ws = tempdir();
        let shim_dir = tempdir();
        let (mut child, _exe) = spawn_fake_tagma(shim_dir.path(), sleep);
        let pid = child.id();
        let dd = mk_stopped_data_dir(root.path(), "dd");
        runtime_json(&dd, pid);

        let state = adopt_at(root.path(), "team-a", ws.path(), &dd, &[])
            .expect("a live tagma adopts as running");
        assert_eq!(state, InstanceState::Running);
        let record = records::read_record(root.path(), "team-a").expect("record exists");
        let anchor = record
            .identity
            .expect("a live verifiable tagma pins its anchor");
        assert_eq!(anchor.pid, pid, "the anchor names the observed process");
        assert!(
            scan::proc_starttime(pid).is_some_and(|t| t == anchor.starttime),
            "the anchor pins the incarnation, not just the pid"
        );

        // End to end: the adopted instance answers to stop like any
        // daemon-spawned one.
        crate::stop::stop(root.path(), "team-a", peer()).expect("stop works through the anchor");
        child.wait().expect("reap");
        assert!(!scan::pid_is_alive(pid), "the adopted process is gone");
    }

    #[test]
    fn adopt_treats_a_missing_runtime_as_stopped() {
        let root = tempdir();
        let ws = tempdir();
        let dd = mk_stopped_data_dir(root.path(), "dd");
        let state = adopt_at(root.path(), "team-a", ws.path(), &dd, &[]).expect("adopts");
        assert_eq!(state, InstanceState::Stopped);
        let record = records::read_record(root.path(), "team-a").expect("record exists");
        assert!(record.identity.is_none(), "no runtime: no anchor");
    }

    #[test]
    fn adopt_treats_a_corrupt_runtime_as_stopped() {
        let root = tempdir();
        let ws = tempdir();
        let dd = mk_stopped_data_dir(root.path(), "dd");
        write(&dd.join("runtime.json"), "not json");
        let state = adopt_at(root.path(), "team-a", ws.path(), &dd, &[]).expect("adopts");
        assert_eq!(state, InstanceState::Stopped);
        let record = records::read_record(root.path(), "team-a").expect("record exists");
        assert!(record.identity.is_none());
    }

    #[test]
    fn adopt_treats_a_dead_pid_runtime_as_stopped() {
        let Some(sleep) = sleep_exe() else {
            return;
        };
        let mut child = Command::new(sleep)
            .arg("60")
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        child.kill().expect("kill");
        child.wait().expect("reap");
        assert!(
            !scan::pid_is_alive(pid),
            "the pid must be dead for this test"
        );

        let root = tempdir();
        let ws = tempdir();
        let dd = mk_stopped_data_dir(root.path(), "dd");
        runtime_json(&dd, pid);
        let state = adopt_at(root.path(), "team-a", ws.path(), &dd, &[]).expect("adopts");
        assert_eq!(state, InstanceState::Stopped, "a dead pid is not running");
        let record = records::read_record(root.path(), "team-a").expect("record exists");
        assert!(record.identity.is_none(), "a dead pid pins no anchor");
    }

    #[test]
    fn adopt_rejects_a_live_foreign_pid_with_observations() {
        let Some(sleep) = sleep_exe() else {
            return;
        };
        let mut child = Command::new(sleep)
            .arg("60")
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("spawn plain sleep");
        let pid = child.id();

        let root = tempdir();
        let ws = tempdir();
        let dd = mk_stopped_data_dir(root.path(), "dd");
        runtime_json(&dd, pid);
        let error = adopt_at(root.path(), "team-a", ws.path(), &dd, &[]).unwrap_err();
        let SpawnError::Invalid(message) = error else {
            panic!("expected Invalid, got {error}")
        };
        assert!(message.contains("not a kallip-tagma"), "{message}");
        assert!(message.contains("exe"), "{message}");

        child.kill().expect("kill");
        child.wait().expect("reap");
    }

    #[test]
    fn adopt_refuses_a_data_dir_copy_already_anchored_elsewhere() {
        let Some(sleep) = sleep_exe() else {
            return;
        };
        let root = tempdir();
        let ws = tempdir();
        let shim_dir = tempdir();
        let (mut child, _exe) = spawn_fake_tagma(shim_dir.path(), sleep);
        let pid = child.id();

        let original = mk_stopped_data_dir(root.path(), "original");
        runtime_json(&original, pid);
        adopt_at(root.path(), "first", ws.path(), &original, &[]).expect("the original adopts");

        // The copy-and-readopt attack: an identical data dir under a
        // second slug would share the first record's anchor, and each
        // record could then stop the other's process. (A second
        // workspace, so workspace disjointness is not what fires here
        // — the data-dir anchor mutex must be.)
        let copy = mk_stopped_data_dir(root.path(), "copy");
        runtime_json(&copy, pid);
        let ws2 = tempdir();
        let error = adopt_at(root.path(), "second", ws2.path(), &copy, &[]).unwrap_err();
        let SpawnError::Invalid(message) = error else {
            panic!("expected Invalid, got {error}")
        };
        assert!(message.contains("already anchored"), "{message}");

        // Cleanup: only the first record owns the process.
        crate::stop::stop(root.path(), "first", peer()).expect("stop the original");
        let _ = child.wait();
    }

    // --- relay probe ------------------------------------------------------

    #[test]
    fn adopt_demands_a_relay_choice_for_enrolled_instances() {
        let root = tempdir();
        let ws = tempdir();
        let dd = mk_enrolled_data_dir(root.path(), "dd");
        let error = adopt_at(root.path(), "team-a", ws.path(), &dd, &[]).unwrap_err();
        let SpawnError::Invalid(message) = error else {
            panic!("expected Invalid, got {error}")
        };
        assert!(message.contains("local-only"), "{message}");
        assert!(message.contains("--accept-local-only"), "{message}");
    }

    #[test]
    fn accept_local_only_is_the_explicit_choice() {
        let root = tempdir();
        let ws = tempdir();
        let dd = mk_enrolled_data_dir(root.path(), "dd");
        let state = adopt(
            root.path(),
            "team-a",
            ws.path().to_str().unwrap(),
            dd.to_str().unwrap(),
            &[],
            peer(),
            Some(&daemon_user()),
            true,
        )
        .expect("the flag carries the choice");
        assert_eq!(state, InstanceState::Stopped);
    }

    #[test]
    fn a_root_daemon_refuses_an_inferred_in_place_adopt() {
        // Live on this host (root sandbox): self-owned dirs and no
        // --user would launch the instance as the daemon's real-root
        // identity; non-root hosts keep the in-place default.
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let root = tempdir();
        let ws = tempdir();
        let dd = mk_stopped_data_dir(root.path(), "dd");
        let error = adopt(
            root.path(),
            "team-a",
            ws.path().to_str().unwrap(),
            dd.to_str().unwrap(),
            &[],
            peer(),
            None,
            false,
        )
        .unwrap_err();
        let SpawnError::Invalid(message) = error else {
            panic!("expected Invalid, got {error}")
        };
        assert!(message.contains("--user"), "{message}");
    }

    #[test]
    fn publish_revalidation_only_redecides_a_changed_owner() {
        // Explicit ids throughout: on this host the real euid is root,
        // whose authorized() exemption would blank the denial arm.
        let mut snapshot = mk_record(Path::new("/dd"));
        snapshot.owner_uid = 1000;
        let mut raced = mk_record(Path::new("/dd"));
        raced.owner_uid = 2000;
        // Unchanged owner: the snapshot decision stands.
        assert!(publish_authorized(1000, Some(&snapshot), 1000));
        // A racer's owner this peer may not touch: the adopt loses.
        assert!(!publish_authorized(1000, Some(&raced), 1000));
        // A root peer — and the fresh owner itself — still decide.
        assert!(publish_authorized(1000, Some(&raced), 0));
        assert!(publish_authorized(1000, Some(&raced), 2000));
        // A vanished record publishes as a fresh registration.
        assert!(publish_authorized(1000, None, 1000));
    }
    #[test]
    fn a_relays_toml_neutralizes_the_probe() {
        let root = tempdir();
        let ws = tempdir();
        let dd = mk_enrolled_data_dir(root.path(), "dd");
        write(&dd.join("relays.toml"), "grandfathered config");
        adopt_at(root.path(), "team-a", ws.path(), &dd, &[])
            .expect("an existing relay plan is its own answer");
    }

    #[test]
    fn relay_env_neutralizes_the_probe() {
        let root = tempdir();
        let ws = tempdir();
        let dd = mk_enrolled_data_dir(root.path(), "dd");
        adopt_at(
            root.path(),
            "team-a",
            ws.path(),
            &dd,
            &["KALLIP_TAGMA_RELAY_URL=ws://x".to_string()],
        )
        .expect("an explicit relay env is its own answer");
    }

    #[test]
    fn an_unenrolled_data_dir_never_triggers_the_probe() {
        let root = tempdir();
        let ws = tempdir();
        let dd = mk_stopped_data_dir(root.path(), "dd");
        adopt_at(root.path(), "team-a", ws.path(), &dd, &[])
            .expect("no credentials: nothing can degrade");
    }
}
