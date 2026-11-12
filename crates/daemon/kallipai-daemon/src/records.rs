//! The daemon record area: the only registry of managed instances.
//!
//! One JSON file per instance, `<slug>.json`, flat under the record root
//! (`<state home>/kallipai/daemon/instances` in user mode; an override env
//! wins verbatim). The directory IS the inventory: enumerating the json
//! files lists the instances, writing a file registers one, deleting it
//! deregisters. There is no aggregate index and no per-slug subdirectory.
//! Flat and per-slug on purpose: concurrent spawns each write their own
//! file and there is no aggregate index to corrupt, so discovery has
//! exactly one place to look — the record area, never the data trees.
//!
//! A record carries the instance's identity, spawn-time env, and anchored
//! incarnation, plus what the split layouts need: the three resolved
//! roots the instance lives in (data, configuration, state), the target
//! user the instance runs as, and the log placement derived from the
//! recorded state root.

use anyhow::Context as _;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use kallipai_daemon_common::roots;

use crate::scan::Identity;

/// The daemon's per-instance record. `data_dir` points at the tagma-owned
/// data directory (the instance's runtime.json, credentials and agents
/// live there, and the daemon never writes inside it); `config_dir` and
/// `state_dir` are the other two roots the launch resolved, so every
/// reader that needs a tree reads it here instead of rebuilding it.
/// `target_uid` / `target_username` name the user the instance runs as
/// — today always the daemon's own user; the fields exist so the
/// multi-user launch can persist the target without a format change.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InstanceRecord {
    pub instance_id: String,
    pub owner_uid: u32,
    /// The user the instance runs as. Same as `owner_uid` while the
    /// daemon launches with its own identity.
    pub target_uid: u32,
    /// Passwd name for `target_uid` when resolvable; informational.
    #[serde(default)]
    pub target_username: Option<String>,
    /// Canonical workspace the instance was spawned with; `None` only
    /// for records that opt out of overlap checks.
    #[serde(default)]
    pub workspace: Option<String>,
    /// The KEY=VALUE user env pairs the instance was spawned with,
    /// persisted so a stopped instance relaunches (via wire Start) with
    /// its original configuration. Daemon-owned keys are never stored.
    #[serde(default)]
    pub env: Vec<String>,
    /// The launch claim anchor: the pid and kernel starttime of the
    /// exact process incarnation a launch verified as its own.
    #[serde(default)]
    pub identity: Option<Identity>,
    /// Absolute path of the tagma-owned data directory.
    pub data_dir: PathBuf,
    /// The resolved configuration root (`profiles.toml`, `exec_hooks.toml`).
    /// `None` only in a record written before the roots were recorded; the
    /// boot migration fills it, and a reader that needs the tree refuses a
    /// record still in that state instead of guessing a path.
    #[serde(default)]
    pub config_dir: Option<PathBuf>,
    /// The resolved state root (every instance's log leaves live under it).
    /// `None` on the same terms as `config_dir`.
    #[serde(default)]
    pub state_dir: Option<PathBuf>,
}

/// Why a record cannot name one of the instance's trees: the record
/// predates the field, and the startup migration could not resolve that
/// root for it (the daemon log carries that record's cause). A reader
/// that needs the tree refuses with the levels the resolver reads,
/// rather than guess a path.
#[derive(Debug)]
pub struct RootsNotRecorded {
    /// The record's slug.
    pub slug: String,
    /// Which root is missing; the message names it and its remedies.
    pub kind: roots::RootKind,
}

impl std::fmt::Display for RootsNotRecorded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [own, xdg, home] = self.kind.remedies();
        write!(
            f,
            "instance record {} has no recorded {} root: the record predates the \
             field and the startup migration could not resolve one; name the root \
             with {own}, {xdg} or {home} and restart the daemon",
            self.slug,
            self.kind.noun()
        )
    }
}

impl std::error::Error for RootsNotRecorded {}

impl InstanceRecord {
    /// The recorded configuration root (`profiles.toml`, `exec_hooks.toml`).
    pub fn config_root(&self, slug: &str) -> Result<&Path, RootsNotRecorded> {
        self.config_dir.as_deref().ok_or_else(|| RootsNotRecorded {
            slug: slug.to_owned(),
            kind: roots::RootKind::Config,
        })
    }

    /// Both roots a reader other than the log pointer needs, refused as
    /// one check: a relaunch that cannot name them would leave the record
    /// unable to answer for the trees the launch just used.
    pub fn recorded_roots(&self, slug: &str) -> Result<(), RootsNotRecorded> {
        self.config_root(slug)?;
        self.state_root(slug).map(|_| ())
    }

    /// The recorded state root (instance logs live under it).
    pub fn state_root(&self, slug: &str) -> Result<&Path, RootsNotRecorded> {
        self.state_dir.as_deref().ok_or_else(|| RootsNotRecorded {
            slug: slug.to_owned(),
            kind: roots::RootKind::State,
        })
    }

    /// Where this instance's log files live: the recorded state root plus
    /// the `tagmata/<slug>/logs` leaf (one spelling, `roots::log_tree`) —
    /// the daemon never assembles a log path from a home directory.
    pub fn logs_dir(&self, slug: &str) -> Result<PathBuf, RootsNotRecorded> {
        Ok(roots::log_tree(self.state_root(slug)?, slug))
    }
}

/// Record root: an explicit override wins verbatim, else the platform
/// state home gains exactly the `kallipai/daemon/instances` segments —
/// the same shape both deployment forms use (`/var/lib` for the system
/// unit via `StateDirectory=kallipai/daemon`, `~/.local/state` for a
/// user daemon). Pure so the default shape is testable without
/// touching process-environment state.
pub fn resolve_record_root(override_dir: Option<OsString>, state_home: PathBuf) -> PathBuf {
    match override_dir {
        Some(dir) => PathBuf::from(dir),
        None => state_home.join("kallipai").join("daemon").join("instances"),
    }
}

/// The daemon's record root from the process environment:
/// `KALLIPAI_DAEMON_RECORD_DIR` verbatim, else the platform state home.
pub fn record_root() -> anyhow::Result<PathBuf> {
    match std::env::var_os("KALLIPAI_DAEMON_RECORD_DIR") {
        Some(dir) => Ok(resolve_record_root(Some(dir), PathBuf::new())),
        None => Ok(resolve_record_root(
            None,
            dirs::state_dir().context("could not determine platform state directory")?,
        )),
    }
}

/// The slugs the record area holds: one per `<slug>.json` file. A
/// staging sibling never ends in `.json`, so a write in flight stays
/// invisible here.
pub fn record_slugs(root: &Path) -> std::io::Result<Vec<String>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut slugs = Vec::new();
    for entry in entries {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some(slug) = name.strip_suffix(".json") {
            slugs.push(slug.to_owned());
        }
    }
    slugs.sort();
    Ok(slugs)
}

/// Read one record; `None` when the file is absent. A record that is
/// unreadable or unparseable also reads as `None`, but is warned about
/// first: a torn record must be visible in the log, because silently
/// dropping it hides the instance from every list while its slug looks
/// free — the destructive-recovery shape.
pub fn read_record(root: &Path, slug: &str) -> Option<InstanceRecord> {
    let text = match std::fs::read_to_string(record_path(root, slug)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(slug, error = %e, "instance record unreadable");
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(record) => Some(record),
        Err(e) => {
            tracing::warn!(slug, error = %e, "instance record unparseable");
            None
        }
    }
}

/// Stage path for an atomic record write: a pid- and sequence-suffixed
/// sibling the `*.json` enumeration never matches. The pid keeps two
/// racing daemon processes from staging into each other's file; the
/// per-write sequence keeps two threads of one process (a same-slug
/// retry storm) apart, so every write owns its staging file end to end
/// and a loser's cleanup or rewrite can never tear a winner's bytes.
fn staging_path(root: &Path, slug: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    root.join(format!("{slug}.json.{}.{}.tmp", std::process::id(), seq))
}

fn record_bytes(record: &InstanceRecord) -> std::io::Result<Vec<u8>> {
    serde_json::to_vec(record)
        .map_err(|e| std::io::Error::other(format!("serializing instance record: {e}")))
}

/// The record is owner-only. Its env can carry an enrollment code (a live
/// credential until it is spent) or any other `-e` value, and the daemon is
/// its only reader, so no other user on the host needs it.
fn restrict_to_owner(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Register a new record with an exclusive, atomic publish: the full
/// JSON is staged beside the record, then `hard_link`ed into place.
/// link(2) fails with `AlreadyExists` when a record for the slug
/// already exists, so of two concurrent spawns of one slug exactly one
/// registers and the loser learns so without a read-then-write window —
/// a pre-check plus an overwriting write would let both pass and make
/// the registry last-writer-wins while two tagmas run on one data
/// directory (split brain). The overwrite path (`write_record`) cannot
/// give this: rename(2) silently replaces.
pub fn create_record(root: &Path, slug: &str, record: &InstanceRecord) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    let staged = staging_path(root, slug);
    std::fs::write(&staged, record_bytes(record)?)?;
    restrict_to_owner(&staged)?;
    let published = std::fs::hard_link(&staged, record_path(root, slug));
    let _ = std::fs::remove_file(&staged);
    published
}

/// Update a record in one atomic overwrite: the full JSON is staged,
/// then rename(2)d over the record — a concurrent reader sees either
/// the whole old or the whole new file, never a torn one, and a crash or
/// a full disk mid-write cannot leave a half-written record that reads
/// back as unparseable.
pub fn write_record(root: &Path, slug: &str, record: &InstanceRecord) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    let staged = staging_path(root, slug);
    std::fs::write(&staged, record_bytes(record)?)?;
    restrict_to_owner(&staged)?;
    match std::fs::rename(&staged, record_path(root, slug)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&staged);
            Err(e)
        }
    }
}

/// Deregister: the record file goes away. `Ok` when it is already gone.
pub fn delete_record(root: &Path, slug: &str) -> std::io::Result<()> {
    match std::fs::remove_file(record_path(root, slug)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn record_path(root: &Path, slug: &str) -> PathBuf {
    root.join(format!("{slug}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(data_dir: &Path) -> InstanceRecord {
        InstanceRecord {
            instance_id: "instance-1".into(),
            owner_uid: 1000,
            target_uid: 1000,
            target_username: Some("simplex".into()),
            workspace: Some("/ws".into()),
            env: vec!["K=V".into()],
            identity: None,
            data_dir: data_dir.to_path_buf(),
            config_dir: Some(data_dir.join("config")),
            state_dir: Some(data_dir.join("state")),
        }
    }

    #[test]
    fn resolve_record_root_overrides_win_verbatim() {
        assert_eq!(
            resolve_record_root(Some("/custom/records".into()), PathBuf::from("/xdg/state")),
            PathBuf::from("/custom/records")
        );
    }

    #[test]
    fn resolve_record_root_default_joins_state_segments() {
        assert_eq!(
            resolve_record_root(None, PathBuf::from("/xdg/state")),
            PathBuf::from("/xdg/state/kallipai/daemon/instances")
        );
    }

    #[test]
    fn record_round_trips_by_slug_filename() {
        let root = tempfile::tempdir().expect("tempdir");
        write_record(root.path(), "team", &sample(root.path())).expect("write");
        let record = read_record(root.path(), "team").expect("record");
        assert_eq!(record.instance_id, "instance-1");
        assert_eq!(record.data_dir, root.path());
        // A different slug does not see it.
        assert!(read_record(root.path(), "other").is_none());
        delete_record(root.path(), "team").expect("delete");
        assert!(read_record(root.path(), "team").is_none());
        // Deleting an absent record is success (idempotent deregister).
        delete_record(root.path(), "team").expect("delete again");
    }

    #[test]
    fn create_record_is_exclusive_and_the_loser_gets_already_exists() {
        let root = tempfile::tempdir().expect("tempdir");
        create_record(root.path(), "team", &sample(root.path())).expect("first create");
        // A second registration of the same slug must lose (AlreadyExists),
        // not silently overwrite: the first writer's record is what a read
        // sees, so two racing spawns cannot both claim one data directory.
        let mut second = sample(root.path());
        second.instance_id = "instance-2".into();
        let error =
            create_record(root.path(), "team", &second).expect_err("second create of a taken slug");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        let record = read_record(root.path(), "team").expect("record");
        assert_eq!(record.instance_id, "instance-1");
    }

    #[test]
    fn record_writes_consume_their_staging_file() {
        let root = tempfile::tempdir().expect("tempdir");
        create_record(root.path(), "team", &sample(root.path())).expect("create");
        let mut updated = sample(root.path());
        updated.instance_id = "instance-2".into();
        write_record(root.path(), "team", &updated).expect("update");
        // Both the link-publish and the rename-overwrite consume the
        // staging sibling: the directory holds exactly the one record
        // file, so the `*.json` enumeration never sees a partial.
        let mut names: Vec<String> = std::fs::read_dir(root.path())
            .expect("read_dir")
            .map(|e| {
                e.expect("entry")
                    .file_name()
                    .into_string()
                    .expect("utf8 name")
            })
            .collect();
        names.sort();
        assert_eq!(names, vec!["team.json".to_string()]);
        let record = read_record(root.path(), "team").expect("record");
        assert_eq!(record.instance_id, "instance-2");
    }

    #[test]
    fn concurrent_creates_of_one_slug_elect_a_single_winner() {
        let root = tempfile::tempdir().expect("tempdir");
        // Per-write staging names keep this race exact: shared staging
        // would let a loser's cleanup drop the winner's staged file
        // (wrong error kind) or interleave loser bytes into the
        // winner's publish (a rejected request's config registered).
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let root = root.path().to_path_buf();
                std::thread::spawn(move || {
                    let mut record = sample(&root);
                    record.instance_id = format!("racer-{i}");
                    create_record(&root, "duel", &record).map(|()| i)
                })
            })
            .collect();
        let mut winners = Vec::new();
        for handle in handles {
            match handle.join().expect("join") {
                Ok(i) => winners.push(i),
                Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists),
            }
        }
        assert_eq!(winners.len(), 1);
        let record = read_record(root.path(), "duel").expect("record");
        assert_eq!(record.instance_id, format!("racer-{}", winners[0]));
    }

    #[test]
    fn staging_names_are_unique_per_write() {
        // Two writes of one slug in one process (a retry storm on two
        // threads) must never share a staging file: a shared name let a
        // loser's cleanup drop the winner's staged bytes mid-publish.
        let root = PathBuf::from("/unused");
        let a = staging_path(&root, "team");
        let b = staging_path(&root, "team");
        assert_ne!(a, b);
    }

    #[cfg(unix)]
    #[test]
    fn the_record_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().expect("tempdir");
        create_record(root.path(), "team", &sample(root.path())).expect("create");
        let path = root.path().join("team.json");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        // The env can carry an enrollment code, a live credential until it is
        // spent; the record stays unreadable to every other user on the host.
        assert_eq!(mode, 0o600, "record mode is {mode:04o}");
    }
}
