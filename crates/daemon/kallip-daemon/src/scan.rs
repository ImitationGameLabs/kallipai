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

use kallip_daemon_common::wire::{HealthReport, InstanceInfo, InstanceState};

use crate::records::InstanceRecord;

/// One scanned instance: what the record area and the pointed-to data
/// directory say, without judging it.
#[derive(Debug, Clone)]
pub struct ScannedInstance {
    pub slug: String,
    pub instance_id: String,
    pub workspace: Option<String>,
    /// The instance's `runtime.json` pid when present and parse-able.
    pub pid: Option<u32>,
    /// The instance's `runtime.json` listen port. Surfaced so panel
    /// process actions (open) survive a page reload — the session-held
    /// spawn memory is the fallback, not the source.
    pub port: Option<u16>,
    /// The spawn-time uid of the requesting peer, from the record.
    pub owner: Option<u32>,
    /// The enrolled tagma identity (archeion-issued id) if the instance's
    /// own credentials tree carries one; see `read_tagma_id`.
    pub tagma_id: Option<String>,
    /// The launch-time identity anchor from the record (the
    /// `identity` key): the kernel incarnation this instance was
    /// claimed as. `None` when the claim point could not pin one —
    /// classification then falls back to the exe/comm name chain.
    pub anchored: Option<Identity>,
    /// The record's data-directory pointer, surfaced verbatim so the
    /// adopt path can check tree disjointness against every
    /// registered instance's data dir, not just its workspace.
    pub data_dir: PathBuf,
}

impl ScannedInstance {
    /// The daemon's classification: a pid verifiably this instance's
    /// live tagma is Running (launch-anchor verified), a missing pid
    /// file is a clean Stopped, and anything else (dead pid, reused
    /// pid, unidentifiable process) is Dead. Single classification
    /// source — info() and health() derive from it.
    pub fn state(&self) -> InstanceState {
        match self.pid {
            None => InstanceState::Stopped,
            Some(pid) => match classify_with_facts(self.anchored.as_ref(), pid, &self.slug).0 {
                Verdict::Match => InstanceState::Running,
                _ => InstanceState::Dead,
            },
        }
    }
    pub fn info(&self) -> InstanceInfo {
        let state = self.state();
        let live = state == InstanceState::Running;
        InstanceInfo {
            slug: self.slug.clone(),
            instance_id: self.instance_id.clone(),
            workspace: self.workspace.clone().unwrap_or_default(),
            running: live,
            state,
            // The runtime file survives a stop, so its port is only a
            // live listen port while the instance is live; anything
            // else would leak a stale, dead endpoint.
            port: live.then_some(self.port).flatten(),
            owner: self.owner,
            tagma_id: self.tagma_id.clone(),
        }
    }

    pub fn health(&self) -> HealthReport {
        let state = self.state();
        let live = state == InstanceState::Running;
        let detail = if live {
            None
        } else if self.pid.is_some() {
            Some("pid does not match a live instance (stale or reused)".to_string())
        } else {
            Some("no runtime.json".to_string())
        };
        HealthReport {
            slug: Some(self.slug.clone()),
            running: live,
            state,
            detail,
        }
    }
}

/// A live /proc entry is not enough: a zombie keeps its entry (and its
/// comm) until someone reaps it, and under a shell-PID1 init nobody
/// does. The kernel's state field tells a corpse from a live process.
pub fn pid_is_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(status) => !status
            .lines()
            .any(|l| l.starts_with("State:") && l.split_whitespace().nth(1) == Some("Z")),
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
/// directory names a kallip-tagma once leading dots (the nix wrapper
/// convention `.kallip-tagma-wrapped`) are trimmed. The store layout
/// `<hash>-kallip-tagma-<ver>/bin/kallip-tagma` matches on the binary
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
        .any(|n| n.trim_start_matches('.').starts_with("kallip-tagma"))
}
/// The comm fallback: the existing prefix rule plus the nix wrapper
/// shim's exact truncated name (comm caps at 15 bytes).
fn tagma_comm_matches(comm: &str) -> bool {
    comm.trim_start_matches("kallip-").starts_with("tagma") || comm == ".kallip-tagma-w"
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

/// `runtime.json`: the instance's runtime identity, written by the
/// tagma itself. The key set (`pid`, `port`, `starttime`) is a
/// cross-crate contract — kallip-tagma serializes its own mirror of
/// these keys and does not depend on the daemon crates, so the two
/// definitions stay in lockstep by hand. The self-report half of the
/// contract stays: the tagma keeps self-reporting its incarnation
/// start time even though the daemon verifies liveness against its
/// own `/proc` read.
#[derive(Debug, serde::Deserialize)]
pub struct RuntimeFile {
    pub pid: u32,
    pub port: u16,
    /// Mirrored for the key-set contract; not read by the daemon.
    /// `default` keeps pre-key contract files (older tagmas) parsing.
    #[allow(dead_code)]
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
            pid: runtime.as_ref().map(|r| r.pid),
            port: runtime.as_ref().map(|r| r.port),
            owner: Some(record.owner_uid),
            tagma_id: read_tagma_id(&record.data_dir),
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
/// The enrolled tagma identity: the first (sorted) credential entry
/// under the data directory's `credentials/` carrying a non-empty `tagma.id`.
fn read_tagma_id(data_dir: &Path) -> Option<String> {
    let creds = data_dir.join("credentials");
    let Ok(entries) = std::fs::read_dir(&creds) else {
        return None;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let is_dir = e.file_type().ok()?.is_dir();
            is_dir.then(|| e.file_name().into_string().ok()).flatten()
        })
        .collect();
    names.sort();
    for name in names {
        let path = creds.join(name).join("tagma.id");
        // An empty tagma.id (an enroll interrupted mid-write) is not an
        // identity: skip it and keep looking at the remaining entries.
        if let Ok(text) = std::fs::read_to_string(&path)
            && !text.trim().is_empty()
        {
            return Some(text.trim().to_owned());
        }
    }
    None
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
    fn live_foreign_process_is_not_a_match() {
        // This test binary is not kallip-tagma, so our own pid must NOT
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
        let tagma_exe = Some("/nix/store/xyz-kallip-tagma-0.1.0/bin/kallip-tagma".to_string());
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
            &facts(Some(1000), tagma_exe.clone(), Some("kallip-tagma")),
        );
        assert_eq!((v, how), (Verdict::Match, Some(Verify::Anchored)));
        // Same pid, different starttime: the pid was recycled.
        let (v, _) = classify_identity(
            Some(&anchor),
            42,
            &facts(Some(2000), tagma_exe.clone(), Some("kallip-tagma")),
        );
        assert_eq!(v, Verdict::Mismatch);
        // The runtime pid is not the anchored pid at all.
        let (v, _) = classify_identity(
            Some(&anchor),
            43,
            &facts(Some(1000), tagma_exe.clone(), Some("kallip-tagma")),
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
            &facts(Some(1000), tagma_exe.clone(), Some("kallip-tagma")),
        );
        assert_eq!((v, how), (Verdict::Match, Some(Verify::ByName)));
        // No anchor at all: name-chain matches are by-name matches.
        let (v, how) = classify_identity(
            None,
            42,
            &facts(Some(1000), tagma_exe.clone(), Some("kallip-tagma")),
        );
        assert_eq!((v, how), (Verdict::Match, Some(Verify::ByName)));
        // Wrapped binary: comm falls back to the truncated shim name.
        let (v, how) = classify_identity(None, 42, &facts(None, None, Some(".kallip-tagma-w")));
        assert_eq!((v, how), (Verdict::Match, Some(Verify::ByName)));
        // A readable exe that is not family decides against comm.
        let (v, _) = classify_identity(None, 42, &facts(None, foreign_exe, Some("kallip-tagma")));
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
    fn scan_of_a_non_tagma_runtime_process_stays_dead() {
        // The runtime.json names this very process — live, with a real
        // starttime — but no anchor vouches for it and its exe is not a
        // kallip-tagma, so the name chain refuses: Dead, never Running.
        let root = tempfile_dir("foreign-live");
        instance(&root, "a", "id", 1000);
        let pid = std::process::id();
        let starttime = proc_starttime(pid).expect("own /proc stat readable");
        write(
            &root.join("data/a/runtime.json"),
            &format!(r#"{{"pid":{pid},"port":7000,"starttime":{starttime}}}"#),
        );
        let scanned = scan_instances(&root);
        assert_eq!(scanned[0].state(), InstanceState::Dead);
        assert!(!scanned[0].info().running);
    }

    #[test]
    fn pid_comm_reads_the_test_process_name() {
        let comm = pid_comm(std::process::id()).expect("own /proc entry readable");
        assert!(!comm.is_empty());
    }

    #[test]
    fn tagma_id_reads_first_entry_and_never_the_token() {
        let root = tempfile_dir("tagma-id");
        instance(&root, "team", "id-1", 1000);
        // Two entries: alphabetical-first wins as the conservative primary.
        write(&root.join("data/team/credentials/b/tagma.id"), "tid-b\n");
        write(&root.join("data/team/credentials/a/tagma.id"), "tid-a\n");
        // The 0o600 secret sits next to the id; it must never surface.
        write(
            &root.join("data/team/credentials/a/tagma.token"),
            "sk-secret-token",
        );
        let info = scan_instances(&root)[0].info();
        assert_eq!(info.tagma_id.as_deref(), Some("tid-a"));
        let wire = serde_json::to_string(&info).expect("serialize InstanceInfo");
        assert!(!wire.contains("sk-secret-token"));
    }

    #[test]
    fn tagma_id_none_when_unenrolled_or_blank() {
        let root = tempfile_dir("tagma-id-none");
        instance(&root, "local", "id-2", 1000); // never enrolled: no credentials tree
        instance(&root, "blank", "id-3", 1000);
        write(
            &root.join("data/blank/credentials/default/tagma.id"),
            "  \n",
        );
        for scanned in scan_instances(&root) {
            assert_eq!(scanned.tagma_id, None);
        }
    }

    #[test]
    fn tagma_id_degrades_to_none_on_unreadable_credentials() {
        let root = tempfile_dir("tagma-id-unreadable");
        instance(&root, "u1", "id-4", 1000);
        // `tagma.id` as a directory: the read fails and the entry is skipped.
        std::fs::create_dir_all(root.join("data/u1/credentials/default/tagma.id"))
            .expect("fixture dir-as-file");
        instance(&root, "u2", "id-5", 1000);
        // `credentials` itself not a directory: the listing fails outright.
        write(&root.join("data/u2/credentials"), "not a dir");
        for scanned in scan_instances(&root) {
            assert_eq!(scanned.tagma_id, None);
        }
    }

    #[test]
    fn scan_surfaces_runtime_port() {
        let root = tempfile_dir("runtime-port");
        instance(&root, "a", "id-1", 1000);
        write(
            &root.join("data/a/runtime.json"),
            r#"{"pid":1,"port":7301}"#,
        );
        instance(&root, "b", "id-2", 1000);
        let scanned = scan_instances(&root);
        // The raw scan keeps the runtime file's port (pid 1 is not a
        // live tagma, so this instance is NOT Running)...
        assert_eq!(scanned[0].port, Some(7301));
        assert_eq!(scanned[0].state(), InstanceState::Dead);
        // ...but the wire only surfaces a port for a live Running instance:
        // the dead runtime.json's stale port must never reach the wire.
        assert_eq!(scanned[0].info().port, None);
        // No runtime file at all: no port anywhere.
        assert_eq!(scanned[1].port, None);
    }

    /// A guard under the managed /tmp/kallipai-dev root: the TempDir
    /// deletes the tree on drop, so nothing outlives the run (Deref/
    /// AsRef keep call sites reading as plain paths).
    struct DevDir(tempfile::TempDir);

    impl std::ops::Deref for DevDir {
        type Target = std::path::Path;

        fn deref(&self) -> &Self::Target {
            self.0.path()
        }
    }

    impl AsRef<std::path::Path> for DevDir {
        fn as_ref(&self) -> &std::path::Path {
            self.0.path()
        }
    }

    fn tempfile_dir(name: &str) -> DevDir {
        let root = std::env::temp_dir().join("kallipai-dev");
        std::fs::create_dir_all(&root).expect("create /tmp/kallipai-dev");
        DevDir(
            tempfile::Builder::new()
                .prefix(&format!("scan-test-{name}-"))
                .tempdir_in(root)
                .expect("create test tempdir"),
        )
    }
}
