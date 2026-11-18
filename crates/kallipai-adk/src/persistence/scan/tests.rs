use super::*;

use crate::context::AgenticContext as _;
use crate::history::{HistoryWriter, RecordKind};
use crate::persistence::test_support::{NO_TRUNCATION, with_data_dir};
use crate::persistence::{
    agent_dir, archive_agent_dir, create_agent_dir, data_dir_root, persist_context,
};
use crate::test_support::{assistant_msg, user_msg};
use kallipai_common::AgentId;
use serial_test::serial;
use tempfile::TempDir;

#[test]
#[serial]
fn scan_agents_ignores_archived() {
    with_data_dir(|_| {
        let id = AgentId::from("scan-ignores-1".to_owned());
        create_agent_dir(
            &id,
            AgentMeta {
                workspace_root: Path::new("/app").to_path_buf(),
                created_by: None,
                role: String::new(),
                description: String::new(),
                profile_set: None,
                permissions_class: crate::config::PermissionClass::Normal,
                delegation_mode: crate::config::DelegationMode::CarveOut,
            },
        )
        .unwrap();
        archive_agent_dir(&id).unwrap();

        let (pending, refused) = scan_agents().expect("scan");
        assert!(
            pending.iter().all(|p| p.agent_id != id),
            "archived agent must not be eligible for restore"
        );
        assert!(refused.is_empty());
    })
}

#[test]
#[serial]
fn scan_agents_reports_unreadable_meta_as_refused() {
    with_data_dir(|_| {
        let id = AgentId::from("scan-refused-1".to_owned());
        create_agent_dir(
            &id,
            AgentMeta {
                workspace_root: Path::new("/app").to_path_buf(),
                created_by: None,
                role: String::new(),
                description: String::new(),
                profile_set: None,
                permissions_class: crate::config::PermissionClass::Normal,
                delegation_mode: crate::config::DelegationMode::CarveOut,
            },
        )
        .unwrap();
        // Corrupt the meta so the directory cannot be scanned.
        std::fs::write(agent_dir(&id).unwrap().join("meta.json"), "not json").unwrap();
        let (pending, refused) = scan_agents().expect("scan");
        assert!(pending.iter().all(|p| p.agent_id != id));
        let hit = refused
            .iter()
            .find(|r| r.agent_id == id)
            .expect("refused entry");
        assert!(
            hit.error.contains("meta.json"),
            "error carries the cause: {}",
            hit.error
        );
    })
}

#[test]
#[serial]
fn find_disk_root_returns_the_unsupervised_agent_only() {
    with_data_dir(|_| {
        assert_eq!(
            find_disk_root().expect("find"),
            None,
            "empty data dir has no root"
        );
        let root = AgentId::from("disk-root-1".to_owned());
        let sub = AgentId::from("disk-sub-1".to_owned());
        create_agent_dir(
            &root,
            AgentMeta {
                workspace_root: Path::new("/r").to_path_buf(),
                created_by: None,
                role: "root".to_string(),
                description: String::new(),
                profile_set: None,
                permissions_class: crate::config::PermissionClass::Normal,
                delegation_mode: crate::config::DelegationMode::CarveOut,
            },
        )
        .unwrap();
        create_agent_dir(
            &sub,
            AgentMeta {
                workspace_root: Path::new("/s").to_path_buf(),
                created_by: Some(root.clone()),
                role: "sub".to_string(),
                description: String::new(),
                profile_set: None,
                permissions_class: crate::config::PermissionClass::Normal,
                delegation_mode: crate::config::DelegationMode::CarveOut,
            },
        )
        .unwrap();
        assert_eq!(
            find_disk_root().expect("find"),
            Some(root),
            "the created_by-less agent is the root"
        );
    })
}

#[test]
#[serial]
fn scan_and_find_error_when_the_agents_dir_is_unreadable() {
    with_data_dir(|_| {
        // The agents base exists but as a regular file: read_dir fails with
        // ENOTDIR, standing in for permission-denied states the runner
        // cannot reproduce (root ignores file modes).
        std::fs::create_dir_all(data_dir_root().unwrap()).unwrap();
        let base = agents_base().unwrap();
        std::fs::create_dir_all(base.parent().unwrap()).unwrap();
        std::fs::write(&base, "not a directory").unwrap();
        let err = match scan_agents() {
            Ok(_) => panic!("scan must not swallow an unreadable dir"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("cannot read agents directory"),
            "error names the cause: {err:#}"
        );
        let err = match find_disk_root() {
            Ok(_) => panic!("find must refuse to guess"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("cannot read agents directory"),
            "error names the cause: {err:#}"
        );
    })
}

/// A split directory whose manifest lists a restart notice, as a manifest
/// written by an earlier binary can: the second restore must not grow a second
/// notice.
#[test]
fn restore_twice_leaves_a_single_restart_notice() {
    let dir = TempDir::new().unwrap();
    let mut store = ContextStore::new();
    let stale = vec![user_msg(RESTART_MESSAGE)];
    let (stale_id, _) = store.push_turn(stale.clone());
    HistoryWriter::new(dir.path().to_path_buf())
        .append(
            Some(stale_id.0),
            &stale,
            8,
            RecordKind::Turn,
            None,
            &[],
            None,
        )
        .unwrap();
    persist_context(&store, dir.path()).unwrap();

    let id = AgentId::from("twice".to_owned());
    let _ = restore_agent(&id, dir.path(), NO_TRUNCATION).unwrap();
    let again = restore_agent(&id, dir.path(), NO_TRUNCATION).unwrap();

    let notices = again
        .store
        .turns()
        .iter()
        .filter(|t| {
            !t.is_pinned()
                && t.messages.len() == 1
                && t.messages[0].content() == Some(RESTART_MESSAGE)
        })
        .count();
    assert_eq!(
        notices, 1,
        "exactly one fresh notice after the second restore"
    );
    assert!(again.degraded.is_empty());
}

/// A manifest naming a turn whose history record never landed degrades the
/// restore instead of failing it, and the hydratable turns still come back.
#[test]
fn missing_history_records_degrade_instead_of_failing() {
    let dir = TempDir::new().unwrap();
    let mut store = ContextStore::new();
    store.pin("note", user_msg("pinned note")).unwrap();
    let first = vec![user_msg("first question")];
    let (first_id, _) = store.push_turn(first.clone());
    HistoryWriter::new(dir.path().to_path_buf())
        .append(
            Some(first_id.0),
            &first,
            8,
            RecordKind::Turn,
            None,
            &[],
            None,
        )
        .unwrap();
    // No history record for the second turn.
    store.push_turn(vec![assistant_msg("first answer")]);
    persist_context(&store, dir.path()).unwrap();

    let restored =
        restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

    assert_eq!(restored.degraded.len(), 1);
    assert_eq!(
        restored.degraded[0].kind,
        DegradationKind::MissingHistoryTurns
    );
    let turns = restored.store.turns();
    assert_eq!(turns.len(), 3, "pin + one hydratable turn + restart notice");
}
