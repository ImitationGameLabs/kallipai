//! Deregistration semantics: the record goes away, the process does
//! not. Stop first when the goal is a stopped instance — remove never
//! signals anything, so a running instance keeps running unmanaged
//! (its record, and with it the daemon's ability to stop it, is gone).
use std::path::Path;

use kallip_daemon_common::wire::{ErrorCode, valid_slug};

#[derive(Debug, thiserror::Error)]
pub enum RemoveError {
    #[error("no instance named {0}")]
    NotFound(String),
    #[error("remove of {slug} failed: {message}")]
    Internal { slug: String, message: String },
}

impl From<&RemoveError> for ErrorCode {
    fn from(error: &RemoveError) -> Self {
        match error {
            RemoveError::NotFound(_) => ErrorCode::NotFound,
            RemoveError::Internal { .. } => ErrorCode::Internal,
        }
    }
}

/// Blocking deregister. Idempotent by contract: a slug with no record
/// removes as a successful no-op — for a peer who may remove at all.
/// Authorization reads the record's registration owner (the owner
/// themselves or root, the re-registration rule); a foreign peer gets
/// NotFound, distinguishable from the missing-slug no-op — an
/// existence signal the spawn path already gives via SlugTaken.
/// The true cause stays in the log.
pub fn remove(record_root: &Path, slug: &str, peer_uid: u32) -> Result<(), RemoveError> {
    // Same grammar gate as stop: an invalid slug is not an instance
    // name, so it cannot exist — NotFound without a record-area read
    // (the slug must not become a path probe).
    if !valid_slug(slug) {
        return Err(RemoveError::NotFound(slug.to_string()));
    }
    let Some(record) = crate::records::read_record(record_root, slug) else {
        return Ok(());
    };
    if !crate::spawn::authorized(peer_uid, record.owner_uid) {
        tracing::warn!(
            slug = %slug,
            peer_uid,
            record_owner = record.owner_uid,
            "remove denied: foreign peer"
        );
        return Err(RemoveError::NotFound(slug.to_string()));
    }
    crate::records::delete_record(record_root, slug).map_err(|e| RemoveError::Internal {
        slug: slug.to_string(),
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::InstanceRecord;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn record(data_dir: &Path) -> InstanceRecord {
        InstanceRecord {
            instance_id: uuid::Uuid::new_v4().to_string(),
            owner_uid: unsafe { libc::geteuid() },
            target_uid: unsafe { libc::geteuid() },
            target_username: None,
            workspace: None,
            env: Vec::new(),
            identity: None,
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// The roundtrip the wire contract promises: remove takes the
    /// record away, and the second remove is a successful no-op.
    #[test]
    fn remove_is_an_idempotent_roundtrip() {
        let root = tempdir();
        let dd = root.path().join("dd");
        std::fs::create_dir_all(&dd).expect("data dir");
        crate::records::create_record(root.path(), "team-a", &record(&dd)).expect("seed");
        assert!(crate::records::read_record(root.path(), "team-a").is_some());
        remove(root.path(), "team-a", unsafe { libc::geteuid() }).expect("first remove");
        assert!(crate::records::read_record(root.path(), "team-a").is_none());
        remove(root.path(), "team-a", unsafe { libc::geteuid() })
            .expect("second remove is a no-op");
    }

    #[test]
    fn remove_refuses_an_invalid_slug_before_touching_the_record_area() {
        let root = tempdir();
        let error = remove(root.path(), "Not_A_Slug", unsafe { libc::geteuid() }).unwrap_err();
        assert!(matches!(error, RemoveError::NotFound(_)), "{error}");
    }

    /// A foreign peer's NotFound is distinguishable from a missing
    /// slug's no-op — and the record survives the attempt.
    #[test]
    fn remove_denies_a_foreign_peer_and_keeps_the_record() {
        let root = tempdir();
        let dd = root.path().join("dd");
        std::fs::create_dir_all(&dd).expect("data dir");
        let mut seeded = record(&dd);
        seeded.owner_uid = 65535;
        crate::records::create_record(root.path(), "team-a", &seeded).expect("seed");
        let error = remove(root.path(), "team-a", 65534).unwrap_err();
        assert!(matches!(error, RemoveError::NotFound(_)), "{error}");
        let kept = crate::records::read_record(root.path(), "team-a").expect("record survives");
        assert_eq!(kept.instance_id, seeded.instance_id);
    }
}
