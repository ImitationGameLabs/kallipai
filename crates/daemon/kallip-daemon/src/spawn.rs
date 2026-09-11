//! The spawn pipeline: authorize → validate → register the record →
//! harvest the login environment → detach-exec via the helper → wait for
//! the instance's self-written runtime.json, rolling back the record on
//! timeout.
//!
//! The record area is the registry: a spawn registers `<slug>.json`
//! before launching and deregisters it on failure, so collision checks
//! and discovery read the same authority. The instance's data directory
//! is handed to the tagma explicitly (`KALLIP_TAGMA_DATA_DIR`) — the
//! two sides no longer assume a shared tree root.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::io::Read;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use kallip_daemon_common::wire::valid_slug;

use crate::{bins, records, scan};

/// How the request can fail, mapped 1:1 onto wire error codes by the server.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("slug {0:?} already exists")]
    SlugTaken(String),
    #[error("workspace {requested} overlaps instance {existing_slug} ({existing_workspace})")]
    Overlap {
        requested: String,
        existing_slug: String,
        existing_workspace: String,
    },
    #[error("{0}")]
    Invalid(String),
    /// The requester may not launch an instance as the target user.
    /// Fired before any state change — a denied requester learns
    /// nothing about slug existence from the error.
    #[error("uid {peer_uid} may not launch an instance as uid {target_uid}")]
    Denied { peer_uid: u32, target_uid: u32 },
    #[error(
        "instance did not publish pid/port within {timeout_secs}s; see the \
         instance log files under <state-home>/kallipai/tagmata/<slug>/logs/ and system OOM records"
    )]
    Timeout { timeout_secs: u64 },
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// Env keys the daemon owns; a request may not override them.
/// (`KALLIP_TAGMA_ADDR` is deliberately absent — it is the one user-set
/// listen knob: the daemon defaults it, a request pair wins, and the
/// value is shape-checked at validation.)
const RESERVED_KEYS: [&str; 3] = [
    "KALLIP_TAGMA_SLUG",
    "KALLIP_WORKSPACE_ROOT",
    "KALLIP_TAGMA_DATA_DIR",
];

/// The launch authorization (platform-hosting access rule): an
/// instance may run as the requesting peer itself, as any declared
/// user when the peer is root (the admin exemption behind
/// `sudo kallipctl`), or — for peers on the delegates list — as any
/// declared user (the delegated-administration grant fed by the
/// deployment). Called before any state change — in particular
/// before the slug collision probe — so a denied peer cannot probe
/// slug existence through error deltas.
pub(crate) fn authorized_with(
    delegates: &std::collections::HashSet<u32>,
    peer_uid: u32,
    target_uid: u32,
) -> bool {
    peer_uid == target_uid || peer_uid == 0 || delegates.contains(&peer_uid)
}

/// The process-wide view: the grant parsed at startup (empty unless
/// the deployment configured one).
pub(crate) fn authorized(peer_uid: u32, target_uid: u32) -> bool {
    authorized_with(crate::delegates::current(), peer_uid, target_uid)
}

/// The keyless form is always a refusal for a delegate peer: it would
/// silently claim the instance for the delegate's own service
/// account, which owns nothing. Pure so each verb (and each test)
/// drives the same check.
pub(crate) fn delegate_keyless_rejection(
    delegates: &std::collections::HashSet<u32>,
    peer_uid: u32,
) -> Option<SpawnError> {
    if delegates.contains(&peer_uid) {
        Some(SpawnError::Invalid(format!(
            "peer uid {peer_uid} is a delegate: name the launch user explicitly"
        )))
    } else {
        None
    }
}

/// The targets a delegate peer may not name: root (tagma's own boot
/// guard would be the only backstop), the daemon's own account (an
/// instance inside the daemon's identity holds the record area), and
/// another delegate account (service accounts own nothing). Pure for
/// the same reason; the refusal carries the alternative action.
pub(crate) fn delegate_target_rejection(
    delegates: &std::collections::HashSet<u32>,
    peer_uid: u32,
    target_uid: u32,
    daemon_uid: u32,
) -> Option<SpawnError> {
    if !delegates.contains(&peer_uid) {
        return None;
    }
    if target_uid == 0 {
        return Some(SpawnError::Invalid(
            "delegate peers may not launch instances as root; name a declared user instead"
                .to_string(),
        ));
    }
    if target_uid == daemon_uid {
        return Some(SpawnError::Invalid(
            "delegate peers may not launch instances as the daemon's own account; name a declared user instead"
                .to_string(),
        ));
    }
    if delegates.contains(&target_uid) {
        return Some(SpawnError::Invalid(
            "delegate peers may not target another delegate account; name a declared user instead"
                .to_string(),
        ));
    }
    None
}

/// The execution identity a launch resolves to. The two forms carry
/// the platform-hosting model: in place, the instance runs inside the
/// daemon's own context (the single-user story, byte-identical to the
/// pre-dedicated behavior); drop-to, it runs as another, externally
/// declared user through a privilege transition in the helper. Why the
/// split: the daemon's own user needs no resolution (its environment
/// IS the context, and XDG overrides keep working), while any other
/// user must be resolved from the passwd database and reached by
/// dropping privilege — half measures would leave a record naming one
/// user while the process runs as another.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LaunchIdentity {
    InPlace { uid: u32, username: Option<String> },
    DropTo(ResolvedUser),
}

/// A passwd-resolved execution identity for the drop-to form.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedUser {
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) username: String,
    pub(crate) home: PathBuf,
}

impl LaunchIdentity {
    pub(crate) fn uid(&self) -> u32 {
        match self {
            LaunchIdentity::InPlace { uid, .. } => *uid,
            LaunchIdentity::DropTo(user) => user.uid,
        }
    }

    pub(crate) fn username(&self) -> Option<String> {
        match self {
            LaunchIdentity::InPlace { username, .. } => username.clone(),
            LaunchIdentity::DropTo(user) => Some(user.username.clone()),
        }
    }
}

/// Resolve the launch identity for a spawn request. `request_user`
/// names a pre-declared user (the dedicated form); absence means "as
/// myself" — the requesting peer's own uid. A target that collapses to
/// the daemon's own effective user takes the in-place form, so the
/// historical single-user path stays unchanged. Authorization (against
/// the resolved target) is the caller's next step and stays ahead of
/// every state change.
pub(crate) fn resolve_launch_identity(
    request_user: Option<&str>,
    peer_uid: u32,
) -> Result<LaunchIdentity, SpawnError> {
    let daemon_uid = unsafe { libc::geteuid() };
    match request_user {
        None => {
            if peer_uid == daemon_uid {
                // An inferred self-launch under a root daemon would run the
                // instance as the host's real root — refuse and ask for an
                // explicit identity instead.
                ensure_not_root_inplace(daemon_uid)?;
                return Ok(LaunchIdentity::InPlace {
                    uid: peer_uid,
                    username: cached_passwd_identity()
                        .map(|(name, _)| name.to_string_lossy().into_owned()),
                });
            }
            if let Some(err) = delegate_keyless_rejection(crate::delegates::current(), peer_uid) {
                return Err(err);
            }
            // Another user's self-form request: full resolution with
            // no name to go by — the uid must carry its own passwd
            // entry.
            require_root_for_drop()?;
            passwd_by_uid(peer_uid)
                .ok_or_else(|| {
                    SpawnError::Invalid(format!(
                        "uid {peer_uid} has no passwd entry; cannot resolve a home for it"
                    ))
                })
                .map(LaunchIdentity::DropTo)
        }
        Some(name) => {
            let user = passwd_by_name(name).ok_or_else(|| {
                SpawnError::Invalid(format!("user {name:?} does not exist on this host"))
            })?;

            // Delegate targets are validated, not inherited.
            if let Some(err) = delegate_target_rejection(
                crate::delegates::current(),
                peer_uid,
                user.uid,
                daemon_uid,
            ) {
                return Err(err);
            }
            if user.uid == daemon_uid {
                Ok(LaunchIdentity::InPlace {
                    uid: user.uid,
                    username: Some(user.username),
                })
            } else {
                require_root_for_drop()?;
                Ok(LaunchIdentity::DropTo(user))
            }
        }
    }
}

/// The inferred in-place form under a root daemon would launch the
/// instance as the host's real root — exactly what tagma's own startup
/// guard refuses. Explicit `--user` selections skip this check: an
/// operator who names a user has made the decision the inference must
/// not make for them, and tagma enforces the real-root rule at its own
/// boot. Record replays skip it too: a stored identity is the explicit
/// decision of the adopt that wrote it.
pub(crate) fn ensure_not_root_inplace(daemon_uid: u32) -> Result<(), SpawnError> {
    if daemon_uid == 0 {
        return Err(SpawnError::Invalid(
            "the daemon runs as root and no --user was given: an inferred in-place \
             launch would run the instance as the host's real root — pass --user \
             <dedicated user> to name an unprivileged launch identity"
                .to_string(),
        ));
    }
    Ok(())
}

/// Re-resolve a record's target identity for a relaunch. The record's
/// uid is authoritative; the passwd entry supplies gid/home fresh at
/// relaunch time — a persisted username is a lookup hint, never the
/// truth itself.
pub(crate) fn identity_from_record(
    target_uid: u32,
    target_username: Option<&str>,
) -> Result<LaunchIdentity, SpawnError> {
    if target_uid == unsafe { libc::geteuid() } {
        return Ok(LaunchIdentity::InPlace {
            uid: target_uid,
            username: target_username.map(str::to_owned),
        });
    }
    require_root_for_drop()?;
    let user = match target_username {
        Some(name) => passwd_by_name(name)
            .filter(|user| user.uid == target_uid)
            .or_else(|| passwd_by_uid(target_uid)),
        None => passwd_by_uid(target_uid),
    };
    let user = user.ok_or_else(|| {
        SpawnError::Invalid(format!(
            "recorded target uid {target_uid} has no resolvable passwd entry"
        ))
    })?;
    Ok(LaunchIdentity::DropTo(user))
}

/// Dropping privilege to another user is a root-only act: a daemon
/// that is not root cannot launch for anyone but itself, and saying
/// so here (a plain request rejection) beats a setuid failure deep in
/// the launch.
pub(crate) fn require_root_for_drop() -> Result<(), SpawnError> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(SpawnError::Invalid(
            "launching an instance for another user requires the daemon to run as root".into(),
        ));
    }
    Ok(())
}

/// The uid that owns a path: the input to the ownership-inference
/// rules (adopt's default identity, spawn's divergence guard). A
/// stat on a canonical path — the caller owns canon first.
pub(crate) fn path_owner_uid(path: &Path) -> std::io::Result<u32> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(path)?.uid())
}

/// An actionable gate on the tagma resolution: the helper execve's
/// its payload without a PATH search, so a bare-name resolution
/// dies as exit 66 deep in the helper with a message about a file
/// that is not there. Refuse here instead, with the fixes named
/// (the helper's own error stays as the backstop).
fn require_resolved_binary(path: &Path, name: &str, hint: &str) -> Result<(), SpawnError> {
    if path.is_file() {
        return Ok(());
    }
    Err(SpawnError::Invalid(format!(
        "{name} resolved to {path:?}, which is not an existing file; {hint}"
    )))
}

/// The ownership-inference decision shared by spawn's divergence
/// guard and adopt's default identity: two externally-supplied
/// owners that disagree leave the launch ambiguous, agreeing
/// owners resolve through the data dir's owner against the
/// daemon's own uid. Pure so the whole matrix is testable
/// without filesystem ownership (chown needs privileges and a
/// uid the host namespace actually maps).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OwnerInference {
    InPlace,
    DropTo(u32),
    Divergent,
}

pub(crate) fn infer_owner_decision(
    workspace_owner: u32,
    data_dir_owner: u32,
    daemon_uid: u32,
) -> OwnerInference {
    if workspace_owner != data_dir_owner {
        OwnerInference::Divergent
    } else if data_dir_owner == daemon_uid {
        OwnerInference::InPlace
    } else {
        OwnerInference::DropTo(data_dir_owner)
    }
}
/// NSS scratch-buffer floor for the `_r` lookups: what sysconf
/// falls back to on platforms that report no hint.
const NSS_BUF_FLOOR: usize = 1024;

/// passwd lookup by name: uid, primary gid, home. The `_r` family
/// hands the NSS storage to the caller, so concurrent launches on
/// the server's blocking pool cannot tear one launch's copy while
/// another rewrites shared static state — the hazard the non-`_r`
/// calls carry.
pub(crate) fn passwd_by_name(name: &str) -> Option<ResolvedUser> {
    let name_c = CString::new(name).ok()?;
    let mut buf = vec![0u8; nss_buf_len()];
    loop {
        let mut pwd: std::mem::MaybeUninit<libc::passwd> = std::mem::MaybeUninit::uninit();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: `buf` is the caller-owned scratch the `_r`
        // contract requires; `pwd` is written by the callee before
        // `result` is checked, and nothing escapes `buf`'s lifetime.
        let rc = unsafe {
            libc::getpwnam_r(
                name_c.as_ptr(),
                pwd.as_mut_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        // SAFETY: on success the callee initialized `pwd` and pointed
        // `result` at it; the strings live in `buf`, alive to the end
        // of this scope where [`resolved_from_passwd`] copies out.
        return resolved_from_passwd(Some(unsafe { pwd.assume_init_ref() }));
    }
}

/// passwd lookup by uid (the nameless cross-user self form); same
/// `_r` discipline as [`passwd_by_name`].
pub(crate) fn passwd_by_uid(uid: u32) -> Option<ResolvedUser> {
    let mut buf = vec![0u8; nss_buf_len()];
    loop {
        let mut pwd: std::mem::MaybeUninit<libc::passwd> = std::mem::MaybeUninit::uninit();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: as passwd_by_name — caller-owned scratch, callee
        // writes `pwd` before `result` is checked.
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                pwd.as_mut_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        // SAFETY: as passwd_by_name — initialized on success, copied
        // out within `buf`'s lifetime.
        return resolved_from_passwd(Some(unsafe { pwd.assume_init_ref() }));
    }
}

fn nss_buf_len() -> usize {
    // SAFETY: a plain sysconf query with no precondition.
    let hint = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    if hint < 0 {
        NSS_BUF_FLOOR
    } else {
        hint as usize
    }
}

fn resolved_from_passwd(pw: Option<&libc::passwd>) -> Option<ResolvedUser> {
    let pw = pw?;
    if pw.pw_dir.is_null() {
        return None;
    }
    // SAFETY: passwd fields are NUL-terminated C strings.
    let home = unsafe { CStr::from_ptr(pw.pw_dir) }.to_bytes();
    let username = if pw.pw_name.is_null() {
        &[]
    } else {
        unsafe { CStr::from_ptr(pw.pw_name) }.to_bytes()
    };
    Some(ResolvedUser {
        uid: pw.pw_uid,
        gid: pw.pw_gid,
        username: OsStr::from_bytes(username).to_string_lossy().into_owned(),
        home: PathBuf::from(OsStr::from_bytes(home)),
    })
}

/// The instance's data directory: `<data home>/kallipai/tagmata/<slug>`.
/// In-place launches derive it from the daemon's XDG data home; drop-to
/// launches from the target user's passwd home. The exact path is
/// handed to the tagma at launch (`KALLIP_TAGMA_DATA_DIR`), replacing
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

/// Bash used for the login harvest: an explicit path, never `$SHELL`, so
/// the user's shell preference cannot swap the interpreter under us.
/// `KALLIP_HARVEST_BASH` lets a deployment name the path (NixOS has no
/// `/bin/bash`); it stays an administrative constant — nothing a
/// request or a user environment supplies can move it.
fn harvest_bash() -> PathBuf {
    harvest_bash_from(std::env::var_os("KALLIP_HARVEST_BASH").as_deref())
}

/// Pure core of [`harvest_bash`] so the resolution stays testable
/// without mutating process-global environment state in tests.
fn harvest_bash_from(explicit: Option<&std::ffi::OsStr>) -> PathBuf {
    explicit
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/bin/bash"))
}

/// Wall-clock budget one launch may spend harvesting; past it the shell is
/// killed and the launch degrades to the fallback PATH.
const HARVEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Ceiling on the harvested stream: a profile spamming output fails the
/// harvest instead of wedging the launch.
const HARVEST_MAX_BYTES: usize = 1024 * 1024;

/// Ceiling on a single harvested value: execve caps one argument or
/// environment string at MAX_ARG_STRLEN (128 KiB), and a longer value
/// would fail the helper spawn — the opposite of degrading. Half the
/// cap leaves room for the key and the rest of the launch arguments.
const HARVEST_MAX_VALUE_BYTES: usize = 64 * 1024;

/// The system tail of the fallback PATH used when harvesting fails.
const FALLBACK_PATH_TAIL: &str = ":/usr/local/bin:/usr/bin:/bin";

/// Why a harvest failed. Every variant degrades the launch to the fallback
/// PATH — a broken profile must not cost the instance its boot.
#[derive(Debug, thiserror::Error)]
enum HarvestError {
    #[error("harvest shell could not run: {0}")]
    Spawn(String),
    #[error("harvest shell exited with {0}")]
    Exit(std::process::ExitStatus),
    #[error("harvest shell exceeded the time budget")]
    Timeout,
    #[error("harvested stream exceeded the size cap")]
    TooLarge,
    #[error("harvested environment has no PATH key")]
    NoPath,
}

/// The minimal identity a login shell needs before it can find the user's
/// profile chain: HOME locates ~/.profile; USER/LOGNAME are what profile
/// scripts expect to see.
#[derive(Debug, Default)]
struct HarvestSeed {
    home: Option<OsString>,
    user: Option<OsString>,
}

impl HarvestSeed {
    /// Identity of the user the daemon itself runs as: the daemon's own
    /// environment first, the passwd entry for whatever key is missing.
    ///
    /// Invariant: in the same-uid profile the daemon environment *is* the
    /// executing user's environment, so seeding from it is
    /// self-description, not caller trust. A setuid form must instead
    /// seed (and run the whole harvest) inside the per-owner execution
    /// context — never from the requesting peer's environment.
    fn for_current_process() -> Self {
        let passwd = cached_passwd_identity();
        let home = std::env::var_os("HOME").or_else(|| passwd.as_ref().map(|(_, dir)| dir.clone()));
        let user = std::env::var_os("USER").or_else(|| passwd.map(|(name, _)| name));
        Self { home, user }
    }
    /// Identity of a drop-to target: everything comes from the passwd
    /// resolution, never from the daemon's environment — the daemon's
    /// HOME/USER describe the wrong user, and the requesting peer's
    /// environment is not trusted at all.
    fn for_target(target: &ResolvedUser) -> Self {
        Self {
            home: Some(target.home.clone().into_os_string()),
            user: Some(OsString::from(target.username.as_str())),
        }
    }
}

/// Cached passwd identity of the effective user: NSS storage is not
/// safe to touch from concurrent threads, and the daemon's euid
/// never changes during its lifetime — so read it once (through the
/// `_r` lookup, same as every launch-time resolution) and share the
/// snapshot.
pub(crate) fn cached_passwd_identity() -> Option<(OsString, OsString)> {
    static PASSWD: std::sync::OnceLock<Option<(OsString, OsString)>> = std::sync::OnceLock::new();
    PASSWD
        .get_or_init(|| {
            let user = passwd_by_uid(unsafe { libc::geteuid() })?;
            Some((OsString::from(user.username), user.home.into()))
        })
        .clone()
}

/// Run one login shell under a cleared environment (seeding only the
/// identity above) and capture the environment it computes. `env -0`
/// keeps multiline values intact; the NUL-separated stream is parsed
/// strictly — see [`parse_harvest_output`].
fn harvest_login_env(
    bash: &Path,
    seed: &HarvestSeed,
    timeout: Duration,
    run_as: Option<(u32, u32)>,
) -> Result<Vec<(String, String)>, HarvestError> {
    let mut command = std::process::Command::new(bash);
    command
        .args(["-l", "-c", "env -0"])
        // Own process group: a timeout must kill the whole tree — a
        // profile background job inherits the pipe and would otherwise
        // outlive the shell, holding the harvest open with it.
        .process_group(0)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if let Some(home) = &seed.home {
        command.env("HOME", home);
    }
    if let Some(user) = &seed.user {
        command.env("USER", user).env("LOGNAME", user);
    }
    if let Some((uid, gid)) = run_as {
        // The drop must include the supplementary-group wipe: a
        // profile chain is the target user's editable territory, and
        // std's own uid/gid application never calls setgroups — the
        // child would carry the daemon's groups into someone else's
        // profile. std also applies uid/gid BEFORE the pre_exec
        // closures (setgid, setuid, then closures), so a
        // closure-time setgroups would already be EPERM. The whole
        // drop therefore lives in one closure on an un-set Command:
        // wipe supplements, then gid, then uid — pure syscalls, this
        // runs post-fork. The exec'd grandchild (the real launch)
        // gets the full initgroups treatment inside the helper; the
        // harvest shell deliberately runs with the primary group
        // alone. A failed drop surfaces as a spawn error below.
        unsafe {
            command.pre_exec(move || {
                // SAFETY: direct syscalls on the forked child, nothing
                // allocated; a failed step must not reach exec.
                if libc::setgroups(0, std::ptr::null()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setgid(gid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setuid(uid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = command
        .spawn()
        .map_err(|e| HarvestError::Spawn(e.to_string()))?;

    // Drain stdout on its own thread: a profile writing more than the
    // pipe buffer would otherwise deadlock against the wait loop below.
    // The reader enforces the stream cap while reading — an endless
    // writer must not grow memory waiting for an EOF that never comes.
    let pipe = child.stdout.take().expect("piped stdout");
    let (is_done, reader_done) = std::sync::mpsc::channel::<()>();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe
            .take(HARVEST_MAX_BYTES as u64 + 1)
            .read_to_end(&mut buf);
        let _ = is_done.send(());
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_process_group(&mut child);
                    return Err(HarvestError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(HarvestError::Spawn(e.to_string())),
        }
    };
    if !status.success() {
        kill_process_group(&mut child);
        return Err(HarvestError::Exit(status));
    }
    // The shell is gone, but a profile background job may still hold
    // the pipe: wait for the reader inside the budget, never join
    // blindly — the join would block for the job's whole lifetime.
    let grace = deadline.max(Instant::now() + Duration::from_millis(200));
    if reader_done
        .recv_timeout(grace.saturating_duration_since(Instant::now()))
        .is_err()
    {
        kill_process_group(&mut child);
        return Err(HarvestError::Timeout);
    }
    let output = reader
        .join()
        .map_err(|_| HarvestError::Spawn("stdout reader panicked".into()))?;
    let parsed = parse_harvest_output(&output);
    if parsed.is_err() {
        // A capped stream means a background writer may still be alive
        // past the shell; every failure path leaves a clean process group.
        kill_process_group(&mut child);
    }
    parsed
}

/// Kill the whole harvest process group — the shell plus any profile
/// background job that inherited its stdio — and reap the shell. The
/// group id equals the shell's pid because the command spawns inside
/// `process_group(0)`. Safe to call once per failure path; killing a
/// dead group is a no-op ESRCH.
fn kill_process_group(child: &mut std::process::Child) {
    // SAFETY: kill(2) on a process group we created; errors (ESRCH on
    // an already-dead group) are meaningless here.
    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
}

/// Parse the NUL-separated stream `env -0` produced. Each token must be
/// a well-formed KEY=VALUE with a legal key name; anything else — the
/// empty tail, profile stdout glued onto the first token, stray output —
/// is dropped with a warning. Later duplicates win, matching shell
/// `export` semantics. A stream without a PATH key counts as total
/// failure so the caller falls back rather than launching a blank-PATH
/// instance.
fn parse_harvest_output(bytes: &[u8]) -> Result<Vec<(String, String)>, HarvestError> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    if bytes.len() > HARVEST_MAX_BYTES {
        return Err(HarvestError::TooLarge);
    }
    for token in bytes.split(|&b| b == 0) {
        if token.is_empty() {
            continue;
        }
        let token = String::from_utf8_lossy(token);
        let Some((key, value)) = token.split_once('=') else {
            tracing::warn!(%token, "harvest: dropping token without KEY=VALUE shape");
            continue;
        };
        if !valid_env_key(key) {
            tracing::warn!(%key, "harvest: dropping token with a malformed key");
            continue;
        }
        if value.len() > HARVEST_MAX_VALUE_BYTES {
            tracing::warn!(key = %key, "harvest: dropping oversized value");
            continue;
        }
        if key == "PATH" && value.is_empty() {
            // Defensive: an empty PATH is as unusable as a missing one,
            // and the explicit channel rejects empty values the same way.
            tracing::warn!("harvest: empty PATH value treated as missing");
            continue;
        }
        if let Some(slot) = pairs.iter_mut().find(|(k, _)| k == key) {
            slot.1 = value.to_owned();
        } else {
            pairs.push((key.to_owned(), value.to_owned()));
        }
    }
    if pairs.iter().any(|(k, _)| k == "PATH") {
        Ok(pairs)
    } else {
        Err(HarvestError::NoPath)
    }
}

/// `[A-Za-z_][A-Za-z0-9_]*` — the key shape every consumer of these pairs
/// assumes; anything else means pollution, not environment.
fn valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some('a'..='z' | 'A'..='Z' | '_'))
        && chars.all(|c| matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_'))
}

/// The degraded PATH for a failed harvest: the daemon's own binary
/// directory (where the helper and tagma resolve from) plus the
/// conventional system locations.
fn fallback_path(bin_dir: Option<&Path>) -> String {
    match bin_dir.filter(|dir| !dir.as_os_str().is_empty()) {
        Some(dir) => format!("{}{FALLBACK_PATH_TAIL}", dir.display()),
        None => FALLBACK_PATH_TAIL.trim_start_matches(':').to_owned(),
    }
}

/// The launch env base: the harvested login environment, or — on any
/// harvest failure — the fallback PATH. Always a usable base: harvesting
/// is a best-effort upgrade, never a launch gate.
fn harvest_base_env(
    fallback_bin_dir: Option<&Path>,
    identity: &LaunchIdentity,
) -> Vec<(String, String)> {
    let (seed, run_as) = match identity {
        LaunchIdentity::InPlace { .. } => (HarvestSeed::for_current_process(), None),
        LaunchIdentity::DropTo(user) => (HarvestSeed::for_target(user), Some((user.uid, user.gid))),
    };
    match harvest_login_env(&harvest_bash(), &seed, HARVEST_TIMEOUT, run_as) {
        Ok(pairs) => pairs,
        Err(error) => {
            tracing::warn!(%error, "login environment harvest failed; using fallback PATH");
            degrade_to_fallback(fallback_bin_dir)
        }
    }
}

/// The degraded base for a failed harvest: just the fallback PATH.
/// Extracted so the Err→fallback→compose chain carries a direct test
/// (the surrounding harvest_base_env shells out and cannot).
fn degrade_to_fallback(fallback_bin_dir: Option<&Path>) -> Vec<(String, String)> {
    vec![("PATH".to_owned(), fallback_path(fallback_bin_dir))]
}

/// Compose the instance env from a map, one value per key:
///
/// * the base (harvest, or fallback PATH) supplies every key it has;
/// * explicit request pairs win per key over the base;
/// * the daemon-owned keys win over everything — a polluted profile
///   cannot smuggle them in;
/// * `RUST_LOG=info` appears only when neither base nor explicit pair
///   supplied one (map composition ends duplicate-key shadowing);
/// * `KALLIP_TAGMA_ADDR` defaults to `127.0.0.1:0` when neither base nor
///   explicit pair supplies one — the one user-set listen knob;
/// * the data-dir handoff and the state anchor land last — daemon-owned
///   like the slug, they pin where the instance's data root and logs
///   resolve. The dedicated form additionally pins the whole XDG tree
///   (HOME, XDG_CONFIG_HOME/XDG_DATA_HOME/XDG_STATE_HOME, the runtime
///   dir when it exists) to the target user's home — daemon-owned like
///   the rest, so neither a polluted profile nor a request pair can
///   describe a foreign home into the instance.
fn compose_launch_env(
    base: &[(String, String)],
    user_env: &[String],
    slug: &str,
    workspace_canon: &Path,
    data_dir: &Path,
    state_home: Option<&Path>,
    target: Option<&ResolvedUser>,
) -> Vec<String> {
    let mut env: BTreeMap<&str, String> = BTreeMap::new();
    for (key, value) in base {
        env.insert(key, value.clone());
    }
    for pair in user_env {
        // Shape, emptiness and key ownership are validated before launch
        // (spawn validates the request; start re-validates the persisted
        // copy) — a pair without `=` here would be a bug elsewhere.
        if let Some((key, value)) = pair.split_once('=') {
            env.insert(key, value.to_owned());
        }
    }
    env.insert("KALLIP_TAGMA_SLUG", slug.to_owned());
    env.insert(
        "KALLIP_WORKSPACE_ROOT",
        workspace_canon.display().to_string(),
    );
    env.entry("KALLIP_TAGMA_ADDR")
        .or_insert_with(|| "127.0.0.1:0".to_owned());
    env.insert("KALLIP_TAGMA_DATA_DIR", data_dir.display().to_string());
    if let Some(state) = state_home {
        env.insert("XDG_STATE_HOME", state.display().to_string());
    }
    if let Some(user) = target {
        env.insert("HOME", user.home.display().to_string());
        env.insert("USER", user.username.clone());
        env.insert("LOGNAME", user.username.clone());
        env.insert(
            "XDG_CONFIG_HOME",
            user.home.join(".config").display().to_string(),
        );
        env.insert(
            "XDG_DATA_HOME",
            user.home.join(".local/share").display().to_string(),
        );
        env.insert(
            "XDG_STATE_HOME",
            user.home.join(".local/state").display().to_string(),
        );
        // linger (declared in the system deployment) is what guarantees
        // this directory; a dev host without it just omits the key —
        // the instance binds TCP and never reads XDG_RUNTIME_DIR.
        let runtime = Path::new("/run/user").join(user.uid.to_string());
        if runtime.is_dir() {
            env.insert("XDG_RUNTIME_DIR", runtime.display().to_string());
        }
    }
    env.entry("RUST_LOG").or_insert_with(|| "info".to_owned());
    env.into_iter().map(|(k, v)| format!("{k}={v}")).collect()
}

/// The addr key is user-set, and the base (harvest) channel can carry it
/// past request validation (`validate_user_env` never sees the base): the
/// composed launch env is the last choke point, so a malformed pin from
/// any channel dies here instead of at tagma bind time.
fn ensure_addr_parses(env: &[String]) -> Result<(), SpawnError> {
    let Some(pair) = env.iter().find(|p| p.starts_with("KALLIP_TAGMA_ADDR=")) else {
        return Ok(());
    };
    let value = pair.split_once('=').map(|(_, v)| v).unwrap_or_default();
    if value.parse::<std::net::SocketAddr>().is_err() {
        return Err(SpawnError::Invalid(format!(
            "env key \"KALLIP_TAGMA_ADDR\" must be a SocketAddr (got {value:?})"
        )));
    }
    Ok(())
}

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
    // env signaling relay intent (any `KALLIP_TAGMA_RELAY_*` entry) gets
    // missing URL filled from the daemon's own environment; explicit
    // values pass through; an unset -- or empty (the kallip-runtime's
    // persistence.rs `filter(!is_empty)` precedent) -- daemon value
    // means unconfigured:
    // nothing is filled for that URL. This runs before the record
    // snapshot is written, so the record stores the filled env: a restart
    // replays relay-complete env, and the fill cannot be lost between a
    // successful spawn and the first restart (the local-only detection
    // only covers the request moment).
    let user_env = fill_relay_defaults(
        user_env,
        daemon_relay_url("KALLIP_DAEMON_RELAY_ARCHEION_URL").as_deref(),
        daemon_relay_url("KALLIP_DAEMON_RELAY_LESCHE_URL").as_deref(),
    );
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
        return Err(SpawnError::Overlap {
            requested: workspace.to_string(),
            existing_slug: "(instance tree)".into(),
            existing_workspace: data_root.display().to_string(),
        });
    }
    for instance in scan::scan_instances(record_root) {
        if let Some(existing) = instance.workspace {
            let existing_path = PathBuf::from(&existing);
            if overlaps(&workspace_canon, &existing_path) {
                return Err(SpawnError::Overlap {
                    requested: workspace.to_string(),
                    existing_slug: instance.slug,
                    existing_workspace: existing,
                });
            }
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

/// One path contains the other (ancestor/descendant), including equality.
/// Non-canonical `b` still compares correctly when it is a prefix/suffix
/// match of the canonical `a` only in pathological trees; existing
/// workspaces were canonicalized when written.
/// The daemon-side relay-URL defaults (read once per spawn from this
/// process's environment; the unit env carries them). An unset -- or
/// empty, aligned with the kallip-runtime's persistence.rs
/// `filter(!is_empty)` precedent
/// so a stray empty value cannot silently switch the fill off -- value
/// counts as unconfigured.
fn daemon_relay_url(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Fill the relay-URL entries for a relay-intent request env.
///
/// Fires only when the env signals relay intent (any
/// `KALLIP_TAGMA_RELAY_*` entry): each URL key survives exactly
/// once, with its explicit value or -- when absent and the daemon has
/// one configured -- the default. A URL the daemon has not configured
/// is left untouched (an explicit empty entry then dies in
/// `validate_user_env`, the same fail-loud end an empty default
/// reached before the fill moved here); no relay signal at all
/// returns the env unchanged -- local-only spawns must not carry a
/// URL the tagma boot would fail on.
fn fill_relay_defaults(
    env: &[String],
    archeion: Option<&str>,
    lesche: Option<&str>,
) -> Vec<String> {
    let mut env = env.to_vec();
    if !env.iter().any(|e| e.starts_with("KALLIP_TAGMA_RELAY_")) {
        return env;
    }
    fill_one(&mut env, "KALLIP_TAGMA_RELAY_ARCHEION_URL=", archeion);
    fill_one(&mut env, "KALLIP_TAGMA_RELAY_LESCHE_URL=", lesche);
    env
}

/// One entry per key survives: the first explicit value wins
/// outright, an empty or absent key is filled with the configured
/// default (None: left as-is), and every duplicate is dropped.
/// Ordering must stay irrelevant downstream -- the spawn helper passes
/// duplicate pairs straight to execve, and the daemon's env validation
/// rejects empty values -- so collapsing here is the one place that
/// keeps both properties airtight.
fn fill_one(env: &mut Vec<String>, prefix: &str, default: Option<&str>) {
    let explicit = env
        .iter()
        .any(|e| e.strip_prefix(prefix).is_some_and(|v| !v.is_empty()));
    let mut kept = false;
    env.retain(|e| {
        let Some(v) = e.strip_prefix(prefix) else {
            return true;
        };
        if kept || (explicit && v.is_empty()) {
            return false;
        }
        kept = true;
        true
    });
    if explicit {
        return;
    }
    let Some(default) = default else {
        return;
    };
    match env.iter_mut().find(|e| e.starts_with(prefix)) {
        Some(slot) => *slot = format!("{prefix}{default}"),
        None => env.push(format!("{prefix}{default}")),
    }
}

pub(crate) fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Request env pairs: KEY=VALUE shape, KALLIP_* (plus RUST_LOG and PATH —
/// both normally arrive via the login harvest; an explicit pair wins),
/// none of the daemon-owned keys. Shared by spawn (fresh request
/// env) and start (re-validating the persisted copy against hand-edited
/// records).
pub(crate) fn validate_user_env(user_env: &[String]) -> Result<(), SpawnError> {
    for pair in user_env {
        let Some((key, value)) = pair.split_once('=') else {
            return Err(SpawnError::Invalid(format!(
                "env arg {pair:?} is not KEY=VALUE"
            )));
        };
        if RESERVED_KEYS.contains(&key) {
            return Err(SpawnError::Invalid(format!(
                "env key {key} is set by the daemon and cannot be overridden"
            )));
        }
        if !(key.starts_with("KALLIP_") || key == "RUST_LOG" || key == "PATH") {
            return Err(SpawnError::Invalid(format!(
                "env key {key:?} is not allowlisted (KALLIP_*, RUST_LOG, or PATH)"
            )));
        }
        // The addr key is user-set: shape-check it here
        // so a bad value dies at request time, not at tagma bind time.
        if key == "KALLIP_TAGMA_ADDR" && value.parse::<std::net::SocketAddr>().is_err() {
            return Err(SpawnError::Invalid(format!(
                "env key {key:?} must be a SocketAddr (got {value:?})"
            )));
        }
        if value.is_empty() {
            return Err(SpawnError::Invalid(format!(
                "env arg {pair:?} has an empty value"
            )));
        }
    }
    Ok(())
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
    let helper = bins::resolve("kallip-daemon-spawn");
    // The daemon spawns the helper itself through Command, which
    // searches PATH — a bare-name helper rides the caller's PATH
    // (the unit's `path` in the nix deployment). The tagma is
    // different: the helper execve's it without a PATH search, so
    // its resolution gets the file gate below.
    // Explicit dev exe wins over the resolved one; the resolver stays
    // the production path (KALLIP_BIN_DIR, then CARGO_BIN_EXE_* in
    // tests).
    let tagma = match exe {
        Some(exe) => PathBuf::from(exe),
        None => bins::resolve("kallip-tagma"),
    };
    require_resolved_binary(
        &tagma,
        "kallip-tagma",
        "set KALLIP_BIN_DIR to the bin directory of the installed kallip package, or pass --bin with an explicit path",
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
fn clear_stale_runtime(data_dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(data_dir.join("runtime.json")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- clear_stale_runtime ----------------------------------------------

    #[test]
    fn clear_stale_runtime_is_ok_when_no_file_exists() {
        let dir = tempdir();
        clear_stale_runtime(dir.path()).expect("absent file is the clean state");
    }

    #[test]
    fn clear_stale_runtime_removes_a_leftover() {
        let dir = tempdir();
        std::fs::write(dir.path().join("runtime.json"), b"{}").expect("write leftover");
        clear_stale_runtime(dir.path()).expect("leftover removes");
        assert!(!dir.path().join("runtime.json").exists());
    }

    #[test]
    fn clear_stale_runtime_fails_when_instance_dir_is_not_a_directory() {
        let dir = tempdir();
        let not_a_dir = dir.path().join("file");
        std::fs::write(&not_a_dir, b"x").expect("write file");
        assert!(clear_stale_runtime(&not_a_dir).is_err());
    }

    /// Write an executable `#!/bin/bash` script and return its path. The
    /// body must stick to bash builtins: the shim runs under the
    /// harvest's cleared env, where no PATH exists to resolve external
    /// binaries (the first draft's `sleep 30` failed with exit 127).
    /// Exec it via harvest_shim, never harvest_login_env directly: a
    /// just-written script sits in the close-to-exec ETXTBSY window
    /// (see there).
    fn shim(dir: &Path, body: &str) -> PathBuf {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("shim");
        let mut script = std::fs::File::create(&path).expect("create shim");
        writeln!(script, "#!/bin/bash").expect("write shebang");
        writeln!(script, "{body}").expect("write body");
        drop(script);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod shim");
        path
    }

    /// Run the harvest against a just-written shim. Exec of a file
    /// whose write handle closed microseconds ago races the kernel's
    /// deferred release of that handle: ETXTBSY with no writer left
    /// holding the path (the same probe-verified window behind
    /// adopt's fake-tagma retry: filesystem-independent, and it
    /// fires without any concurrent writer). The window is
    /// transient and every legitimate outcome is stable, so a
    /// bounded retry reruns the whole call.
    /// The 10-attempt bound is this helper's own conservative
    /// ceiling (its callers assert elapsed time); adopt's fake
    /// tagma picks 50 for the same window.
    fn harvest_shim(
        bash: &Path,
        seed: &HarvestSeed,
        timeout: Duration,
        run_as: Option<(u32, u32)>,
    ) -> Result<Vec<(String, String)>, HarvestError> {
        for _ in 0..10 {
            match harvest_login_env(bash, seed, timeout, run_as) {
                // Spawn(String) flattens the io error to text, so
                // match the message: the process never setlocale(3)s,
                // glibc/musl wording is stable, and a missed match
                // degrades into the test's explicit assertion
                // failure, not a silent flake.
                Err(HarvestError::Spawn(message)) if message.contains("Text file busy") => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                outcome => return outcome,
            }
        }
        panic!(
            "harvest shim still ETXTBSY after 10 attempts: {}",
            bash.display()
        );
    }

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// The login bash the harvest hardcodes; `None` skips the tests that
    /// need a real shell. Probed by exec, not by stat: under a landlock
    /// sandbox the interpreter is executable while being unstattable.
    fn bash() -> Option<PathBuf> {
        std::process::Command::new("/bin/bash")
            .arg("-c")
            .arg("true")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
            .then(|| PathBuf::from("/bin/bash"))
    }

    /// Generous harvest budget for the behavior tests. The production
    /// HARVEST_TIMEOUT (2 s) measures login init on a quiet host; a loaded
    /// test runner can spend it there alone, which would map the asserted
    /// outcomes to Timeout. The timeout-behavior test uses its own tight
    /// 300 ms budget on purpose -- it asserts Timeout itself.
    const HARVEST_TEST_BUDGET: Duration = Duration::from_secs(30);
    // --- parse_harvest_output ------------------------------------------

    #[test]
    fn parse_takes_a_clean_nul_stream() {
        let pairs = parse_harvest_output(b"PATH=/bin:/usr/bin\0HOME=/home/u\0")
            .expect("clean stream parses");
        assert_eq!(pairs[0], ("PATH".to_owned(), "/bin:/usr/bin".to_owned()));
        assert_eq!(pairs[1], ("HOME".to_owned(), "/home/u".to_owned()));
    }

    #[test]
    fn parse_drops_pollution_glued_onto_the_path_token() {
        // Profile stdout lands before `env -0` output with no separator:
        // the PATH token loses its key shape, and a lost PATH must count
        // as total failure (the caller falls back) — never a partial env.
        let bytes = b"banner text\nPATH=/bin\0HOME=/home/u\0";
        assert!(matches!(
            parse_harvest_output(bytes),
            Err(HarvestError::NoPath)
        ));
    }

    #[test]
    fn parse_drops_stray_tokens_but_keeps_valid_ones() {
        let bytes = b"PATH=/bin\0not a pair\0HOME=/home/u\0trailing junk";
        let pairs = parse_harvest_output(bytes).expect("valid keys survive");
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["PATH", "HOME"]);
    }

    #[test]
    fn parse_rejects_malformed_key_shapes() {
        for bad in [
            "=v\0PATH=/bin\0",
            "1KEY=v\0PATH=/bin\0",
            "KEY.B=v\0PATH=/bin\0",
        ] {
            let pairs = parse_harvest_output(bad.as_bytes()).expect("PATH present");
            let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
            assert_eq!(keys, ["PATH"], "only well-formed keys survive");
        }
    }

    #[test]
    fn parse_last_duplicate_wins() {
        let pairs = parse_harvest_output(b"PATH=/one\0PATH=/two\0").expect("parses");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].1, "/two");
    }

    #[test]
    fn parse_empty_stream_is_no_path() {
        assert!(matches!(
            parse_harvest_output(b""),
            Err(HarvestError::NoPath)
        ));
    }

    // --- valid_env_key -------------------------------------------------

    #[test]
    fn env_key_shape() {
        for good in ["PATH", "KALLIP_X", "_x", "a1_"] {
            assert!(valid_env_key(good), "{good}");
        }
        for bad in ["", "1A", "A-B", "A.B", "A B"] {
            assert!(!valid_env_key(bad), "{bad}");
        }
    }

    // --- harvest_login_env (subprocess) -------------------------------

    #[test]
    fn harvest_reads_the_seeded_profile_chain() {
        let Some(bash) = bash() else {
            eprintln!("skip: no /bin/bash on this host");
            return;
        };
        let home = tempdir();
        std::fs::write(
            home.path().join(".profile"),
            "export KALLIP_UNIT_MARKER=green\nexport PATH=/fixture/bin:$PATH\n",
        )
        .expect("write profile");
        let seed = HarvestSeed {
            home: Some(home.path().as_os_str().to_owned()),
            user: Some("probe".into()),
        };
        let pairs = harvest_login_env(&bash, &seed, HARVEST_TEST_BUDGET, None).expect("harvest ok");
        let marker = pairs.iter().find(|(k, _)| k == "KALLIP_UNIT_MARKER");
        assert_eq!(marker.map(|(_, v)| v.as_str()), Some("green"));
        let path = pairs.iter().find(|(k, _)| k == "PATH").expect("PATH");
        assert!(
            path.1.starts_with("/fixture/bin"),
            "fixture PATH applied: {}",
            path.1
        );
    }

    #[test]
    fn harvest_shell_failure_maps_to_exit_error() {
        let Some(_) = bash() else {
            eprintln!("skip: no /bin/bash on this host");
            return;
        };
        let dir = tempdir();
        let bash = shim(dir.path(), "exit 3");
        let error =
            harvest_shim(&bash, &HarvestSeed::default(), HARVEST_TEST_BUDGET, None).unwrap_err();
        assert!(matches!(error, HarvestError::Exit(_)));
    }

    #[test]
    fn harvest_timeout_kills_the_shell_and_returns_promptly() {
        let Some(_) = bash() else {
            eprintln!("skip: no /bin/bash on this host");
            return;
        };
        let dir = tempdir();
        let bash = shim(dir.path(), "while :; do :; done");
        let start = Instant::now();
        let error = harvest_shim(
            &bash,
            &HarvestSeed::default(),
            Duration::from_millis(300),
            None,
        )
        .unwrap_err();
        assert!(matches!(error, HarvestError::Timeout));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "returned promptly"
        );
    }

    #[test]
    fn harvest_missing_interpreter_maps_to_spawn_error() {
        let error = harvest_login_env(
            Path::new("/nonexistent/bash"),
            &HarvestSeed::default(),
            HARVEST_TIMEOUT,
            None,
        )
        .unwrap_err();
        assert!(matches!(error, HarvestError::Spawn(_)));
    }

    // --- authorization + record collision -------------------------------

    #[test]
    fn authorization_passes_the_target_user_and_root_only() {
        let uid = unsafe { libc::getuid() };
        assert!(authorized(uid, uid), "the target user itself passes");
        assert!(authorized(0, uid), "root passes (the admin exemption)");
        assert!(!authorized(uid + 1, uid), "a foreign uid is denied");
    }

    /// The delegate grant, pure: a listed peer may act for a declared
    /// user (any target outside the grant), a foreign peer naming
    /// others is denied, the self and root arms stay untouched, and
    /// an empty grant is the status quo. The full-stack path with a
    /// real non-root delegate peer is not exercisable in this sandbox
    /// (the test process is root, and root passes by the admin
    /// exemption before delegates are consulted).
    #[test]
    fn delegate_grant_extends_the_authorization_matrix() {
        use std::collections::HashSet as Set;
        let delegates: Set<u32> = [61000u32, 61001].into_iter().collect();
        let uid = 62000u32;
        // A delegate peer may act for a declared user.
        assert!(
            authorized_with(&delegates, 61000, uid),
            "delegate acts for a declared user"
        );
        // The grant is per-delegate, not universal.
        assert!(
            !authorized_with(&delegates, uid, 62001),
            "a foreign uid is denied"
        );
        // Self and root arms are untouched by the grant.
        assert!(
            authorized_with(&delegates, 61000, 61000),
            "self still passes"
        );
        assert!(authorized_with(&delegates, 0, uid), "root still passes");
        // An empty grant is the status quo.
        assert!(
            !authorized_with(&Set::new(), 61000, uid),
            "empty set denies"
        );
    }

    /// Each delegate refusal is independently falsifiable: deleting
    /// any single check turns exactly its assert red. Pure inputs
    /// only — the in-process grant is empty in tests, so the arms
    /// drive the set directly. The keyless arm covers both the spawn
    /// resolver and the adopt inference, which share the check.
    #[test]
    fn delegate_refusal_arms_are_independently_falsifiable() {
        use std::collections::HashSet as Set;
        let delegates: Set<u32> = [61000u32].into_iter().collect();
        let uid = 62000u32;

        // Keyless form (spawn resolver + adopt inference).
        let err = delegate_keyless_rejection(&delegates, 61000).unwrap();
        assert!(
            err.to_string().contains("name the launch user explicitly"),
            "{err}"
        );
        assert!(
            delegate_keyless_rejection(&delegates, uid).is_none(),
            "non-delegates keep the keyless form"
        );
        assert!(
            delegate_keyless_rejection(&Set::new(), 61000).is_none(),
            "an empty grant keeps it too"
        );

        // Target arms: root, the daemon's own account, another delegate.
        let err = delegate_target_rejection(&delegates, 61000, 0, uid).unwrap();
        assert!(err.to_string().contains("as root"), "{err}");
        let err = delegate_target_rejection(&delegates, 61000, uid, uid).unwrap();
        assert!(err.to_string().contains("own account"), "{err}");
        let err = delegate_target_rejection(&delegates, 61000, 61000, uid).unwrap();
        assert!(err.to_string().contains("another delegate"), "{err}");
        // A declared user outside the grant passes.
        assert!(
            delegate_target_rejection(&delegates, 61000, uid, 0).is_none(),
            "a declared non-root target passes"
        );
        // Non-delegate peers skip the target checks entirely.
        assert!(
            delegate_target_rejection(&delegates, uid, 0, uid).is_none(),
            "the checks are delegate-gated"
        );
    }

    /// The full inference matrix, pure: divergence refuses, agreeing
    /// foreign owners drop to the data dir's owner, the daemon's own
    /// owner stays in place. The adopt wiring maps these onto passwd
    /// resolution and the root gate; real-owner coverage would need
    /// chown privileges a test host cannot assume.
    #[test]
    fn diverging_path_owners_refuse_the_inferred_default() {
        assert!(matches!(
            infer_owner_decision(1000, 1001, 1000),
            OwnerInference::Divergent
        ));
    }

    #[test]
    fn agreeing_foreign_owners_infer_the_drop_to_target() {
        assert_eq!(
            infer_owner_decision(65534, 65534, 1000),
            OwnerInference::DropTo(65534)
        );
    }

    #[test]
    fn the_daemons_own_owner_collapses_to_in_place() {
        assert_eq!(
            infer_owner_decision(1000, 1000, 1000),
            OwnerInference::InPlace
        );
    }

    #[test]
    fn a_non_file_resolution_refuses_with_an_actionable_error() {
        let error = require_resolved_binary(
            Path::new("definitely-not-a-real-kallip-binary"),
            "kallip-tagma",
            "set KALLIP_BIN_DIR, or pass --bin",
        )
        .expect_err("that path does not exist");
        let SpawnError::Invalid(message) = error else {
            panic!("expected Invalid, got {error}")
        };
        assert!(message.contains("KALLIP_BIN_DIR"), "{message}");
    }
    #[test]
    fn resolve_defaults_to_the_peer_and_collapses_to_in_place() {
        let peer = unsafe { libc::geteuid() };
        // A root daemon refuses the inferred self-form (the guard fires
        // below); the collapse stays observable on non-root dev hosts.
        if peer == 0 {
            let error = resolve_launch_identity(None, peer).unwrap_err();
            assert!(error.to_string().contains("--user"), "{error}");
            return;
        }
        let identity = resolve_launch_identity(None, peer).expect("resolves");
        assert_eq!(
            identity,
            LaunchIdentity::InPlace {
                uid: peer,
                username: cached_passwd_identity()
                    .map(|(name, _)| name.to_string_lossy().into_owned()),
            }
        );
    }

    #[test]
    fn resolve_rejects_a_cross_user_self_form_without_root() {
        if unsafe { libc::geteuid() } == 0 {
            // The suite runs as root: the drop is possible, the refusal
            // does not apply.
            return;
        }
        let identity = resolve_launch_identity(None, unsafe { libc::geteuid() } + 1);
        assert!(
            identity.is_err(),
            "a non-root daemon refuses a foreign self-form instead of failing late"
        );
    }

    #[test]
    fn resolve_rejects_an_unknown_user_name() {
        let error = resolve_launch_identity(Some("no-such-kallip-user-xyz"), 0).unwrap_err();
        assert!(matches!(error, SpawnError::Invalid(_)));
    }

    #[test]
    fn resolve_with_the_daemon_user_collapses_to_in_place() {
        let name = cached_passwd_identity().map(|(name, _)| name.to_string_lossy().into_owned());
        let name = name.expect("test host has a passwd entry");
        let identity =
            resolve_launch_identity(Some(&name), unsafe { libc::geteuid() }).expect("resolves");
        assert!(matches!(identity, LaunchIdentity::InPlace { .. }));
    }

    #[test]
    fn dedicated_env_pins_the_target_home_daemon_owned() {
        let target = ResolvedUser {
            uid: 4242,
            gid: 4242,
            username: "kallip-team".into(),
            home: PathBuf::from("/home/kallip-team"),
        };
        let env = compose_launch_env(
            &base_env(),
            &[
                "HOME=/polluted".to_owned(),
                "XDG_STATE_HOME=/polluted/state".to_owned(),
            ],
            "i1",
            Path::new("/ws"),
            Path::new("/data/i1"),
            None,
            Some(&target),
        );
        assert_eq!(
            get(&env, "HOME"),
            Some("/home/kallip-team"),
            "daemon-owned wins"
        );
        assert_eq!(get(&env, "USER"), Some("kallip-team"));
        assert_eq!(get(&env, "LOGNAME"), Some("kallip-team"));
        assert_eq!(
            get(&env, "XDG_CONFIG_HOME"),
            Some("/home/kallip-team/.config")
        );
        assert_eq!(
            get(&env, "XDG_DATA_HOME"),
            Some("/home/kallip-team/.local/share")
        );
        assert_eq!(
            get(&env, "XDG_STATE_HOME"),
            Some("/home/kallip-team/.local/state")
        );
        // The runtime dir appears only when the host actually has it
        // (linger supplies it in the system form); either way the
        // instance never sees a polluted value.
        if let Some(runtime) = get(&env, "XDG_RUNTIME_DIR") {
            assert_eq!(runtime, "/run/user/4242");
        }
    }

    #[test]
    fn in_place_env_gains_no_identity_keys() {
        // The same-uid form is pinned: the pre-dedicated env shape —
        // no HOME/XDG/USER injection — stays byte-identical.
        let env = compose_launch_env(
            &base_env(),
            &[],
            "i1",
            Path::new("/ws"),
            Path::new("/data/i1"),
            Some(Path::new("/state")),
            None,
        );
        // HOME may ride through from the harvest base (a profile export,
        // not a daemon injection); the identity keys must not appear.
        assert_eq!(get(&env, "USER"), None);
        assert_eq!(get(&env, "LOGNAME"), None);
        assert_eq!(get(&env, "XDG_CONFIG_HOME"), None);
    }
    #[test]
    fn harvest_bash_override_wins_and_default_survives() {
        // The deployment constant: an explicit KALLIP_HARVEST_BASH path
        // (NixOS store bash) replaces /bin/bash; with no override the
        // historical default must stay.
        assert_eq!(
            harvest_bash_from(Some(std::ffi::OsStr::new("/nix/store/x/bash"))),
            PathBuf::from("/nix/store/x/bash")
        );
        assert_eq!(harvest_bash_from(None), PathBuf::from("/bin/bash"));
    }
    #[test]
    fn harvest_privilege_drop_runs_end_to_end_and_wipes_supplements() {
        // Root-only (the drop needs real setuid; the suite's host
        // convention). `nobody` is a real passwd entry, so the
        // target resolution is genuine. Two properties: the prod
        // drop closure executes inside a real harvest, and the
        // dropped child carries the primary group alone — no
        // supplementary groups survive the wipe.
        if unsafe { libc::getuid() } != 0 {
            return;
        }
        let Some(user) = passwd_by_uid(65534) else {
            return; // no `nobody` on this host
        };
        let seed = HarvestSeed::for_target(&user);
        // Capability probe first: a sandboxed suite host may lack
        // setgid/setuid reach over its user namespace (a documented
        // limitation — the reviewer probes hit the same wall). A
        // host that cannot drop proves nothing here; skip.
        let mut probe = std::process::Command::new(harvest_bash());
        probe
            .args(["-c", "id -Gn"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped());
        let (uid, gid) = (user.uid, user.gid);
        // SAFETY: same drop sequence as harvest_login_env's run_as
        // arm, applied to a throwaway probe; the child only runs
        // `id -Gn`.
        unsafe {
            probe.pre_exec(move || {
                if libc::setgroups(0, std::ptr::null()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setgid(gid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setuid(uid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let Ok(output) = probe.output() else {
            return; // the sandbox cannot drop; a real host can
        };
        let groups = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            groups.trim(),
            "nogroup",
            "supplementary groups must be wiped: {groups}"
        );
        let pairs = harvest_login_env(
            &harvest_bash(),
            &seed,
            HARVEST_TIMEOUT,
            Some((user.uid, user.gid)),
        )
        .expect("harvest through the privilege drop");
        assert!(
            pairs.iter().any(|(k, v)| k == "PATH" && !v.is_empty()),
            "dropped harvest must still produce a usable PATH"
        );
    }

    #[test]
    fn spawn_refuses_a_slug_already_in_the_record_area() {
        // Collision is a registry verdict: an existing record refuses
        // the spawn even though no data directory exists.
        let root = tempdir();
        let data = tempdir();
        let record = records::InstanceRecord {
            instance_id: "instance-1".into(),
            owner_uid: unsafe { libc::getuid() },
            target_uid: unsafe { libc::getuid() },
            target_username: None,
            workspace: None,
            env: Vec::new(),
            identity: None,
            data_dir: data.path().to_path_buf(),
        };
        records::write_record(root.path(), "taken", &record).expect("write record");
        // An explicit user skips the root-daemon inference guard — this
        // test's subject is the collision verdict, not the identity.
        let user = cached_passwd_identity()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .expect("test host has a passwd entry");
        let error = spawn(
            root.path(),
            "taken",
            ".",
            &[],
            None,
            Duration::from_secs(1),
            unsafe { libc::getuid() },
            Some(&user),
        )
        .unwrap_err();
        assert!(matches!(error, SpawnError::SlugTaken(_)), "{error}");
    }

    // --- spawn request validation -------------------------------------
    #[test]
    fn a_missing_dev_exe_is_rejected_at_request_time() {
        // The exe gate fires before any filesystem work: no tree, no
        // helper, no 30s wait - the request just fails.
        let error = spawn(
            Path::new("."),
            "valid-slug",
            ".",
            &[],
            Some("/nonexistent/kallip-tagma"),
            Duration::from_secs(1),
            unsafe { libc::getuid() },
            None,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("not an existing executable"),
            "{error}"
        );
    }

    #[test]
    fn exe_runnable_checks_file_and_exec_bit() {
        assert!(exe_runnable("/bin/sh"), "system shell is runnable");
        assert!(!exe_runnable("/nonexistent/binary"));
    }

    /// A base with every interesting collision in it.
    fn base_env() -> Vec<(String, String)> {
        vec![
            ("PATH".into(), "/harvested/path".into()),
            ("HOME".into(), "/home/harvest".into()),
            ("RUST_LOG".into(), "harvest-level".into()),
            ("EDITOR".into(), "vi".into()),
        ]
    }

    fn get<'a>(env: &'a [String], key: &str) -> Option<&'a str> {
        env.iter()
            .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
    }

    #[test]
    fn compose_explicit_pairs_win_per_key() {
        let env = compose_launch_env(
            &base_env(),
            &["PATH=/explicit".into(), "KALLIP_X=1".into()],
            "i1",
            Path::new("/ws"),
            Path::new("/data/i1"),
            Some(Path::new("/state")),
            None,
        );
        assert_eq!(get(&env, "PATH"), Some("/explicit"));
        assert_eq!(get(&env, "KALLIP_X"), Some("1"));
        assert_eq!(get(&env, "HOME"), Some("/home/harvest"), "harvest key kept");
        assert_eq!(get(&env, "EDITOR"), Some("vi"), "harvest key kept");
    }

    #[test]
    fn compose_daemon_keys_win_over_a_polluted_base() {
        let env = compose_launch_env(
            &base_env(),
            &[],
            "i1",
            Path::new("/ws"),
            Path::new("/data/i1"),
            Some(Path::new("/state")),
            None,
        );
        assert_eq!(get(&env, "KALLIP_TAGMA_SLUG"), Some("i1"));
        assert_eq!(get(&env, "KALLIP_WORKSPACE_ROOT"), Some("/ws"));
        assert_eq!(get(&env, "KALLIP_TAGMA_DATA_DIR"), Some("/data/i1"));
    }

    #[test]
    fn compose_addr_key_is_first_writer() {
        let explicit = compose_launch_env(
            &base_env(),
            &["KALLIP_TAGMA_ADDR=127.0.0.1:7".into()],
            "d",
            Path::new("/w"),
            Path::new("/data/d"),
            None,
            None,
        );
        assert_eq!(get(&explicit, "KALLIP_TAGMA_ADDR"), Some("127.0.0.1:7"));
        let default = compose_launch_env(
            &base_env(),
            &[],
            "d",
            Path::new("/w"),
            Path::new("/data/d"),
            None,
            None,
        );
        assert_eq!(get(&default, "KALLIP_TAGMA_ADDR"), Some("127.0.0.1:0"));
        let from_base = compose_launch_env(
            &[("KALLIP_TAGMA_ADDR".into(), "127.0.0.1:9".into())],
            &[],
            "d",
            Path::new("/w"),
            Path::new("/data/d"),
            None,
            None,
        );
        assert_eq!(
            get(&from_base, "KALLIP_TAGMA_ADDR"),
            Some("127.0.0.1:9"),
            "first writer wins: a base-resident addr beats the daemon default"
        );
    }

    #[test]
    fn addr_guard_rejects_a_malformed_base_value() {
        let env = compose_launch_env(
            &[("KALLIP_TAGMA_ADDR".into(), "not-an-addr".into())],
            &[],
            "d",
            Path::new("/w"),
            Path::new("/data/d"),
            None,
            None,
        );
        let error = ensure_addr_parses(&env)
            .expect_err("a malformed pin from any channel dies at the guard");
        assert!(error.to_string().contains("SocketAddr"), "{error}");
        let good = compose_launch_env(
            &[("KALLIP_TAGMA_ADDR".into(), "127.0.0.1:9".into())],
            &[],
            "d",
            Path::new("/w"),
            Path::new("/data/d"),
            None,
            None,
        );
        ensure_addr_parses(&good).expect("a well-formed base pin passes");
    }

    #[test]
    fn compose_rust_log_default_appears_only_when_absent() {
        let explicit = compose_launch_env(
            &base_env(),
            &["RUST_LOG=debug".into()],
            "d",
            Path::new("/w"),
            Path::new("/data/d"),
            None,
            None,
        );
        assert!(explicit.contains(&"RUST_LOG=debug".to_owned()));
        let harvested = compose_launch_env(
            &base_env(),
            &[],
            "d",
            Path::new("/w"),
            Path::new("/data/d"),
            None,
            None,
        );
        assert!(harvested.contains(&"RUST_LOG=harvest-level".to_owned()));
        let base: Vec<(String, String)> = vec![("PATH".into(), "/p".into())];
        let neither = compose_launch_env(
            &base,
            &[],
            "d",
            Path::new("/w"),
            Path::new("/data/d"),
            None,
            None,
        );
        assert!(neither.contains(&"RUST_LOG=info".to_owned()));
    }

    #[test]
    fn compose_emits_each_key_exactly_once() {
        let env = compose_launch_env(
            &base_env(),
            &["PATH=/explicit".into(), "RUST_LOG=debug".into()],
            "d",
            Path::new("/w"),
            Path::new("/data/d"),
            Some(Path::new("/state")),
            None,
        );
        let mut keys: Vec<&str> = env
            .iter()
            .map(|pair| pair.split('=').next().unwrap())
            .collect();
        let total = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), total, "duplicate keys in {env:?}");
    }

    #[test]
    fn compose_hands_the_data_dir_and_state_anchor_daemon_owned() {
        // The handoff pair is daemon-owned: neither a polluted profile
        // nor an explicit request pair may move the instance's data root
        // or its logs. No XDG_DATA_HOME anchor exists at all — the
        // explicit handoff replaced the shared-root assumption.
        let base = vec![
            ("PATH".to_owned(), "/p".to_owned()),
            (
                "KALLIP_TAGMA_DATA_DIR".to_owned(),
                "/polluted/data".to_owned(),
            ),
            ("XDG_DATA_HOME".to_owned(), "/polluted/data-home".to_owned()),
        ];
        let env = compose_launch_env(
            &base,
            &["XDG_STATE_HOME=/polluted/state".to_owned()],
            "i1",
            Path::new("/ws"),
            Path::new("/daemon/data/i1"),
            Some(Path::new("/daemon/state")),
            None,
        );
        assert_eq!(get(&env, "KALLIP_TAGMA_DATA_DIR"), Some("/daemon/data/i1"));
        assert_eq!(get(&env, "XDG_STATE_HOME"), Some("/daemon/state"));
        assert_eq!(
            get(&env, "XDG_DATA_HOME"),
            Some("/polluted/data-home"),
            "not daemon-owned, rides through"
        );
    }

    #[test]
    fn compose_without_a_state_home_carries_no_state_anchor() {
        let base = vec![("PATH".to_owned(), "/p".to_owned())];
        let env = compose_launch_env(
            &base,
            &[],
            "i1",
            Path::new("/ws"),
            Path::new("/data/i1"),
            None,
            None,
        );
        assert_eq!(get(&env, "XDG_STATE_HOME"), None);
        assert_eq!(get(&env, "KALLIP_TAGMA_DATA_DIR"), Some("/data/i1"));
    }

    #[test]
    fn fallback_path_anchors_at_the_binary_directory() {
        assert_eq!(
            fallback_path(Some(Path::new("/opt/kallip/bin"))),
            "/opt/kallip/bin:/usr/local/bin:/usr/bin:/bin"
        );
        assert_eq!(
            fallback_path(Some(Path::new(""))),
            "/usr/local/bin:/usr/bin:/bin"
        );
        assert_eq!(fallback_path(None), "/usr/local/bin:/usr/bin:/bin");
    }

    #[test]
    fn a_failed_harvest_degrades_to_a_usable_base() {
        // The Err branch of harvest_base_env, tested directly: the
        // degraded base flows through compose into a launchable env —
        // if the Err branch ever returned an empty base, nothing else
        // would catch it (the shell-out wrapper cannot be unit-tested).
        let base = degrade_to_fallback(Some(Path::new("/opt/kallip/bin")));
        let env = compose_launch_env(
            &base,
            &[],
            "i1",
            Path::new("/ws"),
            Path::new("/data/i1"),
            None,
            None,
        );
        assert!(
            env.contains(&"PATH=/opt/kallip/bin:/usr/local/bin:/usr/bin:/bin".to_owned()),
            "fallback PATH rides through composition: {env:?}"
        );
        assert!(
            env.contains(&"RUST_LOG=info".to_owned()),
            "default fills in"
        );
        assert!(env.contains(&"KALLIP_TAGMA_SLUG=i1".to_owned()));
    }

    #[test]
    fn parse_rejects_a_stream_over_the_total_cap() {
        let mut bytes = b"PATH=/bin\0PAD=".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', HARVEST_MAX_BYTES));
        bytes.push(0);
        assert!(matches!(
            parse_harvest_output(&bytes),
            Err(HarvestError::TooLarge)
        ));
    }

    // --- validate_user_env ---------------------------------------------

    #[test]
    fn validate_accepts_an_explicit_path_key() {
        validate_user_env(&["PATH=/custom/bin".into()]).expect("PATH is allowlisted");
        assert!(
            validate_user_env(&["PATH=".into()]).is_err(),
            "empty rejected"
        );
        assert!(
            validate_user_env(&["NOT_KALLIP=1".into()]).is_err(),
            "others rejected"
        );
        assert!(
            validate_user_env(&["KALLIP_TAGMA_SLUG=/x".into()]).is_err(),
            "reserved rejected"
        );
        assert!(
            validate_user_env(&["KALLIP_TAGMA_DATA_DIR=/x".into()]).is_err(),
            "the data-dir handoff is daemon-owned"
        );
        assert!(
            validate_user_env(&["KALLIP_WORKSPACE_ROOT=/x".into()]).is_err(),
            "the workspace handoff is daemon-owned"
        );
    }

    #[test]
    fn validate_addr_is_user_key_shape_checked() {
        validate_user_env(&["KALLIP_TAGMA_ADDR=127.0.0.1:1".into()])
            .expect("the addr key is user-set");
        let error = validate_user_env(&["KALLIP_TAGMA_ADDR=not-an-addr".into()])
            .expect_err("a bad addr shape dies at request time");
        assert!(error.to_string().contains("SocketAddr"), "{error}");
    }

    #[test]
    fn ensure_not_root_inplace_refuses_only_uid_zero() {
        assert!(ensure_not_root_inplace(0).is_err());
        assert!(ensure_not_root_inplace(1).is_ok());
        assert!(ensure_not_root_inplace(1000).is_ok());
        assert!(ensure_not_root_inplace(65535).is_ok());
        let error = ensure_not_root_inplace(0).unwrap_err();
        assert!(error.to_string().contains("--user"), "{error}");
    }

    #[test]
    fn the_tagma_real_root_escape_rides_the_request_env_pairs() {
        // The escape hatch is a plain KALLIP_* key: it must survive the
        // request-env allowlist and land in the composed child env — a
        // rootful container sets it via kallipctl --env, and a pair
        // dropped here would quietly disarm tagma's own unlock.
        let pair = "KALLIP_TAGMA_ACCEPT_UNSAFE_RUN_AS_ROOT=1";
        validate_user_env(&[pair.to_string()]).expect("KALLIP_* allowlist");
        let env = compose_launch_env(
            &[],
            &[pair.to_string()],
            "team-a",
            Path::new("/ws"),
            Path::new("/dd"),
            None,
            None,
        );
        assert!(env.iter().any(|entry| entry == pair), "{env:?}");
    }

    // --- fill_relay_defaults --------------------------------------------
    // The daemon's own URL env is a parameter here, so the branches
    // test without process-global env mutation.

    const ARCHEION: &str = "http://localhost:7100";
    const LESCHE: &str = "http://localhost:7200";

    fn has(env: &[String], prefix: &str) -> bool {
        env.iter().any(|e| e.starts_with(prefix))
    }

    /// Relay intent without URLs -- both configured defaults are filled.
    #[test]
    fn relay_intent_code_only_fills_both_urls() {
        let out = fill_relay_defaults(
            &["KALLIP_TAGMA_RELAY_ENROLLMENT_CODE=sk-x".into()],
            Some(ARCHEION),
            Some(LESCHE),
        );
        assert!(has(
            &out,
            "KALLIP_TAGMA_RELAY_ARCHEION_URL=http://localhost:7100"
        ));
        assert!(has(
            &out,
            "KALLIP_TAGMA_RELAY_LESCHE_URL=http://localhost:7200"
        ));
    }

    /// No relay signal -- nothing is injected.
    #[test]
    fn local_only_env_stays_untouched() {
        let out = fill_relay_defaults(
            &["KALLIP_LLM_PROVIDER=deepseek".into()],
            Some(ARCHEION),
            Some(LESCHE),
        );
        assert!(!has(&out, "KALLIP_TAGMA_RELAY_"));
        assert_eq!(out.len(), 1);
    }

    /// Explicit values win; empty entries count as missing and are
    /// replaced in place (no duplicate keys).
    #[test]
    fn explicit_values_win_and_empty_is_filled_in_place() {
        let out = fill_relay_defaults(
            &[
                "KALLIP_TAGMA_RELAY_ARCHEION_URL=https://archeion.example.com".into(),
                "KALLIP_TAGMA_RELAY_LESCHE_URL=".into(),
            ],
            Some(ARCHEION),
            Some(LESCHE),
        );
        assert!(has(
            &out,
            "KALLIP_TAGMA_RELAY_ARCHEION_URL=https://archeion.example.com"
        ));
        assert!(has(
            &out,
            "KALLIP_TAGMA_RELAY_LESCHE_URL=http://localhost:7200"
        ));
        assert_eq!(out.len(), 2);
    }

    /// Duplicate entries collapse to the single explicit value
    /// regardless of order: an empty duplicate must neither gain
    /// the default (an order-sensitive consumer could let it
    /// win) nor survive (the daemon's env validation rejects
    /// empty values).
    #[test]
    fn duplicate_keys_collapse_to_the_explicit_value() {
        let out = fill_relay_defaults(
            &[
                "KALLIP_TAGMA_RELAY_ARCHEION_URL=".into(),
                "KALLIP_TAGMA_RELAY_ENROLLMENT_CODE=sk-x".into(),
                "KALLIP_TAGMA_RELAY_ARCHEION_URL=https://archeion.example.com".into(),
            ],
            Some(ARCHEION),
            Some(LESCHE),
        );
        assert!(has(
            &out,
            "KALLIP_TAGMA_RELAY_ARCHEION_URL=https://archeion.example.com"
        ));
        assert_eq!(
            out.iter()
                .filter(|e| e.starts_with("KALLIP_TAGMA_RELAY_ARCHEION_URL"))
                .count(),
            1
        );
        // Reversed order: an empty duplicate ahead of the explicit one.
        let out = fill_relay_defaults(
            &[
                "KALLIP_TAGMA_RELAY_LESCHE_URL=".into(),
                "KALLIP_TAGMA_RELAY_LESCHE_URL=https://lesche.example.com".into(),
            ],
            Some(ARCHEION),
            Some(LESCHE),
        );
        assert!(has(
            &out,
            "KALLIP_TAGMA_RELAY_LESCHE_URL=https://lesche.example.com"
        ));
        assert_eq!(
            out.iter()
                .filter(|e| e.starts_with("KALLIP_TAGMA_RELAY_LESCHE_URL"))
                .count(),
            1
        );
        // Two empties fill once, never duplicate.
        let out = fill_relay_defaults(
            &[
                "KALLIP_TAGMA_RELAY_ARCHEION_URL=".into(),
                "KALLIP_TAGMA_RELAY_ARCHEION_URL=".into(),
            ],
            Some(ARCHEION),
            Some(LESCHE),
        );
        assert_eq!(out.len(), 2, "archeion collapsed plus the lesche default");
        assert!(has(
            &out,
            "KALLIP_TAGMA_RELAY_ARCHEION_URL=http://localhost:7100"
        ));
    }

    /// An unconfigured daemon URL (unset, or empty -- the
    /// kallip-runtime's persistence.rs `filter(!is_empty)` precedent)
    /// means nothing is
    /// filled for it: an absent key stays absent, and an empty request
    /// entry stays as-is to fail env validation downstream -- the same
    /// fail-loud end an empty configured default reached before the
    /// fill moved to the daemon.
    #[test]
    fn unconfigured_daemon_url_fills_nothing() {
        let out = fill_relay_defaults(
            &["KALLIP_TAGMA_RELAY_ENROLLMENT_CODE=sk-x".into()],
            None,
            Some(LESCHE),
        );
        assert!(!has(&out, "KALLIP_TAGMA_RELAY_ARCHEION_URL"));
        assert!(has(
            &out,
            "KALLIP_TAGMA_RELAY_LESCHE_URL=http://localhost:7200"
        ));
        let out = fill_relay_defaults(&["KALLIP_TAGMA_RELAY_ARCHEION_URL=".into()], None, None);
        assert_eq!(
            out,
            vec!["KALLIP_TAGMA_RELAY_ARCHEION_URL=".to_string()],
            "an empty entry stays for env validation to reject"
        );
    }

    #[test]
    fn parse_drops_an_oversized_value() {
        // A profile echoing megabytes of KEY=VALUE-shaped text after the
        // real environment stays under the stream cap yet produces one
        // token big enough to fail execve (MAX_ARG_STRLEN) — the harvest
        // must drop it, not hand it to the launch args.
        let mut bytes = b"PATH=/bin\0BIG=".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', 65 * 1024));
        bytes.push(0);
        let pairs = parse_harvest_output(&bytes).expect("PATH survives");
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["PATH"], "only in-cap values survive");
    }

    #[test]
    fn parse_treats_an_empty_path_as_missing() {
        // Defensive depth: an empty PATH is as unusable as none, and the
        // explicit channel rejects empty values the same way.
        assert!(matches!(
            parse_harvest_output(b"PATH=\0HOME=/home/u\0"),
            Err(HarvestError::NoPath)
        ));
    }

    #[test]
    fn harvest_background_pipe_holder_is_bounded() {
        // The wedge shape: a profile background job holds
        // the stdout pipe after the shell itself exits. The wait for the
        // reader must stay inside the budget, not inside the job's
        // lifetime (10s here — an unbounded join blocks exactly that
        // long and this test goes red on the elapsed bound).
        let Some(_) = bash() else {
            eprintln!("skip: no /bin/bash on this host");
            return;
        };
        let dir = tempdir();
        let wedge = shim(dir.path(), "read -t 10 x < /dev/zero & exit 0");
        let start = Instant::now();
        let error = harvest_shim(
            &wedge,
            &HarvestSeed::default(),
            Duration::from_millis(300),
            None,
        )
        .unwrap_err();
        assert!(matches!(error, HarvestError::Timeout), "got {error:?}");
        assert!(start.elapsed() < Duration::from_secs(5), "bounded wait");
    }

    #[test]
    fn harvest_stream_cap_is_enforced_while_reading() {
        // Endless writer as a background job: memory must stop at the
        // stream cap (read side) and the result must be a capped error,
        // never an unbounded read or an unbounded wait.
        let Some(_) = bash() else {
            eprintln!("skip: no /bin/bash on this host");
            return;
        };
        let dir = tempdir();
        let spammer = shim(
            dir.path(),
            "while :; do printf 'A%.0s' {1..4096}; done & exit 0",
        );
        let start = Instant::now();
        let error = harvest_shim(
            &spammer,
            &HarvestSeed::default(),
            Duration::from_secs(2),
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error, HarvestError::TooLarge | HarvestError::Timeout),
            "got {error:?}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(6),
            "bounded either way"
        );
    }
}
