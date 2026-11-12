//! Stop the instances a test fixture's daemon registered.
//!
//! Shared by every fixture that boots a daemon: a daemon death leaves its
//! instances running, because the launch helper double-forks and calls
//! `setsid`, and the instance outlives the daemon under init.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a signalled process gets before the force kill, and how long the
/// force kill itself gets to land. One pair for every fixture: the tagma
/// harness, the daemon fixtures and the instance stop all wait the same way.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(20);
pub const KILL_GRACE: Duration = Duration::from_secs(2);

/// Stop every instance the fixture's daemon registered and report the pids
/// that survived.
///
/// An instance is not the daemon's child process tree: the launch helper
/// double-forks and calls `setsid`, so killing the daemon leaves the instance
/// running under init. The production stop path goes first (it judges identity
/// and escalates SIGTERM to SIGKILL); when it cannot settle the record, the
/// record's `data_dir`, the instance's `runtime.json` and the group the launch
/// created are the fallbacks.
fn stop_instances(socket: &Path, records: &Path) -> Vec<u32> {
    let mut survivors = Vec::new();
    let Ok(entries) = std::fs::read_dir(records) else {
        return survivors;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(slug) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let answered = stop_through_socket(socket, slug);
        let data_dir = recorded_data_dir(&path);
        let pid = data_dir.as_deref().and_then(recorded_pid);
        let anchor = anchor_pid(&path);
        // The verb answering is not the end of it: a group whose leader is gone
        // can still hold a re-exec'd child, so the record settles only when the
        // pid is gone and no member of the launch group is left.
        let group_clear = anchor.is_none_or(|anchor| group_pids(anchor).is_empty());
        let settled = match pid {
            // A named pid says all there is to say: gone is settled, whatever
            // the verb answered (an instance can exit before the call lands).
            Some(pid) => !process_running(pid) && group_clear,
            None => answered && group_clear,
        };
        if settled {
            continue;
        }
        // The tagma re-execs as it drops privileges, so the pid the record
        // names can be gone while a child of the group the launch created
        // still runs. The group is the unit the fixture owns either way.
        // Ownership is the record's pinned start time, nothing else: a pid
        // number without that pin is an integer, and an integer is not a
        // licence to signal a process.
        // The pin can come from the launch record or from the instance's own
        // `runtime.json`: both name a start time, and a pid plus its start time
        // is the incarnation. Where a fixture launched before anchoring, neither
        // carries one, and the instance is reported rather than signalled.
        let pinned = anchored_start(&path).or_else(|| data_dir.as_deref().and_then(runtime_start));
        let owns = pid.is_some_and(|pid| instance_owns(pid, pinned));
        if owns && stop_pid(pid.expect("owns implies a pid")) {
            // A pid that died holding its children is not a settled record. The
            // fall-through tries the group, and with the leader gone there is no
            // pin left to point at it, so those members end up in `survivors`
            // instead of being signalled.
            if anchor.is_none_or(|anchor| group_pids(anchor).is_empty()) {
                continue;
            }
        }
        // The group is the launch's only if a pinned pid is still alive in it:
        // the leader, or the recorded pid itself when a re-exec moved the group
        // membership around. A member nobody pinned cannot prove the group is
        // ours, so a group with no pinned survivor is reported, not signalled.
        let group_owns = anchor.is_some_and(|anchor| {
            pinned.is_some_and(|pinned| starttime_of(anchor) == Some(pinned))
                || pid.is_some_and(|pid| {
                    instance_owns(pid, pinned) && unsafe { libc::getpgid(pid) } == anchor
                })
        });
        if group_owns
            && let Some(anchor) = anchor
            && stop_group(anchor)
        {
            continue;
        }
        // Report the pids that are alive, not the numbers that led to them: the
        // recorded pid may be the one a kill just took, and a reader chasing a
        // corpse learns nothing.
        let mut alive = anchor.map(group_pids).unwrap_or_default();
        if let Some(pid) = pid
            && process_running(pid)
            && !alive.contains(&pid)
        {
            alive.push(pid);
        }
        for member in alive {
            let member = member as u32;
            if !survivors.contains(&member) {
                survivors.push(member);
            }
        }
    }
    survivors
}

/// The pid the launch pinned: the instance's own process, a member of the
/// session the spawn created (the spawn's `setsid` runs in an intermediate
/// child, so the instance is not the session leader). Cleanup works in
/// groups, not sessions, because one signal has to reach the few processes
/// that belong together (`kill -- -<pgid>`); naming a leak across a larger
/// session is `scripts/leakcheck.sh`'s job, and the two report different
/// words on purpose.
fn anchor_pid(record_path: &Path) -> Option<i32> {
    let identity = record_json(record_path)?;
    i32::try_from(identity.get("identity")?.get("pid")?.as_u64()?).ok()
}

/// The kernel start time the record pinned at launch: the incarnation a pid
/// number belonged to, which a recycled number no longer matches.
fn anchored_start(record_path: &Path) -> Option<u64> {
    let identity = record_json(record_path)?;
    identity
        .get("identity")?
        .get("starttime")?
        .as_u64()
        // A launch that did not pin wrote 0; that is "unknown", not a pin that
        // could stand in front of the instance's own start time.
        .filter(|starttime| *starttime > 0)
}

fn record_json(record_path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(record_path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Field 22 of `/proc/<pid>/stat`: the process start time in clock ticks.
fn starttime_of(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// The real uid of a process, or `None` when it is gone. A signal cannot be
/// aimed at a stranger, so the group path checks this before it fires, and the
/// field it reads is the one the comparison uses.
fn uid_of(pid: i32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|line| line.starts_with("Uid:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Never signal a number the record cannot vouch for: a pid whose kernel start
/// time equals the one the record pinned is that incarnation, whoever it runs
/// as. `/proc/<pid>/environ` is not a substitute, because the parent directory
/// of a path in some process's environment says nothing about who wrote it.
///
/// A record without a pin predates anchoring: its instance is reported, never
/// signalled.
fn instance_owns(pid: i32, anchor: Option<u64>) -> bool {
    if pid <= 1 || pid == std::process::id() as i32 {
        return false;
    }
    let Some(anchor) = anchor else {
        return false;
    };
    starttime_of(pid) == Some(anchor)
}

/// The daemon's stop verb: identity check, SIGTERM, grace, SIGKILL.
///
/// The call runs on its own thread with its own runtime: a fixture that drops
/// inside an async test already owns one, and a nested `block_on` panics.
fn stop_through_socket(socket: &Path, slug: &str) -> bool {
    let socket = socket.to_path_buf();
    let slug = slug.to_owned();
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return false;
        };
        let client = kallipai_daemon_client::DaemonClient::new(&socket);
        let request = kallipai_daemon_common::wire::RequestBody::Stop { slug };
        // A daemon that accepts the connection and never answers must not hang
        // the fixture: a timeout reads as "not answered" and the record path
        // takes over.
        let answered = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), client.call(request))
                .await
                .ok()
        });
        match answered {
            Some(Ok(response)) => matches!(
                response.body,
                kallipai_daemon_common::wire::ResponseBody::Ok { .. }
            ),
            _ => false,
        }
    })
    .join()
    .unwrap_or(false)
}

fn recorded_data_dir(record_path: &Path) -> Option<PathBuf> {
    let record = record_json(record_path)?;
    Some(PathBuf::from(record.get("data_dir")?.as_str()?))
}

/// The start time an instance wrote about itself in `runtime.json`. The
/// instance fills this at boot, so it is the same kind of evidence as the
/// launcher's pin — and it exists where the launch record carries none.
fn runtime_start(data_dir: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(data_dir.join("runtime.json")).ok()?;
    let runtime: serde_json::Value = serde_json::from_str(&text).ok()?;
    runtime.get("starttime")?.as_u64()
}

fn recorded_pid(data_dir: &Path) -> Option<i32> {
    let text = std::fs::read_to_string(data_dir.join("runtime.json")).ok()?;
    let runtime: serde_json::Value = serde_json::from_str(&text).ok()?;
    i32::try_from(runtime.get("pid")?.as_u64()?).ok()
}

/// The live processes that belong to `pgid`, so a report can name the pids a
/// reader has to deal with rather than the group number that led there.
fn group_pids(pgid: i32) -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| group_of(*pid) == Some(pgid) && process_running(*pid))
        .collect()
}

/// Field 5 of `/proc/<pid>/stat`: the process group.
fn group_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(2)?.parse().ok()
}

pub fn process_running(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return false;
    };
    !matches!(rest.trim_start().chars().next(), Some('Z' | 'X'))
}

/// Signal a whole process group, never our own and never init's.
fn signal_group(pgid: i32, sig: i32) -> bool {
    if pgid <= 1 || pgid == unsafe { libc::getpgid(0) } {
        return false;
    }
    unsafe { libc::kill(-pgid, sig) == 0 }
}

fn stop_group(pgid: i32) -> bool {
    if group_pids(pgid).is_empty() {
        return true;
    }
    // A group is a number too: never signal one whose leader is not a process of
    // ours, which a recycled pid would otherwise make possible.
    if uid_of(pgid) != Some(unsafe { libc::getuid() }) {
        return false;
    }
    let _ = signal_group(pgid, libc::SIGTERM);
    if wait_until(SHUTDOWN_GRACE, || group_pids(pgid).is_empty()) {
        return true;
    }
    let _ = signal_group(pgid, libc::SIGKILL);
    wait_until(KILL_GRACE, || group_pids(pgid).is_empty())
}

fn stop_pid(pid: i32) -> bool {
    let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
    if wait_until(SHUTDOWN_GRACE, || !process_running(pid)) {
        return true;
    }
    let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
    wait_until(KILL_GRACE, || !process_running(pid))
}

/// Poll `done` until it holds or `grace` runs out, and answer whether it held.
fn wait_until(grace: Duration, done: impl Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + grace;
    loop {
        if done() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Stop the fixture's instances, reap the daemon, and hand back the pids that
/// outlived the stop. The order is the point: a daemon that dies first takes
/// the stop verb with it.
pub fn tear_down(
    socket: &Path,
    records: &Path,
    sandbox: &Path,
    daemon: &mut std::process::Child,
) -> Vec<u32> {
    // The records area names the instances this fixture will signal, so it has
    // to be the fixture's own: a wrong path here stops real processes instead of
    // leaking a test instance.
    crate::sandbox::assert_contained("records", records, &[sandbox]);
    crate::sandbox::assert_contained("socket", socket, &[sandbox]);
    let survivors = stop_instances(socket, records);
    let _ = daemon.kill();
    let _ = daemon.wait();
    survivors
}

/// The process group a live pid belongs to; `None` when the lookup failed, which
/// the report says out loud rather than guessing a group.
fn process_group_of(pid: u32) -> Option<i32> {
    let group = unsafe { libc::getpgid(pid as i32) };
    (group > 0).then_some(group)
}

/// Fail a fixture that left an instance running: a panic names the pids and the
/// process groups they belong to (a survivor's group is what a reader needs to
/// clear the rest of it), and a fixture already unwinding only logs them so the
/// original panic survives.
pub fn blame_survivors(survivors: &[u32]) {
    if survivors.is_empty() {
        return;
    }
    let named: Vec<String> = survivors
        .iter()
        .map(|pid| match process_group_of(*pid) {
            Some(group) => format!("{pid} (group {group})"),
            None => format!("{pid} (group unknown)"),
        })
        .collect();
    let report = format!("fixture left instances running: pids {}", named.join(", "));
    if std::thread::panicking() {
        eprintln!("{report}");
    } else {
        panic!("{report}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_group_of_names_the_group_a_pid_is_in() {
        let pid = std::process::id();
        assert_eq!(
            process_group_of(pid),
            Some(unsafe { libc::getpgid(pid as i32) })
        );
    }

    #[test]
    fn process_group_of_declines_a_pid_that_cannot_exist() {
        assert_eq!(process_group_of(4_000_000_000), None);
    }
}
