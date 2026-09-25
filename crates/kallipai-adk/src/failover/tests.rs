//! `FailoverState`'s shared snapshot cell: each mutator that establishes the
//! active profile must mirror it, and only the success paths mutate.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use super::*;
use crate::test_support::{MapSource, ds_backend, profile};

/// Two-profile chain over one working provider, sharing a snapshot cell the
/// test can also read directly (as the tagma does).
fn two_profile_state() -> (FailoverState, Arc<Mutex<ProfileSnapshot>>) {
    let set = ProfileSet {
        name: "default".into(),
        description: None,
        profiles: vec![
            profile("p0", "prov-a", 1_000),
            profile("p1", "prov-a", 1_000),
        ],
    };
    let source = MapSource(HashMap::from([("prov-a".to_string(), ds_backend())]));
    let registry = Arc::new(
        ProfileRegistry::new(
            BTreeMap::from([("default".to_string(), set.clone())]),
            Arc::new(source),
        )
        .unwrap(),
    );
    let snapshot = Arc::new(Mutex::new(ProfileSnapshot::default()));
    let state = FailoverState::new(set, registry, None, snapshot.clone());
    (state, snapshot)
}

#[test]
fn new_seeds_snapshot_with_active_profile() {
    let (state, snapshot) = two_profile_state();
    let want = ProfileSnapshot {
        set_name: "default".into(),
        profile_id: "p0".into(),
        provider: "prov-a".into(),
        model: "p0-model".into(),
    };
    assert_eq!(state.profile_snapshot(), want);
    // The tagma's own Arc handle observes the same cell content.
    assert_eq!(*snapshot.lock().unwrap(), want);
}

#[test]
fn advance_to_rewrites_snapshot() {
    let (mut state, snapshot) = two_profile_state();
    state.advance_to(1);
    let want = ProfileSnapshot {
        // Within-set advance never changes the set name.
        set_name: "default".into(),
        profile_id: "p1".into(),
        provider: "prov-a".into(),
        model: "p1-model".into(),
    };
    assert_eq!(state.profile_snapshot(), want);
    assert_eq!(*snapshot.lock().unwrap(), want);
}

#[test]
fn reset_and_rebuild_rewrites_snapshot_on_success() {
    let (mut state, _) = two_profile_state();
    state.advance_to(1);
    let fresh = ProfileSet {
        name: "fresh".into(),
        description: None,
        profiles: vec![profile("q0", "prov-a", 2_000)],
    };
    let source = MapSource(HashMap::from([("prov-a".to_string(), ds_backend())]));
    let registry = Arc::new(
        ProfileRegistry::new(
            BTreeMap::from([("fresh".to_string(), fresh.clone())]),
            Arc::new(source),
        )
        .unwrap(),
    );
    state.reset_and_rebuild(fresh, registry).unwrap();
    assert_eq!(
        state.profile_snapshot(),
        ProfileSnapshot {
            set_name: "fresh".into(),
            profile_id: "q0".into(),
            provider: "prov-a".into(),
            model: "q0-model".into(),
        }
    );
}

#[test]
fn reset_failure_leaves_snapshot_untouched() {
    let (mut state, _) = two_profile_state();
    state.advance_to(1);
    // The new set's provider has no backend: the rebuild fails before any
    // commit, so the snapshot must still show the pre-reset active profile.
    let bad = ProfileSet {
        name: "bad".into(),
        description: None,
        profiles: vec![profile("z0", "prov-missing", 1_000)],
    };
    let registry = Arc::new(
        ProfileRegistry::new(
            BTreeMap::from([("bad".to_string(), bad.clone())]),
            Arc::new(MapSource(HashMap::new())),
        )
        .unwrap(),
    );
    assert!(state.reset_and_rebuild(bad, registry).is_err());
    assert_eq!(
        state.profile_snapshot(),
        ProfileSnapshot {
            set_name: "default".into(),
            provider: "prov-a".into(),
            profile_id: "p1".into(),
            model: "p1-model".into(),
        }
    );
}
