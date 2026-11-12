//! Boot migration of records written before the daemon captured the roots.
//!
//! Such a record carries only the data root; the config and state roots were
//! recomputed at every use, and the log pointer rebuilt its own path from a
//! home directory. They are recorded now, so one stored answer feeds the log
//! pointer, the relaunch and every reader. This runs once at startup, before
//! anything reads a record: each old record is completed in place, and a
//! record that cannot be completed is left exactly as it was for `start` and
//! `logs` to refuse with the parsed remedies.

use std::path::Path;

use crate::records::{self, InstanceRecord};
use crate::spawn::{
    Harvest, LaunchIdentity, deployment_root_vars, identity_from_record, launch_harvest,
    prepare_launch_with, same_tree,
};

/// Complete every record under `record_root` that predates the recorded
/// roots. Returns how many records were written back.
///
/// Nothing is retried and no record is half-written (`write_record` is one
/// rename). A record left behind keeps the roots it had, so the readers that
/// need a tree refuse it with the remedies that name this migration.
pub(crate) fn migrate_records(record_root: &Path) -> usize {
    let slugs = match records::record_slugs(record_root) {
        Ok(slugs) => slugs,
        Err(error) => {
            tracing::warn!(
                root = %record_root.display(),
                error = %error,
                "listing instance records for the root migration failed"
            );
            return 0;
        }
    };
    slugs
        .iter()
        .filter(|slug| migrate_record(record_root, slug))
        .count()
}

/// One record: resolve the roots its stored environment names, then write
/// them back. `false` when nothing was written (already complete, or the
/// record could not be resolved).
fn migrate_record(record_root: &Path, slug: &str) -> bool {
    let Some(record) = records::read_record(record_root, slug) else {
        tracing::warn!(
            slug,
            cause = "unreadable",
            "instance record left as it was: it could not be read"
        );
        return false;
    };
    if record.config_dir.is_some() && record.state_dir.is_some() {
        return false;
    }
    let identity = match identity_from_record(record.target_uid, record.target_username.as_deref())
    {
        Ok(identity) => identity,
        Err(error) => {
            tracing::warn!(
                slug,
                cause = "identity",
                error = %error,
                "instance record left as it was: its target user does not resolve"
            );
            return false;
        }
    };
    let harvest = launch_harvest(&identity);
    complete(
        record_root,
        slug,
        &record,
        &identity,
        &harvest,
        &deployment_root_vars(),
    )
}

/// Complete one record from an already-resolved identity and an
/// already-harvested environment. Split from the boot walk so a test can
/// hand it a synthetic harvest instead of the host's login profile.
///
/// Only a missing root is filled: a record that already names a tree keeps
/// it. A record whose own environment resolves a different data root keeps
/// its old roots too — the three roots a record carries always come from one
/// launch that held, and this environment never launched this instance.
fn complete(
    record_root: &Path,
    slug: &str,
    record: &InstanceRecord,
    identity: &LaunchIdentity,
    harvest: &Harvest,
    deployment: &[(String, String)],
) -> bool {
    if record.config_dir.is_some() && record.state_dir.is_some() {
        return false;
    }
    let workspace = record.workspace.as_deref().unwrap_or_default();
    let resolved = match prepare_launch_with(
        slug,
        Path::new(workspace),
        &record.env,
        harvest,
        deployment,
        identity,
    ) {
        Ok((_, resolved)) => resolved,
        Err(error) => {
            tracing::warn!(
                slug,
                cause = "roots",
                error = %error,
                "instance record left as it was: its stored environment does not resolve three roots"
            );
            return false;
        }
    };
    if !same_tree(&record.data_dir, &resolved.data) {
        tracing::warn!(
            slug,
            cause = "data-root-moved",
            recorded = %record.data_dir.display(),
            resolved = %resolved.data.display(),
            "instance record left as it was: its stored environment resolves a different data root"
        );
        return false;
    }
    let mut updated = record.clone();
    if updated.config_dir.is_none() {
        updated.config_dir = Some(resolved.config);
    }
    if updated.state_dir.is_none() {
        updated.state_dir = Some(resolved.state);
    }
    match records::write_record(record_root, slug, &updated) {
        Ok(()) => {
            tracing::info!(
                slug,
                config = %updated.config_dir.as_deref().unwrap_or(Path::new("")).display(),
                state = %updated.state_dir.as_deref().unwrap_or(Path::new("")).display(),
                "instance record completed with the roots its stored environment resolves"
            );
            true
        }
        Err(error) => {
            tracing::warn!(
                slug,
                cause = "write",
                error = %error,
                "instance record left as it was: writing the completed record failed"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> LaunchIdentity {
        LaunchIdentity::InPlace {
            uid: unsafe { libc::geteuid() },
            username: Some("alice".to_string()),
        }
    }

    fn harvest() -> Harvest {
        Harvest {
            seed: vec![("HOME".to_string(), "/home/alice".to_string())],
            pairs: vec![("PATH".to_string(), "/usr/bin:/bin".to_string())],
        }
    }

    fn old_record(env: &[&str]) -> InstanceRecord {
        InstanceRecord {
            instance_id: "instance-1".to_string(),
            owner_uid: 1000,
            target_uid: unsafe { libc::geteuid() },
            target_username: Some("alice".to_string()),
            workspace: Some("/ws".to_string()),
            env: env.iter().map(|pair| pair.to_string()).collect(),
            identity: None,
            data_dir: Path::new("/home/alice/.local/share/kallipai/tagmata/instance-1").into(),
            config_dir: None,
            state_dir: None,
        }
    }

    fn record_bytes(root: &Path) -> Vec<u8> {
        std::fs::read(root.join("instance-1.json")).expect("read the record file")
    }

    /// The completed record answers both needs the old one could not: the
    /// config tree, and a log pointer under the state root the instance
    /// actually writes. The data root is untouched.
    #[test]
    fn an_old_record_is_completed_so_its_log_pointer_follows_the_state_root() {
        let root = tempfile::tempdir().expect("record root");
        let record = old_record(&[]);
        records::write_record(root.path(), "instance-1", &record).expect("write record");
        assert!(complete(
            root.path(),
            "instance-1",
            &record,
            &identity(),
            &harvest(),
            &[],
        ));
        let stored = records::read_record(root.path(), "instance-1").expect("record persists");
        assert_eq!(
            stored.config_dir.as_deref(),
            Some(Path::new("/home/alice/.config/kallipai/tagmata/instance-1"))
        );
        assert_eq!(
            stored.state_dir.as_deref(),
            Some(Path::new("/home/alice/.local/state/kallipai"))
        );
        assert_eq!(
            stored
                .logs_dir("instance-1")
                .expect("the roots are recorded"),
            Path::new("/home/alice/.local/state/kallipai/tagmata/instance-1/logs"),
            "the log pointer is the recorded state root plus the log leaf"
        );
        assert_eq!(
            stored.data_dir, record.data_dir,
            "the data root is never rewritten"
        );
    }

    /// A second pass leaves the bytes alone: the completion is skipped by
    /// the roots already present, and nothing is written at all.
    #[test]
    fn a_second_pass_changes_nothing() {
        let root = tempfile::tempdir().expect("record root");
        let record = old_record(&[]);
        records::write_record(root.path(), "instance-1", &record).expect("write record");
        assert!(complete(
            root.path(),
            "instance-1",
            &record,
            &identity(),
            &harvest(),
            &[],
        ));
        let once = record_bytes(root.path());
        let completed = records::read_record(root.path(), "instance-1").expect("record persists");
        assert!(
            !complete(
                root.path(),
                "instance-1",
                &completed,
                &identity(),
                &harvest(),
                &[],
            ),
            "a complete record is not written again"
        );
        assert_eq!(record_bytes(root.path()), once, "the bytes are unchanged");
    }

    /// A record whose own environment resolves a different data root keeps
    /// its roots: the three a record carries always come from one launch that
    /// held, and this environment never launched this instance.
    #[test]
    fn a_record_with_a_moved_data_root_keeps_its_roots() {
        let root = tempfile::tempdir().expect("record root");
        let mut record = old_record(&[]);
        record.data_dir = Path::new("/srv/elsewhere/kallipai/tagmata/instance-1").into();
        records::write_record(root.path(), "instance-1", &record).expect("write record");
        let before = record_bytes(root.path());
        assert!(
            !complete(
                root.path(),
                "instance-1",
                &record,
                &identity(),
                &harvest(),
                &[]
            ),
            "a moved data root is not completed"
        );
        assert_eq!(record_bytes(root.path()), before, "the record is untouched");
        let stored = records::read_record(root.path(), "instance-1").expect("record persists");
        assert!(
            stored.config_dir.is_none() && stored.state_dir.is_none(),
            "no root is filled from a tree this environment does not resolve"
        );
    }

    /// A torn record is reported and left exactly as it was: rewriting it
    /// would replace a file whose content nobody could read.
    #[test]
    fn a_torn_record_is_left_as_it_was() {
        let root = tempfile::tempdir().expect("record root");
        let path = root.path().join("instance-1.json");
        std::fs::write(&path, "{ not json").expect("write a torn record");
        assert_eq!(migrate_records(root.path()), 0);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read the torn record"),
            "{ not json"
        );
    }
}
