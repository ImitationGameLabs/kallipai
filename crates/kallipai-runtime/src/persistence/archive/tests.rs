use super::*;

use crate::history::{HistoryWriter, RecordKind};
use crate::persistence::test_support::with_data_dir;
use crate::persistence::{AgentMeta, create_agent_dir, data_dir_root, scan_agents};
use crate::test_support::user_msg;
use serial_test::serial;
use std::path::PathBuf;
use tempfile::TempDir;

#[test]
#[serial]
fn archive_moves_dir_and_preserves_contents() {
    with_data_dir(|_| {
        let id = AgentId::from("archive-rt-1".to_owned());
        let dir = create_agent_dir(
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

        // One history record (as the live writer would produce).
        HistoryWriter::new(dir.clone())
            .append(
                Some(0),
                &[user_msg("hello")],
                16,
                RecordKind::Turn,
                None,
                &[],
            )
            .unwrap();
        // A context.json carrying non-zero cumulative usage.
        std::fs::write(
            dir.join("context.json"),
            r#"{"cumulative_usage":{"prompt_tokens":100,"completion_tokens":50,"cache_hit_tokens":10}}"#,
        )
        .unwrap();

        archive_agent_dir(&id).unwrap();

        assert!(!dir.exists(), "agent dir must be gone from agents/");
        let archived = archived_dir(&id).unwrap();
        assert!(
            archived.join("history").exists(),
            "history survives archival"
        );
        let ctx: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(archived.join("context.json")).unwrap())
                .unwrap();
        assert_eq!(ctx["cumulative_usage"]["prompt_tokens"], 100);
        // Live and archived trees share one root (the installed data
        // root; single filesystem here, so `rename` is atomic — a cross-fs
        // EXDEV falls back to copy + delete).
        let root = data_dir_root().unwrap();
        assert!(root.join("agents").join("active").exists());
        assert!(root.join("agents").join("archived").exists());
    })
}

#[test]
#[serial]
fn rollback_remove_leaves_no_archive_residue() {
    with_data_dir(|_| {
        let id = AgentId::from("rollback-1".to_owned());
        let dir = create_agent_dir(
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
        // Rollback of a never-alive agent removes the live dir directly,
        // never archiving (the abort/create-rollback call sites).
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!agent_dir(&id).unwrap().exists());
        assert!(!archived_dir(&id).unwrap().exists());
    })
}

#[test]
#[serial]
fn archive_missing_source_is_noop() {
    with_data_dir(|_| {
        let id = AgentId::from("missing-src-1".to_owned());
        // No create_agent_dir — source is absent.
        archive_agent_dir(&id).unwrap();
        assert!(!agent_dir(&id).unwrap().exists());
        assert!(!archived_dir(&id).unwrap().exists());
    })
}

#[test]
#[serial]
fn archive_bails_when_destination_exists() {
    with_data_dir(|_| {
        let id = AgentId::from("collision-1".to_owned());
        let dir = create_agent_dir(
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
        // Pre-create the archived destination (an anomaly: UUIDs should not collide).
        std::fs::create_dir_all(archived_dir(&id).unwrap()).unwrap();

        let res = archive_agent_dir(&id);
        assert!(res.is_err(), "must bail when destination already exists");
        assert!(dir.exists(), "source must be left intact on bail");
    })
}

#[test]
fn copy_dir_all_round_trips_tree_with_symlink() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("sub/file.txt"), "body").unwrap();
    std::fs::write(src.join("top.txt"), "top").unwrap();
    std::os::unix::fs::symlink("top.txt", src.join("link")).unwrap();

    let dst = tmp.path().join("dst");
    copy_dir_all(&src, &dst).unwrap();

    assert_eq!(std::fs::read_to_string(dst.join("top.txt")).unwrap(), "top");
    assert_eq!(
        std::fs::read_to_string(dst.join("sub/file.txt")).unwrap(),
        "body"
    );
    // The symlink is preserved as a symlink (read_link gives the target),
    // not dereferenced into a copy of `top.txt`.
    assert_eq!(
        std::fs::read_link(dst.join("link")).unwrap(),
        std::path::Path::new("top.txt")
    );
    assert!(
        std::fs::symlink_metadata(dst.join("link"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

fn meta_for(role: &str) -> AgentMeta {
    AgentMeta {
        workspace_root: PathBuf::from("/app"),
        created_by: None,
        role: role.into(),
        description: String::new(),
        profile_set: None,
        permissions_class: crate::config::PermissionClass::Normal,
        delegation_mode: crate::config::DelegationMode::CarveOut,
    }
}

/// The structural restore boundary: deactivating moves the directory out
/// of `agents/`, so the next scan neither sees nor restores it. The
/// declarative-team converge flow relies on this — "not referenced by
/// the declaration" must mean "not resurrected at boot".
#[test]
#[serial]
fn scan_agents_ignores_inactive() {
    with_data_dir(|_| {
        let id = AgentId::from("aaaa1111-1111-4111-8111-111111111111".to_owned());
        create_agent_dir(&id, meta_for("scout")).unwrap();
        assert_eq!(
            scan_agents().unwrap().0.len(),
            1,
            "live before deactivation"
        );

        deactivate_agent_dir(&id).unwrap();
        assert!(!agent_dir(&id).unwrap().exists(), "left the live area");
        assert!(inactive_dir(&id).unwrap().exists(), "parked in inactive");

        let (pending, refused) = scan_agents().unwrap();
        assert!(
            pending.iter().all(|p| p.agent_id != id),
            "inactive agent must not be scanned for restore"
        );
        assert!(refused.is_empty());

        // Round trip: reactivation puts it back where the scan finds it.
        reactivate_agent_dir(&id).unwrap();
        assert_eq!(scan_agents().unwrap().0.len(), 1, "live after reactivation");
    })
}

/// The pre-check is bidirectional and refuses to overwrite: a stale body
/// in the destination area means two preserved bodies for one identity,
/// and picking between them silently is exactly what must never happen.
#[test]
#[serial]
fn deactivate_and_reactivate_refuse_existing_destination() {
    with_data_dir(|_| {
        let id = AgentId::from("aaaa2222-2222-4222-8222-222222222222".to_owned());
        create_agent_dir(&id, meta_for("scout")).unwrap();
        // Stale inactive body: deactivate must refuse, leaving the live
        // directory untouched.
        std::fs::create_dir_all(inactive_dir(&id).unwrap()).unwrap();
        let err = deactivate_agent_dir(&id).unwrap_err();
        assert!(err.to_string().contains("refusing to overwrite"));
        assert!(agent_dir(&id).unwrap().exists(), "live body intact");

        // Clean the stale body, deactivate for real, then plant a live
        // body: reactivation must refuse the same way.
        std::fs::remove_dir_all(inactive_dir(&id).unwrap()).unwrap();
        deactivate_agent_dir(&id).unwrap();
        create_agent_dir(&id, meta_for("scout")).unwrap();
        let err = reactivate_agent_dir(&id).unwrap_err();
        assert!(err.to_string().contains("refusing to overwrite"));
        assert!(inactive_dir(&id).unwrap().exists(), "inactive body intact");
    })
}

/// The archive path is deliberately one-way: deactivate operates on the
/// live area only, and an archived id never re-enters the flow through
/// it (archived is terminal: nothing in the lifecycle moves an archived
/// directory back — reactivation reads the inactive area only).
#[test]
#[serial]
fn deactivate_requires_a_live_directory() {
    with_data_dir(|_| {
        let id = AgentId::from("aaaa3333-3333-4333-8333-333333333333".to_owned());
        let err = deactivate_agent_dir(&id).unwrap_err();
        assert!(err.to_string().contains("no live directory"));
    })
}
