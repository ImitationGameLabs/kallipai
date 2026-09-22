//! Persisted env management for managed instances: read the record's
//! user env pairs, merge new pairs into them, or remove keys. All three
//! verbs share the stop verb's authorization shape (the instance owner
//! or root; a foreign peer is refused after the record is read) and
//! spawn's env validation for anything they write. None of them touch
//! a running process: the record is the only thing that changes, and
//! the new pairs take effect on the next start.

use crate::records::InstanceRecord;
use kallip_daemon_common::wire::ErrorCode;
use std::path::Path;

/// The env verbs' failure vocabulary, mapped onto the wire's stable
/// error codes the same way the other verbs map theirs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EnvError {
    /// No managed instance carries this slug (also fired for a slug
    /// that fails the grammar gate, without a record-area read).
    NotFound(String),
    /// A foreign peer addressed an instance it does not own.
    Denied { slug: String, target_uid: u32 },
    /// The request itself is invalid: a rejected env pair, or an
    /// unset naming keys the record does not carry.
    Invalid(String),
    /// Anything that should not fail but did (record area I/O).
    Internal { slug: String, message: String },
}

impl From<&EnvError> for ErrorCode {
    fn from(error: &EnvError) -> Self {
        match error {
            EnvError::NotFound(_) => ErrorCode::NotFound,
            EnvError::Denied { .. } => ErrorCode::Denied,
            EnvError::Invalid(_) => ErrorCode::BadRequest,
            EnvError::Internal { .. } => ErrorCode::Internal,
        }
    }
}

impl std::fmt::Display for EnvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvError::NotFound(slug) => write!(f, "no such instance: {slug}"),
            EnvError::Denied { slug, target_uid } => {
                write!(f, "not authorized: {slug} is owned by uid {target_uid}")
            }
            EnvError::Invalid(message) => write!(f, "{message}"),
            EnvError::Internal { slug, message } => {
                write!(f, "{slug}: {message}")
            }
        }
    }
}

/// Grammar gate + record read + authorization, shared by all three
/// verbs. The grammar check precedes any record-area access (the slug
/// must not become a path probe); authorization fires after the read,
/// like stop — slug existence is already public via SlugTaken, so the
/// refusal names the owner instead of hiding it.
fn authorized_record(
    record_root: &Path,
    slug: &str,
    peer_uid: u32,
) -> Result<InstanceRecord, EnvError> {
    if !kallip_daemon_common::wire::valid_slug(slug) {
        return Err(EnvError::NotFound(slug.to_string()));
    }
    let Some(record) = crate::records::read_record(record_root, slug) else {
        return Err(EnvError::NotFound(slug.to_string()));
    };
    if !crate::spawn::authorized(peer_uid, record.target_uid) {
        tracing::warn!(
            slug = %slug,
            peer_uid,
            target_uid = record.target_uid,
            "env verb denied: foreign peer"
        );
        return Err(EnvError::Denied {
            slug: slug.to_string(),
            target_uid: record.target_uid,
        });
    }
    Ok(record)
}

/// The record's user env pairs, verbatim and unmasked: the pairs are
/// operator-authored configuration (spawn -e wrote them), and the
/// list-and-copy workflow needs the exact bytes back.
pub(crate) fn env_get(
    record_root: &Path,
    slug: &str,
    peer_uid: u32,
) -> Result<Vec<String>, EnvError> {
    let record = authorized_record(record_root, slug, peer_uid)?;
    Ok(record.env)
}
/// Merge the given KEY=VALUE pairs into the record's env: existing
/// keys are replaced in place and new keys are appended. A key with
/// several existing copies (spawn batches may write duplicates) is
/// replaced at the first position and the remaining copies removed,
/// mirroring env_unset's removal of every copy. The request
/// list is validated exactly like spawn's request env (same shape,
/// same allowlist, same reserved-key refusals). An empty request is
/// refused: removing keys belongs to env_unset.
pub(crate) fn env_set(
    record_root: &Path,
    slug: &str,
    peer_uid: u32,
    env: &[String],
) -> Result<(), EnvError> {
    let record = authorized_record(record_root, slug, peer_uid)?;
    crate::spawn::validate_user_env(env).map_err(|error| EnvError::Invalid(error.to_string()))?;
    if env.is_empty() {
        return Err(EnvError::Invalid(
            "no pairs provided; to remove keys use env unset".to_string(),
        ));
    }
    let mut updated = record.env.clone();
    for pair in env {
        let key = env_key(pair);
        let mut replaced = false;
        updated.retain_mut(|existing| {
            if env_key(existing) == key {
                if replaced {
                    return false;
                }
                *existing = pair.clone();
                replaced = true;
            }
            true
        });
        if !replaced {
            updated.push(pair.clone());
        }
    }
    let updated = InstanceRecord {
        env: updated,
        ..record
    };
    crate::records::write_record(record_root, slug, &updated).map_err(|e| EnvError::Internal {
        slug: slug.to_string(),
        message: format!("record write: {e}"),
    })?;
    Ok(())
}

/// Remove keys from the record's env. Atomic across the batch: if any
/// requested key is absent from the record the whole request fails and
/// names every missing key (git config --unset's shape — an explicit
/// error beats a silent no-op). On success the removed keys are
/// returned in request order.
pub(crate) fn env_unset(
    record_root: &Path,
    slug: &str,
    peer_uid: u32,
    keys: &[String],
) -> Result<Vec<String>, EnvError> {
    let record = authorized_record(record_root, slug, peer_uid)?;
    let missing: Vec<&String> = keys
        .iter()
        .filter(|key| !record.env.iter().any(|pair| env_key(pair) == Some(*key)))
        .collect();
    if !missing.is_empty() {
        let listed = missing
            .iter()
            .map(|key| format!("{key:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(EnvError::Invalid(format!(
            "env key(s) not present on {slug}: {listed}"
        )));
    }
    let updated = InstanceRecord {
        env: record
            .env
            .iter()
            .filter(|pair| !keys.iter().any(|key| env_key(pair) == Some(key.as_str())))
            .cloned()
            .collect(),
        ..record
    };
    crate::records::write_record(record_root, slug, &updated).map_err(|e| EnvError::Internal {
        slug: slug.to_string(),
        message: format!("record write: {e}"),
    })?;
    Ok(keys.to_vec())
}

/// The key half of a `KEY=VALUE` pair, or `None` when the pair has no
/// `=` (a shape the record should never carry, but reading must not
/// panic on).
fn env_key(pair: &str) -> Option<&str> {
    pair.split_once('=').map(|(key, _)| key)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn sample(data_dir: &Path) -> InstanceRecord {
        InstanceRecord {
            instance_id: "instance-1".into(),
            owner_uid: 1000,
            target_uid: 1000,
            target_username: None,
            workspace: Some("/ws".into()),
            env: vec!["KALLIP_EXISTING=1".into(), "RUST_LOG=info".into()],
            identity: None,
            data_dir: data_dir.to_path_buf(),
        }
    }

    fn setup() -> (tempfile::TempDir, String) {
        let root = tempfile::tempdir().expect("tempdir");
        crate::records::create_record(root.path(), "team", &sample(root.path()))
            .expect("create record");
        (root, "team".to_string())
    }

    #[test]
    fn get_returns_the_persisted_pairs_verbatim() {
        let (root, slug) = setup();
        let env = env_get(root.path(), &slug, 1000).expect("get");
        assert_eq!(
            env,
            vec!["KALLIP_EXISTING=1".to_owned(), "RUST_LOG=info".to_owned()]
        );
    }

    #[test]
    fn set_merges_new_keys_and_keeps_existing() {
        let (root, slug) = setup();
        env_set(root.path(), &slug, 1000, &["KALLIP_NEW=2".to_owned()]).expect("set");
        let env = env_get(root.path(), &slug, 1000).expect("get");
        assert_eq!(
            env,
            vec![
                "KALLIP_EXISTING=1".to_owned(),
                "RUST_LOG=info".to_owned(),
                "KALLIP_NEW=2".to_owned()
            ]
        );
    }

    #[test]
    fn set_replaces_an_existing_key_in_place() {
        let (root, slug) = setup();
        env_set(root.path(), &slug, 1000, &["RUST_LOG=debug".to_owned()]).expect("set");
        let env = env_get(root.path(), &slug, 1000).expect("get");
        assert_eq!(
            env,
            vec!["KALLIP_EXISTING=1".to_owned(), "RUST_LOG=debug".to_owned()]
        );
    }

    #[test]
    fn set_refuses_an_empty_request() {
        let (root, slug) = setup();
        let error =
            env_set(root.path(), &slug, 1000, &[]).expect_err("empty request must be refused");
        let EnvError::Invalid(message) = error else {
            panic!("expected Invalid, got {error:?}");
        };
        assert!(message.contains("env unset"), "{message}");
    }

    #[test]
    fn set_resolves_duplicate_keys_last_wins() {
        let (root, slug) = setup();
        env_set(
            root.path(),
            &slug,
            1000,
            &["RUST_LOG=debug".to_owned(), "RUST_LOG=trace".to_owned()],
        )
        .expect("set");
        let env = env_get(root.path(), &slug, 1000).expect("get");
        assert_eq!(
            env,
            vec!["KALLIP_EXISTING=1".to_owned(), "RUST_LOG=trace".to_owned()]
        );
    }

    #[test]
    fn set_collapses_pre_existing_duplicates_to_one_pair() {
        let root = tempfile::tempdir().expect("tempdir");
        let record = InstanceRecord {
            env: vec![
                "KALLIP_DUP=1".into(),
                "RUST_LOG=info".into(),
                "KALLIP_DUP=2".into(),
            ],
            ..sample(root.path())
        };
        crate::records::create_record(root.path(), "team", &record).expect("create record");
        env_set(root.path(), "team", 1000, &["KALLIP_DUP=9".to_owned()]).expect("set");
        let env = env_get(root.path(), "team", 1000).expect("get");
        assert_eq!(
            env,
            vec!["KALLIP_DUP=9".to_owned(), "RUST_LOG=info".to_owned()]
        );
    }

    #[test]
    fn set_rejects_non_allowlisted_keys_like_spawn() {
        let (root, slug) = setup();
        let error = env_set(root.path(), &slug, 1000, &["HOME=/x".to_owned()])
            .expect_err("non-allowlisted key must be refused");
        assert!(matches!(error, EnvError::Invalid(_)), "{error:?}");
        // The record is untouched by the failed request.
        let env = env_get(root.path(), &slug, 1000).expect("get");
        assert_eq!(env.first().map(String::as_str), Some("KALLIP_EXISTING=1"));
    }

    #[test]
    fn unset_removes_only_the_named_keys() {
        let (root, slug) = setup();
        let removed =
            env_unset(root.path(), &slug, 1000, &["KALLIP_EXISTING".to_owned()]).expect("unset");
        assert_eq!(removed, vec!["KALLIP_EXISTING".to_owned()]);
        let env = env_get(root.path(), &slug, 1000).expect("get");
        assert_eq!(env, vec!["RUST_LOG=info".to_owned()]);
    }

    #[test]
    fn unset_fails_the_whole_batch_and_names_missing_keys() {
        let (root, slug) = setup();
        let error = env_unset(
            root.path(),
            &slug,
            1000,
            &["KALLIP_EXISTING".to_owned(), "KALLIP_ABSENT".to_owned()],
        )
        .expect_err("missing key must fail the batch");
        let EnvError::Invalid(message) = error else {
            panic!("expected Invalid, got {error:?}");
        };
        assert!(message.contains("KALLIP_ABSENT"), "{message}");
        // Atomic: the present key survives because the batch failed.
        let env = env_get(root.path(), &slug, 1000).expect("get");
        assert!(env.iter().any(|pair| pair == "KALLIP_EXISTING=1"));
    }

    #[test]
    fn a_foreign_peer_is_refused_and_the_owner_passes() {
        let (root, slug) = setup();
        let error = env_get(root.path(), &slug, 4242).expect_err("foreign peer");
        assert_eq!(
            error,
            EnvError::Denied {
                slug: "team".into(),
                target_uid: 1000,
            }
        );
        env_get(root.path(), &slug, 1000).expect("owner passes");
        env_get(root.path(), &slug, 0).expect("root passes");
    }

    #[test]
    fn an_unknown_slug_reads_as_not_found_without_probe_shape() {
        let root = tempfile::tempdir().expect("tempdir");
        let error = env_get(root.path(), "ghost", 1000).expect_err("no record");
        assert_eq!(error, EnvError::NotFound("ghost".into()));
        // A slug failing the grammar gate is refused the same way,
        // before any record-area access.
        let error = env_get(root.path(), "../escape", 1000).expect_err("bad slug");
        assert_eq!(error, EnvError::NotFound("../escape".into()));
    }
}
