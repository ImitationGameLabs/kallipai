//! The capture's two resolution paths: a live agent answers from its store,
//! and an agent with no readable body is skipped rather than counted as zero.

use super::*;
use crate::test_helpers::{add_faulted_root, add_root, ensure_test_data_dir, make_state};
use just_llm_client::types::generation::Usage;
use kallipai_common::AgentId;

fn usage(prompt: u32, completion: u32) -> Usage {
    Usage {
        completion_tokens: completion,
        prompt_tokens: prompt,
        cache_read_tokens: None,
        cache_write_tokens: None,
        total_tokens: prompt + completion,
        completion_tokens_details: None,
    }
}

#[tokio::test]
async fn a_live_agent_answers_from_its_context_store() {
    ensure_test_data_dir();
    let state = make_state();
    let id = AgentId::random();
    let store = {
        let mut registry = state.registry.write().await;
        add_root(&mut registry, &id);
        registry
            .get(&id)
            .and_then(|entry| entry.as_live())
            .map(|live| live.agent.store.clone())
            .expect("live entry")
    };
    store.lock().await.accumulate_usage(&usage(10, 5));

    let snapshot = capture_usage(&state).await;
    let captured = snapshot.agents.get(&id.to_string()).expect("captured");
    assert_eq!(captured.usage.consumed(), 15);
    assert_eq!(captured.source, UsageSource::Live);
}

#[tokio::test]
async fn an_agent_without_a_readable_body_is_skipped() {
    ensure_test_data_dir();
    let state = make_state();
    let id = AgentId::random();
    add_faulted_root(&mut *state.registry.write().await, &id, "boom");

    let snapshot = capture_usage(&state).await;
    // The disk scan reads process-wide roots that sibling tests also touch,
    // so assert on this agent: it must be named as a skip, never counted.
    assert!(!snapshot.agents.contains_key(&id.to_string()));
    let skipped = snapshot
        .skipped
        .iter()
        .find(|skipped| skipped.agent_id == id.to_string())
        .expect("this agent is skipped");
    assert_eq!(skipped.reason, "no agent directory");
}

/// A restorable body on disk: `meta.json` plus, when given, the manifest the
/// counters live in.
fn write_body(id: &AgentId, manifest: Option<&str>) {
    let dir = persistence::agent_dir(id).expect("agent dir");
    std::fs::create_dir_all(&dir).expect("create body");
    std::fs::write(
        dir.join("meta.json"),
        "{\"workspace_root\":\"/tmp/kallip\"}",
    )
    .expect("write meta");
    if let Some(manifest) = manifest {
        std::fs::write(dir.join("manifest.json"), manifest).expect("write manifest");
    }
}

fn manifest_json(prompt_tokens: u64, completion_tokens: u64) -> String {
    serde_json::json!({
        "version": 1,
        "conversation_turn_ids": [],
        "cumulative_usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "cache_read_tokens": 0,
            "cache_write_tokens": 0,
        },
        "next_turn_id": 0,
        "retry_log": [],
    })
    .to_string()
}

#[tokio::test]
async fn a_parked_body_is_read_from_disk() {
    ensure_test_data_dir();
    let state = make_state();
    let id = AgentId::random();
    write_body(&id, Some(&manifest_json(120, 30)));
    persistence::deactivate_agent_dir(&id).expect("park the body");

    let snapshot = capture_usage(&state).await;
    let captured = snapshot
        .agents
        .get(&id.to_string())
        .expect("cold body read");
    assert_eq!(captured.usage.consumed(), 150);
    assert_eq!(captured.source, UsageSource::Disk);
}

#[tokio::test]
async fn a_live_reading_outranks_the_directory() {
    ensure_test_data_dir();
    let state = make_state();
    let id = AgentId::random();
    // A stale body on disk says 999; the running agent's store says 10.
    write_body(&id, Some(&manifest_json(999, 0)));
    let store = {
        let mut registry = state.registry.write().await;
        add_root(&mut registry, &id);
        registry
            .get(&id)
            .and_then(|entry| entry.as_live())
            .map(|live| live.agent.store.clone())
            .expect("live entry")
    };
    store.lock().await.accumulate_usage(&usage(5, 5));

    let snapshot = capture_usage(&state).await;
    let captured = snapshot.agents.get(&id.to_string()).expect("captured");
    assert_eq!(captured.usage.consumed(), 10, "the live reading wins");
    assert_eq!(captured.source, UsageSource::Live);
}

#[tokio::test]
async fn a_body_without_a_readable_manifest_is_skipped() {
    ensure_test_data_dir();
    let state = make_state();
    let id = AgentId::random();
    // Restorable (its meta parses), but its counters are unreadable: the
    // capture must name the skip rather than count the agent as zero.
    write_body(&id, None);

    let snapshot = capture_usage(&state).await;
    assert!(!snapshot.agents.contains_key(&id.to_string()));
    let skipped = snapshot
        .skipped
        .iter()
        .find(|skipped| skipped.agent_id == id.to_string())
        .expect("this agent is skipped");
    assert!(
        skipped.reason.contains("no readable manifest"),
        "{}",
        skipped.reason
    );
}

#[tokio::test(start_paused = true)]
async fn a_capture_that_cannot_finish_degrades_to_a_capture_error() {
    ensure_test_data_dir();
    let state = make_state();
    let id = AgentId::random();
    let store = {
        let mut registry = state.registry.write().await;
        add_root(&mut registry, &id);
        registry
            .get(&id)
            .and_then(|entry| entry.as_live())
            .map(|live| live.agent.store.clone())
            .expect("live entry")
    };
    // Hold the agent's store lock: the capture parks on it, so only the
    // budget can end the call. The paused clock reaches it at once.
    let held = store.lock().await;
    // An outer budget above the capture's own: the inner 10s fires first
    // for an implementation that keeps its budget, while one that lost the
    // budget fails here in 0.01s instead of hanging the suite.
    let snapshot = tokio::time::timeout(std::time::Duration::from_secs(30), capture_usage(&state))
        .await
        .expect("capture returned");
    drop(held);

    assert!(snapshot.agents.is_empty());
    let error = snapshot.capture_error.expect("capture_error");
    assert!(error.contains("capture exceeded"), "{error}");
}
