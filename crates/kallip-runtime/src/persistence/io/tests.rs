use super::*;

use crate::context::AgenticContext as _;
use crate::persistence::restore_agent;
use crate::persistence::test_support::{NO_TRUNCATION, wreckable_dir};
use crate::test_support::{assistant_msg, user_msg};
use kallip_common::AgentId;
use tempfile::TempDir;

// --- split persistence (manifest/pins) ---

#[test]
fn persist_writes_manifest_and_pins_with_backup() {
    let dir = TempDir::new().unwrap();
    let mut store = ContextStore::new();
    store.pin("note", assistant_msg("keep")).unwrap();
    store.push_turn(vec![user_msg("hello")]);
    let first_ids = store.to_manifest_doc().conversation_turn_ids.clone();

    persist_context(&store, dir.path()).unwrap();
    let manifest_v1 = std::fs::read_to_string(dir.path().join("manifest.json")).unwrap();
    assert!(dir.path().join("pins.json").exists());
    assert!(
        !dir.path().join("manifest.json.bak").exists(),
        "no backup on first write"
    );

    // Second turn changes the manifest; the backup keeps version 1.
    store.push_turn(vec![user_msg("more")]);
    persist_context(&store, dir.path()).unwrap();
    let manifest_v2 = std::fs::read_to_string(dir.path().join("manifest.json")).unwrap();
    let bak = std::fs::read_to_string(dir.path().join("manifest.json.bak")).unwrap();
    assert_eq!(bak, manifest_v1);
    assert_ne!(manifest_v2, manifest_v1);

    let doc: crate::context::manifest::ManifestDoc = serde_json::from_str(&manifest_v2).unwrap();
    assert_eq!(doc.conversation_turn_ids.len(), first_ids.len() + 1);
    assert!(
        !dir.path().join("context.json").exists(),
        "legacy file not written"
    );
}

#[test]
fn persist_excludes_injected_turns_from_manifest() {
    let dir = TempDir::new().unwrap();
    let mut store = ContextStore::new();
    let kept = store.push_turn(vec![user_msg("kept")]).0;
    let injected = store.push_turn(vec![user_msg("[system] restart")]).0;
    store.register_injected_turn(injected);

    persist_context(&store, dir.path()).unwrap();
    let manifest = std::fs::read_to_string(dir.path().join("manifest.json")).unwrap();
    let doc: crate::context::manifest::ManifestDoc = serde_json::from_str(&manifest).unwrap();
    assert_eq!(doc.conversation_turn_ids, vec![kept.0]);
    assert_eq!(doc.next_turn_id, injected.0 + 1);
}

/// manifest.json corrupt but the backup intact: the backup wins, the
/// store rehydrates normally, and nothing is degraded.
#[test]
fn corrupt_manifest_falls_back_to_backup() {
    let (dir, _store) = wreckable_dir(false);
    std::fs::write(dir.path().join("manifest.json"), "{wreck").unwrap();

    let restored =
        restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

    assert!(restored.degraded.is_empty(), "backup recovery is silent");
    let turns = restored.store.turns();
    assert_eq!(turns.len(), 5, "pin + 3 hydrated + restart notice");
    assert!(
        restored
            .store
            .usage_snapshot()
            .cumulative_usage
            .prompt_tokens
            > 0
    );
    assert_eq!(restored.store.retry_log.len(), 1);
}

/// manifest.json deleted but its backup intact: the backup feeds the
/// restore (the existence gate must not skip the whole chain),
/// and the primary is rewritten by the next persist, not by the load.
#[test]
fn missing_manifest_falls_back_to_backup() {
    let (dir, _store) = wreckable_dir(false);
    std::fs::remove_file(dir.path().join("manifest.json")).unwrap();

    let restored =
        restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

    assert!(restored.degraded.is_empty(), "backup recovery is silent");
    assert_eq!(
        restored.store.turns().len(),
        5,
        "pin + 3 hydrated + restart notice"
    );
    assert!(
        !dir.path().join("manifest.json").exists(),
        "rewrite happens at persist, not during restore"
    );
}
