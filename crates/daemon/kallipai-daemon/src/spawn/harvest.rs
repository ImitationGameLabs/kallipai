//! The login-environment harvest: the shell run, the NUL-stream parser
//! with its validation, and the fallback path when the harvest fails.

use super::identity::{
    LaunchIdentity, ResolvedUser, cached_passwd_identity, runtime_dir_for, runtime_dir_in_place,
};
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Bash used for the login harvest: an explicit path, never `$SHELL`, so
/// the user's shell preference cannot swap the interpreter under us.
/// `KALLIPAI_HARVEST_BASH` lets a deployment name the path (NixOS has no
/// `/bin/bash`); it stays an administrative constant — nothing a
/// request or a user environment supplies can move it.
pub(super) fn harvest_bash() -> PathBuf {
    harvest_bash_from(std::env::var_os("KALLIPAI_HARVEST_BASH").as_deref())
}

/// Pure core of [`harvest_bash`] so the resolution stays testable
/// without mutating process-global environment state in tests.
pub(super) fn harvest_bash_from(explicit: Option<&std::ffi::OsStr>) -> PathBuf {
    explicit
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/bin/bash"))
}

/// Wall-clock budget one launch may spend harvesting; past it the shell is
/// killed and the launch degrades to the fallback PATH.
pub(super) const HARVEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Ceiling on the harvested stream: a profile spamming output fails the
/// harvest instead of wedging the launch.
pub(super) const HARVEST_MAX_BYTES: usize = 1024 * 1024;

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
pub(super) enum HarvestError {
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
/// scripts expect to see. XDG_RUNTIME_DIR, when the user's runtime
/// directory exists, lets session scripts that derive from it (a
/// DOCKER_HOST socket, for one) evaluate as in a real login.
#[derive(Debug, Default)]
pub(super) struct HarvestSeed {
    pub(super) home: Option<OsString>,
    pub(super) user: Option<OsString>,
    pub(super) runtime_dir: Option<OsString>,
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
        let runtime_dir = runtime_dir_in_place(
            std::env::var_os("XDG_RUNTIME_DIR"),
            runtime_dir_for(unsafe { libc::geteuid() }),
        );
        Self {
            home,
            user,
            runtime_dir,
        }
    }
    /// Identity of a drop-to target: everything comes from the passwd
    /// resolution, never from the daemon's environment — the daemon's
    /// HOME/USER describe the wrong user, and the requesting peer's
    /// environment is not trusted at all.
    pub(super) fn for_target(target: &ResolvedUser) -> Self {
        Self {
            home: Some(target.home.clone().into_os_string()),
            user: Some(OsString::from(target.username.as_str())),
            runtime_dir: runtime_dir_for(target.uid),
        }
    }
}

/// Run one login shell under a cleared environment (seeding only the
/// identity and runtime dir above) and capture the environment it computes. `env -0`
/// keeps multiline values intact; the NUL-separated stream is parsed
/// strictly — see [`parse_harvest_output`].
pub(super) fn harvest_login_env(
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
    if let Some(dir) = &seed.runtime_dir {
        command.env("XDG_RUNTIME_DIR", dir);
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
pub(super) fn parse_harvest_output(bytes: &[u8]) -> Result<Vec<(String, String)>, HarvestError> {
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
pub(super) fn valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some('a'..='z' | 'A'..='Z' | '_'))
        && chars.all(|c| matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_'))
}

/// The degraded PATH for a failed harvest: the daemon's own binary
/// directory (where the helper and tagma resolve from) plus the
/// conventional system locations.
pub(super) fn fallback_path(bin_dir: Option<&Path>) -> String {
    match bin_dir.filter(|dir| !dir.as_os_str().is_empty()) {
        Some(dir) => format!("{}{FALLBACK_PATH_TAIL}", dir.display()),
        None => FALLBACK_PATH_TAIL.trim_start_matches(':').to_owned(),
    }
}

/// The launch env base: the harvested login environment, or — on any
/// harvest failure — the fallback PATH. Always a usable base: harvesting
/// is a best-effort upgrade, never a launch gate.
pub(super) fn harvest_base_env(
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
pub(super) fn degrade_to_fallback(fallback_bin_dir: Option<&Path>) -> Vec<(String, String)> {
    vec![("PATH".to_owned(), fallback_path(fallback_bin_dir))]
}
