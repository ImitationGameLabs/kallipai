use std::collections::HashMap;
use std::path::PathBuf;

use super::{
    AgentConfig, AgentId, DelegationMode, MAX_ACTIVITY_CHARS, PermissionClass, PermissionProfile,
    interrupt_agent, list_agents, remove_agent, resolve_granted_class, truncate_chars,
};
use crate::auth::{AuthIdentity, Identity};
use crate::lifecycle::{
    EstablishLockFailure, compose_system_prompt, establish_workspace_lock, inject_identity_env,
    meta_skill_content, resolve_root_agent,
};
use crate::state::RegistryEntry;
use crate::test_helpers::{
    add_faulted_root, add_faulted_sub, add_root, add_sub, make_entry, make_entry_with_rx,
    make_state,
};
use axum::extract::{Path, Query, State};
use kallip_common::protocol::ListAgentsQuery;

#[test]
fn truncate_keeps_short_strings() {
    let mut s = String::from("abc");
    truncate_chars(&mut s, 10);
    assert_eq!(s, "abc");
    let mut s = String::new();
    truncate_chars(&mut s, 10);
    assert!(s.is_empty());
}

#[test]
fn truncate_caps_on_char_boundary() {
    // "héllo" is 5 chars (é is one char, two bytes); cap at 2 → "hé".
    let mut s = String::from("héllo");
    truncate_chars(&mut s, 2);
    assert_eq!(s, "hé");
    let mut s = String::from("abcdef");
    truncate_chars(&mut s, 3);
    assert_eq!(s, "abc");
}

#[test]
fn truncate_caps_to_max_activity_chars() {
    let mut s = "x".repeat(MAX_ACTIVITY_CHARS + 100);
    truncate_chars(&mut s, MAX_ACTIVITY_CHARS);
    assert_eq!(s.chars().count(), MAX_ACTIVITY_CHARS);
}

// -- inject_identity_env (shared by fresh spawn + reactivation) --

#[test]
fn inject_identity_env_sets_root_always_and_supervisor_only_for_subagents() {
    let root = AgentId::from("root-1".to_owned());
    let sup = AgentId::from("sup-1".to_owned());

    // Root: supervisor unset, root set to self.
    let mut env = HashMap::new();
    inject_identity_env(&mut env, None, &root);
    assert_eq!(
        env.get("KALLIP_ROOT_AGENT_ID").map(String::as_str),
        Some("root-1")
    );
    assert!(
        !env.contains_key("KALLIP_SUPERVISOR_AGENT_ID"),
        "root must have no supervisor env (absent, not empty)"
    );

    // Subagent: both set.
    let mut env = HashMap::new();
    inject_identity_env(&mut env, Some(&sup), &root);
    assert_eq!(
        env.get("KALLIP_ROOT_AGENT_ID").map(String::as_str),
        Some("root-1")
    );
    assert_eq!(
        env.get("KALLIP_SUPERVISOR_AGENT_ID").map(String::as_str),
        Some("sup-1")
    );
}

#[test]
fn inject_identity_env_clears_stale_supervisor_on_root() {
    // A reused env map may carry a stale `KALLIP_SUPERVISOR_AGENT_ID` from a
    // prior incarnation. The helper advertises "safe on a reused map", so
    // passing `None` (root) must REMOVE the stale key, not leave it.
    let root = AgentId::from("root-1".to_owned());
    let mut env = HashMap::new();
    env.insert("KALLIP_SUPERVISOR_AGENT_ID".into(), "stale-sup".into());
    inject_identity_env(&mut env, None, &root);
    assert!(
        !env.contains_key("KALLIP_SUPERVISOR_AGENT_ID"),
        "stale supervisor key must be removed on root, not left dangling: {env:?}"
    );
    assert_eq!(
        env.get("KALLIP_ROOT_AGENT_ID").map(String::as_str),
        Some("root-1")
    );
}

#[test]
fn resolve_root_agent_returns_registry_root() {
    // The root is the tagma's single registered root, resolved
    // independently of any supervisor chain.
    let root = AgentId::from("root-1".to_owned());
    assert_eq!(resolve_root_agent(Some(&root)), root);
}

// -- compose_system_prompt (per-agent identity section) --

/// Minimal config exercising only the fields `compose_system_prompt` reads.
fn identity_config(created_by: Option<AgentId>, role: &str, description: &str) -> AgentConfig {
    AgentConfig {
        created_by,
        role: role.into(),
        description: description.into(),
        permissions_class: PermissionClass::Normal,
        // Synthetic base so this test exercises composition mechanics, not
        // the (separately guarded) content of DEFAULT_SYSTEM_PROMPT.
        system_prompt: "TEST BASE BODY".into(),
        ..AgentConfig::default()
    }
}

#[test]
fn compose_system_prompt_root_has_no_unsubstituted_placeholder() {
    // The `.replace()` chain must consume every `{placeholder}`. A typo'd
    // name would leave a literal `{...}` in the production prompt; the
    // compiler can't catch it, so this generic check does.
    let cfg = identity_config(None, "root", "");
    let id = AgentId::from("root-1".to_owned());
    let prompt = compose_system_prompt(&cfg, id.clone(), id.clone());
    // The id value must actually render — guards against a `.replace` that
    // silently substitutes an empty/wrong value without leaving braces.
    assert!(prompt.contains("root-1"), "own id must render: {prompt}");
    assert!(
        !prompt.contains('{') && !prompt.contains('}'),
        "unsubstituted placeholder in root prompt: {prompt}"
    );
}

#[test]
fn compose_system_prompt_subagent_has_no_unsubstituted_placeholder() {
    // Distinct placeholder set from the root template — exercise both.
    let cfg = identity_config(
        Some(AgentId::from("sup-1".to_owned())),
        "researcher",
        "gathers sources",
    );
    let id = AgentId::from("sub-1".to_owned());
    let root = AgentId::from("root-1".to_owned());
    let prompt = compose_system_prompt(&cfg, id.clone(), root.clone());
    // Values must actually render (not just "no braces left") — guards
    // against a `.replace` silently substituting empty/wrong values.
    assert!(prompt.contains("sub-1"), "own id must render: {prompt}");
    assert!(prompt.contains("root-1"), "root id must render: {prompt}");
    assert!(
        !prompt.contains('{') && !prompt.contains('}'),
        "unsubstituted placeholder in subagent prompt: {prompt}"
    );
}

#[test]
fn compose_system_prompt_user_text_with_braces_is_not_rescanned() {
    // User-controlled `role`/`description` are substituted LAST; a value
    // containing a `{...}` fragment must survive as a literal and NOT be
    // re-scanned by an earlier placeholder's pass. We exercise this by
    // embedding the `{permission_class}` token (substituted earlier to
    // `Normal`) in the user text — the literal must appear in the prompt,
    // proving the role/description slot was not re-scanned.
    let root_cfg = identity_config(None, "{permission_class}", "");
    let root_id = AgentId::from("root-1".to_owned());
    let root_prompt = compose_system_prompt(&root_cfg, root_id.clone(), root_id.clone());
    // The real permission class renders in its own line...
    assert!(
        root_prompt.contains("- permission class:"),
        "permission-class line must render: {root_prompt}"
    );
    // ...and the literal token from the user-controlled role survives
    // unsubstituted (it was inserted only after the `{permission_class}` pass).
    assert!(
        root_prompt.contains("{permission_class}"),
        "user-text brace fragment must survive as a literal: {root_prompt}"
    );

    // Same contract for the subagent `description` slot.
    let sub_cfg = identity_config(
        Some(AgentId::from("sup-1".to_owned())),
        "researcher",
        "desc {permission_class} end",
    );
    let sub_prompt =
        compose_system_prompt(&sub_cfg, AgentId::from("sub-1".to_owned()), root_id.clone());
    assert!(
        sub_prompt.contains("desc {permission_class} end"),
        "user-text brace fragment must survive as a literal: {sub_prompt}"
    );
}

#[test]
fn compose_system_prompt_static_tail_identical_across_variants() {
    // The static-shared tail (base + meta-skill) is the byte-identical,
    // cache-friendly suffix across every agent. Verify both variants end
    // with exactly that tail built from the same config base.
    let root_cfg = identity_config(None, "root", "");
    let sub_cfg = identity_config(Some(AgentId::from("sup-1".to_owned())), "researcher", "x");
    let root_id = AgentId::from("root-1".to_owned());
    let root_prompt = compose_system_prompt(&root_cfg, root_id.clone(), root_id.clone());
    let sub_prompt =
        compose_system_prompt(&sub_cfg, AgentId::from("sub-1".to_owned()), root_id.clone());
    let tail = format!("{}\n\n{}", root_cfg.system_prompt, meta_skill_content());
    assert!(
        root_prompt.ends_with(&tail),
        "root prompt must end with the shared static tail"
    );
    assert!(
        sub_prompt.ends_with(&tail),
        "subagent prompt must end with the shared static tail"
    );
}

// -- resolve_granted_class (the §2.3 reference-monitor decision, extracted) --

#[test]
fn granted_defaults_to_tier_ceiling_when_unrequested() {
    // No explicit request -> historical behavior: grant the ceiling.
    assert_eq!(
        resolve_granted_class(PermissionClass::Normal, PermissionClass::Normal, None).unwrap(),
        PermissionClass::Normal
    );
    assert_eq!(
        resolve_granted_class(PermissionClass::Guest, PermissionClass::Guest, None).unwrap(),
        PermissionClass::Guest
    );
}

#[test]
fn granted_accepts_explicit_downgrade() {
    // A Normal-ceiling, Normal supervisor may actively grant Guest.
    assert_eq!(
        resolve_granted_class(
            PermissionClass::Normal,
            PermissionClass::Normal,
            Some(PermissionClass::Guest)
        )
        .unwrap(),
        PermissionClass::Guest
    );
    // Asking for exactly the ceiling is fine too.
    assert_eq!(
        resolve_granted_class(
            PermissionClass::Normal,
            PermissionClass::Normal,
            Some(PermissionClass::Normal)
        )
        .unwrap(),
        PermissionClass::Normal
    );
}

#[test]
fn granted_rejects_request_above_tier_ceiling() {
    // depth-2 tier (ceiling Guest) cannot be bumped to Normal, even though the
    // supervisor is Normal.
    let err = resolve_granted_class(
        PermissionClass::Guest,
        PermissionClass::Normal,
        Some(PermissionClass::Normal),
    )
    .unwrap_err();
    assert!(err.to_string().contains("tier ceiling"), "{}", err);
}

#[test]
fn granted_rejects_request_above_downgraded_supervisor() {
    // M1: a supervisor downgraded to Guest can no longer grant a child at its
    // tier's default Normal ceiling — the child's granted (Normal, the ceiling)
    // exceeds the supervisor's granted (Guest). Fail-closed: correct escalation
    // prevention, newly reachable once downgrade exists.
    let err = resolve_granted_class(
        PermissionClass::Normal,
        PermissionClass::Guest,
        None, // child asks for the default ceiling, which is now too high
    )
    .unwrap_err();
    assert!(err.to_string().contains("supervisor"), "{}", err);
}

// -- establish_workspace_lock (the shared carve: transfer + acquire + guards) --

/// A Normal `AgentConfig` rooted at `ws`, reusing `make_entry`'s template so
/// every field is populated.
fn normal_config(ws: &std::path::Path) -> AgentConfig {
    let mut config = make_entry(None, String::new()).identity.config;
    config.workspace_root = ws.to_path_buf();
    config.permissions = PermissionProfile::new(ws.to_path_buf());
    config.permissions_class = PermissionClass::Normal;
    config.created_by = None;
    config
}

/// Unique existing temp dir (acquire canonicalizes the path).
fn ws_dir(label: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "ja-acquire-ws-test-{}-{label}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn establish_workspace_lock_normal_root_acquires() {
    let state = make_state();
    let root = AgentId::from("root".to_owned());
    let ws = ws_dir("root");
    let cfg = normal_config(&ws);
    let established = establish_workspace_lock(&state, &root, &cfg, &[])
        .expect("Normal root acquires its workspace");
    // Lock is held while the guard lives and releases on drop.
    assert_eq!(state.lock_manager.holder(&ws).unwrap(), Some(root.clone()));
    drop(established);
    assert!(state.lock_manager.holder(&ws).unwrap().is_none());
}

#[tokio::test]
async fn establish_workspace_lock_nested_child_no_longer_conflicts() {
    // The original bug: a Normal root holding /proj made any Normal nested
    // child's workspace acquire 409. With the chain, the child acquires.
    let state = make_state();
    let root = AgentId::from("root".to_owned());
    let root_ws = ws_dir("proj");
    let child_ws = root_ws.join("sub");
    std::fs::create_dir_all(&child_ws).unwrap();

    // Root holds /proj for the duration of the child acquire.
    let root_established = establish_workspace_lock(&state, &root, &normal_config(&root_ws), &[])
        .expect("root acquires");

    // Child's chain contains root → delegation, not conflict.
    let mut child_cfg = normal_config(&child_ws);
    child_cfg.created_by = Some(root.clone());
    let child = AgentId::from("child".to_owned());
    let child_established =
        establish_workspace_lock(&state, &child, &child_cfg, std::slice::from_ref(&root))
            .expect("nested child acquires via delegation chain");
    // Carve-out: the child's region appears read-only in the root's view.
    let ro = state.lock_manager.readonly_paths(&root).unwrap();
    assert_eq!(ro, vec![std::fs::canonicalize(&child_ws).unwrap()]);
    drop(child_established);
    drop(root_established);
}

#[tokio::test]
async fn establish_workspace_lock_peer_without_chain_conflicts() {
    // Same topology, but the acquirer is NOT a delegation descendant
    // (empty chain) → Busy, the pre-fix behavior.
    let state = make_state();
    let root = AgentId::from("root".to_owned());
    let root_ws = ws_dir("proj2");
    let nested = root_ws.join("sub");
    std::fs::create_dir_all(&nested).unwrap();

    let _root_established = establish_workspace_lock(&state, &root, &normal_config(&root_ws), &[])
        .expect("root acquires");

    let peer = AgentId::from("peer".to_owned());
    let err = establish_workspace_lock(&state, &peer, &normal_config(&nested), &[])
        .err()
        .expect("peer without chain must conflict");
    assert!(matches!(err, EstablishLockFailure::Busy { .. }));
}

#[tokio::test]
async fn establish_workspace_lock_guest_acquires_nothing() {
    let state = make_state();
    let id = AgentId::from("guest".to_owned());
    let ws = ws_dir("guest");
    let mut cfg = normal_config(&ws);
    cfg.permissions_class = PermissionClass::Guest;
    let established = establish_workspace_lock(&state, &id, &cfg, &[])
        .expect("guest establishes (acquires nothing)");
    assert!(established.workspace.is_none());
    assert!(state.lock_manager.holder(&ws).unwrap().is_none());
    drop(established);
}

#[tokio::test]
async fn establish_workspace_lock_full_handoff_transfers_and_rolls_back() {
    // The drop-order invariant: on an unwind (drop without disarm) the
    // reverse transfer runs while writer==child, BEFORE the workspace guard's
    // release_all(child). A FullHandoff child must end up returning the lock
    // to the supervisor.
    let state = make_state();
    let supervisor = AgentId::from("sup".to_owned());
    let child = AgentId::from("child".to_owned());
    let ws = ws_dir("handoff");

    // Supervisor holds its workspace (the precondition for a real spawn:
    // validate guarantees a Live Normal supervisor owns its lock).
    let _sup_lock = state
        .lock_manager
        .acquire(&supervisor, &ws, &[])
        .expect("supervisor acquires");
    assert_eq!(
        state.lock_manager.holder(&ws).unwrap(),
        Some(supervisor.clone())
    );

    let mut cfg = normal_config(&ws);
    cfg.delegation_mode = DelegationMode::FullHandoff;
    cfg.created_by = Some(supervisor.clone());

    let established = establish_workspace_lock(&state, &child, &cfg, &[])
        .expect("full-handoff child establishes");
    // The forward transfer reassigned writer to the child.
    assert_eq!(state.lock_manager.holder(&ws).unwrap(), Some(child.clone()));
    // Simulate a spawn-failure unwind: drop WITHOUT disarm. EstablishedLock's
    // manual Drop runs the reverse transfer before the workspace guard releases,
    // so it runs while writer==child and the supervisor regains the lock.
    drop(established);
    assert_eq!(
        state.lock_manager.holder(&ws).unwrap(),
        Some(supervisor.clone())
    );
}

#[tokio::test]
async fn establish_workspace_lock_rejects_handoff_without_supervisor() {
    // A corrupt meta.json with delegation_mode=full_handoff and no created_by
    // must fail gracefully (replaces the prior `.expect` that crashed restore).
    let state = make_state();
    let id = AgentId::from("orphan".to_owned());
    let ws = ws_dir("orphan");
    let mut cfg = normal_config(&ws);
    cfg.delegation_mode = DelegationMode::FullHandoff;
    cfg.created_by = None;
    let err = establish_workspace_lock(&state, &id, &cfg, &[])
        .err()
        .expect("full-handoff without supervisor is rejected");
    assert!(matches!(
        err,
        EstablishLockFailure::HandoffWithoutSupervisor
    ));
}

#[test]
fn establish_lock_api_error_maps_status_codes() {
    // The HTTP-status selection lives here (not in the helper), so pin it.
    use kallip_common::protocol::ApiError;
    let busy = EstablishLockFailure::Busy {
        holder: AgentId::from("x".to_owned()),
        conflict: PathBuf::from("/p"),
    };
    assert_eq!(crate::lifecycle::establish_lock_api_error(busy).status, 409);
    let other = EstablishLockFailure::AcquireFailed(std::io::Error::other("boom"));
    assert_eq!(
        crate::lifecycle::establish_lock_api_error(other).status,
        ApiError::bad_request("").status
    );
}

// -- FullHandoff exclusivity (validate_subagent_request) --

#[tokio::test]
async fn validate_rejects_full_handoff_when_supervisor_has_a_child() {
    // Direction 1: a full-handoff child requires the supervisor to have NO
    // other children. Seed a supervisor with an existing child slot and the
    // request must be refused before any workspace/depth check runs.
    let state = make_state();
    let sup = AgentId::from("sup".to_owned());
    let sibling = AgentId::from("sibling".to_owned());
    {
        let mut reg = state.registry.write().await;
        add_root(&mut reg, &sup);
        reg.get_mut(&sup)
            .expect("supervisor registered")
            .subagent_ids_mut()
            .push(sibling.clone());
    }
    let reg = state.registry.read().await;
    let ws = PathBuf::from("/tmp");
    let err = super::validate_subagent_request(
        &reg,
        &Identity::Operator,
        &sup,
        &ws,
        None,
        DelegationMode::FullHandoff,
    )
    .expect_err("full-handoff with an existing child must be refused");
    assert_eq!(err.status, 409);
    // Direction-specific substring: this arm is the "supervisor has other
    // children" rejection. Asserting merely `.contains("full-handoff")`
    // would also pass against the other direction's message, hiding a swap.
    assert!(
        err.message.contains("no other subagents"),
        "should cite the no-other-subagents rule, got: {}",
        err.message
    );
}

#[tokio::test]
async fn validate_rejects_new_child_when_full_handoff_child_exists() {
    // Direction 2: once a full-handoff child lives, no other child (of any
    // mode) may be spawned under the same supervisor.
    let state = make_state();
    let sup = AgentId::from("sup".to_owned());
    let fh_child = AgentId::from("fh".to_owned());
    {
        let mut reg = state.registry.write().await;
        add_root(&mut reg, &sup);
        let mut entry = make_entry(Some(sup.clone()), format!("agent-{fh_child}"));
        entry.identity.config.delegation_mode = DelegationMode::FullHandoff;
        reg.register(fh_child.clone(), crate::state::RegistryEntry::Live(entry));
        reg.get_mut(&sup)
            .expect("supervisor registered")
            .subagent_ids_mut()
            .push(fh_child.clone());
    }
    let reg = state.registry.read().await;
    let ws = PathBuf::from("/tmp");
    let err = super::validate_subagent_request(
        &reg,
        &Identity::Operator,
        &sup,
        &ws,
        None,
        DelegationMode::CarveOut,
    )
    .expect_err("a new child while a full-handoff child lives must be refused");
    assert_eq!(err.status, 409);
    // Direction-specific substring: this arm is the "supervisor already has
    // a full-handoff child" rejection. Asserting merely
    // `.contains("full-handoff")` would also pass against the other
    // direction's message, hiding a swap.
    assert!(
        err.message.contains("already has"),
        "should cite the existing full-handoff child, got: {}",
        err.message
    );
}

/// On removal, a FullHandoff child's workspace lock is transferred back to
/// the supervisor (the happy path; the drop-without-disarm unwind path is
/// covered by `establish_workspace_lock_full_handoff_transfers_and_rolls_back`).
#[tokio::test]
async fn remove_agent_returns_full_handoff_lock_to_supervisor() {
    let state = make_state();
    let sup = AgentId::from("sup".to_owned());
    let child = AgentId::from("child".to_owned());
    let ws = ws_dir("fh-remove");

    // Register a Live FullHandoff child under `sup`. Set its workspace_root
    // to `ws`: remove_agent's transfer-back targets config.workspace_root.
    {
        let mut reg = state.registry.write().await;
        let mut entry = make_entry(Some(sup.clone()), format!("agent-{child}"));
        entry.identity.config.delegation_mode = DelegationMode::FullHandoff;
        entry.identity.config.permissions_class = PermissionClass::Normal;
        entry.identity.config.workspace_root = ws.clone();
        reg.register(child.clone(), crate::state::RegistryEntry::Live(entry));
    }
    // Simulate the spawn carve: sup held ws, then transferred it to the child.
    state.lock_manager.acquire(&sup, &ws, &[]).unwrap();
    state.lock_manager.transfer(&sup, &child, &ws).unwrap();
    assert_eq!(state.lock_manager.holder(&ws).unwrap(), Some(child.clone()));

    remove_agent(
        State(state.clone()),
        AuthIdentity::test_new(Identity::Operator),
        Path(child.clone()),
    )
    .await
    .expect("remove succeeds");

    // The transfer-back branch reassigned the workspace lock to the supervisor.
    assert_eq!(state.lock_manager.holder(&ws).unwrap(), Some(sup));
}

// -- Faulted agent manageability (the headline bug fix) --

/// `subagent list` includes faulted agents, marking state and surfacing reason.
#[tokio::test]
async fn list_agents_includes_faulted() {
    let state = make_state();
    let live = AgentId::random();
    let faulted = AgentId::random();
    {
        let mut reg = state.registry.write().await;
        // Two roots is intentionally invalid for a live tagma; this test
        // exercises list filtering, not the singleton invariant, so it uses
        // the raw `add_root`/`add_faulted_root` helpers (see their docs).
        add_root(&mut reg, &live);
        add_faulted_root(&mut reg, &faulted, "restore failed: boom");
    }
    let resp = list_agents(
        State(state),
        AuthIdentity::test_new(Identity::Operator),
        Query(ListAgentsQuery { created_by: None }),
    )
    .await;
    let agents = resp.0.agents;
    let f = agents
        .iter()
        .find(|a| a.id == faulted)
        .expect("faulted agent listed");
    assert_eq!(f.state, super::AgentState::Faulted);
    assert_eq!(f.faulted_reason.as_deref(), Some("restore failed: boom"));
    assert!(agents.iter().any(|a| a.id == live));
}

/// Removing a faulted subagent succeeds (204) -- the bug was 403/404 because
/// the agent was never registered. The fast path skips shutdown (no task);
/// the archive is a best-effort no-op when the dir is absent.
#[tokio::test]
async fn remove_faulted_agent_succeeds() {
    let state = make_state();
    let root = AgentId::random();
    let faulted = AgentId::random();
    {
        let mut reg = state.registry.write().await;
        add_root(&mut reg, &root);
        add_faulted_sub(&mut reg, &faulted, &root, "broken");
    }
    let status = remove_agent(
        State(state.clone()),
        AuthIdentity::test_new(Identity::Operator),
        Path(faulted.clone()),
    )
    .await
    .expect("remove succeeds");
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
    // Entry is gone from the registry.
    assert!(!state.registry.read().await.contains_key(&faulted));
}

/// A *live* tagma root is non-removable (clients target subagents).
#[tokio::test]
async fn remove_live_root_returns_conflict() {
    let state = make_state();
    let root = AgentId::random();
    {
        let mut reg = state.registry.write().await;
        add_root(&mut reg, &root);
    }
    let err = remove_agent(
        State(state),
        AuthIdentity::test_new(Identity::Operator),
        Path(root.clone()),
    )
    .await
    .expect_err("live root is non-removable");
    assert_eq!(err.status, 409);
}

/// A *faulted* root IS removable so an operator can recover from a restore
/// failure through the API (the next tagma restart re-creates the root).
/// `add_faulted_root` bypasses `register_root` to seed this single-root
/// faulted state (test-only; see `add_root`'s doc).
#[tokio::test]
async fn remove_faulted_root_succeeds() {
    let state = make_state();
    let root = AgentId::random();
    {
        let mut reg = state.registry.write().await;
        add_faulted_root(&mut reg, &root, "restore failed: boom");
    }
    let status = remove_agent(
        State(state.clone()),
        AuthIdentity::test_new(Identity::Operator),
        Path(root.clone()),
    )
    .await
    .expect("faulted root is removable");
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
    assert!(!state.registry.read().await.contains_key(&root));
}

/// Interrupting a faulted agent returns 409 (nothing to interrupt) instead
/// of touching runtime fields that don't exist on a faulted entry.
#[tokio::test]
async fn interrupt_faulted_returns_conflict() {
    let state = make_state();
    let faulted = AgentId::random();
    {
        let mut reg = state.registry.write().await;
        add_faulted_root(&mut reg, &faulted, "broken");
    }
    let err = interrupt_agent(
        State(state),
        AuthIdentity::test_new(Identity::Operator),
        Path(faulted),
    )
    .await
    .expect_err("interrupt faulted is a conflict");
    assert_eq!(err.status, 409);
}

/// The remove gate admits parked agents (204) — parking is a removable
/// state by design; the busy/waiting/retrying rejections are covered by
/// `remove_rejects_busy_waiting_retrying_states` below.
#[tokio::test]
async fn remove_parked_agent_succeeds() {
    let state = make_state();
    let root = AgentId::random();
    let parked = AgentId::random();
    {
        let mut reg = state.registry.write().await;
        add_root(&mut reg, &root);
        add_sub(&mut reg, &parked, &root);
        let live = reg.get(&parked).unwrap().as_live().unwrap();
        live.agent.state.store(crate::state::AgentState::PARKED, std::sync::atomic::Ordering::Relaxed);
    }
    let status = remove_agent(
        State(state.clone()),
        AuthIdentity::test_new(Identity::Operator),
        Path(parked.clone()),
    )
    .await
    .expect("parked agent is removable");
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
    assert!(!state.registry.read().await.contains_key(&parked));
}

/// The remove gate rejects every mid-lifecycle state (busy / waiting /
/// retrying) with 409 — only idle and parked are quiescent.
#[tokio::test]
async fn remove_rejects_busy_waiting_retrying_states() {
    for u8_state in [
        crate::state::AgentState::BUSY,
        crate::state::AgentState::WAITING,
        crate::state::AgentState::RETRYING,
    ] {
        let state = make_state();
        let root = AgentId::random();
        let child = AgentId::random();
        {
            let mut reg = state.registry.write().await;
            add_root(&mut reg, &root);
            add_sub(&mut reg, &child, &root);
            let live = reg.get(&child).unwrap().as_live().unwrap();
            live.agent.state.store(u8_state, std::sync::atomic::Ordering::Relaxed);
        }
        let err = remove_agent(
            State(state),
            AuthIdentity::test_new(Identity::Operator),
            Path(child),
        )
        .await
        .expect_err("mid-lifecycle state must be rejected");
        assert_eq!(err.status, 409, "state {u8_state} must conflict");
    }
}

/// The wake endpoint rejects non-parked agents (409) — a kick on an agent
/// that is running/waiting/retrying/idle is meaningless.
#[tokio::test]
async fn wake_rejects_non_parked_states() {
    let state = make_state();
    let root = AgentId::random();
    let child = AgentId::random();
    {
        let mut reg = state.registry.write().await;
        add_root(&mut reg, &root);
        add_sub(&mut reg, &child, &root);
    }
    let err = super::wake_agent(
        State(state),
        AuthIdentity::test_new(Identity::Operator),
        Path(child),
    )
    .await
    .expect_err("idle agent cannot be kicked");
    assert_eq!(err.status, 409);
}

/// The wake endpoint enqueues the kick turn: a parked agent receives the
/// `[system]` prompt carrying the park reason and elapsed duration, and
/// the parked payload survives until the agent's own round starts (the
/// bridge clears it on Busy).
#[tokio::test]
async fn wake_parked_agent_enqueues_kick_turn() {
    let state = make_state();
    let root = AgentId::random();
    let child = AgentId::random();
    {
        let mut reg = state.registry.write().await;
        add_root(&mut reg, &root);
        add_sub(&mut reg, &child, &root);
        let live = reg.get(&child).unwrap().as_live().unwrap();
        live.agent.state.store(crate::state::AgentState::PARKED, std::sync::atomic::Ordering::Relaxed);
        *live.agent.parked.lock().unwrap() = Some(crate::state::ParkedSnapshot {
            reason: kallip_common::protocol::ParkedReason::FatalError {
                message: "boom".to_string(),
            },
            at: std::time::Instant::now(),
        });
    }
    let (prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(4);
    {
        let mut reg = state.registry.write().await;
        let live = reg.get_mut(&child).unwrap().as_live_mut().unwrap();
        live.agent.prompt_tx = prompt_tx;
    }
    let status = super::wake_agent(
        State(state),
        AuthIdentity::test_new(Identity::Operator),
        Path(child),
    )
    .await
    .expect("parked agent is kickable");
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);
    let turn = tokio::time::timeout(std::time::Duration::from_millis(500), prompt_rx.recv())
        .await
        .expect("kick turn must be enqueued")
        .expect("prompt channel open");
    assert!(
        turn.starts_with("[system] you were parked") && turn.contains("ago: fatal error: boom"),
        "kick turn must carry the duration and reason: {turn}"
    );
    assert!(
        turn.contains("Decide whether to retry, adjust, or report."),
        "kick turn must end with the decision prompt: {turn}"
    );
}

#[tokio::test]
async fn update_duty_rejects_non_operator() {
    use crate::test_helpers::install_inbox_store;
    let state = make_state();
    install_inbox_store(&state).await;
    let agent = AgentId::random();
    {
        let mut reg = state.registry.write().await;
        add_root(&mut reg, &agent);
    }

    let result = super::update_duty(
        State(state),
        AuthIdentity::test_new(Identity::Agent { id: agent.clone() }),
        Path(agent),
        axum::Json(super::UpdateDutyRequest {
            status: crate::duty::DutyStatus::OffDuty,
        }),
    )
    .await;
    assert!(result.is_err(), "non-operator should be rejected");
}

#[tokio::test]
async fn update_duty_toggles_on_off() {
    use crate::test_helpers::install_inbox_store;
    let state = make_state();
    install_inbox_store(&state).await;
    let agent = AgentId::random();
    {
        let mut reg = state.registry.write().await;
        add_root(&mut reg, &agent);
    }

    // Set off-duty.
    let resp = super::update_duty(
        State(state.clone()),
        AuthIdentity::test_new(Identity::Operator),
        Path(agent.clone()),
        axum::Json(super::UpdateDutyRequest {
            status: crate::duty::DutyStatus::OffDuty,
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp.0.duty, crate::duty::DutyStatus::OffDuty);

    // Set back on-duty.
    let resp = super::update_duty(
        State(state),
        AuthIdentity::test_new(Identity::Operator),
        Path(agent),
        axum::Json(super::UpdateDutyRequest {
            status: crate::duty::DutyStatus::OnDuty,
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp.0.duty, crate::duty::DutyStatus::OnDuty);
}

#[tokio::test]
async fn update_duty_on_notifies_agent() {
    use crate::inbox::BufferedEvent;
    use crate::test_helpers::install_inbox_store;
    let state = make_state();
    install_inbox_store(&state).await;
    let agent = AgentId::random();
    // Use make_entry_with_rx so the prompt channel stays open —
    // add_root discards the receiver, causing the channel to close,
    // which makes enqueue_prompt fall through to reactivation.
    let (entry, mut _rx) = make_entry_with_rx(None, format!("agent-{agent}"));
    state
        .registry
        .write()
        .await
        .register(agent.clone(), RegistryEntry::Live(entry));

    // Set off-duty and buffer a message.
    state
        .duty
        .set(agent.clone(), crate::duty::DutyStatus::OffDuty);
    state
        .inboxes
        .get()
        .unwrap()
        .push(
            agent.clone(),
            BufferedEvent {
                timestamp: time::OffsetDateTime::now_utc(),
                source: "operator".into(),
                body: "test buffered message".into(),
            },
        )
        .await;

    // Transition to on-duty via the route — notifies the agent. The
    // message stays in the inbox until the agent task loop pulls it.
    let _ = super::update_duty(
        State(state.clone()),
        AuthIdentity::test_new(Identity::Operator),
        Path(agent.clone()),
        axum::Json(super::UpdateDutyRequest {
            status: crate::duty::DutyStatus::OnDuty,
        }),
    )
    .await
    .unwrap();

    // Message is still in the inbox (pull happens in the agent task loop).
    assert_eq!(state.inboxes.get().unwrap().len_for(&agent).await, 1);
}
