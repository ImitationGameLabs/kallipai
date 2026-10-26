//! Tests for the spawn pipeline: the harvest parser and its fallback, the
//! authorization matrix, identity resolution, the environment assembly and
//! the launch helpers.

use super::authorize::*;
use super::env::*;
use super::error::*;
use super::harvest::*;
use super::identity::*;
use super::launch::*;

use crate::records;
use std::ffi::OsString;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod shim");
    path
}

/// Upper bound on how long a just-written shim is given to become
/// execable, and the interval spent waiting for it. Each caller's
/// whole-call assertion must fit this window plus one poll, its own
/// timeout and the reap that follows: 3.05s + 0.5s inside a 5s bound
/// leaves ~1.45s, and 3.05s + 2.2s inside a 6s bound leaves ~0.75s,
/// the thinnest. A per-call latency assertion measures one call with
/// the retry waits excluded, so this window never reaches it.
const SHIM_EXEC_WINDOW: Duration = Duration::from_secs(3);
/// The gap between probes: the "one poll" the window doc charges a bound with.
const SHIM_EXEC_POLL: Duration = Duration::from_millis(50);

/// Run the harvest against a just-written shim, reporting the outcome
/// with the wall-clock time the answering call took: retries that only
/// waited are excluded, so a caller asserting a call's latency measures
/// the call, not this helper's patience.
///
/// Exec of a file whose write handle closed microseconds ago races the
/// kernel's deferred release of that handle: ETXTBSY with no writer left
/// holding the path (the same probe-verified window behind adopt's
/// fake-tagma retry: filesystem-independent, and it fires without any
/// concurrent writer). The window closes on its own schedule, so the
/// waiting is bounded by wall clock, not by an attempt count -- a count
/// caps the wait at an arbitrary fraction of the window. Every
/// legitimate outcome is stable, so only ETXTBSY is retried, and the
/// bound is generous because a loaded host stretches the window. Running
/// past it means the window never closed: the panic carries the number
/// of calls made and the time spent.
fn harvest_shim(
    bash: &Path,
    seed: &HarvestSeed,
    timeout: Duration,
    run_as: Option<(u32, u32)>,
) -> (Result<Vec<(String, String)>, HarvestError>, Duration) {
    let start = Instant::now();
    let mut attempts = 0u32;
    loop {
        let call = Instant::now();
        let outcome = harvest_login_env(bash, seed, timeout, run_as);
        let call_elapsed = call.elapsed();
        attempts += 1;
        match outcome {
            // Spawn(String) flattens the io error to text, so match the
            // message: the process never setlocale(3)s, glibc/musl
            // wording is stable, and a missed match degrades into the
            // test's explicit assertion failure, not a silent flake.
            Err(HarvestError::Spawn(message)) if message.contains("Text file busy") => {
                if start.elapsed() >= SHIM_EXEC_WINDOW {
                    panic!(
                        "harvest shim never became execable: {attempts} attempts, {:?}: {}",
                        start.elapsed(),
                        bash.display()
                    );
                }
                std::thread::sleep(SHIM_EXEC_POLL);
            }
            outcome => return (outcome, call_elapsed),
        }
    }
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
    let pairs =
        parse_harvest_output(b"PATH=/bin:/usr/bin\0HOME=/home/u\0").expect("clean stream parses");
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
    for good in ["PATH", "KALLIPAI_X", "_x", "a1_"] {
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
        "export KALLIPAI_UNIT_MARKER=green\nexport PATH=/fixture/bin:$PATH\n",
    )
    .expect("write profile");
    let seed = HarvestSeed {
        home: Some(home.path().as_os_str().to_owned()),
        user: Some("probe".into()),
        runtime_dir: None,
    };
    let pairs = harvest_login_env(&bash, &seed, HARVEST_TEST_BUDGET, None).expect("harvest ok");
    let marker = pairs.iter().find(|(k, _)| k == "KALLIPAI_UNIT_MARKER");
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
    let (outcome, _) = harvest_shim(&bash, &HarvestSeed::default(), HARVEST_TEST_BUDGET, None);
    let error = outcome.unwrap_err();
    assert!(matches!(error, HarvestError::Exit(_)));
}

#[test]
fn harvest_timeout_kills_the_shell_and_returns_promptly() {
    let Some(_) = bash() else {
        eprintln!("skip: no /bin/bash on this host");
        return;
    };
    let dir = tempdir();
    // The shim publishes its own pid before it wedges: asserting that a
    // timeout kills the shell needs a pid to look for. The harvest execs
    // this script directly, so bash runs it as a script, not a login
    // session: `-l`, `-c` and `env -0` arrive as positional arguments, so
    // no profile chain runs. The first statement is therefore milliseconds
    // away from the spawn, which is why publishing the pid ahead of the
    // wedge fits inside the harvest budget.
    let pid_file = dir.path().join("shell.pid");
    let bash = shim(
        dir.path(),
        &format!("echo $$ > '{}'; while :; do :; done", pid_file.display()),
    );
    let start = Instant::now();
    let (outcome, call_elapsed) = harvest_shim(
        &bash,
        &HarvestSeed::default(),
        Duration::from_millis(300),
        None,
    );
    let error = outcome.unwrap_err();
    assert!(matches!(error, HarvestError::Timeout), "got {error:?}");
    // The measured call is the one that timed out: waiting for the shim
    // to become execable is bounded separately and must not show up here.
    assert!(
        call_elapsed < Duration::from_secs(3),
        "the timed-out call returned promptly: {call_elapsed:?}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "returned promptly"
    );
    let pid_text = std::fs::read_to_string(&pid_file).unwrap_or_else(|error| {
        panic!(
            "no pid file after the timeout ({error}): the shell never \
             reached its first statement, or the write did not land"
        )
    });
    let pid: i32 = pid_text
        .trim()
        .parse()
        .unwrap_or_else(|error| panic!("the pid file holds no pid ({error}): {pid_text:?}"));
    // SAFETY: signal 0 probes pid existence without delivering a signal.
    let probe = unsafe { libc::kill(pid, 0) };
    // errno answers only for a failed call: on a zero return what it holds
    // is not a value this code may trust, so it is read only where the probe
    // failed.
    let probe_error = (probe == -1).then(std::io::Error::last_os_error);
    // A reaped pid answers ESRCH. EPERM means the pid is alive and owned
    // elsewhere, so folding it into "reaped" would pass over a shell that
    // is still running.
    assert!(
        probe_error
            .as_ref()
            .is_some_and(|error| error.raw_os_error() == Some(libc::ESRCH)),
        "the timed-out shell {pid} was reaped, not left running: kill returned \
         {probe}{}",
        match &probe_error {
            Some(error) => format!(" ({error})"),
            None => String::new(),
        }
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
        Path::new("definitely-not-a-real-kallipai-binary"),
        "kallipai-tagma",
        "set KALLIPAI_BIN_DIR, or pass --bin",
    )
    .expect_err("that path does not exist");
    let SpawnError::Invalid(message) = error else {
        panic!("expected Invalid, got {error}")
    };
    assert!(message.contains("KALLIPAI_BIN_DIR"), "{message}");
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
            username: cached_passwd_identity().map(|(name, _)| name.to_string_lossy().into_owned()),
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
    let error = resolve_launch_identity(Some("no-such-kallipai-user-xyz"), 0).unwrap_err();
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
        username: "kallipai-team".into(),
        home: PathBuf::from("/home/kallipai-team"),
        shell: "/bin/bash".into(),
    };
    let env = compose_launch_env(
        &base_env(),
        &[
            "HOME=/polluted".to_owned(),
            "XDG_STATE_HOME=/polluted/state".to_owned(),
            "XDG_CACHE_HOME=/polluted/cache".to_owned(),
            "SHELL=/polluted/shell".to_owned(),
        ],
        "i1",
        Path::new("/ws"),
        Path::new("/data/i1"),
        None,
        Some(&target),
    );
    assert_eq!(
        get(&env, "HOME"),
        Some("/home/kallipai-team"),
        "daemon-owned wins"
    );
    assert_eq!(get(&env, "USER"), Some("kallipai-team"));
    assert_eq!(get(&env, "LOGNAME"), Some("kallipai-team"));
    assert_eq!(
        get(&env, "XDG_CONFIG_HOME"),
        Some("/home/kallipai-team/.config")
    );
    assert_eq!(
        get(&env, "XDG_DATA_HOME"),
        Some("/home/kallipai-team/.local/share")
    );
    assert_eq!(
        get(&env, "XDG_STATE_HOME"),
        Some("/home/kallipai-team/.local/state")
    );
    assert_eq!(
        get(&env, "XDG_CACHE_HOME"),
        Some("/home/kallipai-team/.cache")
    );
    assert_eq!(get(&env, "SHELL"), Some("/bin/bash"));
    // The runtime dir appears only when the host actually has it
    // (linger supplies it in the system form); either way the
    // instance never sees a polluted value.
    if let Some(runtime) = get(&env, "XDG_RUNTIME_DIR") {
        assert_eq!(runtime, "/run/user/4242");
    }
}
#[test]
fn dedicated_env_pins_no_shell_when_the_passwd_shell_is_empty() {
    // An empty passwd shell pins nothing: SHELL stays absent rather
    // than degrading to a guess; the rest of the identity set is
    // unaffected.
    let target = ResolvedUser {
        uid: 4242,
        gid: 4242,
        username: "kallipai-team".into(),
        home: PathBuf::from("/home/kallipai-team"),
        shell: String::new(),
    };
    let env = compose_launch_env(
        &base_env(),
        &[],
        "i1",
        Path::new("/ws"),
        Path::new("/data/i1"),
        None,
        Some(&target),
    );
    assert_eq!(get(&env, "SHELL"), None);
    assert_eq!(get(&env, "HOME"), Some("/home/kallipai-team"));
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
    assert_eq!(get(&env, "SHELL"), None);
    assert_eq!(get(&env, "XDG_CACHE_HOME"), None);
}

#[test]
fn drop_to_seed_derives_the_runtime_dir_only_when_it_exists() {
    // The derivation answers with the directory's existence, not an
    // error: no per-user tmpfs, no seed. The profile then sees
    // exactly what a real login on such a host would see.
    if Path::new("/run/user/4242").is_dir() {
        return;
    }
    let target = ResolvedUser {
        uid: 4242,
        gid: 4242,
        username: "kallipai-team".into(),
        home: PathBuf::from("/home/kallipai-team"),
        shell: String::new(),
    };
    let seed = HarvestSeed::for_target(&target);
    assert!(seed.runtime_dir.is_none());
}
#[test]
fn in_place_runtime_dir_env_value_wins() {
    let env_value = Some(OsString::from("/run/user/1000"));
    let derived = Some(OsString::from("/run/user/0"));
    assert_eq!(runtime_dir_in_place(env_value.clone(), derived), env_value);
}
#[test]
fn in_place_runtime_dir_empty_or_missing_env_falls_back() {
    assert_eq!(
        runtime_dir_in_place(Some(OsString::new()), Some(OsString::from("/run/user/0"))),
        Some(OsString::from("/run/user/0"))
    );
    assert_eq!(
        runtime_dir_in_place(None, Some(OsString::from("/run/user/0"))),
        Some(OsString::from("/run/user/0"))
    );
    assert_eq!(runtime_dir_in_place(Some(OsString::new()), None), None);
}
#[test]
fn harvest_bash_override_wins_and_default_survives() {
    // The deployment constant: an explicit KALLIPAI_HARVEST_BASH path
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

#[test]
fn the_spawn_overlap_error_names_input_requested_and_rival() {
    let error = SpawnError::overlaps_instance_data_dir(
        OverlapInput::Workspace,
        Path::new("/chosen/ws"),
        "settled",
        Path::new("/elsewhere/dd"),
    );
    assert_eq!(
        error.to_string(),
        "workspace /chosen/ws overlaps instance settled's data dir at /elsewhere/dd"
    );
}

#[test]
fn spawn_refuses_a_workspace_overlapping_a_registered_data_dir() {
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
    records::write_record(root.path(), "settled", &record).expect("write record");
    let user = cached_passwd_identity()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .expect("test host has a passwd entry");
    let error = spawn(
        root.path(),
        "fresh",
        data.path().to_str().expect("utf8 data dir"),
        &[],
        Some("/bin/sh"),
        Duration::from_secs(1),
        unsafe { libc::getuid() },
        Some(&user),
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "workspace {} overlaps instance settled's data dir at {}",
            data.path().canonicalize().unwrap().display(),
            data.path().display(),
        )
    );
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
        Some("/nonexistent/kallipai-tagma"),
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
        &["PATH=/explicit".into(), "KALLIPAI_X=1".into()],
        "i1",
        Path::new("/ws"),
        Path::new("/data/i1"),
        Some(Path::new("/state")),
        None,
    );
    assert_eq!(get(&env, "PATH"), Some("/explicit"));
    assert_eq!(get(&env, "KALLIPAI_X"), Some("1"));
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
    assert_eq!(get(&env, "KALLIPAI_TAGMA_SLUG"), Some("i1"));
    assert_eq!(get(&env, "KALLIPAI_WORKSPACE_ROOT"), Some("/ws"));
    assert_eq!(get(&env, "KALLIPAI_TAGMA_DATA_DIR"), Some("/data/i1"));
}

#[test]
fn compose_addr_key_is_first_writer() {
    let explicit = compose_launch_env(
        &base_env(),
        &["KALLIPAI_TAGMA_ADDR=127.0.0.1:7".into()],
        "d",
        Path::new("/w"),
        Path::new("/data/d"),
        None,
        None,
    );
    assert_eq!(get(&explicit, "KALLIPAI_TAGMA_ADDR"), Some("127.0.0.1:7"));
    let default = compose_launch_env(
        &base_env(),
        &[],
        "d",
        Path::new("/w"),
        Path::new("/data/d"),
        None,
        None,
    );
    assert_eq!(get(&default, "KALLIPAI_TAGMA_ADDR"), Some("127.0.0.1:0"));
    let from_base = compose_launch_env(
        &[("KALLIPAI_TAGMA_ADDR".into(), "127.0.0.1:9".into())],
        &[],
        "d",
        Path::new("/w"),
        Path::new("/data/d"),
        None,
        None,
    );
    assert_eq!(
        get(&from_base, "KALLIPAI_TAGMA_ADDR"),
        Some("127.0.0.1:9"),
        "first writer wins: a base-resident addr beats the daemon default"
    );
}

#[test]
fn addr_guard_rejects_a_malformed_base_value() {
    let env = compose_launch_env(
        &[("KALLIPAI_TAGMA_ADDR".into(), "not-an-addr".into())],
        &[],
        "d",
        Path::new("/w"),
        Path::new("/data/d"),
        None,
        None,
    );
    let error =
        ensure_addr_parses(&env).expect_err("a malformed pin from any channel dies at the guard");
    assert!(error.to_string().contains("SocketAddr"), "{error}");
    let good = compose_launch_env(
        &[("KALLIPAI_TAGMA_ADDR".into(), "127.0.0.1:9".into())],
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
            "KALLIPAI_TAGMA_DATA_DIR".to_owned(),
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
    assert_eq!(
        get(&env, "KALLIPAI_TAGMA_DATA_DIR"),
        Some("/daemon/data/i1")
    );
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
    assert_eq!(get(&env, "KALLIPAI_TAGMA_DATA_DIR"), Some("/data/i1"));
}

#[test]
fn fallback_path_anchors_at_the_binary_directory() {
    assert_eq!(
        fallback_path(Some(Path::new("/opt/kallipai/bin"))),
        "/opt/kallipai/bin:/usr/local/bin:/usr/bin:/bin"
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
    let base = degrade_to_fallback(Some(Path::new("/opt/kallipai/bin")));
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
        env.contains(&"PATH=/opt/kallipai/bin:/usr/local/bin:/usr/bin:/bin".to_owned()),
        "fallback PATH rides through composition: {env:?}"
    );
    assert!(
        env.contains(&"RUST_LOG=info".to_owned()),
        "default fills in"
    );
    assert!(env.contains(&"KALLIPAI_TAGMA_SLUG=i1".to_owned()));
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
        validate_user_env(&["KALLIPAI_TAGMA_SLUG=/x".into()]).is_err(),
        "reserved rejected"
    );
    assert!(
        validate_user_env(&["KALLIPAI_TAGMA_DATA_DIR=/x".into()]).is_err(),
        "the data-dir handoff is daemon-owned"
    );
    assert!(
        validate_user_env(&["KALLIPAI_WORKSPACE_ROOT=/x".into()]).is_err(),
        "the workspace handoff is daemon-owned"
    );
}

#[test]
fn validate_addr_is_user_key_shape_checked() {
    validate_user_env(&["KALLIPAI_TAGMA_ADDR=127.0.0.1:1".into()])
        .expect("the addr key is user-set");
    let error = validate_user_env(&["KALLIPAI_TAGMA_ADDR=not-an-addr".into()])
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
    // The escape hatch is a plain KALLIPAI_* key: it must survive the
    // request-env allowlist and land in the composed child env — a
    // rootful container sets it via kallipctl --env, and a pair
    // dropped here would quietly disarm tagma's own unlock.
    let pair = "KALLIPAI_TAGMA_ACCEPT_UNSAFE_RUN_AS_ROOT=1";
    validate_user_env(&[pair.to_string()]).expect("KALLIPAI_* allowlist");
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
// The daemon's own origin env is a parameter here, so the branches
// test without process-global env mutation.

const POLIS: &str = "http://localhost:8080";

fn has(env: &[String], prefix: &str) -> bool {
    env.iter().any(|e| e.starts_with(prefix))
}

/// Relay intent via the code: the configured origin is filled.
#[test]
fn relay_intent_code_only_fills_the_origin() {
    let out = fill_relay_defaults(
        &["KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE=sk-x".into()],
        Some(POLIS),
    );
    assert!(has(&out, "KALLIPAI_POLIS_URL=http://localhost:8080"));
}

/// Relay intent via an explicit origin: it survives as-is.
#[test]
fn explicit_origin_passes_through() {
    let out = fill_relay_defaults(
        &["KALLIPAI_POLIS_URL=https://api.example.com".into()],
        Some(POLIS),
    );
    assert!(has(&out, "KALLIPAI_POLIS_URL=https://api.example.com"));
    assert_eq!(out.len(), 1);
}

/// No relay signal -- nothing is injected.
#[test]
fn local_only_env_stays_untouched() {
    let out = fill_relay_defaults(&["KALLIPAI_TEST_LLM_PROVIDER=deepseek".into()], Some(POLIS));
    assert!(!has(&out, "KALLIPAI_POLIS_URL="));
    assert!(!has(&out, "KALLIPAI_TAGMA_RELAY_"));
    assert_eq!(out.len(), 1);
}

/// An empty explicit origin counts as missing and is filled in
/// place (no duplicate keys).
#[test]
fn empty_origin_is_filled_in_place() {
    let out = fill_relay_defaults(&["KALLIPAI_POLIS_URL=".into()], Some(POLIS));
    assert!(has(&out, "KALLIPAI_POLIS_URL=http://localhost:8080"));
    assert_eq!(out.len(), 1);
}

/// Duplicate origins collapse to the single explicit value.
#[test]
fn duplicate_origins_collapse_to_the_explicit_value() {
    let out = fill_relay_defaults(
        &[
            "KALLIPAI_POLIS_URL=".into(),
            "KALLIPAI_POLIS_URL=https://api.example.com".into(),
        ],
        Some(POLIS),
    );
    assert!(has(&out, "KALLIPAI_POLIS_URL=https://api.example.com"));
    assert_eq!(
        out.iter()
            .filter(|e| e.starts_with("KALLIPAI_POLIS_URL"))
            .count(),
        1
    );
}

/// An unconfigured daemon origin (unset, or empty) means nothing
/// is filled: an absent key stays absent, and an empty request
/// entry stays as-is to fail env validation downstream.
#[test]
fn unconfigured_daemon_origin_fills_nothing() {
    let out = fill_relay_defaults(&["KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE=sk-x".into()], None);
    assert!(!has(&out, "KALLIPAI_POLIS_URL="));
    let out = fill_relay_defaults(&["KALLIPAI_POLIS_URL=".into()], None);
    assert_eq!(
        out,
        vec!["KALLIPAI_POLIS_URL=".to_string()],
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
    let (outcome, _) = harvest_shim(
        &wedge,
        &HarvestSeed::default(),
        Duration::from_millis(300),
        None,
    );
    let error = outcome.unwrap_err();
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
    let (outcome, _) = harvest_shim(
        &spammer,
        &HarvestSeed::default(),
        Duration::from_secs(2),
        None,
    );
    let error = outcome.unwrap_err();
    assert!(
        matches!(error, HarvestError::TooLarge | HarvestError::Timeout),
        "got {error:?}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(6),
        "bounded either way"
    );
}
