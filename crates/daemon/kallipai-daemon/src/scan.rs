//! The record area is the registry: instance discovery reads the daemon's
//! per-slug records, never the data tree.
//!
//! Every record points at the instance's tagma-owned data directory; the
//! volatile runtime facts (`runtime.json`: pid, port, starttime) are read
//! through that pointer. `list`/`health` re-read everything fresh on every
//! call, so a daemon restart rebuilds the full view from the record area.
//!
//! Liveness verification has one root: the launch anchor the daemon's
//! own spawn wrote into the record (state `running`). An instance the
//! daemon has no record for is not the daemon's business.

use std::path::{Path, PathBuf};

use kallipai_daemon_common::wire::{HealthReport, InstanceIdentity, InstanceInfo, InstanceState};

use crate::records::InstanceRecord;

/// One scanned instance: what the record area and the pointed-to data
/// directory say, without judging it.
#[derive(Debug, Clone)]
pub struct ScannedInstance {
    pub slug: String,
    pub instance_id: String,
    pub workspace: Option<String>,
    /// The instance's `runtime.json`: the tagma's own pid, listen port and
    /// incarnation start time, kept as the file it is. The port matters only
    /// while the incarnation it came from is the live one, and the start time
    /// is the fallback pin for a record that carries none.
    pub runtime: Option<RuntimeFile>,
    /// The spawn-time uid of the requesting peer, from the record.
    pub owner: Option<u32>,
    /// The enrolled identities the instance's own credentials tree carries:
    /// the archeion-issued id plus the polis each entry belongs to, in
    /// credentials-listing order. Empty when it carries none.
    pub identities: Vec<InstanceIdentity>,
    /// The launch-time identity anchor from the record (the
    /// `identity` key): the kernel incarnation this instance was
    /// claimed as. `None` when the claim point could not pin one —
    /// the runtime self-report answers for it instead.
    pub anchored: Option<Identity>,
    /// The record's data-directory pointer, surfaced verbatim so the
    /// adopt path can check tree disjointness against every
    /// registered instance's data dir, not just its workspace.
    pub data_dir: PathBuf,
}

impl ScannedInstance {
    /// The daemon's classification, from the one resolution: `Live` and
    /// `Unverifiable` both mean Running (the pid is alive either way — an
    /// unpinned one is still someone running), `Refuted` is Dead, and no
    /// claim at all is a clean Stopped. Single rule — info() and health()
    /// derive from it.
    pub fn state(&self) -> InstanceState {
        self.state_with(self.resolved())
    }

    /// The classification for a resolution the caller already made: one
    /// `/proc` read answers a whole `info()`.
    fn state_with(&self, resolved: Option<Resolution>) -> InstanceState {
        match resolved {
            Some(resolution) if resolution.verdict == Liveness::Refuted => InstanceState::Dead,
            Some(_) => InstanceState::Running,
            None => InstanceState::Stopped,
        }
    }

    /// The one resolution: anchor first, self-report second, read once so
    /// every field derived from it describes the same frame.
    fn resolved(&self) -> Option<Resolution> {
        resolve(
            self.anchored.as_ref(),
            self.runtime.as_ref(),
            &incarnation_liveness,
        )
    }
    pub fn info(&self) -> InstanceInfo {
        let resolved = self.resolved();
        let state = self.state_with(resolved);
        let live = state == InstanceState::Running;
        InstanceInfo {
            slug: self.slug.clone(),
            instance_id: self.instance_id.clone(),
            workspace: self.workspace.clone().unwrap_or_default(),
            running: live,
            state,
            // The runtime file survives a stop, and callers reach the
            // instance through the port it names: surface that port only
            // for a claim that names this very incarnation. A stale file,
            // one the anchor outvoted, or an unpinned pid (someone running,
            // not provably this instance) never becomes a live endpoint.
            port: self.live_port(resolved),
            owner: self.owner,
            identities: self.identities.clone(),
        }
    }

    pub fn health(&self) -> HealthReport {
        let resolved = self.resolved();
        let state = self.state_with(resolved);
        let live = state == InstanceState::Running;
        // The detail explains what the pid alone does not: an unpinned live
        // pid is Running but its claim is weaker, and a refuted one says
        // whether the runtime file or the record's anchor was the stale side.
        let detail = match resolved {
            Some(resolution) if resolution.verdict == Liveness::Unverifiable => {
                Some("alive, but no start time pins it".to_string())
            }
            Some(resolution) if resolution.verdict == Liveness::Refuted => Some(
                if self.runtime.is_some() {
                    "pid does not match a live instance (stale or reused)"
                } else {
                    "the launch anchor's pid is not live"
                }
                .to_string(),
            ),
            Some(_) => None,
            None => Some(self.stopped_detail()),
        };
        HealthReport {
            slug: Some(self.slug.clone()),
            running: live,
            state,
            detail,
        }
    }

    /// The pid this instance's `runtime.json` names, live or not.
    /// Diagnostic (the reconcile log names it); the verdict comes from
    /// [`Self::state`].
    pub fn runtime_pid(&self) -> Option<u32> {
        self.runtime.as_ref().map(|runtime| runtime.pid)
    }

    /// The listen port to surface: the runtime file's, and only while the
    /// resolution names that file's own pid as the live incarnation. A file
    /// the anchor outvoted, one whose pid died, and one that carries no
    /// start time all withhold it: an unpinned pid is someone running, not
    /// provably this instance, and callers reach the endpoint with the
    /// instance's own credentials.
    fn live_port(&self, resolved: Option<Resolution>) -> Option<u16> {
        let runtime = self.runtime.as_ref()?;
        let resolution = resolved?;
        (resolution.pid == runtime.pid && resolution.verdict == Liveness::Live)
            .then_some(runtime.port)
    }

    /// The detail for an instance with no claim at all. Something at the
    /// path that yields no claim — a half-written file, a dangling link, a
    /// directory — is a different situation from an empty data dir: the
    /// first is damage, and telling an operator "no runtime.json" would send
    /// them looking for a file that is there.
    fn stopped_detail(&self) -> String {
        if std::fs::symlink_metadata(self.data_dir.join("runtime.json")).is_ok() {
            "runtime.json is unreadable".to_string()
        } else {
            "no runtime.json".to_string()
        }
    }
}

/// A live /proc entry is not enough: a zombie keeps its entry (and its
/// comm) until someone reaps it, and under a shell-PID1 init nobody
/// does. The kernel's state field tells a corpse from a live process: `Z`
/// is a zombie, `X` a task the kernel has already marked dead.
pub fn pid_is_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(status) => !status.lines().any(|l| {
            l.starts_with("State:") && matches!(l.split_whitespace().nth(1), Some("Z" | "X"))
        }),
        Err(_) => false,
    }
}
/// The process name from `/proc/<pid>/comm`, whitespace-trimmed. `None`
/// when the entry is unreadable (the process is gone). This is the
/// diagnostic surface for launch-timeout logs: recording the actual
/// comm value is what makes a naming mismatch investigable.
pub fn pid_comm(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|c| c.trim().to_owned())
}
/// The launch-time identity anchor persisted in the instance record: the
/// pid plus the kernel start time of the exact process incarnation a
/// launch claimed. `starttime` is the reuse discriminator (a recycled
/// pid gets a fresh start time); `anchored_at` is a wall-clock
/// diagnostic of when the claim happened, never compared.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Identity {
    pub pid: u32,
    pub starttime: u64,
    #[serde(default)]
    pub anchored_at: u64,
}
/// The kernel start time from `/proc/<pid>/stat` (field 22, clock
/// ticks since boot): the incarnation discriminator that survives pid
/// reuse. Parsed after the comm's closing paren because comm may
/// contain spaces, digits, and parentheses of its own.
pub fn proc_starttime(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rfind(')')? + 1;
    stat[after_comm..].split_whitespace().nth(19)?.parse().ok()
}
/// The resolved executable path (`/proc/<pid>/exe`). `None` when the
/// link cannot be read — the process is gone, or the reader lacks
/// permission (same-uid always has it).
pub fn pid_exe(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}
/// Family check on an exe path: the binary's own name or its parent
/// directory names a kallipai-tagma once leading dots (the nix wrapper
/// convention `.kallipai-tagma-wrapped`) are trimmed. The store layout
/// `<hash>-kallipai-tagma-<ver>/bin/kallipai-tagma` matches on the binary
/// name, and a binary rebuilt underneath a running process keeps
/// matching through the ` (deleted)` suffix.
fn tagma_exe_family(exe: &str) -> bool {
    let path = std::path::Path::new(exe);
    let file = path.file_name().and_then(|n| n.to_str());
    let parent = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str());
    [file, parent]
        .into_iter()
        .flatten()
        .any(|n| n.trim_start_matches('.').starts_with("kallipai-tagma"))
}
/// The comm fallback: the existing prefix rule plus the nix wrapper
/// shim's exact truncated name (comm caps at 15 bytes).
fn tagma_comm_matches(comm: &str) -> bool {
    comm.trim_start_matches("kallipai-").starts_with("tagma") || comm == ".kallipai-tagma-w"
}
/// The identity verdict for a recorded pid: is this live process
/// this instance's tagma — the anchored incarnation?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Live and positively identified as this instance's tagma.
    Match,
    /// Live, but provably not a verifiable incarnation of this
    /// instance: the anchor denies it, the pid was reused, or a
    /// foreign process occupies it.
    Mismatch,
    /// Live, but every identity probe failed — unverifiable.
    Unknown,
    /// No live process (dead or zombie).
    Gone,
}
/// How a `Verdict::Match` verified. An anchor verify is the daemon's
/// own launch claim (`running`); a name verify is the degraded chain
/// worth a warn (still `running`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verify {
    Anchored,
    ByName,
}
/// What /proc says about a pid, gathered once for classification.
#[derive(Debug, Default, Clone)]
pub struct ProcFacts {
    pub starttime: Option<u64>,
    pub exe: Option<String>,
    pub comm: Option<String>,
}
/// Read the identity facts for `pid` in one pass.
pub fn observe_identity(pid: u32) -> ProcFacts {
    ProcFacts {
        starttime: proc_starttime(pid),
        exe: pid_exe(pid),
        comm: pid_comm(pid),
    }
}
/// Pure classification over the anchored identity and the observed
/// facts. The `Verify` is `Some` only on a Match: which root verified —
/// the launch anchor (exact incarnation) or the exe/comm name chain
/// (degraded). Level order: existence (a dead pid is quietly
/// `Gone`), a positive anchor verify, the anti split-brain anchor
/// denial (a live second claimant must not pass while the anchored
/// incarnation is still up), then the name chain for a launch whose
/// claim point could not pin an anchor.
pub(crate) fn classify_identity(
    anchored: Option<&Identity>,
    pid: u32,
    facts: &ProcFacts,
) -> (Verdict, Option<Verify>) {
    if !pid_is_alive(pid) {
        return (Verdict::Gone, None);
    }
    if let Some(anchor) = anchored {
        if anchor.pid == pid && anchor.starttime != 0 && facts.starttime == Some(anchor.starttime) {
            return (Verdict::Match, Some(Verify::Anchored));
        }
        // The anchor names a different live incarnation: the instance
        // already has its daemon-launched tagma, so a second live
        // claimant must not pass (split brain, or a runtime.json
        // retargeted at someone else's process).
        if anchor.pid != pid && anchor_incarnation_live(anchor) {
            return (Verdict::Mismatch, None);
        }
    }
    // No anchor vouched for this pid (the claim point could not pin
    // one) — the degraded chain: the anchor must at least not name a
    // different or differently-timed incarnation, then the exe/comm
    // name chain decides.
    if let Some(anchor) = anchored {
        if anchor.pid != pid {
            return (Verdict::Mismatch, None);
        }
        if anchor.starttime != 0
            && let Some(starttime) = facts.starttime
            && starttime != anchor.starttime
        {
            return (Verdict::Mismatch, None);
        }
    }
    let family_match = match (&facts.exe, &facts.comm) {
        (Some(exe), _) => tagma_exe_family(exe),
        (None, Some(comm)) => tagma_comm_matches(comm),
        (None, None) => return (Verdict::Unknown, None),
    };
    if family_match {
        (Verdict::Match, Some(Verify::ByName))
    } else {
        (Verdict::Mismatch, None)
    }
}

/// Whether the anchor's own claimed incarnation is still live. A live
/// pid whose starttime matches the anchor is the claiming process; an
/// alive-but-unverifiable pid (a legacy anchor without a starttime, or
/// an unreadable /proc) counts as claiming — a second claimant is
/// refused rather than risk two live tagmas on one instance.
fn anchor_incarnation_live(anchor: &Identity) -> bool {
    if !pid_is_alive(anchor.pid) {
        return false;
    }
    match proc_starttime(anchor.pid) {
        Some(starttime) => anchor.starttime == 0 || starttime == anchor.starttime,
        None => true,
    }
}
/// Classify a recorded pid against `anchored`, warning exactly when the
/// verdict is a degraded by-name Match: the one "live, but only by
/// name" case worth investigating.
fn classify_with_facts(
    anchored: Option<&Identity>,
    pid: u32,
    slug: &str,
) -> (Verdict, Option<Verify>) {
    let facts = observe_identity(pid);
    let (verdict, verify) = classify_identity(anchored, pid, &facts);
    if verify == Some(Verify::ByName) {
        tracing::warn!(
            slug = %slug,
            pid,
            comm = ?facts.comm,
            "pid matches tagma only by name (launch anchor missing or unverified)"
        );
    }
    (verdict, verify)
}
/// Classification for callers holding a record rather than a scanned
/// struct: classifies the recorded pid against the record's anchor.
pub fn identity_matches(record: &InstanceRecord, slug: &str, pid: u32) -> Verdict {
    classify_with_facts(record.identity.as_ref(), pid, slug).0
}

/// The kernel's answer to "is the process at `pid` the incarnation
/// `starttime` names": the whole liveness judgement, and the only place a
/// pid is compared against the kernel.
///
/// Three answers, because two are not enough. `Live` and `Refuted` are
/// verdicts; `Unverifiable` is the absence of one — a live pid whose file
/// carries no start time (older tagmas wrote `0` when `/proc/self/stat`
/// was unreadable). Reading that cell as `Refuted` would call a running
/// instance dead and let a second tagma start on its data tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// Alive, and the pin names this very process.
    Live,
    /// Dead, or alive under a pin that names a different incarnation.
    Refuted,
    /// Alive, but nothing pins it: no start time to compare.
    Unverifiable,
}

/// Judge one claim: does `starttime` pin the process at `pid`? A zombie or
/// a dead entry is `Refuted`; a zero pin is `Unverifiable`, because the
/// file says "I do not know", not "not me".
///
/// Binary names, records and sockets are the callers' business: this reads
/// the kernel and nothing else.
pub fn incarnation_liveness(pid: u32, starttime: u64) -> Liveness {
    if !pid_is_alive(pid) {
        return Liveness::Refuted;
    }
    if starttime == 0 {
        return Liveness::Unverifiable;
    }
    if proc_starttime(pid) == Some(starttime) {
        Liveness::Live
    } else {
        Liveness::Refuted
    }
}

/// Which claim vouched for a live incarnation: the record's launch anchor
/// (the daemon pinned it at the launch point) or the instance's own
/// `runtime.json` self-report (the only claim a hand-booted tagma, one
/// started without the daemon, leaves behind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveSource {
    Anchored,
    SelfReported,
}

/// What an instance's claims add up to: the claim that won, and the answer
/// it carries. A `Refuted` resolution still names a claim, so a diagnostic
/// can say which one was refuted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub verdict: Liveness,
    pub pid: u32,
    pub starttime: u64,
    pub source: LiveSource,
}

/// Resolve an instance's claims into one answer: the launch anchor first
/// (the daemon's own claim), the runtime self-report second (a hand boot
/// keeps the data tree running without touching the record).
///
/// `Live` outranks `Unverifiable` — an anchor that still verifies outvotes
/// a file that lost its pin — and `Unverifiable` outranks `Refuted`, since
/// "alive but unpinned" is still someone running. Anchor order decides
/// between equals. `None` means no claim at all.
///
/// `judge` is the three-valued judgement, injected so tests can fake it.
pub fn resolve(
    anchor: Option<&Identity>,
    runtime: Option<&RuntimeFile>,
    judge: &dyn Fn(u32, u64) -> Liveness,
) -> Option<Resolution> {
    let claims: Vec<Resolution> = [
        anchor.map(|anchor| (anchor.pid, anchor.starttime, LiveSource::Anchored)),
        runtime.map(|runtime| (runtime.pid, runtime.starttime, LiveSource::SelfReported)),
    ]
    .into_iter()
    .flatten()
    .map(|(pid, starttime, source)| Resolution {
        verdict: judge(pid, starttime),
        pid,
        starttime,
        source,
    })
    .collect();
    claims
        .iter()
        .find(|claim| claim.verdict == Liveness::Live)
        .or_else(|| {
            claims
                .iter()
                .find(|claim| claim.verdict == Liveness::Unverifiable)
        })
        .or_else(|| claims.first())
        .copied()
}

/// The uid that owns `/proc/<pid>`: the process's own uid, as the kernel
/// reports it for the entry. `None` when the entry is gone or unreadable —
/// a pid that cannot be attributed is not one to signal.
pub fn proc_uid(pid: u32) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(format!("/proc/{pid}"))
        .ok()
        .map(|meta| meta.uid())
}

/// Send `sig` to `pid` only when the kernel attributes it to `uid`.
///
/// A signal is the one thing here that cannot be taken back, and every pid
/// this crate signals comes from a file the instance's owner can write. So
/// the ownership check sits next to the `kill` instead of at a distance: an
/// entry that is gone or unreadable is a refusal too, because a pid that
/// cannot be attributed cannot be proven ours.
pub fn signal_owned(pid: u32, uid: u32, sig: i32) -> Result<(), SignalError> {
    match proc_uid(pid) {
        Some(owner) if owner == uid => {}
        owner => {
            return Err(SignalError::Foreign {
                pid,
                expected: uid,
                owner,
            });
        }
    }
    let rc = unsafe { libc::kill(pid as i32, sig) };
    if rc == 0 {
        Ok(())
    } else {
        Err(SignalError::Kill {
            pid,
            sig,
            error: std::io::Error::last_os_error(),
        })
    }
}

/// Why a signal was not sent. Not one failure but two kinds: the kernel
/// does not attribute the pid to the expected uid (a refusal, and the caller
/// knows what it means), or `kill(2)` itself failed (the only internal
/// fault).
#[derive(Debug)]
pub enum SignalError {
    /// The kernel does not attribute this pid to `expected`. `owner` is
    /// `None` when `/proc` holds no readable entry (the pid is gone).
    Foreign {
        pid: u32,
        expected: u32,
        owner: Option<u32>,
    },
    /// `kill(2)` returned an error.
    Kill {
        pid: u32,
        sig: i32,
        error: std::io::Error,
    },
}

impl std::fmt::Display for SignalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignalError::Foreign {
                pid,
                expected,
                owner: Some(owner),
            } => write!(
                f,
                "pid {pid} belongs to uid {owner}, not uid {expected}; not signalling it"
            ),
            SignalError::Foreign { pid, .. } => {
                write!(
                    f,
                    "pid {pid} has no readable /proc entry; not signalling it"
                )
            }
            SignalError::Kill { pid, sig, error } => {
                write!(f, "kill({pid}, {sig}): {error}")
            }
        }
    }
}

/// `runtime.json`: the instance's runtime identity, written by the
/// tagma itself. The key set (`pid`, `port`, `starttime`) is a
/// cross-crate contract — kallipai-tagma serializes its own mirror of
/// these keys and does not depend on the daemon crates, so the two
/// definitions stay in lockstep by hand. The daemon reads all three:
/// `starttime` is the incarnation this instance published for itself, the
/// fallback pin when the record carries no launch anchor, and it is
/// compared against the daemon's own `/proc` read, never trusted as text.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RuntimeFile {
    pub pid: u32,
    pub port: u16,
    /// The incarnation the instance published for itself: the fallback pin
    /// when the record carries none. `default` keeps files written before
    /// the key existed parsing (a zero pin verifies nothing).
    #[serde(default)]
    pub starttime: u64,
}
/// Enumerate the record area: every `<slug>.json` is one managed
/// instance. The runtime facts and the enrolled identity are read
/// through the record's data-directory pointer.
pub fn scan_instances(record_root: &Path) -> Vec<ScannedInstance> {
    let entries = match std::fs::read_dir(record_root) {
        // A missing record area is an empty registry; any other read
        // failure is an environment fault that must not pass silently
        // as an empty registry.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!(
                root = %record_root.display(),
                error = %e,
                "record area unreadable; reporting an empty registry"
            );
            return Vec::new();
        }
        Ok(entries) => entries,
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            tracing::warn!(path = %path.display(), "instance record unreadable; skipped");
            continue;
        };
        let Ok(record) = serde_json::from_str::<InstanceRecord>(&text) else {
            tracing::warn!(path = %path.display(), "instance record unparseable; skipped");
            continue;
        };
        let runtime = read_runtime(&record.data_dir);
        out.push(ScannedInstance {
            slug: slug.to_owned(),
            instance_id: record.instance_id,
            // An empty-string workspace (a hand-written or half-written
            // record) must read back as absent, not as a zero-component
            // path that would compare oddly against real workspaces.
            workspace: record.workspace.filter(|w| !w.is_empty()),
            runtime,
            owner: Some(record.owner_uid),
            identities: kallipai_daemon_common::credentials::read_enrolled_identities(
                &record.data_dir,
            ),
            anchored: record.identity,
            data_dir: record.data_dir,
        });
    }
    out.sort_by(|a, b| a.slug.cmp(&b.slug));
    out
}
pub(crate) fn read_runtime(dir: &Path) -> Option<RuntimeFile> {
    let text = std::fs::read_to_string(dir.join("runtime.json")).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("fixture parent dir");
        }
        std::fs::write(path, text).expect("write fixture");
    }

    /// One record-area file plus the data tree it points at. Tests add
    /// runtime.json / credentials inside the data tree as needed.
    fn instance(root: &Path, slug: &str, instance_id: &str, owner_uid: u32) {
        let data_dir = root.join("data").join(slug);
        std::fs::create_dir_all(&data_dir).expect("data dir");
        let record = format!(
            r#"{{"instance_id":"{instance_id}","owner_uid":{owner_uid},"target_uid":{owner_uid},"data_dir":{}}}"#,
            serde_json::to_string(&data_dir).expect("json path"),
        );
        write(&root.join(format!("{slug}.json")), &record);
    }

    fn instance_with_workspace(root: &Path, slug: &str, instance_id: &str, workspace: &str) {
        let data_dir = root.join("data").join(slug);
        std::fs::create_dir_all(&data_dir).expect("data dir");
        let record = format!(
            r#"{{"instance_id":"{instance_id}","owner_uid":1000,"target_uid":1000,"workspace":{},"data_dir":{}}}"#,
            serde_json::to_string(workspace).expect("json ws"),
            serde_json::to_string(&data_dir).expect("json path"),
        );
        write(&root.join(format!("{slug}.json")), &record);
    }

    #[test]
    fn scan_skips_non_record_files_and_sorts_by_slug() {
        let root = tempfile_dir("skips");
        instance(&root, "beta", "id-2", 1001);
        instance_with_workspace(&root, "alpha", "id-1", "/tmp/w");
        // An unparseable record file is skipped (with a warn), not a
        // hard error: one corrupt file cannot hide the registry.
        write(&root.join("broken.json"), "{ not json");
        write(&root.join("noise.txt"), "x"); // not a `*.json` record
        std::fs::create_dir(root.join("noise.json")).expect("noise dir");

        let scanned = scan_instances(&root);
        let slugs: Vec<_> = scanned.iter().map(|s| s.slug.as_str()).collect();
        assert_eq!(slugs, ["alpha", "beta"]);
        assert_eq!(scanned[0].instance_id, "id-1");
        assert_eq!(scanned[0].workspace.as_deref(), Some("/tmp/w"));
        assert_eq!(scanned[1].owner, Some(1001));
        assert_eq!(scanned[1].workspace, None);
        instance_with_workspace(&root, "gamma", "id-3", "");
        // An empty-string workspace must read back as absent, not as a
        // zero-component path that overlaps everything.
        assert_eq!(scan_instances(&root)[2].workspace, None);
    }

    #[test]
    fn dead_pid_reports_not_running_with_detail() {
        let root = tempfile_dir("dead-pid");
        instance(&root, "a", "id", 1000);
        write(
            &root.join("data/a/runtime.json"),
            r#"{"pid":99999999,"port":1}"#,
        );
        let scanned = scan_instances(&root);
        assert_eq!(scanned[0].state(), InstanceState::Dead);
        let health = scanned[0].health();
        assert!(!health.running);
        assert_eq!(health.state, InstanceState::Dead);
        assert!(
            health.detail.unwrap().contains("pid"),
            "stale pid named in detail"
        );
    }

    #[test]
    fn no_runtime_file_means_not_running() {
        let root = tempfile_dir("no-pid");
        instance(&root, "a", "id", 1000);
        let scanned = scan_instances(&root);
        let health = scanned[0].health();
        assert!(!health.running);
        assert_eq!(health.state, InstanceState::Stopped);
        assert_eq!(health.detail.as_deref(), Some("no runtime.json"));
    }

    #[test]
    fn an_unparseable_runtime_file_is_named_as_damage() {
        // The file is there but does not parse: there are no claims to read,
        // and "no runtime.json" would send an operator looking for a file
        // that exists.
        let root = tempfile_dir("broken-runtime");
        instance(&root, "a", "id", 1000);
        write(&root.join("data/a/runtime.json"), "{\"pid\":");
        let health = scan_instances(&root)[0].health();
        assert_eq!(health.state, InstanceState::Stopped);
        assert_eq!(health.detail.as_deref(), Some("runtime.json is unreadable"));
    }

    #[test]
    fn signal_owned_refuses_a_pid_it_cannot_attribute() {
        // Signal 0 asks the kernel without delivering anything: the
        // attribution is what this primitive is for.
        let uid = unsafe { libc::getuid() };
        let mine = signal_owned(std::process::id(), uid, 0);
        assert!(mine.is_ok(), "{mine:?}");
        let foreign = signal_owned(std::process::id(), uid.wrapping_add(1), 0).unwrap_err();
        assert!(
            matches!(
                foreign,
                SignalError::Foreign {
                    owner: Some(owner),
                    expected,
                    ..
                } if owner == uid && expected == uid.wrapping_add(1)
            ),
            "{foreign}"
        );
        let gone = signal_owned(u32::MAX, uid, 0).unwrap_err();
        assert!(
            matches!(gone, SignalError::Foreign { owner: None, .. }),
            "{gone}"
        );
    }

    #[test]
    fn live_foreign_process_is_not_a_match() {
        // This test binary is not kallipai-tagma, so our own pid must NOT
        // count even though it is alive: the exe/comm fallback checks
        // are load-bearing, not decoration.
        let pid = std::process::id();
        let (verdict, verify) = classify_identity(None, pid, &observe_identity(pid));
        assert_eq!(verdict, Verdict::Mismatch);
        assert!(verify.is_none());
    }

    #[test]
    fn classify_matrix() {
        let anchor = Identity {
            pid: 42,
            starttime: 1000,
            anchored_at: 7,
        };
        let tagma_exe = Some("/nix/store/xyz-kallipai-tagma-0.1.0/bin/kallipai-tagma".to_string());
        let foreign_exe = Some("/usr/bin/sleep".to_string());
        let facts = |starttime: Option<u64>, exe: Option<String>, comm: Option<&str>| ProcFacts {
            starttime,
            exe,
            comm: comm.map(str::to_string),
        };
        // Anchor level: same pid, same starttime — the exact incarnation.
        let (v, how) = classify_identity(
            Some(&anchor),
            42,
            &facts(Some(1000), tagma_exe.clone(), Some("kallipai-tagma")),
        );
        assert_eq!((v, how), (Verdict::Match, Some(Verify::Anchored)));
        // Same pid, different starttime: the pid was recycled.
        let (v, _) = classify_identity(
            Some(&anchor),
            42,
            &facts(Some(2000), tagma_exe.clone(), Some("kallipai-tagma")),
        );
        assert_eq!(v, Verdict::Mismatch);
        // The runtime pid is not the anchored pid at all.
        let (v, _) = classify_identity(
            Some(&anchor),
            43,
            &facts(Some(1000), tagma_exe.clone(), Some("kallipai-tagma")),
        );
        assert_eq!(v, Verdict::Mismatch);
        // Anchor present but starttime unreadable: falls below the
        // anchor; a family exe still matches, but by name.
        let (v, how) = classify_identity(Some(&anchor), 42, &facts(None, tagma_exe.clone(), None));
        assert_eq!((v, how), (Verdict::Match, Some(Verify::ByName)));
        // A hand-crafted zero starttime in the anchor cannot anchor
        // anything (the launch path never writes one): the anchor
        // level is skipped and the name chain decides.
        let zero_anchor = Identity {
            pid: 42,
            starttime: 0,
            anchored_at: 7,
        };
        let (v, how) = classify_identity(
            Some(&zero_anchor),
            42,
            &facts(Some(1000), tagma_exe.clone(), Some("kallipai-tagma")),
        );
        assert_eq!((v, how), (Verdict::Match, Some(Verify::ByName)));
        // No anchor at all: name-chain matches are by-name matches.
        let (v, how) = classify_identity(
            None,
            42,
            &facts(Some(1000), tagma_exe.clone(), Some("kallipai-tagma")),
        );
        assert_eq!((v, how), (Verdict::Match, Some(Verify::ByName)));
        // Wrapped binary: comm falls back to the truncated shim name.
        let (v, how) = classify_identity(None, 42, &facts(None, None, Some(".kallipai-tagma-w")));
        assert_eq!((v, how), (Verdict::Match, Some(Verify::ByName)));
        // A readable exe that is not family decides against comm.
        let (v, _) = classify_identity(None, 42, &facts(None, foreign_exe, Some("kallipai-tagma")));
        assert_eq!(v, Verdict::Mismatch);
        // Nothing readable on a live process: unverifiable.
        let (v, _) = classify_identity(None, 42, &facts(None, None, None));
        assert_eq!(v, Verdict::Unknown);
        // A dead pid is Gone — quiet, never verified (u32::MAX names no
        // process; kernel pids cap far below it).
        let (v, how) = classify_identity(None, u32::MAX, &ProcFacts::default());
        assert_eq!((v, how), (Verdict::Gone, None));
    }

    #[test]
    fn a_runtime_self_report_is_the_fallback_pin() {
        // No anchor: the record predates anchoring, or the claim write
        // never landed. The runtime file's own start time is then the only
        // pin, and it is checked against /proc like any other, so a
        // hand-booted tagma keeps its instance Running without the daemon
        // having pinned anything. The verdict does not consult the binary
        // name: a debug build, a rename and a nix wrapper all drift, the
        // kernel's start time does not.
        let root = tempfile_dir("self-reported");
        instance(&root, "a", "id", 1000);
        let pid = std::process::id();
        let starttime = proc_starttime(pid).expect("own /proc stat readable");
        write(
            &root.join("data/a/runtime.json"),
            &format!(r#"{{"pid":{pid},"port":7000,"starttime":{starttime}}}"#),
        );
        let scanned = scan_instances(&root);
        assert_eq!(scanned[0].state(), InstanceState::Running);
        assert!(scanned[0].info().running);
    }

    #[test]
    fn a_runtime_self_report_with_a_wrong_pin_is_dead() {
        // The same live pid, a start time that is not its own: the pid was
        // recycled, or the file was hand-made. A pin that does not name
        // the live incarnation vouches for nothing.
        let root = tempfile_dir("self-reported-stale");
        instance(&root, "a", "id", 1000);
        let pid = std::process::id();
        let starttime = proc_starttime(pid).expect("own /proc stat readable") + 1;
        write(
            &root.join("data/a/runtime.json"),
            &format!(r#"{{"pid":{pid},"port":7000,"starttime":{starttime}}}"#),
        );
        let scanned = scan_instances(&root);
        assert_eq!(scanned[0].state(), InstanceState::Dead);
        assert_eq!(scanned[0].runtime_pid(), Some(pid));
        assert_eq!(scanned[0].info().port, None);
    }

    #[test]
    fn pid_comm_reads_the_test_process_name() {
        let comm = pid_comm(std::process::id()).expect("own /proc entry readable");
        assert!(!comm.is_empty());
    }

    #[test]
    fn identities_report_every_enrolled_entry_with_its_polis() {
        let root = tempfile_dir("identities");
        instance(&root, "team", "id-1", 1000);
        // Two entries under two polis origins, written out of name order: the
        // report follows the credentials listing, not the creation order.
        write(&root.join("data/team/credentials/b/tagma.id"), "tid-b\n");
        write(
            &root.join("data/team/credentials/b/tagma.token"),
            "sk-secret-b",
        );
        write(
            &root.join("data/team/credentials/b/archeion.url"),
            "https://api.kallipai.lan\n",
        );
        write(&root.join("data/team/credentials/a/tagma.id"), "tid-a\n");
        write(
            &root.join("data/team/credentials/a/tagma.token"),
            "sk-secret-a",
        );
        write(
            &root.join("data/team/credentials/a/archeion.url"),
            "https://api.kallipai.com\n",
        );
        let info = scan_instances(&root)[0].info();
        let pairs: Vec<(&str, &str)> = info
            .identities
            .iter()
            .map(|identity| (identity.polis.as_str(), identity.tagma_id.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("https://api.kallipai.com", "tid-a"),
                ("https://api.kallipai.lan", "tid-b"),
            ]
        );
        // The 0o600 secrets sit next to the ids; they must never surface.
        let wire = serde_json::to_string(&info).expect("serialize InstanceInfo");
        assert!(!wire.contains("sk-secret-a"));
        assert!(!wire.contains("sk-secret-b"));
        // The client mirrors this shape: a rename or a dropped field on
        // either side leaves the panel joining nothing.
        let identities = serde_json::json!([
            {"polis": "https://api.kallipai.com", "tagma_id": "tid-a"},
            {"polis": "https://api.kallipai.lan", "tagma_id": "tid-b"},
        ]);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&wire).expect("parse the wire")["identities"],
            identities
        );
    }

    #[test]
    fn an_entry_without_a_polis_origin_is_not_reported() {
        let root = tempfile_dir("identities-no-url");
        instance(&root, "team", "id-1", 1000);
        // Enrolled (id + token), but with no origin recorded: no polis to
        // attribute it to, so the scan leaves it out. The tagma writes one
        // on its next start, and the entry joins the list then.
        write(
            &root.join("data/team/credentials/old/tagma.id"),
            "tid-old\n",
        );
        write(
            &root.join("data/team/credentials/old/tagma.token"),
            "sk-secret-old",
        );
        write(
            &root.join("data/team/credentials/new/tagma.id"),
            "tid-new\n",
        );
        write(
            &root.join("data/team/credentials/new/tagma.token"),
            "sk-secret-new",
        );
        write(
            &root.join("data/team/credentials/new/archeion.url"),
            "https://api.kallipai.com\n",
        );
        let info = scan_instances(&root)[0].info();
        let ids: Vec<&str> = info
            .identities
            .iter()
            .map(|identity| identity.tagma_id.as_str())
            .collect();
        assert_eq!(ids, ["tid-new"], "the entry without a polis is absent");
        let wire = serde_json::to_string(&info).expect("serialize InstanceInfo");
        assert!(!wire.contains("sk-secret-old"));
    }

    #[test]
    fn identities_are_empty_when_unenrolled_or_blank() {
        let root = tempfile_dir("identities-none");
        instance(&root, "local", "id-2", 1000); // never enrolled: no credentials tree
        instance(&root, "blank", "id-3", 1000);
        write(
            &root.join("data/blank/credentials/default/tagma.id"),
            "  \n",
        );
        for scanned in scan_instances(&root) {
            assert!(scanned.identities.is_empty());
        }
    }

    #[test]
    fn identities_degrade_to_empty_on_unreadable_credentials() {
        let root = tempfile_dir("identities-unreadable");
        instance(&root, "u1", "id-4", 1000);
        // `tagma.id` as a directory: the read fails and the entry is skipped.
        std::fs::create_dir_all(root.join("data/u1/credentials/default/tagma.id"))
            .expect("fixture dir-as-file");
        instance(&root, "u2", "id-5", 1000);
        // `credentials` itself not a directory: the listing fails outright.
        write(&root.join("data/u2/credentials"), "not a dir");
        for scanned in scan_instances(&root) {
            assert!(scanned.identities.is_empty());
        }
    }

    #[test]
    fn an_unpinned_runtime_keeps_running_but_withholds_its_port() {
        let root = tempfile_dir("runtime-port");
        // An alive pid with no pinned start time: Unverifiable, which still
        // counts as Running — but nothing names it as this incarnation, so
        // the port it wrote is not published to callers.
        instance(&root, "a", "id-1", 1000);
        let pid = std::process::id();
        write(
            &root.join("data/a/runtime.json"),
            &format!(r#"{{"pid":{pid},"port":7301}}"#),
        );
        // A pid no kernel can hand out: nothing is running, and the dead
        // file's port never reaches the wire.
        instance(&root, "b", "id-2", 1000);
        write(
            &root.join("data/b/runtime.json"),
            r#"{"pid":4294967295,"port":7302,"starttime":5}"#,
        );
        let scanned = scan_instances(&root);
        assert_eq!(
            scanned[0].runtime.as_ref().map(|runtime| runtime.port),
            Some(7301)
        );
        assert_eq!(scanned[0].state(), InstanceState::Running);
        assert_eq!(
            scanned[0].info().port,
            None,
            "an unpinned pid is not provably this instance"
        );
        // A pin the kernel confirms: the file names this very incarnation,
        // so its port is this instance's endpoint.
        instance(&root, "c", "id-3", 1000);
        let starttime = proc_starttime(pid).expect("own /proc stat readable");
        write(
            &root.join("data/c/runtime.json"),
            &format!(r#"{{"pid":{pid},"port":7303,"starttime":{starttime}}}"#),
        );
        let scanned = scan_instances(&root);
        assert_eq!(
            scanned[0].health().detail.as_deref(),
            Some("alive, but no start time pins it")
        );
        assert_eq!(scanned[2].state(), InstanceState::Running);
        assert_eq!(scanned[2].info().port, Some(7303));
        assert_eq!(scanned[2].health().detail, None);
        assert_eq!(scanned[1].state(), InstanceState::Dead);
        assert_eq!(scanned[1].info().port, None);
        assert_eq!(
            scanned[1].health().detail.as_deref(),
            Some("pid does not match a live instance (stale or reused)")
        );
    }

    #[test]
    fn the_anchor_outvotes_a_stale_runtime_file() {
        // The record's anchor pins this very process (the test's own /proc
        // entry stands in for a launch). runtime.json still names a dead pid
        // from the previous incarnation: the instance is Running, and the
        // stale file's port is not surfaced.
        let root = tempfile_dir("anchor-first");
        let data_dir = root.join("data").join("a");
        std::fs::create_dir_all(&data_dir).expect("data dir");
        let pid = std::process::id();
        let starttime = proc_starttime(pid).expect("own /proc stat readable");
        crate::records::write_record(
            &root,
            "a",
            &crate::records::InstanceRecord {
                instance_id: "id".into(),
                owner_uid: 1000,
                target_uid: 1000,
                target_username: None,
                workspace: Some("/ws".into()),
                env: vec![],
                identity: Some(Identity {
                    pid,
                    starttime,
                    anchored_at: 7,
                }),
                data_dir: data_dir.clone(),
                config_dir: None,
                state_dir: None,
            },
        )
        .expect("write record");
        write(
            &data_dir.join("runtime.json"),
            r#"{"pid":7,"port":7301,"starttime":5}"#,
        );

        let scanned = scan_instances(&root);
        assert_eq!(scanned[0].state(), InstanceState::Running);
        assert!(scanned[0].info().running);
        assert_eq!(
            scanned[0].info().port,
            None,
            "the stale file's port is not this instance's"
        );
    }

    #[test]
    fn incarnation_liveness_answers_three_ways() {
        let pid = std::process::id();
        let starttime = proc_starttime(pid).expect("own /proc stat readable");
        assert_eq!(incarnation_liveness(pid, starttime), Liveness::Live);
        assert_eq!(
            incarnation_liveness(pid, 0),
            Liveness::Unverifiable,
            "a zero pin says \"I do not know\", not \"not me\""
        );
        assert_eq!(
            incarnation_liveness(pid, starttime + 1),
            Liveness::Refuted,
            "a pin from another incarnation is not this process"
        );
        assert_eq!(
            incarnation_liveness(u32::MAX, 1),
            Liveness::Refuted,
            "no process, no verdict"
        );
    }

    #[test]
    fn resolve_prefers_live_then_unverifiable_and_names_the_claim() {
        let anchor = Identity {
            pid: 42,
            starttime: 1000,
            anchored_at: 7,
        };
        let runtime = RuntimeFile {
            pid: 43,
            port: 1,
            starttime: 2000,
        };
        // Both claims verify: the anchor is the daemon's own word.
        let both_live = |_pid: u32, _start: u64| Liveness::Live;
        let resolution = resolve(Some(&anchor), Some(&runtime), &both_live).expect("a claim");
        assert_eq!(
            (resolution.pid, resolution.source, resolution.verdict),
            (42, LiveSource::Anchored, Liveness::Live)
        );
        // The anchor's incarnation is gone: the self-report answers.
        let only_self = |pid: u32, _start: u64| {
            if pid == 43 {
                Liveness::Live
            } else {
                Liveness::Refuted
            }
        };
        let resolution = resolve(Some(&anchor), Some(&runtime), &only_self).expect("a claim");
        assert_eq!(
            (
                resolution.pid,
                resolution.starttime,
                resolution.source,
                resolution.verdict
            ),
            (43, 2000, LiveSource::SelfReported, Liveness::Live)
        );
        // Live is a verdict and Unverifiable the lack of one, so the live
        // self-report outranks an unpinned anchor.
        let unpinned_anchor = |pid: u32, _start: u64| {
            if pid == 42 {
                Liveness::Unverifiable
            } else {
                Liveness::Live
            }
        };
        let resolution = resolve(Some(&anchor), Some(&runtime), &unpinned_anchor).expect("a claim");
        assert_eq!(
            (resolution.pid, resolution.source, resolution.verdict),
            (43, LiveSource::SelfReported, Liveness::Live)
        );
        // Neither verifies, but one is alive and unpinned: still someone
        // running, and the pid to look at.
        let unpinned_self = |pid: u32, _start: u64| {
            if pid == 43 {
                Liveness::Unverifiable
            } else {
                Liveness::Refuted
            }
        };
        let resolution = resolve(Some(&anchor), Some(&runtime), &unpinned_self).expect("a claim");
        assert_eq!(
            (resolution.pid, resolution.source, resolution.verdict),
            (43, LiveSource::SelfReported, Liveness::Unverifiable)
        );
        // Nothing alive: the first claim is reported as refuted, so a
        // diagnostic can name what was looked at.
        let neither = |_pid: u32, _start: u64| Liveness::Refuted;
        let resolution = resolve(Some(&anchor), Some(&runtime), &neither).expect("a claim");
        assert_eq!(
            (resolution.pid, resolution.source, resolution.verdict),
            (42, LiveSource::Anchored, Liveness::Refuted)
        );
        assert!(
            resolve(None, None, &both_live).is_none(),
            "no claim, no answer"
        );
    }

    use kallipai_testkit::DevDir;

    fn tempfile_dir(name: &str) -> DevDir {
        DevDir::new(&format!("scan-test-{name}"))
    }
}
