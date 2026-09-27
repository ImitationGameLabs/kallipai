//! Workspace-relocation route tests (`PUT /agents/{id}/workspace-root`).
//!
//! These tests re-point the process-wide INSTANCE_ROOTS singleton, so per
//! the suite convention (see tests/roots_isolation.rs) they live in their
//! own integration-test target: the singleton mutation cannot race the
//! unit-test process.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use kallipai_adk::persistence::{AgentMeta, inactive_dir};
use kallipai_common::agentid::AgentId;
use kallipai_tagma::routes::router;
use kallipai_tagma::state::SharedState;
use kallipai_tagma::test_helpers::make_state;
use serial_test::serial;
use tower::ServiceExt;

const OP_TOKEN: &str = "op-token";

/// Install the instance roots for this target and keep them for the whole
/// process: the lazy shared installer never overwrites a live slot, so
/// within this target every test sees the same stable tree (agent ids are
/// random, so bodies never collide).
fn install_roots() {
    kallipai_tagma::test_helpers::ensure_test_data_dir();
}

fn park_body(id: &AgentId, ws: &std::path::Path, created_by: Option<&AgentId>) {
    let dir = inactive_dir(id).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let meta = AgentMeta {
        workspace_root: ws.to_path_buf(),
        created_by: created_by.cloned(),
        role: String::new(),
        description: String::new(),
        profile_set: None,
        permissions_class: kallipai_adk::config::PermissionClass::Normal,
        delegation_mode: kallipai_adk::config::DelegationMode::CarveOut,
    };
    std::fs::write(
        dir.join("meta.json"),
        serde_json::to_string_pretty(&meta).unwrap(),
    )
    .unwrap();
}

fn data_root() -> std::path::PathBuf {
    kallipai_adk::persistence::data_dir_root().unwrap()
}

fn sup_base() -> std::path::PathBuf {
    // A stable directory OUTSIDE the data root (its parent) for the
    // supervisor ws trees the nesting tests need.
    data_root().parent().unwrap().to_path_buf()
}

/// One HTTP round-trip against the real router with real bearer auth.
/// Returns (status, parsed body).
async fn relocate(
    state: &SharedState,
    id: &AgentId,
    bearer: &str,
    new_ws: &std::path::Path,
) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method(Method::PUT)
        .uri(format!("/agents/{id}/workspace-root"))
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {bearer}"),
        )
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!(
            "{{\"workspace_root\": {}}}",
            serde_json::to_string(&new_ws.display().to_string()).unwrap()
        )))
        .expect("static parts");
    let response = router()
        .with_state(state.clone())
        .oneshot(request)
        .await
        .unwrap_or_else(|infallible| match infallible {});
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("small body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

fn seed_supervisor_and_body(
    id: &AgentId,
    sup: &AgentId,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let sup_ws = sup_base().join(format!("sup-ws-{id}"));
    std::fs::create_dir_all(&sup_ws).unwrap();
    park_body(sup, &sup_ws, None);
    let old_ws = sup_ws.join("old");
    std::fs::create_dir_all(&old_ws).unwrap();
    park_body(id, &old_ws, Some(sup));
    (sup_ws, old_ws)
}

#[tokio::test]
#[serial]
async fn relocation_is_reserved_for_operator_or_root() {
    install_roots();
    let state = make_state();
    let caller = AgentId::random();
    let id = AgentId::random();
    let sup = AgentId::random();
    seed_supervisor_and_body(&id, &sup);
    // A live agent bearer (authenticated, but neither operator nor root):
    // the authorization face rejects with 403.
    let (entry, _rx) = kallipai_tagma::test_helpers::make_entry_with_rx(
        Some(sup.clone()),
        "tok-caller".to_string(),
    );
    state.registry.write().await.register(
        caller.clone(),
        kallipai_tagma::state::RegistryEntry::Live(entry),
    );
    let (status, body) = relocate(
        &state,
        &id,
        "tok-caller",
        &sup_base().join("sup-ws").join("new-home"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("reserved for the operator or the root agent")
    );
}

#[tokio::test]
#[serial]
async fn relocation_404s_without_a_parked_body() {
    install_roots();
    let state = make_state();
    let id = AgentId::random();
    let ws = tempfile::tempdir().unwrap();
    let (status, _) = relocate(&state, &id, OP_TOKEN, ws.path()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[serial]
async fn relocation_rejects_a_live_double_body() {
    install_roots();
    let state = make_state();
    let id = AgentId::random();
    let sup = AgentId::random();
    let (sup_ws, _) = seed_supervisor_and_body(&id, &sup);
    let live = kallipai_adk::persistence::agent_dir(&id).unwrap();
    std::fs::create_dir_all(&live).unwrap();
    let (status, _) = relocate(&state, &id, OP_TOKEN, &sup_ws).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
#[serial]
async fn relocation_rejects_a_root_body() {
    install_roots();
    let state = make_state();
    let id = AgentId::random();
    let ws = tempfile::tempdir().unwrap();
    park_body(&id, ws.path(), None);
    let (status, _) = relocate(&state, &id, OP_TOKEN, ws.path()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[serial]
async fn relocation_enforces_nesting_disjoint_then_updates() {
    install_roots();
    let state = make_state();
    let id = AgentId::random();
    let sup = AgentId::random();
    let sup_ws = sup_base().join(format!("sup-ws-{id}"));
    std::fs::create_dir_all(&sup_ws).unwrap();
    park_body(&sup, &sup_ws, None);
    let old_ws = sup_ws.join("old");
    std::fs::create_dir_all(&old_ws).unwrap();
    park_body(&id, &old_ws, Some(&sup));

    // Inside the data root: the disjoint predicate rejects first.
    std::fs::create_dir_all(data_root().join("inner")).unwrap();
    let (status, _) = relocate(&state, &id, OP_TOKEN, &data_root().join("inner")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A sibling outside the supervisor's ws: nesting rejects, and the 400
    // carries the boundary message.
    let outside = sup_base().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let (status, body) = relocate(&state, &id, OP_TOKEN, &outside).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("workspace outside supervisor boundary")
    );

    // Nested under the supervisor's ws: the truth switch flips.
    let new_ws = sup_ws.join("new-home");
    std::fs::create_dir_all(&new_ws).unwrap();
    let (status, body) = relocate(&state, &id, OP_TOKEN, &new_ws).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["old_workspace_root"], old_ws.display().to_string());
    assert_eq!(body["workspace_root"], new_ws.display().to_string());
    let meta = kallipai_adk::persistence::read_meta_from_dir(&inactive_dir(&id).unwrap()).unwrap();
    assert_eq!(meta.workspace_root, new_ws);
    assert_eq!(meta.role, "", "only workspace_root moved");
}

#[tokio::test]
#[serial]
async fn relocation_reports_a_lock_collision_as_conflict() {
    install_roots();
    let state = make_state();
    let id = AgentId::random();
    let sup = AgentId::random();
    let peer = AgentId::random();
    let sup_ws = sup_base().join(format!("sup-ws-{id}"));
    std::fs::create_dir_all(&sup_ws).unwrap();
    park_body(&sup, &sup_ws, None);
    let old_ws = sup_ws.join("old");
    std::fs::create_dir_all(&old_ws).unwrap();
    park_body(&id, &old_ws, Some(&sup));
    // A live peer holding the supervisor's dir (not in the chain): the
    // pre-check must surface it as a conflict before anything writes.
    state.lock_manager.acquire(&peer, &sup_ws, &[]).unwrap();
    let new_ws = sup_ws.join("new-home");
    std::fs::create_dir_all(&new_ws).unwrap();
    let (status, _) = relocate(&state, &id, OP_TOKEN, &new_ws).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
#[serial]
async fn relocation_rejects_a_file_target() {
    install_roots();
    let state = make_state();
    let id = AgentId::random();
    let sup = AgentId::random();
    let (sup_ws, _) = seed_supervisor_and_body(&id, &sup);
    // canonicalize succeeds for a plain file; the explicit is_dir check is
    // what keeps a file target from reaching the write point.
    let file_target = sup_ws.join("a-file");
    std::fs::write(&file_target, "x").unwrap();
    let (status, _) = relocate(&state, &id, OP_TOKEN, &file_target).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[serial]
async fn relocation_rejects_a_dangling_gitdir_in_the_new_workspace() {
    install_roots();
    let state = make_state();
    let id = AgentId::random();
    let sup = AgentId::random();
    let (sup_ws, _) = seed_supervisor_and_body(&id, &sup);
    // The new workspace carries a .git file whose gitdir does not resolve:
    // `git worktree move` has not been finished.
    let new_ws = sup_ws.join("new-home");
    std::fs::create_dir_all(&new_ws).unwrap();
    std::fs::write(
        new_ws.join(".git"),
        "gitdir: /nonexistent/path/.git/worktrees/x\n",
    )
    .unwrap();
    let (status, _) = relocate(&state, &id, OP_TOKEN, &new_ws).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[serial]
async fn relocation_rejects_a_malformed_git_file() {
    install_roots();
    let state = make_state();
    let id = AgentId::random();
    let sup = AgentId::random();
    let (sup_ws, _) = seed_supervisor_and_body(&id, &sup);
    // A .git file with no gitdir line is malformed: fail closed, never
    // silently skip the worktree consistency check.
    let new_ws = sup_ws.join("new-home");
    std::fs::create_dir_all(&new_ws).unwrap();
    std::fs::write(new_ws.join(".git"), "not a gitdir pointer\n").unwrap();
    let (status, _) = relocate(&state, &id, OP_TOKEN, &new_ws).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[serial]
async fn relocation_accepts_a_resolvable_relative_gitdir() {
    install_roots();
    let state = make_state();
    let id = AgentId::random();
    let sup = AgentId::random();
    let (sup_ws, _) = seed_supervisor_and_body(&id, &sup);
    // A relative gitdir resolves against the NEW workspace root, and a
    // resolvable one lets the truth switch flip.
    let new_ws = sup_ws.join("new-home");
    std::fs::create_dir_all(new_ws.join("somewhere/.git")).unwrap();
    std::fs::write(new_ws.join(".git"), "gitdir: ./somewhere/.git\n").unwrap();
    let (status, body) = relocate(&state, &id, OP_TOKEN, &new_ws).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["workspace_root"], new_ws.display().to_string());
    let meta = kallipai_adk::persistence::read_meta_from_dir(&inactive_dir(&id).unwrap()).unwrap();
    assert_eq!(meta.workspace_root, new_ws);
}

#[tokio::test]
#[serial]
async fn relocation_survives_the_full_deactivate_relocate_reactivate_chain() {
    // The orchestration-level pin: deactivate (converge face) -> this
    // route -> reactivate -> the restore INPUT contract (config
    // re-derives from the recorded new ws; the lock lands on the new
    // path). Full restore_one (bridge/backend spawn) stays
    // integration-surface; its "restore fails = the body stays inactive"
    // semantics are untouched by this route.
    install_roots();
    let state = make_state();
    let id = AgentId::random();
    let sup = AgentId::random();

    let sup_ws = sup_base().join(format!("sup-ws-{id}"));
    std::fs::create_dir_all(&sup_ws).unwrap();
    park_body(&sup, &sup_ws, None);
    // The body starts LIVE: create_agent_dir writes the active area.
    let old_ws = sup_ws.join("old");
    std::fs::create_dir_all(&old_ws).unwrap();
    kallipai_adk::persistence::create_agent_dir(
        &id,
        AgentMeta {
            workspace_root: old_ws.clone(),
            created_by: Some(sup.clone()),
            role: String::new(),
            description: String::new(),
            profile_set: None,
            permissions_class: kallipai_adk::config::PermissionClass::Normal,
            delegation_mode: kallipai_adk::config::DelegationMode::CarveOut,
        },
    )
    .unwrap();

    // Deactivate: active -> inactive.
    kallipai_adk::persistence::deactivate_agent_dir(&id).unwrap();

    // The route: pre-checks plus the truth switch.
    let new_ws = sup_ws.join("new-home");
    std::fs::create_dir_all(&new_ws).unwrap();
    let (status, body) = relocate(&state, &id, OP_TOKEN, &new_ws).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["workspace_root"], new_ws.display().to_string());

    // Reactivate: inactive -> active; the restore input contract.
    kallipai_adk::persistence::reactivate_agent_dir(&id).unwrap();
    let meta = kallipai_adk::persistence::read_meta(&id).unwrap();
    assert_eq!(meta.workspace_root, new_ws);
    let config =
        kallipai_adk::config::AgentConfig::load(None, vec![], Some(meta.workspace_root)).unwrap();
    assert_eq!(config.workspace_root, new_ws);
    let acquired = state
        .lock_manager
        .acquire(&id, &new_ws, std::slice::from_ref(&sup))
        .unwrap();
    assert_eq!(acquired, kallipai_adk::dirlock::AcquireOutcome::Acquired);
    assert!(state.lock_manager.holds_exact(&id, &new_ws).unwrap());
    assert!(!state.lock_manager.holds_exact(&id, &old_ws).unwrap());
}

#[tokio::test]
#[serial]
async fn relocation_500s_on_a_circular_supervisor_chain() {
    install_roots();
    let state = make_state();
    let a = AgentId::random();
    let b = AgentId::random();
    let sup_ws = sup_base().join(format!("sup-ws-{a}"));
    std::fs::create_dir_all(&sup_ws).unwrap();
    park_body(&a, &sup_ws, Some(&b));
    park_body(&b, &sup_ws, Some(&a));
    let (status, _) = relocate(&state, &a, OP_TOKEN, &sup_ws).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
#[serial]
async fn relocation_500s_when_the_ancestor_is_missing_everywhere() {
    install_roots();
    let state = make_state();
    let id = AgentId::random();
    let ghost = AgentId::random();
    let old_ws = sup_base().join("old-ws");
    std::fs::create_dir_all(&old_ws).unwrap();
    park_body(&id, &old_ws, Some(&ghost));
    let (status, _) = relocate(&state, &id, OP_TOKEN, &old_ws).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
#[serial]
async fn relocation_root_identity_passes_authorization() {
    install_roots();
    let state = make_state();
    let root = AgentId::random();
    let id = AgentId::random();
    // Seed the root the converge tests use, mint its token, then present
    // the root bearer: the failure is the 404 for a missing parked body,
    // never the 403.
    let (entry, _rx) =
        kallipai_tagma::test_helpers::make_entry_with_rx(None, format!("tok-{root}"));
    state.registry.write().await.register(
        root.clone(),
        kallipai_tagma::state::RegistryEntry::Live(entry),
    );
    let ws = tempfile::tempdir().unwrap();
    let (status, _) = relocate(&state, &id, &format!("tok-{root}"), ws.path()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[serial]
async fn relocation_500s_when_the_active_ancestor_meta_is_corrupt() {
    install_roots();
    let state = make_state();
    let id = AgentId::random();
    let sup = AgentId::random();
    let sup_ws = sup_base().join("sup-ws");
    std::fs::create_dir_all(&sup_ws).unwrap();
    // Supervisor body in the ACTIVE area with a corrupt meta.json: present
    // but unreadable is chain corruption (500 naming the surface), never a
    // silent fall-through to the inactive face.
    let active_sup = kallipai_adk::persistence::agent_dir(&sup).unwrap();
    std::fs::create_dir_all(&active_sup).unwrap();
    std::fs::write(active_sup.join("meta.json"), "not json at all").unwrap();
    let old_ws = sup_ws.join("old");
    std::fs::create_dir_all(&old_ws).unwrap();
    park_body(&id, &old_ws, Some(&sup));
    let new_ws = sup_ws.join("new-home");
    std::fs::create_dir_all(&new_ws).unwrap();
    let (status, body) = relocate(&state, &id, OP_TOKEN, &new_ws).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    // The surface name goes to the tracing log, not the wire: the API
    // masks 500 bodies as the generic "internal error".
    assert_eq!(body["error"]["message"].as_str(), Some("internal error"));
}
