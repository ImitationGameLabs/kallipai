use super::*;
use crate::auth::Identity;
use crate::test_helpers::*;
use kallip_common::authtoken::TokenHash;

// -- Agent::shutdown: bounded graceful task drain --

#[tokio::test]
async fn agent_shutdown_aborts_straggler_after_timeout() {
    use std::sync::atomic::AtomicBool;

    // An abortable straggler: `tokio::time::sleep` yields (so the timeout can
    // fire on a single-thread runtime) and respects cancellation. shutdown
    // must time out, return false, and abort the task before it sets the flag.
    let completed = Arc::new(AtomicBool::new(false));
    let flag = completed.clone();
    let mut entry = make_entry(None, "tok".into());
    entry.agent.agent_handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        flag.store(true, Ordering::SeqCst);
    });
    assert!(!entry.agent.shutdown(Duration::from_millis(50)).await);
    // Aborted before the 60s sleep elapsed, so the completion flag stays unset.
    assert!(!completed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn agent_shutdown_graceful_when_fast() {
    // `make_entry` spawns two instantly-completing tasks.
    let entry = make_entry(None, "tok".into());
    assert!(entry.agent.shutdown(Duration::from_secs(1)).await);
}

// -- interrupt: cancels the round token, never the lifecycle token --

/// The core invariant: interrupt cancels only the current round token, so the
/// agent task returns to its outer loop instead of terminating.
#[tokio::test]
async fn interrupt_cancels_round_not_lifecycle() {
    let entry = make_entry(None, "tok".into());
    let round = RoundToken::new(&entry.agent.cancel);
    // Simulate a round in flight: publish the round token into the slot.
    *entry.agent.round_cancel.lock().unwrap() = Some(round.clone());

    // Mirror `interrupt_agent`'s logic: cancel the slot's token, not the lifecycle.
    if let Some(rc) = entry.agent.round_cancel.lock().unwrap().clone() {
        rc.cancel();
    }

    assert!(
        round.handle().is_cancelled(),
        "round token cancelled by interrupt"
    );
    assert!(
        !entry.agent.cancel.is_cancelled(),
        "lifecycle token must NOT be cancelled by interrupt"
    );
}

/// With no round in flight the slot is `None`, so interrupt is a clean no-op.
#[tokio::test]
async fn interrupt_when_idle_is_noop() {
    let entry = make_entry(None, "tok".into());
    assert!(entry.agent.round_cancel.lock().unwrap().is_none());

    if let Some(rc) = entry.agent.round_cancel.lock().unwrap().clone() {
        rc.cancel();
    }

    assert!(!entry.agent.cancel.is_cancelled());
    assert!(entry.agent.round_cancel.lock().unwrap().is_none());
}

// -- Registry consistency: agents + token_index + subagent_ids stay in sync --

#[tokio::test]
async fn register_unregister_syncs_token_index() {
    let mut reg = AgentRegistry::new();
    let id = AgentId::random();
    // The registry indexes by the agent's token hash, derived inside make_entry.
    let token = "test-token";
    let hash = TokenHash::of(token);
    reg.register(
        id.clone(),
        RegistryEntry::Live(make_entry(None, token.into())),
    );
    assert!(reg.contains_key(&id));
    assert_eq!(reg.get_agent_id_by_token(&hash), Some(&id));

    let removed = reg.unregister(&id).unwrap();
    let removed_live = match removed {
        RegistryEntry::Live(l) => l,
        RegistryEntry::Faulted(_) => panic!("expected live entry"),
    };
    assert_eq!(removed_live.agent.auth_token_hash, hash);
    assert!(!reg.contains_key(&id));
    assert!(reg.get_agent_id_by_token(&hash).is_none());
}

#[tokio::test]
async fn register_links_subagent_to_supervisor() {
    let mut reg = AgentRegistry::new();
    let sup = AgentId::random();
    let child = AgentId::random();
    add_root(&mut reg, &sup);
    add_sub(&mut reg, &child, &sup);
    assert_eq!(reg.get(&sup).unwrap().subagent_ids(), &vec![child]);
}

#[tokio::test]
async fn unregister_removes_subagent_pointer() {
    let mut reg = AgentRegistry::new();
    let sup = AgentId::random();
    let child = AgentId::random();
    add_root(&mut reg, &sup);
    add_sub(&mut reg, &child, &sup);
    reg.unregister(&child).unwrap();
    assert!(reg.get(&sup).unwrap().subagent_ids().is_empty());
}

// -- Supervisor chain walking --

#[tokio::test]
async fn walk_chain_traverses_ancestors() {
    let mut reg = AgentRegistry::new();
    let a = AgentId::random();
    let b = AgentId::random();
    let c = AgentId::random();
    add_root(&mut reg, &a);
    add_sub(&mut reg, &b, &a);
    add_sub(&mut reg, &c, &b);
    let chain = reg.walk_supervisor_chain(&c).unwrap();
    assert_eq!(chain.len(), 3);
    assert!(chain[2].identity().config.created_by.is_none()); // root
}

#[tokio::test]
async fn walk_chain_rejects_cycle() {
    let mut reg = AgentRegistry::new();
    let a = AgentId::random();
    let b = AgentId::random();
    reg.register(
        a.clone(),
        RegistryEntry::Live(make_entry(Some(b.clone()), "aa".into())),
    );
    reg.register(
        b,
        RegistryEntry::Live(make_entry(Some(a.clone()), "ab".into())),
    );
    match reg.walk_supervisor_chain(&a) {
        Err(e) => {
            assert_eq!(e.status, 403);
            assert!(e.message.contains("circular"));
        }
        Ok(_) => panic!("expected cycle error"),
    }
}

#[tokio::test]
async fn walk_chain_rejects_broken_link() {
    let mut reg = AgentRegistry::new();
    let a = AgentId::random();
    let ghost = AgentId::random();
    reg.register(
        a.clone(),
        RegistryEntry::Live(make_entry(Some(ghost), "a".into())),
    );
    match reg.walk_supervisor_chain(&a) {
        Err(e) => assert_eq!(e.status, 403),
        Ok(_) => panic!("expected broken chain error"),
    }
}

// -- Faulted entries: chain integrity, registration, authorization --

/// The headline fix: a supervisor chain walks cleanly through faulted
/// nodes, so a superior can authorize against a faulted descendant. Today
/// the whole subtree vanishes and the walk 403s ("broken supervisor chain").
#[tokio::test]
async fn walk_chain_traverses_faulted_nodes() {
    let mut reg = AgentRegistry::new();
    let root = AgentId::random();
    let mid = AgentId::random();
    let leaf = AgentId::random();
    add_root(&mut reg, &root);
    add_faulted_sub(&mut reg, &mid, &root, "mid restore failed");
    add_faulted_sub(&mut reg, &leaf, &mid, "leaf restore failed");
    let chain = reg.walk_supervisor_chain(&leaf).expect("chain is intact");
    assert_eq!(chain.len(), 3);
    // The faulted nodes are present and report their state for summaries.
    assert_eq!(chain[0].state_for_summary(), AgentState::Faulted);
    assert_eq!(chain[1].state_for_summary(), AgentState::Faulted);
}

/// A faulted entry links to a live supervisor via the eager subagent-push,
/// so `subagent list` on the supervisor includes the faulted child.
#[tokio::test]
async fn register_faulted_links_to_live_supervisor() {
    let mut reg = AgentRegistry::new();
    let sup = AgentId::random();
    let child = AgentId::random();
    add_root(&mut reg, &sup);
    add_faulted_sub(&mut reg, &child, &sup, "boom");
    assert!(reg.get(&sup).unwrap().subagent_ids().contains(&child));
}

/// A faulted child links to a faulted supervisor too (subtree stays connected).
#[tokio::test]
async fn register_faulted_links_to_faulted_supervisor() {
    let mut reg = AgentRegistry::new();
    let sup = AgentId::random();
    let child = AgentId::random();
    add_faulted_root(&mut reg, &sup, "sup restore failed");
    add_faulted_sub(&mut reg, &child, &sup, "child restore failed");
    assert!(reg.get(&sup).unwrap().subagent_ids().contains(&child));
}

/// A faulted entry is never inserted into the token index: it has no auth
/// token (the token is minted fresh on each restore and never persisted), so
/// it must not be authenticatable.
#[tokio::test]
async fn register_faulted_not_in_token_index() {
    let mut reg = AgentRegistry::new();
    let id = AgentId::random();
    add_faulted_root(&mut reg, &id, "missing workspace");
    // Any hash lookup misses -- a faulted agent cannot authenticate.
    assert!(
        reg.get_agent_id_by_token(&TokenHash::of("anything"))
            .is_none()
    );
}

/// Operator and a live ancestor both pass `require_superior` against a
/// faulted descendant -- the chain is walkable, so management works.
#[tokio::test]
async fn require_superior_succeeds_through_faulted() {
    let mut reg = AgentRegistry::new();
    let root = AgentId::random();
    let faulted_child = AgentId::random();
    add_root(&mut reg, &root);
    add_faulted_sub(&mut reg, &faulted_child, &root, "restore failed");
    assert!(
        reg.require_superior(&Identity::Operator, &faulted_child)
            .is_ok()
    );
    assert!(
        reg.require_superior(&Identity::Agent { id: root.clone() }, &faulted_child)
            .is_ok()
    );
}

/// `drain` returns both live and faulted entries so the shutdown caller can
/// await live tasks and drop faulted ones.
#[tokio::test]
async fn drain_returns_both_variants() {
    let mut reg = AgentRegistry::new();
    let live = AgentId::random();
    let faulted = AgentId::random();
    add_root(&mut reg, &live);
    add_faulted_root(&mut reg, &faulted, "broken");
    let drained = reg.drain();
    assert_eq!(drained.len(), 2);
    assert!(
        drained
            .iter()
            .any(|(_, e)| matches!(e, RegistryEntry::Live(_)))
    );
    assert!(
        drained
            .iter()
            .any(|(_, e)| matches!(e, RegistryEntry::Faulted(_)))
    );
}

// -- relation_of --

#[tokio::test]
async fn relation_of_operator_is_operator() {
    let mut reg = AgentRegistry::new();
    let a = AgentId::random();
    add_root(&mut reg, &a);
    assert_eq!(
        reg.relation_of(None, &a),
        crate::messaging::SenderRelation::Operator
    );
}

#[tokio::test]
async fn relation_of_self_is_same() {
    let mut reg = AgentRegistry::new();
    let a = AgentId::random();
    add_root(&mut reg, &a);
    assert_eq!(
        reg.relation_of(Some(&a), &a),
        crate::messaging::SenderRelation::Same
    );
}

#[tokio::test]
async fn relation_of_direct_and_transitive_parent_is_superior() {
    let mut reg = AgentRegistry::new();
    let a = AgentId::random();
    let b = AgentId::random();
    let c = AgentId::random();
    add_root(&mut reg, &a);
    add_sub(&mut reg, &b, &a);
    add_sub(&mut reg, &c, &b);
    let superior = crate::messaging::SenderRelation::Superior;
    // a is grandparent of c, b is parent of c.
    assert_eq!(reg.relation_of(Some(&a), &c), superior);
    assert_eq!(reg.relation_of(Some(&b), &c), superior);
}

#[tokio::test]
async fn relation_of_child_is_subordinate() {
    let mut reg = AgentRegistry::new();
    let parent = AgentId::random();
    let child = AgentId::random();
    add_root(&mut reg, &parent);
    add_sub(&mut reg, &child, &parent);
    // Child messaging parent: child is subordinate (receiver is its ancestor).
    assert_eq!(
        reg.relation_of(Some(&child), &parent),
        crate::messaging::SenderRelation::Subordinate
    );
}

#[tokio::test]
async fn relation_of_sibling_and_unrelated_are_peers() {
    let mut reg = AgentRegistry::new();
    let parent = AgentId::random();
    let sib1 = AgentId::random();
    let sib2 = AgentId::random();
    add_root(&mut reg, &parent);
    add_sub(&mut reg, &sib1, &parent);
    add_sub(&mut reg, &sib2, &parent);
    let peer = crate::messaging::SenderRelation::Peer;
    assert_eq!(reg.relation_of(Some(&sib1), &sib2), peer);
    // Unrelated roots are also peers.
    let other = AgentId::random();
    add_root(&mut reg, &other);
    assert_eq!(reg.relation_of(Some(&other), &parent), peer);
}

#[tokio::test]
async fn relation_of_broken_chain_is_unknown() {
    let mut reg = AgentRegistry::new();
    let a = AgentId::random();
    let ghost = AgentId::random();
    reg.register(
        a.clone(),
        RegistryEntry::Live(make_entry(Some(ghost), "a".into())),
    );
    // Self short-circuits before any walk, so a broken chain is irrelevant.
    assert_eq!(
        reg.relation_of(Some(&a), &a),
        crate::messaging::SenderRelation::Same
    );
    let b = AgentId::random();
    add_root(&mut reg, &b);
    // a's chain is broken; relation to the unrelated root b is unknowable.
    let unknown = crate::messaging::SenderRelation::Unknown;
    assert_eq!(reg.relation_of(Some(&a), &b), unknown);
    assert_eq!(reg.relation_of(Some(&b), &a), unknown);
}

// -- Authorization: require_superior --

#[tokio::test]
async fn superior_operator_bypasses_all() {
    let mut reg = AgentRegistry::new();
    let target = AgentId::random();
    add_root(&mut reg, &target);
    reg.require_superior(&Identity::Operator, &target).unwrap();
}

#[tokio::test]
async fn superior_ancestor_accepted() {
    let mut reg = AgentRegistry::new();
    let a = AgentId::random();
    let b = AgentId::random();
    let c = AgentId::random();
    add_root(&mut reg, &a);
    add_sub(&mut reg, &b, &a);
    add_sub(&mut reg, &c, &b);
    // a is grand-supervisor of c.
    reg.require_superior(&Identity::Agent { id: a.clone() }, &c)
        .unwrap();
}

#[tokio::test]
async fn superior_rejects_unrelated() {
    let mut reg = AgentRegistry::new();
    let a = AgentId::random();
    let other = AgentId::random();
    add_root(&mut reg, &a);
    add_root(&mut reg, &other);
    match reg.require_superior(&Identity::Agent { id: other }, &a) {
        Err(e) => assert_eq!(e.status, 403),
        Ok(_) => panic!("expected FORBIDDEN"),
    }
}

#[tokio::test]
async fn superior_rejects_child_accessing_parent() {
    let mut reg = AgentRegistry::new();
    let parent = AgentId::random();
    let child = AgentId::random();
    add_root(&mut reg, &parent);
    add_sub(&mut reg, &child, &parent);
    match reg.require_superior(&Identity::Agent { id: child }, &parent) {
        Err(e) => assert_eq!(e.status, 403),
        Ok(_) => panic!("expected FORBIDDEN"),
    }
}

// -- Authorization: require_direct_supervisor (PUT /metadata) --

#[tokio::test]
async fn direct_supervisor_operator_bypasses() {
    let mut reg = AgentRegistry::new();
    let target = AgentId::random();
    add_root(&mut reg, &target);
    reg.require_direct_supervisor(&Identity::Operator, &target)
        .unwrap();
}

#[tokio::test]
async fn direct_supervisor_accepts_direct_parent() {
    let mut reg = AgentRegistry::new();
    let parent = AgentId::random();
    let child = AgentId::random();
    add_root(&mut reg, &parent);
    add_sub(&mut reg, &child, &parent);
    reg.require_direct_supervisor(&Identity::Agent { id: parent }, &child)
        .unwrap();
}

#[tokio::test]
async fn direct_supervisor_rejects_grandparent() {
    // require_superior allows ancestors; require_direct_supervisor does not.
    let mut reg = AgentRegistry::new();
    let grandparent = AgentId::random();
    let parent = AgentId::random();
    let child = AgentId::random();
    add_root(&mut reg, &grandparent);
    add_sub(&mut reg, &parent, &grandparent);
    add_sub(&mut reg, &child, &parent);
    match reg.require_direct_supervisor(&Identity::Agent { id: grandparent }, &child) {
        Err(e) => assert_eq!(e.status, 403),
        Ok(_) => panic!("expected FORBIDDEN"),
    }
}

#[tokio::test]
async fn direct_supervisor_rejects_unrelated() {
    let mut reg = AgentRegistry::new();
    let parent = AgentId::random();
    let child = AgentId::random();
    let other = AgentId::random();
    add_root(&mut reg, &parent);
    add_sub(&mut reg, &child, &parent);
    add_root(&mut reg, &other);
    match reg.require_direct_supervisor(&Identity::Agent { id: other }, &child) {
        Err(e) => assert_eq!(e.status, 403),
        Ok(_) => panic!("expected FORBIDDEN"),
    }
}

#[tokio::test]
async fn direct_supervisor_root_target_only_operator() {
    // A root agent (created_by None) has no supervisor → only the operator.
    let mut reg = AgentRegistry::new();
    let root = AgentId::random();
    let other = AgentId::random();
    add_root(&mut reg, &root);
    add_root(&mut reg, &other);
    match reg.require_direct_supervisor(&Identity::Agent { id: other }, &root) {
        Err(e) => assert_eq!(e.status, 403),
        Ok(_) => panic!("expected FORBIDDEN"),
    }
}

#[tokio::test]
async fn direct_supervisor_missing_target_is_not_found() {
    let reg = AgentRegistry::new();
    let ghost = AgentId::random();
    match reg.require_direct_supervisor(&Identity::Operator, &ghost) {
        Err(e) => assert_eq!(e.status, 404),
        Ok(_) => panic!("expected NOT_FOUND"),
    }
}

// -- Authorization: require_supervisor --

#[tokio::test]
async fn supervisor_allows_operator_and_self() {
    let mut reg = AgentRegistry::new();
    let id = AgentId::random();
    add_root(&mut reg, &id);
    reg.require_supervisor(&Identity::Operator, &id).unwrap();
    reg.require_supervisor(&Identity::Agent { id: id.clone() }, &id)
        .unwrap();
}

#[tokio::test]
async fn supervisor_rejects_wrong_identity() {
    let mut reg = AgentRegistry::new();
    let a = AgentId::random();
    let other = AgentId::random();
    add_root(&mut reg, &a);
    add_root(&mut reg, &other);
    match reg.require_supervisor(&Identity::Agent { id: other }, &a) {
        Err(e) => assert_eq!(e.status, 403),
        Ok(_) => panic!("expected FORBIDDEN"),
    }
}

#[tokio::test]
async fn supervisor_returns_not_found_for_missing() {
    let reg = AgentRegistry::new();
    let ghost = AgentId::random();
    match reg.require_supervisor(&Identity::Operator, &ghost) {
        Err(e) => assert_eq!(e.status, 404),
        Ok(_) => panic!("expected NOT_FOUND"),
    }
}

// -- Authorization: require_self_or_operator --

#[tokio::test]
async fn self_or_operator_allows_operator() {
    let mut reg = AgentRegistry::new();
    let id = AgentId::random();
    add_root(&mut reg, &id);
    reg.require_self_or_operator(&Identity::Operator, &id)
        .unwrap();
}

#[tokio::test]
async fn self_or_operator_allows_self() {
    let mut reg = AgentRegistry::new();
    let id = AgentId::random();
    add_root(&mut reg, &id);
    reg.require_self_or_operator(&Identity::Agent { id: id.clone() }, &id)
        .unwrap();
}

#[tokio::test]
async fn self_or_operator_rejects_other_agent() {
    let mut reg = AgentRegistry::new();
    let a = AgentId::random();
    let b = AgentId::random();
    add_root(&mut reg, &a);
    add_root(&mut reg, &b);
    match reg.require_self_or_operator(&Identity::Agent { id: b }, &a) {
        Err(e) => assert_eq!(e.status, 403),
        Ok(_) => panic!("expected FORBIDDEN"),
    }
}

#[tokio::test]
async fn self_or_operator_rejects_supervisor_of_target() {
    // Pins the self-only invariant for `PUT /agents/{id}/activity`: a parent
    // (supervisor) must NOT write a subagent's activity — only the agent
    // itself. Guards against a future swap to `require_direct_supervisor`.
    let mut reg = AgentRegistry::new();
    let parent = AgentId::random();
    let child = AgentId::random();
    add_root(&mut reg, &parent);
    add_sub(&mut reg, &child, &parent);
    match reg.require_self_or_operator(&Identity::Agent { id: parent }, &child) {
        Err(e) => assert_eq!(e.status, 403),
        Ok(_) => panic!("expected FORBIDDEN"),
    }
}

// -- root_agent (singleton) --

#[tokio::test]
async fn root_agent_returns_the_single_root() {
    let mut reg = AgentRegistry::new();
    assert!(reg.root_agent().is_none());

    let root = AgentId::random();
    let child = AgentId::random();
    reg.register_root(
        root.clone(),
        RegistryEntry::Live(make_entry(None, format!("agent-{root}"))),
    )
    .unwrap();
    add_sub(&mut reg, &child, &root);

    let (found_id, _) = reg.root_agent().expect("root present");
    assert_eq!(found_id, &root);
}

#[tokio::test]
async fn register_root_rejects_a_second_root() {
    let mut reg = AgentRegistry::new();
    let root = AgentId::random();
    reg.register_root(
        root.clone(),
        RegistryEntry::Live(make_entry(None, format!("agent-{root}"))),
    )
    .unwrap();
    // A second root violates the singleton invariant.
    let dup = AgentId::random();
    let err = reg
        .register_root(
            dup.clone(),
            RegistryEntry::Live(make_entry(None, format!("agent-{dup}"))),
        )
        .unwrap_err();
    assert_eq!(err.status, 409);
    // The original root is unaffected.
    assert_eq!(reg.root_agent().unwrap().0, &root);
}

// -- Resource limits in AppState --

#[test]
fn with_limits_sets_max_agents() {
    let state = AppState::with_limits(
        TokenHash::of("tok"),
        50,
        20,
        5,
        make_profile_bundle(),
        PolicyPreset::Default,
    );
    assert_eq!(state.max_agents, 50);
    assert_eq!(state.max_subagents, 20);
    assert_eq!(state.prompt_queue_size, 5);
}

#[test]
fn new_has_generous_limits() {
    let state = AppState::new(TokenHash::of("tok"), make_profile_bundle());
    assert_eq!(state.max_agents, crate::args::MAX_AGENTS_LIMIT);
    assert_eq!(state.max_subagents, crate::args::MAX_SUBAGENTS_LIMIT);
}
