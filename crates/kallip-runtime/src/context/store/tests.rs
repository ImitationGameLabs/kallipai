use super::*;

fn new_store() -> ContextStore {
    ContextStore::new()
}

/// Assert the `[pinned…][conversation…]` ordering invariant and that every pinned turn has a
/// label. Called at the end of mutating tests.
fn assert_invariant(store: &ContextStore) {
    let mut seen_conversation = false;
    for t in store.turns() {
        if t.is_pinned() {
            assert!(t.label().is_some(), "pinned turn must have a label");
            assert!(
                !seen_conversation,
                "pinned turn must precede all conversation turns"
            );
        } else {
            seen_conversation = true;
        }
    }
}

#[test]
fn push_turn_assigns_sequential_ids() {
    let mut store = new_store();
    let (id0, _) = store.push_turn(vec![ChatMessage::user("a")]);
    let (id1, _) = store.push_turn(vec![ChatMessage::user("b")]);
    assert_eq!(id0, TurnId(0));
    assert_eq!(id1, TurnId(1));
    assert_eq!(store.turn_count(), 2);
}

#[test]
fn drain_turns_removes_correct_range() {
    let mut store = new_store();
    store.push_turn(vec![ChatMessage::user("a")]);
    store.push_turn(vec![ChatMessage::user("b")]);
    store.push_turn(vec![ChatMessage::user("c")]);

    let drained = store.drain_turns(0..2);
    assert_eq!(drained.len(), 2);
    assert_eq!(store.turn_count(), 1);
}

#[test]
fn pinned_items_are_tracked() {
    let mut store = new_store();
    store.pin("test", ChatMessage::user("important")).unwrap();
    assert_eq!(store.pinned_turns().count(), 1);
    assert_eq!(store.pinned_labels(), vec!["test"]);
    assert_invariant(&store);
}

#[test]
fn pin_rejects_duplicate_label() {
    let mut store = new_store();
    store.pin("x", ChatMessage::user("a")).unwrap();
    assert!(store.pin("x", ChatMessage::user("b")).is_err());
}

#[test]
fn unpin_removes_item() {
    let mut store = new_store();
    store.pin("x", ChatMessage::user("a")).unwrap();
    store.unpin("x").unwrap();
    assert_eq!(store.pinned_turns().count(), 0);
}

#[test]
fn unpin_errors_on_missing_label() {
    let mut store = new_store();
    assert!(store.unpin("nonexistent").is_err());
}

#[test]
fn pinned_budget_enforced() {
    let mut store = ContextStore::new();
    // Derive the budget from the same estimator the production path uses, so the test is
    // robust to any estimator: 5 pins fill the budget exactly, the 6th must be rejected.
    let per_pin = estimate_message_tokens(&ChatMessage::user("a"));
    let budget = per_pin.checked_mul(5).expect("non-zero per-pin estimate");
    store.set_pinned_budget(budget);
    for label in ["a", "b", "c", "d", "e"] {
        store.pin(label, ChatMessage::user("a")).unwrap();
    }
    assert!(
        store.pin("f", ChatMessage::user("a")).is_err(),
        "6th pin must exceed the {budget}-token budget (per-pin = {per_pin})"
    );
    // Unpin frees budget.
    store.unpin("a").unwrap();
    assert!(store.pin("f", ChatMessage::user("a")).is_ok());
}

#[test]
fn warning_tracking() {
    let mut store = ContextStore::new();
    assert!(store.should_warn(50));
    assert!(store.should_warn(60));
    store.mark_warned(50);
    assert!(!store.should_warn(50));
    assert!(store.should_warn(60));
    store.mark_warned(60);
    assert!(!store.should_warn(50));
    assert!(!store.should_warn(60));
    assert!(store.should_warn(70));
    store.reset_context_warnings();
    assert!(store.should_warn(50));
}

// --- incremental-estimate anchor / flag mechanics ---

fn usage(prompt_tokens: u32) -> just_llm_client::types::chat::Usage {
    just_llm_client::types::chat::Usage {
        prompt_tokens,
        completion_tokens: 0,
        prompt_cache_hit_tokens: None,
        prompt_cache_miss_tokens: None,
        total_tokens: prompt_tokens,
        completion_tokens_details: None,
    }
}

#[test]
fn new_store_starts_in_full_mode() {
    let store = new_store();
    assert!(store.needs_full_estimate());
    assert_eq!(store.anchored_turn_count(), 0);
    assert_eq!(store.last_prompt_tokens(), None);
}

#[test]
fn accumulate_usage_sets_anchor_and_clears_flag() {
    let mut store = new_store();
    store.push_turn(vec![ChatMessage::user("a")]);
    store.push_turn(vec![ChatMessage::user("b")]);
    assert!(store.needs_full_estimate(), "new store starts in full mode");
    store.accumulate_usage(&usage(100));
    assert_eq!(store.last_prompt_tokens(), Some(100));
    assert_eq!(
        store.anchored_turn_count(),
        2,
        "anchor = turns at response time"
    );
    assert!(!store.needs_full_estimate());
}

#[test]
fn prefix_ops_set_needs_full_estimate() {
    let clear = |s: &mut ContextStore| s.accumulate_usage(&usage(1));
    let mut store = new_store();

    clear(&mut store);
    store.pin("x", ChatMessage::user("a")).unwrap();
    assert!(store.needs_full_estimate(), "pin sets the flag");

    clear(&mut store);
    store.unpin("x").unwrap();
    assert!(store.needs_full_estimate(), "unpin sets the flag");

    clear(&mut store);
    store.replace_pin("y", ChatMessage::user("c")).unwrap();
    assert!(store.needs_full_estimate(), "replace_pin sets the flag");

    store.push_turn(vec![ChatMessage::user("t1")]);
    store.push_turn(vec![ChatMessage::user("t2")]);
    clear(&mut store);
    store.evict_turns(1);
    assert!(store.needs_full_estimate(), "evict_turns sets the flag");

    clear(&mut store);
    store.drain_turns(0..1);
    assert!(store.needs_full_estimate(), "drain_turns sets the flag");
}

#[test]
fn accumulate_usage_no_anchor_leaves_anchor_untouched() {
    let mut store = new_store();
    store.push_turn(vec![ChatMessage::user("a")]);
    store.accumulate_usage(&usage(100)); // anchor at 1 turn, base 100
    assert_eq!(store.last_prompt_tokens(), Some(100));
    assert_eq!(store.anchored_turn_count(), 1);
    assert!(!store.needs_full_estimate());

    // A summarizer-style call must not move the anchor, only grow cumulative usage.
    let prev_cumulative = store.cumulative_usage().prompt_tokens;
    store.accumulate_usage_no_anchor(&usage(50));
    assert_eq!(store.last_prompt_tokens(), Some(100), "base unchanged");
    assert_eq!(store.anchored_turn_count(), 1, "anchor unchanged");
    assert!(!store.needs_full_estimate(), "flag unchanged");
    assert_eq!(
        store.cumulative_usage().prompt_tokens,
        prev_cumulative + 50,
        "cumulative usage still grows"
    );
}

// --- pinned-turn token caching ---

#[test]
fn pinned_turn_caches_estimated_tokens() {
    let mut store = new_store();
    let msg = ChatMessage::user("hello world");
    let expected = estimate_message_tokens(&msg);
    store.pin("x", msg).unwrap();
    let pinned = store.pinned_turns().next().unwrap();
    assert_eq!(pinned.estimated_tokens, expected);
    assert_eq!(store.pinned_tokens_total(), expected);
    // replace_pin updates the cache in place.
    let msg2 = ChatMessage::user("goodbye world and more content here");
    let expected2 = estimate_message_tokens(&msg2);
    store.replace_pin("x", msg2).unwrap();
    let pinned = store.pinned_turns().next().unwrap();
    assert_eq!(pinned.estimated_tokens, expected2);
    assert_invariant(&store);
}

#[test]
fn reestimate_recomputes_cached_tokens() {
    let mut store = new_store();
    // A legacy pinned item folded in carrying a stale (0) estimate.
    let msg = ChatMessage::user("legacy content from a pre-caching format");
    let real = estimate_message_tokens(&msg);
    store.legacy_pinned.push(PinnedItem {
        label: "legacy".into(),
        message: msg,
        estimated_tokens: 0,
    });
    store.migrate_legacy_pinned();
    assert_eq!(store.pinned_turns().count(), 1);
    assert_eq!(
        store.pinned_turns().next().unwrap().estimated_tokens,
        0,
        "migrated turn carries the stale legacy estimate before reestimate"
    );
    // reestimate brings every turn up to the current estimator.
    store.reestimate_cached_tokens();
    assert_eq!(
        store.pinned_tokens_total(),
        real,
        "reestimate recomputes via the current estimator"
    );
    assert_eq!(store.pinned_labels(), vec!["legacy"]);
    assert_invariant(&store);
}

#[test]
fn migrate_legacy_pinned_preserves_order_and_ids() {
    let mut store = new_store();
    // Two conversation turns already in the store.
    store.push_turn(vec![ChatMessage::user("c1")]);
    store.push_turn(vec![ChatMessage::user("c2")]);
    let base_next = store.next_turn_id;
    // Inject three legacy pinned items in a known order.
    for label in ["sum", "skill:foo", "note"] {
        store.legacy_pinned.push(PinnedItem {
            label: label.into(),
            message: ChatMessage::user(label),
            estimated_tokens: 5,
        });
    }
    store.migrate_legacy_pinned();

    // Pinned turns at front in original order, conversation turns after.
    let labels: Vec<&str> = store.turns().iter().filter_map(|t| t.label()).collect();
    assert_eq!(labels, vec!["sum", "skill:foo", "note"]);
    assert_eq!(store.turn_count(), 5);
    // Conversation turns still at the back.
    assert_eq!(store.pinned_turn_count(), 3);
    // TurnIds unique and advanced past the migrated block.
    let ids: std::collections::HashSet<TurnId> = store.turns().iter().map(|t| t.id).collect();
    assert_eq!(ids.len(), 5, "all turn ids unique");
    assert_eq!(store.next_turn_id, base_next + 3);
    assert_invariant(&store);
}

#[test]
fn pinned_item_estimated_tokens_serde_default_is_zero() {
    // New-format pin round-trips with its cached value.
    let item = PinnedItem {
        label: "x".into(),
        message: ChatMessage::user("hi"),
        estimated_tokens: 42,
    };
    let json = serde_json::to_string(&item).unwrap();
    let rt: PinnedItem = serde_json::from_str(&json).unwrap();
    assert_eq!(rt.estimated_tokens, 42);

    // Strip the field to emulate a legacy (pre-caching) serialized pin.
    let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
    v.as_object_mut().unwrap().remove("estimated_tokens");
    let legacy = serde_json::to_string(&v).unwrap();
    let legacy_rt: PinnedItem = serde_json::from_str(&legacy).unwrap();
    assert_eq!(legacy_rt.estimated_tokens, 0, "missing field defaults to 0");
}

// --- F1/F3 safety: eviction skips pinned; ordering invariants ---

#[test]
fn evict_turns_skips_pinned() {
    let mut store = new_store();
    store
        .pin("context_summary", ChatMessage::assistant("sum"))
        .unwrap();
    store.push_turn(vec![ChatMessage::user("c1")]);
    store.push_turn(vec![ChatMessage::user("c2")]);
    store.push_turn(vec![ChatMessage::user("c3")]);

    let res = store.evict_turns(3);
    assert_eq!(res.evicted, 3, "all conversation turns evicted");
    assert_eq!(res.remaining_turns, 0, "no conversation turns remain");
    assert_eq!(
        store.pinned_turns().count(),
        1,
        "pinned summary survives eviction"
    );
    assert_eq!(store.pinned_labels(), vec!["context_summary"]);
    assert_eq!(store.turn_count(), 1, "only the pinned turn remains");

    // Over-evict: pinned still survives, evicted capped at conversation count (already 0).
    let res = store.evict_turns(99);
    assert_eq!(res.evicted, 0);
    assert_eq!(store.pinned_turns().count(), 1);
    assert_invariant(&store);
}

#[test]
fn pin_inserts_after_pinned_partition() {
    let mut store = new_store();
    store.pin("a", ChatMessage::user("a")).unwrap();
    store.push_turn(vec![ChatMessage::user("convo")]);
    store.pin("b", ChatMessage::user("b")).unwrap();
    // Ordering: [a(pinned), b(pinned), convo] — b inserted after the pinned block, not at back.
    let labels: Vec<Option<&str>> = store.turns().iter().map(|t| t.label()).collect();
    assert_eq!(
        labels,
        vec![Some("a"), Some("b"), None],
        "pinned turns stay before conversation turns"
    );
    assert_invariant(&store);
}

#[test]
fn replace_pin_updates_in_place_keeps_position() {
    let mut store = new_store();
    store.pin("a", ChatMessage::user("a-original")).unwrap();
    store.pin("b", ChatMessage::user("b")).unwrap();
    store.push_turn(vec![ChatMessage::user("convo")]);

    let new_msg = ChatMessage::user("a-replaced-longer-content");
    let new_tokens = estimate_message_tokens(&new_msg);
    store.replace_pin("a", new_msg).unwrap();

    let first = &store.turns()[0];
    assert_eq!(first.label(), Some("a"), "position unchanged");
    assert_eq!(first.estimated_tokens, new_tokens);
    // b and convo remain in place.
    assert_eq!(store.turns()[1].label(), Some("b"));
    assert!(!store.turns()[2].is_pinned());
    assert_invariant(&store);
}
