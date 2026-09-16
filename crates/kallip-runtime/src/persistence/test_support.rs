//! Shared test harness for the persistence domain modules.

use crate::context::AgenticContext as _;
use crate::context::ContextStore;
use crate::history::{HistoryWriter, RecordKind};
use crate::persistence::persist_context;
use crate::test_support::{TurnMessage, assistant_msg, tool_calls_msg, tool_result_msg, user_msg};
use tempfile::TempDir;

/// Tail budget for restores that should not truncate: every fixture's
/// history is far below this. Truncation itself is tested with explicit
/// budgets.
pub const NO_TRUNCATION: usize = 65_536;

// ----- archive-on-remove tests -----
// These mutate the process-global KALLIP_TAGMA_SLUG/XDG_DATA_HOME pair, so they are
// serialized (serial_test) and each scopes a tempfile::TempDir via temp_env;
// the data root is `<tmp>/kallipai/tagmata/test-instance` — use
// `data_dir_root()` inside the closure instead of `tmp.path()` directly.
pub fn with_data_dir<R>(f: impl FnOnce(&TempDir) -> R) -> R {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().to_str().unwrap().to_owned();
    temp_env::with_vars(
        [
            ("KALLIP_TAGMA_SLUG", Some("test-instance")),
            ("XDG_DATA_HOME", Some(path.as_str())),
        ],
        || f(&tmp),
    )
}

/// A split-format directory with real damage to inflict: a pin, three
/// conversation turns (the newest pairing-damaged on demand), usage and
/// retry-log state that only the manifest carries, and matching history.
pub fn wreckable_dir(oldest_damaged: bool) -> (TempDir, ContextStore) {
    let dir = TempDir::new().unwrap();
    let mut store = ContextStore::new();
    store.pin("note", assistant_msg("pinned")).unwrap();
    let histories: [Vec<TurnMessage>; 3] = [
        if oldest_damaged {
            vec![tool_result_msg("orphan", "ghost"), assistant_msg("reply-a")]
        } else {
            vec![user_msg("ask-a"), assistant_msg("reply-a")]
        },
        vec![user_msg("ask-b"), assistant_msg("reply-b")],
        vec![
            tool_calls_msg(&[("c1", "read")]),
            tool_result_msg("ok", "c1"),
        ],
    ];
    let history = HistoryWriter::new(dir.path().to_path_buf());
    for msgs in histories {
        let (id, _) = store.push_turn(msgs.clone());
        history
            .append(Some(id.0), &msgs, 8, RecordKind::Turn, None, &[])
            .unwrap();
    }
    store.accumulate_usage(&crate::test_support::usage(1000));
    store.retry_log.push(kallip_common::retry::RetryRecord {
        timestamp: 9,
        round: 0,
        attempt: 1,
        max_attempts: 3,
        error: "wreck".into(),
        delay_secs: 1.0,
        endpoint: None,
        kind: kallip_common::retry::RetryKind::Transport,
        quota_reset: None,
    });
    persist_context(&store, dir.path()).unwrap();
    // A second persist leaves the first manifest as the .bak; the churn
    // turn also reaches history, so the rescan below sees it.
    let churn = vec![user_msg("churn")];
    let (churn_id, _) = store.push_turn(churn.clone());
    let history = HistoryWriter::new(dir.path().to_path_buf());
    history
        .append(Some(churn_id.0), &churn, 8, RecordKind::Turn, None, &[])
        .unwrap();
    persist_context(&store, dir.path()).unwrap();
    (dir, store)
}

/// A legacy agent directory: whole-store `context.json` plus matching
/// history records for the conversation turns.
pub fn legacy_fixture() -> (ContextStore, Vec<TurnMessage>) {
    let mut store = ContextStore::new();
    store.pin("note", assistant_msg("pinned note")).unwrap();
    store.push_turn(vec![user_msg("first question")]);
    store.push_turn(vec![assistant_msg("first answer")]);
    store.retry_log.push(kallip_common::retry::RetryRecord {
        timestamp: 9,
        round: 0,
        attempt: 1,
        max_attempts: 3,
        error: "legacy".into(),
        delay_secs: 1.0,
        endpoint: None,
        kind: kallip_common::retry::RetryKind::Transport,
        quota_reset: None,
    });
    // Expected conversation window, in order, for history.
    let convo = vec![user_msg("first question"), assistant_msg("first answer")];
    (store, convo)
}

pub fn write_legacy(
    dir: &TempDir,
    store: &ContextStore,
    with_history: bool,
    convo: &[TurnMessage],
) {
    std::fs::write(
        dir.path().join("context.json"),
        serde_json::to_string(store).unwrap(),
    )
    .unwrap();
    if with_history {
        let history = HistoryWriter::new(dir.path().to_path_buf());
        let ids: Vec<u64> = store
            .turns()
            .iter()
            .filter(|t| !t.is_pinned())
            .map(|t| t.id.0)
            .collect();
        for (id, msg) in ids.iter().zip(convo) {
            history
                .append(
                    Some(*id),
                    std::slice::from_ref(msg),
                    8,
                    RecordKind::Turn,
                    None,
                    &[],
                )
                .unwrap();
        }
    }
}
