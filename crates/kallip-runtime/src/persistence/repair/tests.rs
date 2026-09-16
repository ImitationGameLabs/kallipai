use super::*;

use crate::context::ContextStore;
use crate::history::{HistoryWriter, RecordKind};
use crate::persistence::test_support::{NO_TRUNCATION, legacy_fixture, write_legacy};
use crate::persistence::{DegradationKind, persist_context, restore_agent};
use crate::test_support::{TurnMessage, tool_calls_msg, tool_result_msg};
use kallip_common::AgentId;
use std::fs;
use tempfile::TempDir;

/// Inspecting (or repairing) a legacy directory writes nothing: the
/// deferred migration is completed only by a restore.
#[test]
fn repair_dry_run_leaves_legacy_directory_untouched() {
    let dir = TempDir::new().unwrap();
    let (store, convo) = legacy_fixture();
    write_legacy(&dir, &store, true, &convo);

    repair_agent_context(dir.path(), NO_TRUNCATION, false).unwrap();

    assert!(!dir.path().join("manifest.json").exists());
    assert!(dir.path().join("context.json").exists());
}

/// Pairing damage in a hydrated turn is reported, not repaired: a
/// declared-but-unanswered call and a result answering nothing each
/// produce their own degradation, and the persisted messages come back
/// untouched.
/// A persisted 400-bug-shaped turn: `c2` declared but never answered,
/// `c9` answering nothing — both pairing directions damaged at once.
fn pairing_damaged_dir() -> (TempDir, Vec<TurnMessage>, u64) {
    let dir = TempDir::new().unwrap();
    let mut store = ContextStore::new();
    let damaged = vec![
        tool_calls_msg(&[("c1", "read"), ("c2", "edit")]),
        tool_result_msg("ok", "c1"),
        tool_result_msg("ghost", "c9"),
    ];
    let (turn_id, _) = store.push_turn(damaged.clone());
    let history = HistoryWriter::new(dir.path().to_path_buf());
    history
        .append(Some(turn_id.0), &damaged, 8, RecordKind::Turn, None, &[])
        .unwrap();
    persist_context(&store, dir.path()).unwrap();
    (dir, damaged, turn_id.0)
}

/// Total non-empty lines across the agent's history files — the
/// dry-run/idempotency "nothing was appended" oracle.
fn history_line_count(dir: &Path) -> usize {
    fs::read_dir(dir.join("history"))
        .unwrap()
        .map(|entry| {
            let content = fs::read_to_string(entry.unwrap().path()).unwrap();
            content.lines().filter(|l| !l.is_empty()).count()
        })
        .sum()
}

#[test]
fn pairing_damage_degrades_instead_of_repairing() {
    let (dir, damaged, _turn_id) = pairing_damaged_dir();

    let restored =
        restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();

    let kinds: Vec<DegradationKind> = restored.degraded.iter().map(|d| d.kind).collect();
    assert!(kinds.contains(&DegradationKind::UnansweredToolCalls));
    assert!(kinds.contains(&DegradationKind::OrphanToolResults));
    // Report-only: the hydrated turn is byte-for-byte what history holds.
    let turns = restored.store.turns();
    assert_eq!(turns.front().unwrap().messages, damaged);
}

/// Repair fixes both damage directions and lands in history: a fresh
/// restore hydrates the repaired record (newest wins) with zero
/// pairing findings.
#[test]
fn repair_agent_context_fixes_pairing_damage() {
    let (dir, _damaged, turn_id) = pairing_damaged_dir();

    let report = repair_agent_context(dir.path(), NO_TRUNCATION, true).unwrap();
    assert_eq!(report.actions.len(), 1);
    let action = &report.actions[0];
    assert_eq!(action.turn_id, turn_id);
    assert_eq!(action.unanswered_calls, vec!["c2"]);
    assert_eq!(action.orphan_results, vec!["c9"]);

    let restored =
        restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();
    let kinds: Vec<DegradationKind> = restored.degraded.iter().map(|d| d.kind).collect();
    assert!(!kinds.contains(&DegradationKind::UnansweredToolCalls));
    assert!(!kinds.contains(&DegradationKind::OrphanToolResults));

    // c2 answered with a not-executed error, c9's orphan result
    // removed, c1's real result kept.
    let turn = restored
        .store
        .turns()
        .iter()
        .find(|t| t.id.0 == turn_id)
        .unwrap();
    let ids: Vec<&str> = turn
        .messages
        .iter()
        .filter_map(|m| m.tool_call_id())
        .collect();
    assert_eq!(ids, vec!["c1", "c2"]);
    let c2 = turn
        .messages
        .iter()
        .find(|m| m.tool_call_id() == Some("c2"))
        .unwrap();
    assert!(c2.content().unwrap().contains("not executed"));
}

/// Dry run reports the same actions and writes nothing: no history
/// record, no manifest churn, damage still present on the next restore.
#[test]
fn repair_agent_context_dry_run_writes_nothing() {
    let (dir, _damaged, _turn_id) = pairing_damaged_dir();
    let manifest_before = fs::read(dir.path().join("manifest.json")).unwrap();
    let lines_before = history_line_count(dir.path());

    let report = repair_agent_context(dir.path(), NO_TRUNCATION, false).unwrap();
    assert_eq!(report.actions.len(), 1);

    assert_eq!(history_line_count(dir.path()), lines_before);
    assert_eq!(
        fs::read(dir.path().join("manifest.json")).unwrap(),
        manifest_before
    );

    let restored =
        restore_agent(&AgentId::from("a".to_owned()), dir.path(), NO_TRUNCATION).unwrap();
    let kinds: Vec<DegradationKind> = restored.degraded.iter().map(|d| d.kind).collect();
    assert!(kinds.contains(&DegradationKind::UnansweredToolCalls));
    assert!(kinds.contains(&DegradationKind::OrphanToolResults));
}

/// A repaired record is clean: a second pass plans nothing and
/// appends nothing.
#[test]
fn repair_agent_context_is_idempotent() {
    let (dir, _damaged, _turn_id) = pairing_damaged_dir();

    let first = repair_agent_context(dir.path(), NO_TRUNCATION, true).unwrap();
    assert_eq!(first.actions.len(), 1);
    let lines_after_first = history_line_count(dir.path());

    let second = repair_agent_context(dir.path(), NO_TRUNCATION, true).unwrap();
    assert!(second.actions.is_empty());
    assert_eq!(history_line_count(dir.path()), lines_after_first);
}
