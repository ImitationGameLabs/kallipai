use super::*;

use crate::persistence::test_support::{NO_TRUNCATION, legacy_fixture, write_legacy};
use crate::persistence::{DegradationKind, copy_dir_all, restore_agent};
use crate::test_support::user_msg;
use kallip_common::AgentId;
use tempfile::TempDir;

#[test]
fn migrate_writes_split_files_renames_legacy_and_strips_restart_turns() {
    let dir = TempDir::new().unwrap();
    let (mut store, convo) = legacy_fixture();
    // A restart notice from a previous restore lives in the legacy store.
    store.push_turn(vec![user_msg(RESTART_MESSAGE)]);
    write_legacy(&dir, &store, true, &convo);
    let legacy_next = store.to_manifest_doc().next_turn_id;

    let restored =
        restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

    // Split files exist; the legacy file is archived, not deleted.
    assert!(dir.path().join("manifest.json").exists());
    assert!(dir.path().join("pins.json").exists());
    assert!(dir.path().join("context.legacy.json").exists());
    assert!(!dir.path().join("context.json").exists());

    let manifest = std::fs::read_to_string(dir.path().join("manifest.json")).unwrap();
    let doc: crate::context::manifest::ManifestDoc = serde_json::from_str(&manifest).unwrap();
    assert_eq!(
        doc.conversation_turn_ids,
        vec![1, 2],
        "restart notice excluded"
    );
    assert_eq!(doc.next_turn_id, legacy_next);
    assert!(
        restored.degraded.is_empty(),
        "clean migration degrades nothing"
    );
}

#[test]
fn legacy_context_json_assistant_pin_without_tool_calls_reads_back() {
    let dir = TempDir::new().unwrap();
    // A true pre-unification `context.json`: pins live in the `pinned`
    // array and the assistant message carries the legacy chat face —
    // `reasoning_content`, with no `tool_calls` key at all (the old
    // binary skipped it when empty).
    let legacy = serde_json::json!({
        "turns": [],
        "pinned": [{
            "label": "note",
            "message": {
                "role": "assistant",
                "content": "old reply",
                "reasoning_content": "done thinking"
            }
        }],
        "last_prompt_tokens": null,
        "next_turn_id": 0
    });
    std::fs::write(dir.path().join("context.json"), legacy.to_string()).unwrap();

    let restored =
        restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

    assert!(
        restored.degraded.is_empty(),
        "clean legacy migration degrades nothing"
    );
    let turn = restored
        .store
        .pinned_turns()
        .find(|t| t.label() == Some("note"))
        .expect("legacy assistant pin folds into a pinned turn");
    let msg = &turn.messages[0];
    assert_eq!(msg.role(), "assistant");
    assert_eq!(msg.content(), Some("old reply"));
    assert_eq!(
        msg.reasoning().and_then(|r| r.text.as_deref()),
        Some("done thinking")
    );
    assert!(
        msg.tool_calls().is_empty(),
        "absent legacy key reads back as no tool calls"
    );
    // Deferred migration completed: split files replaced the legacy doc.
    assert!(dir.path().join("manifest.json").exists());
    assert!(dir.path().join("pins.json").exists());
    assert!(!dir.path().join("context.json").exists());
}

#[test]
fn migration_interrupted_after_pins_reruns_legacy_path_idempotently() {
    let dir = TempDir::new().unwrap();
    let (store, convo) = legacy_fixture();
    write_legacy(&dir, &store, true, &convo);
    // Simulate a crash after pins.json but before manifest.json.
    std::fs::write(
        dir.path().join("pins.json"),
        serde_json::to_string(&store.to_pins_doc()).unwrap(),
    )
    .unwrap();

    let restored =
        restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

    // The dispatcher saw no manifest → full legacy migration re-ran.
    assert!(
        dir.path().join("manifest.json").exists(),
        "migration completed"
    );
    assert!(dir.path().join("context.legacy.json").exists());
    assert!(!dir.path().join("context.json").exists());
    assert!(restored.degraded.is_empty());
}

#[test]
fn migration_interrupted_before_rename_takes_split_path_losing_nothing() {
    let dir = TempDir::new().unwrap();
    let (store, convo) = legacy_fixture();
    write_legacy(&dir, &store, true, &convo);
    // Simulate a crash after both split writes but before the rename:
    // manifest exists, context.json still in place.
    std::fs::write(
        dir.path().join("pins.json"),
        serde_json::to_string(&store.to_pins_doc()).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_string(&store.to_manifest_doc()).unwrap(),
    )
    .unwrap();

    let restored =
        restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

    // Split path taken; the interrupted rename is finished here.
    assert!(dir.path().join("context.legacy.json").exists());
    assert!(!dir.path().join("context.json").exists());
    let window = restored.store.turns();
    assert_eq!(window.len(), 4, "pin + 2 hydrated + fresh restart notice");
    assert!(restored.degraded.is_empty());
}

#[test]
fn legacy_restore_hydrates_equivalent_store() {
    let dir = TempDir::new().unwrap();
    let (store, convo) = legacy_fixture();
    write_legacy(&dir, &store, true, &convo);
    let legacy_ids: Vec<u64> = store
        .turns()
        .iter()
        .filter(|t| !t.is_pinned())
        .map(|t| t.id.0)
        .collect();
    let legacy_next = store.to_manifest_doc().next_turn_id;

    let restored =
        restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

    let turns = restored.store.turns();
    assert_eq!(
        turns.front().unwrap().label(),
        Some("note"),
        "pinned layer rebuilt first"
    );
    let hydrated: Vec<u64> = turns.iter().skip(1).take(2).map(|t| t.id.0).collect();
    assert_eq!(hydrated, legacy_ids, "conversation window identical by ID");
    assert_eq!(
        turns.get(1).unwrap().messages[0].content(),
        Some("first question")
    );
    assert_eq!(restored.store.retry_log.len(), 1);
    assert_eq!(restored.store.retry_log[0].error, "legacy");
    // A second restore rehydrates from the split files, not the archive.
    let again = restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();
    let ids2: Vec<u64> = again
        .store
        .turns()
        .iter()
        .skip(1)
        .take(2)
        .map(|t| t.id.0)
        .collect();
    assert_eq!(ids2, legacy_ids);
    assert_eq!(again.store.retry_log.len(), 1);
    assert!(again.degraded.is_empty());
    // The rehydrated store assigns the next ID from the manifest, not below
    // any historical ID.
    assert_eq!(again.store.turns().back().unwrap().id.0, legacy_next);
}

/// The legacy→split write happens after restore's folds, so pins.json
/// carries the folded legacy state (the summary pin here) rather than
/// the raw legacy shape — a crash between load and write leaves
/// context.json intact for the next attempt.
#[test]
fn migration_writes_split_documents_after_restore_folds() {
    let dir = TempDir::new().unwrap();
    let (store, convo) = legacy_fixture();
    write_legacy(&dir, &store, true, &convo);
    // Inject the legacy `summary` field: restore folds it into a pinned
    // turn after load, so the split documents must carry the fold.
    let mut doc = serde_json::to_value(&store).unwrap();
    doc["summary"] = serde_json::json!("condensed history");
    std::fs::write(dir.path().join("context.json"), doc.to_string()).unwrap();

    restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

    let pins = std::fs::read_to_string(dir.path().join("pins.json")).unwrap();
    assert!(
        pins.contains("condensed history"),
        "pins.json carries the folded summary: {pins}"
    );
    assert!(
        !dir.path().join("context.json").exists(),
        "archived only after the split documents landed"
    );
}

/// Each restore pushes a fresh restart notice; the window must never
/// accumulate notices across restores (the strip before the push is what
/// keeps repeated restarts from growing the context).
#[test]
fn restore_twice_leaves_a_single_restart_notice() {
    let dir = TempDir::new().unwrap();
    let (mut store, convo) = legacy_fixture();
    // A notice from a previous restore lives in the legacy store.
    store.push_turn(vec![user_msg(RESTART_MESSAGE)]);
    write_legacy(&dir, &store, true, &convo);
    let id = AgentId::from("twice".to_owned());
    let _ = restore_agent(&id, dir.path(), NO_TRUNCATION).unwrap();
    // Second restore now loads the split documents the first one wrote.
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

#[test]
fn missing_history_records_degrade_instead_of_failing() {
    let dir = TempDir::new().unwrap();
    let (store, convo) = legacy_fixture();
    // History records only the first turn; the second goes missing.
    write_legacy(&dir, &store, true, &convo[..1]);

    let restored =
        restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

    // The first restore migrates: the full legacy window is in memory,
    // so nothing is degraded yet. The gap surfaces on the next restore,
    // which hydrates from the split files.
    assert!(restored.degraded.is_empty());

    let degraded =
        restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

    assert_eq!(degraded.degraded.len(), 1);
    assert_eq!(
        degraded.degraded[0].kind,
        DegradationKind::MissingHistoryTurns
    );
    let turns = degraded.store.turns();
    assert_eq!(turns.len(), 3, "pin + one hydratable turn + restart notice");
}

/// Manual migration drill against a copy of a real agent directory:
/// `KALLIP_MIGRATION_DRILL_DIR=<agent dir> cargo test -p kallip-runtime
/// real_dir_migration_drill -- --ignored --nocapture`. Verifies the
/// production-shape directory migrates, rehydrates losslessly, and
/// prints the size split. Never touches the source directory.
#[test]
#[ignore = "manual drill: set KALLIP_MIGRATION_DRILL_DIR"]
fn real_dir_migration_drill() {
    let src = std::env::var("KALLIP_MIGRATION_DRILL_DIR")
        .expect("set KALLIP_MIGRATION_DRILL_DIR to a real agent directory");
    let tmp = TempDir::new().unwrap();
    copy_dir_all(std::path::Path::new(&src), tmp.path()).unwrap();
    let id = AgentId::from(
        std::path::Path::new(&src)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    );

    let first = restore_agent(&id, tmp.path(), NO_TRUNCATION).unwrap();
    println!(
        "first restore (migration): {} degradation(s)",
        first.degraded.len()
    );
    for d in &first.degraded {
        println!("  {:?}: {}", d.kind, d.detail);
    }

    let second = restore_agent(&id, tmp.path(), NO_TRUNCATION).unwrap();
    println!(
        "second restore (rehydrate): {} degradation(s)",
        second.degraded.len()
    );
    for d in &second.degraded {
        println!("  {:?}: {}", d.kind, d.detail);
    }
    assert_eq!(second.store.turn_count(), first.store.turn_count());
    assert_eq!(second.store.retry_log.len(), first.store.retry_log.len());

    let size = |name: &str| {
        std::fs::metadata(tmp.path().join(name))
            .map(|m| m.len())
            .unwrap_or(0)
    };
    println!(
        "legacy context.json: {} bytes -> manifest.json: {} + pins.json: {} (per-turn writes)",
        size("context.legacy.json"),
        size("manifest.json"),
        size("pins.json"),
    );
}
