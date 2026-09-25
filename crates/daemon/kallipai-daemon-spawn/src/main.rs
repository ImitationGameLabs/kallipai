//! Detach-and-exec helper for the kallipai local daemon.
//!
//! Fixed argv, zero protocol, no serde, libc only:
//!
//! ```text
//! kallipai-daemon-spawn [--user NAME --uid UID --gid GID] <instance-dir> <exec-path>
//! ```
//!
//! (the flags stay on one line in real use; wrapped here for the doc).
//! With the privilege flags — the dedicated-user form, sent only by a
//! daemon running as root — the grandchild drops to the named user
//! (initgroups → setgid → setuid, an order forced by the semantics:
//! initgroups needs privilege, and setuid is the point of no return)
//! before chdir and exec. A failed drop is fatal: exec'ing the target
//! as root because a setuid call failed would be the worst outcome.
//!
//! The daemon runs this helper; the helper double-forks (fork → setsid →
//! fork) so the exec'd instance survives the daemon and holds no controlling
//! terminal, applies the KEY=VALUE tail args as the child env, then execs
//! the target. The instance writes its own `runtime.json` into
//! `<instance-dir>` (its slug-derived data root) as the daemon's launch anchor.
//!
//! Dependency isolation is the point of a separate crate: this binary is the
//! privilege-transition carrier of the platform-hosting form, so its audit
//! surface stays std + libc, nothing else. In the unpackaged same-uid profile
//! the helper runs unprivileged and flag-free; the privilege flags demand a
//! root caller (the system daemon), so the binary itself needs no setuid bit.
//!
//! `KEY=VALUE` args pass through UNFILTERED here; the frozen allowlist
//! check moves into this helper at packaging time, where the setuid
//! binary cannot trust its caller.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

const USAGE: &str = "usage: kallipai-daemon-spawn [--user NAME --uid UID --gid GID] <instance-dir> <exec-path> [KEY=VALUE ...]";

/// Privilege flags for the drop-to launch. The daemon (running as root)
/// names the target user; the grandchild drops to it before exec. All
/// three flags travel together or not at all — the user name feeds
/// initgroups (the full supplementary-group set), uid and gid are the
/// drop targets.
#[derive(Debug, Default)]
struct Privileges {
    user: Option<String>,
    uid: Option<u32>,
    gid: Option<u32>,
}

impl Privileges {
    fn specified(&self) -> bool {
        self.user.is_some() || self.uid.is_some() || self.gid.is_some()
    }
}

/// Consume leading --user/--uid/--gid flags, leaving the positionals.
/// Unknown flags are a usage error, not positionals: a typoed flag
/// must not silently become the instance dir.
fn parse_flags(args: &[String]) -> Result<(Privileges, &[String]), String> {
    let mut privs = Privileges::default();
    let mut rest = args;
    while let Some(flag) = rest.first() {
        let name = match flag.as_str() {
            "--user" | "--uid" | "--gid" => flag.as_str(),
            other => {
                if other.starts_with("--") {
                    return Err(format!("unknown flag {other:?}"));
                }
                break;
            }
        };
        let Some(value) = rest.get(1) else {
            return Err(format!("flag {name} needs a value"));
        };
        match name {
            "--user" => privs.user = Some(value.clone()),
            "--uid" => {
                privs.uid = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--uid {value:?} is not a number"))?,
                );
            }
            "--gid" => {
                privs.gid = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--gid {value:?} is not a number"))?,
                );
            }
            _ => unreachable!("matched above"),
        }
        rest = &rest[2..];
    }
    if privs.specified() && (privs.user.is_none() || privs.uid.is_none() || privs.gid.is_none()) {
        return Err("--user, --uid and --gid are required together".into());
    }
    Ok((privs, rest))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = run(&args);
    std::process::exit(code);
}

fn run(args: &[String]) -> i32 {
    // Fixed argv grammar: leading privilege flags, one positional pair,
    // then KEY=VALUE pairs.
    let (privs, rest) = match parse_flags(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("kallipai-daemon-spawn: {message}");
            eprintln!("{USAGE}");
            return 64; // EX_USAGE
        }
    };
    let [dir, exec, envs @ ..] = rest else {
        eprintln!("{USAGE}");
        return 64; // EX_USAGE
    };
    for pair in envs {
        if !pair.contains('=') || pair.is_empty() {
            eprintln!("env arg {pair:?} is not KEY=VALUE");
            return 64;
        }
    }
    let instance_dir = Path::new(dir);
    if !instance_dir.is_dir() {
        eprintln!("instance dir {dir:?} is not a directory");
        return 66; // EX_NOINPUT
    }
    let exec_path = Path::new(exec);
    if !exec_path.is_file() {
        eprintln!("exec target {exec:?} is not a file");
        return 66;
    }
    // Resolve the exec target against the CURRENT directory before the
    // chdir into the instance dir: a relative caller path must keep
    // meaning after the grandchild changes directories.
    let exec_abs = std::fs::canonicalize(exec_path).unwrap_or_else(|_| exec_path.to_path_buf());
    match detach_and_exec(instance_dir, &exec_abs, envs, &privs) {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("kallipai-daemon-spawn: {message}");
            71 // EX_OSERR
        }
    }
}

/// Double-fork + exec. Returns Ok on successful detach (the grandchild's
/// exec failures are invisible to us by design — the daemon's 30s pidfile
/// wait is what turns a failed exec into a rollback).
fn detach_and_exec(
    instance_dir: &Path,
    exec_path: &Path,
    envs: &[String],
    privs: &Privileges,
) -> Result<(), String> {
    // The child env: only the explicit KEY=VALUE pairs. A blank env (no
    // PATH, no HOME) is deliberate — the daemon supplies everything the
    // instance needs, and the instance must not inherit daemon context.
    let env: Vec<CString> = envs
        .iter()
        .map(|pair| CString::new(pair.as_str().to_owned()))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("env pair with NUL byte: {e}"))?;

    // chdir into the instance dir so a relative-path exec stays anchored and
    // core dumps land somewhere scoped.
    let dir_c = CString::new(instance_dir.as_os_str().as_bytes())
        .map_err(|e| format!("instance dir NUL byte: {e}"))?;
    let exec_c = CString::new(exec_path.as_os_str().as_bytes())
        .map_err(|e| format!("exec path NUL byte: {e}"))?;
    // argv[0] as the invoked path, no further args: the instance is
    // env-driven by design.
    let argv: Vec<CString> = vec![exec_c.clone()];

    // fork #1
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", std::io::Error::last_os_error()));
    }
    if pid > 0 {
        // Parent: reap child #1 immediately (it exits after forking the
        // grandchild), then return success to the daemon.
        unsafe {
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
        return Ok(());
    }

    // Child #1: new session, detach from the controlling terminal.
    if unsafe { libc::setsid() } < 0 {
        let e = std::io::Error::last_os_error();
        eprintln!("kallipai-daemon-spawn: setsid: {e}");
        std::process::exit(71);
    }

    // fork #2 — the grandchild can never reacquire a controlling terminal
    // and is reparented to init when child #1 exits.
    let pid2 = unsafe { libc::fork() };
    if pid2 < 0 {
        let e = std::io::Error::last_os_error();
        eprintln!("kallipai-daemon-spawn: fork2: {e}");
        std::process::exit(71);
    }
    if pid2 > 0 {
        std::process::exit(0); // child #1 done
    }

    // Grandchild: drop to the target user (drop-to launches only), then
    // chdir, blank env + execve. Null-terminated argv/envp pointer
    // arrays (the CString values above outlive the call).
    if privs.specified() {
        drop_privileges(privs);
    }
    let mut argv_p: Vec<*const libc::c_char> = argv.iter().map(|c| c.as_ptr()).collect();
    argv_p.push(std::ptr::null());
    let mut env_p: Vec<*const libc::c_char> = env.iter().map(|c| c.as_ptr()).collect();
    env_p.push(std::ptr::null());
    unsafe {
        libc::chdir(dir_c.as_ptr());
    }
    unsafe {
        libc::execve(exec_c.as_ptr(), argv_p.as_ptr(), env_p.as_ptr());
        // execve only returns on failure.
    }
    let e = std::io::Error::last_os_error();
    eprintln!("kallipai-daemon-spawn: exec {exec_path:?}: {e}");
    std::process::exit(71);
}

/// Drop to the target user in the grandchild, never returning on
/// failure. Order is forced: initgroups (the supplementary-group set)
/// and setgid both need the privileges setuid takes away, so they run
/// first; setuid is last and irreversible. Exiting 71 on a failed
/// drop is the only safe answer — the daemon's pidfile wait turns it
/// into a rolled-back launch rather than a root-owned instance.
fn drop_privileges(privs: &Privileges) {
    let (user, uid, gid) = (
        privs
            .user
            .as_deref()
            .expect("drop requested without --user"),
        privs.uid.expect("drop requested without --uid"),
        privs.gid.expect("drop requested without --gid"),
    );
    let user_c = match CString::new(user) {
        Ok(name) => name,
        Err(_) => {
            eprintln!("kallipai-daemon-spawn: --user {user:?} contains a NUL byte");
            std::process::exit(71);
        }
    };
    // SAFETY: libc group/uid calls with a NUL-terminated name; each
    // failure exits before the next runs, so a partial drop can never
    // reach exec.
    unsafe {
        if libc::initgroups(user_c.as_ptr(), gid) != 0 {
            let e = std::io::Error::last_os_error();
            eprintln!("kallipai-daemon-spawn: initgroups: {e}");
            std::process::exit(71);
        }
        if libc::setgid(gid) != 0 {
            let e = std::io::Error::last_os_error();
            eprintln!("kallipai-daemon-spawn: setgid: {e}");
            std::process::exit(71);
        }
        if libc::setuid(uid) != 0 {
            let e = std::io::Error::last_os_error();
            eprintln!("kallipai-daemon-spawn: setuid: {e}");
            std::process::exit(71);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_parse_and_leave_the_positionals() {
        let argv = args(&[
            "--user",
            "kallipai-team",
            "--uid",
            "4242",
            "--gid",
            "4242",
            "/data/i1",
            "/bin/tagma",
            "KALLIPAI_TAGMA_SLUG=i1",
        ]);
        let (privs, rest) = parse_flags(&argv).expect("parses");
        assert_eq!(privs.user.as_deref(), Some("kallipai-team"));
        assert_eq!(privs.uid, Some(4242));
        assert_eq!(privs.gid, Some(4242));
        assert_eq!(rest, ["/data/i1", "/bin/tagma", "KALLIPAI_TAGMA_SLUG=i1"]);
    }

    #[test]
    fn no_flags_is_the_in_place_grammar() {
        let argv = args(&["/data/i1", "/bin/tagma"]);
        let (privs, rest) = parse_flags(&argv).expect("parses");
        assert!(!privs.specified());
        assert_eq!(rest, ["/data/i1", "/bin/tagma"]);
    }

    #[test]
    fn partial_flag_sets_are_refused() {
        // uid without gid (or the user name missing) would drop a
        // process into an unintended group set; the grammar keeps the
        // triple atomic.
        assert!(parse_flags(&args(&["--uid", "1", "/d", "/e"])).is_err());
        assert!(parse_flags(&args(&["--gid", "1", "/d", "/e"])).is_err());
        assert!(parse_flags(&args(&["--uid", "1", "--gid", "1", "/d", "/e"])).is_err());
    }

    #[test]
    fn unknown_flags_are_usage_errors_not_positionals() {
        assert!(parse_flags(&args(&["--userr", "x", "/d", "/e"])).is_err());
    }
}
