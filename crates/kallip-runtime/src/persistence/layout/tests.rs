use super::*;

use crate::persistence::test_support::with_data_dir;
use serial_test::serial;
use tempfile::TempDir;

// ----- workspace/data-dir overlap guard tests -----
// The guard backs the data-dir integrity baseline: with no workspace↔data
// overlap, landlock alone keeps the agent out of tagma bookkeeping.

#[test]
#[serial]
fn overlap_detects_workspace_inside_data_root() {
    with_data_dir(|_| {
        // Workspace nested under the slug-derived root → overlap.
        // Exercises the `ws.starts_with(&data)` direction.
        let ws = data_dir_root().unwrap().join("agents/x");
        std::fs::create_dir_all(&ws).unwrap();
        assert!(
            workspace_overlaps_data_root(&ws).unwrap(),
            "workspace inside data root must be detected as overlap"
        );
    });
}

#[test]
#[serial]
fn overlap_detects_workspace_equal_to_data_root() {
    with_data_dir(|_| {
        // workspace == data root → overlap (degenerate case); equal
        // paths satisfy both `starts_with` directions.
        let ws = data_dir_root().unwrap();
        // The workspace must exist (AgentConfig canonicalizes it); the
        // fresh data root does not exist yet, which is exactly the
        // notional-ancestor case canonical_data_root handles.
        std::fs::create_dir_all(&ws).unwrap();
        assert!(
            workspace_overlaps_data_root(&ws).unwrap(),
            "workspace equal to data root must be detected as overlap"
        );
    });
}

#[test]
#[serial]
fn overlap_detects_workspace_containing_data_root() {
    with_data_dir(|tmp| {
        // workspace is a strict ancestor of the data root (the workspace ==
        // $HOME case, the most dangerous: the broad write grant covers the
        // whole data tree). Exercises the `data.starts_with(&ws)` direction.
        // The tmp root itself is the smallest existing on-disk ancestor
        // (the slug-derived root hangs three levels beneath it).
        let ws = tmp.path().to_path_buf();
        assert!(
            workspace_overlaps_data_root(&ws).unwrap(),
            "workspace containing data root must be detected as overlap"
        );
    });
}

#[test]
#[serial]
fn overlap_rejects_disjoint_workspace() {
    with_data_dir(|_tmp| {
        // A workspace entirely outside the data tree → no overlap.
        let ws = TempDir::new().unwrap();
        assert!(
            !workspace_overlaps_data_root(ws.path()).unwrap(),
            "disjoint workspace must not be flagged as overlap"
        );
    });
}

#[test]
#[serial]
fn overlap_rejects_sibling_prefix() {
    with_data_dir(|_| {
        // A sibling whose leaf name is a string prefix of the data root's
        // leaf (`test-instance` → `test-instanc`) must NOT be flagged:
        // `Path::starts_with` is component-wise, not byte-wise. Guards
        // against a future regression to a byte-prefix comparison. The
        // candidate must exist on disk so `canonicalize` succeeds (the guard
        // fails closed — returning Err — on a non-existent workspace).
        let root = data_dir_root().unwrap();
        let parent = root.parent().unwrap();
        let leaf = root.file_name().unwrap().to_str().unwrap();
        let ws = parent.join(&leaf[..leaf.len() - 1]);
        std::fs::create_dir_all(&ws).unwrap();
        assert!(
            !workspace_overlaps_data_root(&ws).unwrap(),
            "sibling prefix must not be flagged as overlap (component-wise check)"
        );
    });
}

#[test]
#[serial]
fn overlap_fails_closed_on_nonexistent_workspace() {
    with_data_dir(|tmp| {
        // A workspace that cannot be canonicalized (does not exist) must
        // surface an error rather than silently allow a potential overlap.
        let ws = tmp.path().join("does-not-exist");
        assert!(
            workspace_overlaps_data_root(&ws).is_err(),
            "non-canonicalizable workspace must fail closed"
        );
    });
}
