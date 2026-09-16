use super::*;

use crate::context::AgenticContext as _;
use crate::history::{HistoryWriter, RecordKind};
use crate::persistence::test_support::{NO_TRUNCATION, wreckable_dir};
use crate::persistence::{persist_context, restore_agent};
use crate::test_support::user_msg;
use kallip_common::AgentId;

/// manifest.json and its backup deleted while history survives: the
/// window rebuilds from the tail instead of booting a silently empty
/// store that would renumber turn IDs from zero and alias history.
#[test]
fn missing_manifest_and_backup_rebuild_from_tail() {
    let (dir, store) = wreckable_dir(false);
    std::fs::remove_file(dir.path().join("manifest.json")).unwrap();
    std::fs::remove_file(dir.path().join("manifest.json.bak")).unwrap();

    let restored =
        restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

    let kinds: Vec<DegradationKind> = restored.degraded.iter().map(|d| d.kind).collect();
    assert_eq!(kinds, vec![DegradationKind::TailRecovery]);
    assert!(
        restored.degraded[0].detail.contains("missing"),
        "detail names the missing-documents case: {}",
        restored.degraded[0].detail
    );
    assert_eq!(
        restored.store.turns().len(),
        6,
        "pin + 4 tail turns + restart notice (full budget keeps all)"
    );
    // Rescanned from history: the restart notice took history max + 1,
    // the first fresh turn one past that — no ID is reused.
    let history_max = store.to_manifest_doc().next_turn_id - 1;
    let mut live = restored.store;
    let (fresh_id, _) = live.push_turn(vec![user_msg("after loss")]);
    assert_eq!(fresh_id.0, history_max + 2, "rescanned past history max");
}

/// manifest.json and .bak both corrupt: the window rebuilds from the
/// history tail within the budget, manifest-only state (usage, retry
/// log) is lost and recorded, and `next_turn_id` is rescanned from
/// history so no ID is ever reused.
#[test]
fn double_corrupt_manifest_rebuilds_from_tail() {
    let (dir, store) = wreckable_dir(false);
    std::fs::write(dir.path().join("manifest.json"), "{wreck").unwrap();
    std::fs::write(dir.path().join("manifest.json.bak"), "{wreck").unwrap();

    let restored = restore_agent(&AgentId::from("a".to_owned()), dir.path(), 24).unwrap();

    let tail: Vec<DegradationKind> = restored.degraded.iter().map(|d| d.kind).collect();
    assert_eq!(tail, vec![DegradationKind::TailRecovery]);
    let turns = restored.store.turns();
    // 24-token budget: the three newest turns (churn, tool round, ask-b)
    // fit exactly; ask-a would cross — and the restart notice is added after.
    assert_eq!(turns.len(), 5, "pin + 3 tail turns + restart notice");
    let ids: Vec<u64> = turns.iter().skip(1).take(3).map(|t| t.id.0).collect();
    assert_eq!(ids, vec![2, 3, 4], "newest three, ascending, ask-a dropped");
    assert_eq!(
        restored
            .store
            .usage_snapshot()
            .cumulative_usage
            .prompt_tokens,
        0
    );
    assert!(restored.store.retry_log.is_empty());

    // Rescanned next_turn_id: history max is the churn turn's ID, and
    // the manifest rewrite references only kept turns — no collision.
    let history_max = store.to_manifest_doc().next_turn_id - 1;
    let mut live = restored.store;
    let fresh = vec![user_msg("after wreck")];
    let (fresh_id, _) = live.push_turn(fresh.clone());
    // The agent task records every turn to history before persisting;
    // mirror that so the rehydrate below finds the fresh turn.
    HistoryWriter::new(dir.path().to_path_buf())
        .append(Some(fresh_id.0), &fresh, 8, RecordKind::Turn, None, &[])
        .unwrap();
    // The restart notice (pushed by restore) took history_max + 1; the
    // first fresh turn is one past that — neither reuses a historical ID.
    assert_eq!(
        fresh_id.0,
        history_max + 2,
        "rescanned from history, never reused"
    );
    persist_context(&live, dir.path()).unwrap();
    let again = restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();
    assert!(
        again.degraded.is_empty(),
        "wrecked files were rewritten clean"
    );
}

/// A pairing-damaged turn at the truncation boundary is dropped (up to
/// the first clean turn), and the drop count is recorded — a dangling
/// call or orphan result at the window head would be rejected by the
/// provider on the next round.
#[test]
fn tail_recovery_drops_damaged_leading_turns() {
    let (dir, _store) = wreckable_dir(true);
    std::fs::write(dir.path().join("manifest.json"), "{wreck").unwrap();
    std::fs::write(dir.path().join("manifest.json.bak"), "{wreck").unwrap();

    let restored =
        restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

    assert_eq!(restored.degraded[0].kind, DegradationKind::TailRecovery);
    assert!(restored.degraded[0].detail.contains("dropped 1"));
    let turns = restored.store.turns();
    assert_eq!(turns.len(), 5, "pin + 3 clean turns + restart notice");
    assert_eq!(turns.get(1).unwrap().messages[0].content(), Some("ask-b"));
}

/// pins.json corrupt falls back to its backup; both corrupt boots with
/// empty pins, recorded as a degradation.
#[test]
fn corrupt_pins_falls_back_then_boots_empty() {
    let (dir, _store) = wreckable_dir(false);

    // Backup intact: silent recovery, pin still there.
    std::fs::write(dir.path().join("pins.json"), "{wreck").unwrap();
    let restored =
        restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();
    assert!(restored.degraded.is_empty());
    assert_eq!(
        restored.store.turns().front().unwrap().label(),
        Some("note")
    );

    // Both wrecked: empty pins, recorded, conversation window intact.
    std::fs::write(dir.path().join("pins.json.bak"), "{wreck").unwrap();
    let degraded =
        restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();
    assert_eq!(degraded.degraded.len(), 1);
    assert_eq!(degraded.degraded[0].kind, DegradationKind::PinsLost);
    let turns = degraded.store.turns();
    assert!(turns.front().unwrap().label().is_none(), "no pin layer");
    assert_eq!(turns.len(), 5, "4 hydrated turns + restart notice");
}
