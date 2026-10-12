//! The launch identity: resolution from the request and from a record,
//! the passwd lookups, the owner inference and the runtime directories.

use super::authorize::{delegate_keyless_rejection, delegate_target_rejection};
use super::error::SpawnError;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

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
    pub(crate) shell: String,
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
pub(super) fn require_resolved_binary(
    path: &Path,
    name: &str,
    hint: &str,
) -> Result<(), SpawnError> {
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
    let shell = if pw.pw_shell.is_null() {
        &[]
    } else {
        unsafe { CStr::from_ptr(pw.pw_shell) }.to_bytes()
    };
    Some(ResolvedUser {
        uid: pw.pw_uid,
        gid: pw.pw_gid,
        username: OsStr::from_bytes(username).to_string_lossy().into_owned(),
        home: PathBuf::from(OsStr::from_bytes(home)),
        shell: OsStr::from_bytes(shell).to_string_lossy().into_owned(),
    })
}

/// The user's runtime directory, derived: it exists only under a
/// per-user tmpfs (linger or an active session); the absence case is
/// the answer, not an error.
pub(super) fn runtime_dir_for(uid: u32) -> Option<OsString> {
    let dir = Path::new("/run/user").join(uid.to_string());
    dir.is_dir().then_some(dir.into_os_string())
}

/// The in-place runtime dir decision: a non-empty daemon environment
/// value wins (the daemon's own login already resolved it); otherwise
/// the euid's runtime-directory derivation answers. Pure so the
/// precedence is testable without touching process state.
pub(super) fn runtime_dir_in_place(
    env_value: Option<OsString>,
    derived: Option<OsString>,
) -> Option<OsString> {
    env_value.filter(|value| !value.is_empty()).or(derived)
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
