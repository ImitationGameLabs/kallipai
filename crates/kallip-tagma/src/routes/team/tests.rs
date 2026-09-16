use super::converge::*;
use super::status::*;
use std::collections::HashMap;

use kallip_common::AgentId;
use kallip_common::declaration::{TeamDeclaration, parse_declaration};
use kallip_common::protocol::{
    RoleDisposition, TeamAction, TeamLockEntry, TeamRejectionKind, TeamRoleStatus, TeamRowOutcome,
};

use kallip_testkit::DevDir;

fn dev_tempdir(label: &str) -> DevDir {
    DevDir::new(label)
}

fn live(role: &str) -> LiveEntry {
    LiveEntry {
        id: AgentId::random(),
        role: role.to_string(),
    }
}

fn lock_pair(role: &str, id: &AgentId, in_inactive: bool) -> LockPair {
    LockPair {
        role: role.to_string(),
        id: id.clone(),
        in_inactive,
        in_archived: false,
    }
}

fn declaration(toml_text: &str) -> TeamDeclaration {
    parse_declaration(toml_text).unwrap()
}

fn disposition_of(rows: &[TeamRoleStatus], role: &str) -> RoleDisposition {
    rows.iter()
        .find(|row| row.role == role)
        .unwrap_or_else(|| panic!("no row for role {role}"))
        .disposition
}

#[test]
fn an_in_sync_role_is_active() {
    let declaration = declaration("[[role]]\nname = \"dev\"\n");
    let body = live("dev");
    let lock = vec![lock_pair("dev", &body.id, false)];
    let rows = compare_team(
        &declaration,
        &lock,
        std::slice::from_ref(&body),
        std::slice::from_ref(&body),
    );
    assert_eq!(disposition_of(&rows, "dev"), RoleDisposition::Active);
    assert!(!rows[0].lock_drift);
    assert_eq!(rows[0].live, vec![body.id.clone()]);
    assert_eq!(rows[0].lock_id.as_ref(), Some(&body.id));
}

#[test]
fn declared_live_without_a_lock_is_adopted() {
    let declaration = declaration("[[role]]\nname = \"dev\"\n");
    let body = live("dev");
    let rows = compare_team(
        &declaration,
        &[],
        std::slice::from_ref(&body),
        std::slice::from_ref(&body),
    );
    assert_eq!(disposition_of(&rows, "dev"), RoleDisposition::Adopt);
}

#[test]
fn an_absent_role_restores_when_the_lock_id_is_inactive() {
    let declaration = declaration("[[role]]\nname = \"dev\"\n");
    let parked = AgentId::random();
    let lock = vec![lock_pair("dev", &parked, true)];
    let rows = compare_team(&declaration, &lock, &[], &[]);
    assert_eq!(disposition_of(&rows, "dev"), RoleDisposition::Restore);
    assert!(rows[0].lock_inactive);
}

#[test]
fn an_absent_role_spawns_when_there_is_nothing_to_restore() {
    let declaration = declaration("[[role]]\nname = \"dev\"\n");
    let gone = AgentId::random();
    let rows = compare_team(&declaration, &[lock_pair("dev", &gone, false)], &[], &[]);
    assert_eq!(disposition_of(&rows, "dev"), RoleDisposition::Spawn);
    // No lock at all spawns too.
    let rows = compare_team(&declaration, &[], &[], &[]);
    assert_eq!(disposition_of(&rows, "dev"), RoleDisposition::Spawn);
}

#[test]
fn an_undeclared_live_role_deactivates() {
    let declaration = declaration("[[role]]\nname = \"dev\"\n");
    let stray = live("stray");
    let rows = compare_team(
        &declaration,
        &[],
        std::slice::from_ref(&stray),
        std::slice::from_ref(&stray),
    );
    assert_eq!(disposition_of(&rows, "stray"), RoleDisposition::Deactivate);
}

#[test]
fn an_unmanaged_role_is_exempt_while_live_and_retained_when_absent() {
    let declaration = declaration("[[role]]\nname = \"guest1\"\nunmanaged = true\n");
    let body = live("guest1");
    let rows = compare_team(
        &declaration,
        &[],
        std::slice::from_ref(&body),
        std::slice::from_ref(&body),
    );
    assert_eq!(disposition_of(&rows, "guest1"), RoleDisposition::Exempt);
    let rows = compare_team(&declaration, &[], &[], &[]);
    assert_eq!(disposition_of(&rows, "guest1"), RoleDisposition::Retain);
}

#[test]
fn duplicate_live_bodies_are_flagged_for_manual_resolution() {
    let declaration = declaration("[[role]]\nname = \"dev\"\n");
    let bodies = vec![live("dev"), live("dev")];
    let rows = compare_team(&declaration, &[], &bodies, &bodies);
    assert_eq!(disposition_of(&rows, "dev"), RoleDisposition::Duplicate);
    assert_eq!(rows[0].live.len(), 2);
}

#[test]
fn a_lock_only_role_is_retained() {
    let declaration = declaration("[[role]]\nname = \"dev\"\n");
    let parked = AgentId::random();
    let lock = vec![lock_pair("scout", &parked, true)];
    let rows = compare_team(&declaration, &lock, &[], &[]);
    assert_eq!(disposition_of(&rows, "scout"), RoleDisposition::Retain);
    // Row order: declaration roles first, undeclared after, by name.
    let names: Vec<&str> = rows.iter().map(|row| row.role.as_str()).collect();
    assert_eq!(names, vec!["dev", "scout"]);
}

#[test]
fn a_lock_id_live_under_another_role_is_drift() {
    let declaration = declaration("[[role]]\nname = \"dev\"\n");
    let body = live("scout");
    let lock = vec![lock_pair("dev", &body.id, false)];
    let rows = compare_team(
        &declaration,
        &lock,
        std::slice::from_ref(&body),
        std::slice::from_ref(&body),
    );
    let row = rows.iter().find(|row| row.role == "dev").unwrap();
    assert!(row.lock_drift);
    // The role has no live body of its own and the lock id is not
    // parked: spawn, with the drift flagged.
    assert_eq!(row.disposition, RoleDisposition::Spawn);
}

#[test]
fn rows_follow_declaration_order_then_undeclared_names() {
    let declaration = declaration("[[role]]\nname = \"zeta\"\n\n[[role]]\nname = \"alpha\"\n");
    let bodies = vec![live("mid")];
    let rows = compare_team(&declaration, &[], &bodies, &bodies);
    let names: Vec<&str> = rows.iter().map(|row| row.role.as_str()).collect();
    assert_eq!(names, vec!["zeta", "alpha", "mid"]);
}

#[test]
fn an_unmanaged_role_stays_exempt_even_with_duplicate_bodies() {
    let declaration = declaration("[[role]]\nname = \"guest1\"\nunmanaged = true\n");
    let bodies = vec![live("guest1"), live("guest1")];
    let rows = compare_team(&declaration, &[], &bodies, &bodies);
    assert_eq!(disposition_of(&rows, "guest1"), RoleDisposition::Exempt);
}

#[test]
fn a_lock_record_pointing_at_the_root_drifts() {
    let declaration = declaration("[[role]]\nname = \"dev\"\n");
    let root = live("root");
    let lock = vec![lock_pair("dev", &root.id, false)];
    // The root sits in the drift pool (all_live) but not in the
    // comparison set (live).
    let rows = compare_team(&declaration, &lock, &[], std::slice::from_ref(&root));
    let row = rows.iter().find(|row| row.role == "dev").unwrap();
    assert!(row.lock_drift);
    assert!(row.live.is_empty());
}

#[test]
fn a_declared_root_role_is_flagged_as_a_dead_end() {
    let declaration = declaration("[[role]]\nname = \"root\"\n");
    let rows = compare_team(&declaration, &[], &[], &[]);
    let row = rows.iter().find(|row| row.role == "root").unwrap();
    assert!(row.root_conflict);
    // Presence level still says spawn; the flag is what shows the
    // declaration cannot converge that way.
    assert_eq!(row.disposition, RoleDisposition::Spawn);
}

#[test]
fn lock_pairs_parse_loose_roles_and_strict_ids() {
    let pairs = parse_lock_pairs("dev:11111111-1111-4111-8111-111111111111").unwrap();
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].0, "dev");
    // The role may carry colons; the split anchors on the last.
    let pairs = parse_lock_pairs(
            "kallip:dev:11111111-1111-4111-8111-111111111111,scout:22222222-2222-4222-8222-222222222222",
        )
        .unwrap();
    assert_eq!(pairs[0].0, "kallip:dev");
    assert_eq!(pairs[1].0, "scout");
    assert!(parse_lock_pairs("dev-no-id").is_err());
    assert!(parse_lock_pairs(":11111111-1111-4111-8111-111111111111").is_err());
    assert!(parse_lock_pairs("dev:not-a-uuid").is_err());
}

#[test]
fn a_whitespace_padded_lock_role_is_refused() {
    let id = "11111111-1111-4111-8111-111111111111";
    assert!(parse_lock_pairs(&format!(" dev:{id}")).is_err());
    assert!(parse_lock_pairs(&format!("dev :{id}")).is_err());
    assert!(parse_lock_pairs(&format!("   :{id}")).is_err());
}

#[test]
fn a_repeated_lock_role_is_refused_rather_than_last_won() {
    let err = parse_lock_pairs(
        "dev:11111111-1111-4111-8111-111111111111,dev:22222222-2222-4222-8222-222222222222",
    )
    .unwrap_err();
    assert!(err.to_string().to_lowercase().contains("duplicate"));
}

// -- converge planning (plan_converge, project_mapping) --

use crate::test_helpers::{make_profile_bundle, make_state};

/// A live body with plausible defaults: idle, childless, healthy,
/// Normal class, no profile. Tests mutate specific fields.
fn body(role: &str) -> LiveBody {
    LiveBody {
        id: AgentId::random(),
        role: role.to_string(),
        workspace_root: std::path::PathBuf::from("/tmp/kallip-team-tests/unused"),
        description: String::new(),
        profile_set: None,
        permissions_class: kallip_runtime::config::PermissionClass::default(),
        busy: false,
        children: 0,
        faulted: false,
    }
}

fn converge_plan(
    declaration: &TeamDeclaration,
    lock: &[LockPair],
    bodies: &[LiveBody],
) -> Vec<PlannedAction> {
    plan_converge(declaration, lock, None, bodies)
}
fn converge_plan_with_root(
    declaration: &TeamDeclaration,
    lock: &[LockPair],
    root: &LiveBody,
    bodies: &[LiveBody],
) -> Vec<PlannedAction> {
    plan_converge(declaration, lock, Some(root), bodies)
}

fn row_of<'a>(plan: &'a [PlannedAction], role: &str) -> &'a PlannedAction {
    plan.iter()
        .find(|a| a.role == role)
        .unwrap_or_else(|| panic!("no plan row for role {role}"))
}

fn action_of(plan: &[PlannedAction], role: &str) -> TeamAction {
    row_of(plan, role).action
}

fn target_of(plan: &[PlannedAction], role: &str) -> AgentId {
    row_of(plan, role)
        .target
        .clone()
        .unwrap_or_else(|| panic!("role {role} has no target"))
}

#[test]
fn converge_plan_retains_an_in_sync_role() {
    let declaration = declaration("[[role]]\nname = \"dev\"\n");
    let b = body("dev");
    let lock = vec![lock_pair("dev", &b.id, false)];
    let plan = converge_plan(&declaration, &lock, std::slice::from_ref(&b));
    assert_eq!(action_of(&plan, "dev"), TeamAction::Retain);
    assert!(row_of(&plan, "dev").target.is_none());
}

#[test]
fn converge_plan_aligns_metadata_a_live_body_drifted_from() {
    let declaration = declaration(
        "[[role]]\nname = \"dev\"\ndescription = \"builds the thing\"\npermission_class = \"guest\"\n",
    );
    let b = body("dev");
    let lock = vec![lock_pair("dev", &b.id, false)];
    let plan = converge_plan(&declaration, &lock, std::slice::from_ref(&b));
    let row = row_of(&plan, "dev");
    assert_eq!(row.action, TeamAction::AlignMetadata);
    assert_eq!(row.align.description.as_deref(), Some("builds the thing"));
    assert_eq!(
        row.align.permissions_class,
        Some(kallip_runtime::config::PermissionClass::Guest)
    );
}

#[test]
fn converge_plan_never_raises_a_permission_class() {
    let declaration = declaration("[[role]]\nname = \"dev\"\npermission_class = \"normal\"\n");
    let mut b = body("dev");
    b.permissions_class = kallip_runtime::config::PermissionClass::Guest;
    let lock = vec![lock_pair("dev", &b.id, false)];
    let plan = converge_plan(&declaration, &lock, std::slice::from_ref(&b));
    let row = row_of(&plan, "dev");
    assert!(row.align.permissions_class.is_none());
    assert!(row.notes.iter().any(|n| n.contains("never raise")));
}

#[test]
fn converge_plan_adopts_a_live_body_the_lock_does_not_know() {
    let declaration = declaration("[[role]]\nname = \"dev\"\n");
    let b = body("dev");
    let plan = converge_plan(&declaration, &[], std::slice::from_ref(&b));
    assert_eq!(action_of(&plan, "dev"), TeamAction::Adopt);
    assert_eq!(target_of(&plan, "dev"), b.id);
}

#[test]
fn converge_plan_supersedes_a_lock_pointing_at_a_parked_body() {
    let declaration = declaration("[[role]]\nname = \"dev\"\n");
    let parked = AgentId::random();
    let b = body("dev");
    let lock = vec![lock_pair("dev", &parked, true)];
    let plan = converge_plan(&declaration, &lock, std::slice::from_ref(&b));
    assert_eq!(action_of(&plan, "dev"), TeamAction::Adopt);
    assert_eq!(target_of(&plan, "dev"), b.id);
    let row = row_of(&plan, "dev");
    assert!(row.notes.iter().any(|n| n.contains("superseded")));
    // Supersede is not a discard: the record was never wrong about
    // being on disk, project_mapping replaces it with the live body.
    assert!(!row.discard_record);
}

#[test]
fn converge_plan_restores_a_parked_body_and_spawns_an_empty_role() {
    let declaration = declaration("[[role]]\nname = \"dev\"\n\n[[role]]\nname = \"scout\"\n");
    let parked = AgentId::random();
    let lock = vec![lock_pair("dev", &parked, true)];
    let plan = converge_plan(&declaration, &lock, &[]);
    assert_eq!(action_of(&plan, "dev"), TeamAction::Restore);
    assert_eq!(target_of(&plan, "dev"), parked);
    assert_eq!(action_of(&plan, "scout"), TeamAction::Spawn);
}

#[test]
fn converge_plan_deactivates_a_stray_live_role() {
    let declaration = declaration("[[role]]\nname = \"dev\"\n");
    let stray = body("stray");
    let plan = converge_plan(&declaration, &[], std::slice::from_ref(&stray));
    assert_eq!(action_of(&plan, "stray"), TeamAction::Deactivate);
    assert_eq!(target_of(&plan, "stray"), stray.id);
}

#[test]
fn converge_plan_discards_archived_and_nowhere_lock_records() {
    let declaration = declaration("[[role]]\nname = \"dev\"\n\n[[role]]\nname = \"scout\"\n");
    let retired = AgentId::random();
    let gone = AgentId::random();
    let lock = vec![
        LockPair {
            role: "dev".to_string(),
            id: retired,
            in_inactive: false,
            in_archived: true,
        },
        lock_pair("scout", &gone, false),
    ];
    let plan = converge_plan(&declaration, &lock, &[]);
    // Archived bodies are never resurrected: spawn a fresh one and
    // drop the record from the mapping.
    let dev = row_of(&plan, "dev");
    assert_eq!(dev.action, TeamAction::Spawn);
    assert!(dev.discard_record);
    assert!(dev.notes.iter().any(|n| n.contains("archived")));
    // A record whose id is nowhere on disk is discarded too.
    let scout = row_of(&plan, "scout");
    assert!(scout.discard_record);
    assert!(scout.notes.iter().any(|n| n.contains("not on disk")));
}

// -- converge mapping projection + preflight refusals --

fn planned(role: &str, action: TeamAction, target: Option<AgentId>) -> PlannedAction {
    PlannedAction {
        role: role.to_string(),
        action,
        disposition: RoleDisposition::Retain,
        target,
        notes: Vec::new(),
        declared: None,
        align: AlignItems::default(),
        discard_record: false,
    }
}

#[test]
fn project_mapping_keeps_known_targets_and_skips_spawnless_rows() {
    let stamped = "2026-01-01T00:00:00Z";
    let parked = AgentId::random();
    let adopted = AgentId::random();
    let aligning = AgentId::random();
    let plan = vec![
        planned("dev", TeamAction::Restore, Some(parked.clone())),
        planned("scout", TeamAction::Adopt, Some(adopted.clone())),
        planned(
            "reviewer",
            TeamAction::AlignMetadata,
            Some(aligning.clone()),
        ),
        planned("guest1", TeamAction::Retain, None),
        planned("pilot", TeamAction::Spawn, None),
    ];
    let mut mapping = Vec::new();
    // Dry run: no spawned ids exist yet, so the spawn row projects
    // nothing; align/retain rows contribute no new bindings.
    project_mapping(&mut mapping, &plan, None, stamped);
    let bound: Vec<(&str, &AgentId)> = mapping.iter().map(|e| (e.role.as_str(), &e.id)).collect();
    assert_eq!(bound, vec![("dev", &parked), ("scout", &adopted)]);
    // Post-execution: a spawned id lands under its role.
    let spawned_id = AgentId::random();
    let mut spawned = HashMap::new();
    spawned.insert("pilot".to_string(), spawned_id.clone());
    project_mapping(&mut mapping, &plan, Some(&spawned), stamped);
    assert_eq!(mapping.len(), 3);
    assert!(
        mapping
            .iter()
            .any(|e| e.role == "pilot" && e.id == spawned_id)
    );
}

#[test]
fn preflight_rejects_a_lock_id_keyed_under_two_roles() {
    let state = make_state();
    let shared = AgentId::random();
    let declaration = declaration("[[role]]\nname = \"dev\"\n\n[[role]]\nname = \"scout\"\n");
    let plan = converge_plan(&declaration, &[], &[]);
    let lock = vec![
        lock_pair("dev", &shared, true),
        lock_pair("scout", &shared, true),
    ];
    let rejections = preflight_converge(&state, &plan, &None, &lock, &[], false);
    assert!(
        rejections
            .iter()
            .any(|r| r.kind == TeamRejectionKind::LockAmbiguity
                && r.message.contains("lock rebuild"))
    );
}

#[test]
fn preflight_rejects_a_declaration_of_the_root_role() {
    let state = make_state();
    let declaration = declaration("[[role]]\nname = \"root\"\n");
    let plan = converge_plan(&declaration, &[], &[]);
    let rejections = preflight_converge(&state, &plan, &None, &[], &[], false);
    assert!(
        rejections
            .iter()
            .any(|r| r.kind == TeamRejectionKind::RootRole)
    );
}

#[test]
fn preflight_rejects_duplicate_live_bodies_under_one_role() {
    let state = make_state();
    let declaration = declaration("[[role]]\nname = \"dev\"\n");
    let bodies = vec![body("dev"), body("dev")];
    let plan = converge_plan(&declaration, &[], &bodies);
    let rejections = preflight_converge(&state, &plan, &None, &[], &bodies, false);
    assert!(
        rejections
            .iter()
            .any(|r| r.kind == TeamRejectionKind::Duplicate)
    );
}

#[test]
fn preflight_rejects_a_busy_deactivation_target_until_forced() {
    let state = make_state();
    // An empty declaration: the stray is the only plan row, so the
    // only rejection possible is the busy/childed one under test.
    let declaration = declaration("");
    let mut stray = body("stray");
    stray.busy = true;
    let bodies = vec![stray];
    let plan = converge_plan(&declaration, &[], &bodies);
    let rejections = preflight_converge(&state, &plan, &None, &[], &bodies, false);
    assert!(rejections.iter().any(|r| r.kind == TeamRejectionKind::Busy));
    // Force is the auditable escape: the same plan passes.
    let rejections = preflight_converge(&state, &plan, &None, &[], &bodies, true);
    assert!(
        rejections.is_empty(),
        "unexpected rejections: {rejections:?}"
    );
}

#[test]
fn preflight_rejects_a_childed_deactivation_target_even_when_forced() {
    let state = make_state();
    let declaration = declaration("[[role]]\nname = \"dev\"\n");
    let mut stray = body("stray");
    stray.children = 2;
    let bodies = vec![stray];
    let plan = converge_plan(&declaration, &[], &bodies);
    let rejections = preflight_converge(&state, &plan, &None, &[], &bodies, true);
    assert!(
        rejections
            .iter()
            .any(|r| r.kind == TeamRejectionKind::LiveChildren)
    );
}

#[test]
fn preflight_rejects_spawns_without_a_usable_profile_set() {
    let state = make_state();
    let root = body("root");
    let declaration = declaration(
        "[[role]]\nname = \"scout\"\n\n[[role]]\nname = \"pilot\"\nprofile_set = \"no-such-set\"\n",
    );
    let plan = converge_plan(&declaration, &[], &[]);
    let rejections = preflight_converge(&state, &plan, &Some(root), &[], &[], false);
    assert!(
        rejections
            .iter()
            .any(|r| r.kind == TeamRejectionKind::SpawnProfileSet)
    );
    assert!(
        rejections
            .iter()
            .any(|r| r.message.contains("unknown profile_set"))
    );
}

#[test]
fn preflight_rejects_a_spawn_class_spelling_it_does_not_know() {
    let state = make_state();
    let root = body("root");
    let declaration = declaration(
        "[[role]]\nname = \"pilot\"\nprofile_set = \"default\"\npermission_class = \"sudo\"\n",
    );
    let plan = converge_plan(&declaration, &[], &[]);
    let rejections = preflight_converge(&state, &plan, &Some(root), &[], &[], false);
    assert!(
        rejections
            .iter()
            .any(|r| r.kind == TeamRejectionKind::SpawnProfileClass)
    );
}

#[test]
fn preflight_rejects_a_spawn_class_above_the_root() {
    let state = make_state();
    let mut root = body("root");
    root.permissions_class = kallip_runtime::config::PermissionClass::Guest;
    let declaration = declaration(
        "[[role]]\nname = \"pilot\"\nprofile_set = \"default\"\npermission_class = \"normal\"\n",
    );
    let plan = converge_plan(&declaration, &[], &[]);
    let rejections = preflight_converge(&state, &plan, &Some(root), &[], &[], false);
    assert!(
        rejections
            .iter()
            .any(|r| r.message.contains("exceeds the root"))
    );
}

#[test]
fn preflight_rejects_a_converge_over_capacity() {
    use kallip_common::authtoken::TokenHash;
    use kallip_common::policy::PolicyPreset;
    let profiles = make_profile_bundle();
    let state: crate::state::SharedState =
        std::sync::Arc::new(crate::state::AppState::with_limits(
            TokenHash::of("op-token"),
            1,
            1,
            5,
            profiles,
            PolicyPreset::Default,
            kallip_runtime::usage_stats::UsageStats::default(),
            kallip_runtime::token_budget::TokenBudget::unlimited(),
            None,
        ));
    let root = body("root");
    let declaration = declaration(
        "[[role]]\nname = \"a\"\nprofile_set = \"default\"\n\n[[role]]\nname = \"b\"\nprofile_set = \"default\"\n",
    );
    let plan = converge_plan(&declaration, &[], &[]);
    let rejections = preflight_converge(&state, &plan, &Some(root), &[], &[], false);
    assert!(
        rejections
            .iter()
            .any(|r| r.kind == TeamRejectionKind::CapacityAgents)
    );
    assert!(
        rejections
            .iter()
            .any(|r| r.kind == TeamRejectionKind::CapacityChildren)
    );
}
#[test]
fn preflight_allows_a_pure_deactivation_plan() {
    let state = make_state();
    let root = body("root");
    let dev = AgentId::random();
    let plan = vec![planned("dev", TeamAction::Deactivate, Some(dev))];
    // A shrink-only plan nets negative children; the signed net
    // must not underflow, and the root reads under its cap after.
    let rejections = preflight_converge(&state, &plan, &Some(root), &[], &[], false);
    assert!(
        rejections.is_empty(),
        "unexpected rejections: {rejections:?}"
    );
}

#[test]
fn converge_plan_discards_a_drifted_lock_record() {
    let declaration = declaration("[[role]]\nname = \"dev\"\n");
    let misplaced = body("scout");
    let lock = vec![lock_pair("dev", &misplaced.id, false)];
    let plan = converge_plan(&declaration, &lock, std::slice::from_ref(&misplaced));
    let dev = row_of(&plan, "dev");
    // Nothing live under the role and the recorded id is live
    // elsewhere: spawn fresh, the stale record is dropped.
    assert_eq!(dev.action, TeamAction::Spawn);
    assert!(dev.discard_record);
    assert!(
        dev.notes
            .iter()
            .any(|n| n.contains("live under another role"))
    );
}

#[test]
fn converge_plan_drifts_a_lock_record_pointing_at_the_root() {
    let declaration = declaration("[[role]]\nname = \"dev\"\n");
    let root = body("root");
    let lock = vec![lock_pair("dev", &root.id, false)];
    let plan = converge_plan_with_root(&declaration, &lock, &root, &[]);
    let dev = row_of(&plan, "dev");
    assert_eq!(dev.action, TeamAction::Spawn);
    assert!(dev.discard_record);
    assert!(
        dev.notes
            .iter()
            .any(|n| n.contains("live under another role"))
    );
    // The drift-pool invariant: the notes never claim the row is
    // nowhere on disk.
    assert!(!dev.notes.iter().any(|n| n.contains("not on disk")));
}

#[tokio::test]
async fn restore_degrades_loudly_when_the_body_meta_is_unreadable() {
    let state = crate::test_helpers::make_state();
    let id = AgentId::random();
    // A parked body whose meta cannot be read: the restore path must
    // degrade loudly (a fresh-spawn attempt surfaces), never land as
    // if nothing happened.
    let dir = kallip_runtime::persistence::inactive_dir(&id).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("meta.json"), "not valid meta").unwrap();
    let row = planned("dev", TeamAction::Restore, Some(id));
    match restore_action(&state, &row).await {
        RestoreFallout::Failed(detail) => {
            assert!(detail.contains("restore degraded to a fresh spawn"));
            assert!(detail.contains("the spawn failed"));
        }
        RestoreFallout::Restored { .. } | RestoreFallout::Degraded { .. } => {
            panic!("restore must not land when the body meta is unreadable")
        }
    }
}

#[tokio::test]
async fn window_violation_stops_the_batch_at_the_failed_deactivate() {
    let state = crate::test_helpers::make_state();
    let aligned = AgentId::random();
    let vanished = AgentId::random();
    let plan = vec![
        planned("dev", TeamAction::AlignMetadata, Some(aligned)),
        planned("scout", TeamAction::Deactivate, Some(vanished)),
    ];
    let mut mapping = Vec::new();
    let root: Option<LiveBody> = None;
    let (results, aborted) = execute_converge(
        &state,
        &plan,
        &mut mapping,
        &root,
        false,
        "2026-01-01T00:00:00Z",
    )
    .await;
    assert!(aborted);
    // Deactivates run first, so the vanished deactivate is row 0: it
    // fails and the fail-fast stop leaves the align row unexecuted.
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].outcome, TeamRowOutcome::Failed);
    assert!(
        results[0]
            .detail
            .contains("vanished in the plan→execute window")
    );
}

#[tokio::test]
async fn window_violation_stops_the_batch_at_an_empty_align_row() {
    let state = crate::test_helpers::make_state();
    let vanished = AgentId::random();
    // An adopt row carries no align fields: the empty-align shortcut
    // must still re-read the target, or a vanished agent reports
    // Applied and the lock mapping rebinds to a dead id.
    let plan = vec![planned("dev", TeamAction::Adopt, Some(vanished))];
    let mut mapping = Vec::new();
    let root: Option<LiveBody> = None;
    let (results, aborted) = execute_converge(
        &state,
        &plan,
        &mut mapping,
        &root,
        false,
        "2026-01-01T00:00:00Z",
    )
    .await;
    assert!(aborted);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].outcome, TeamRowOutcome::Failed);
    assert!(
        results[0]
            .detail
            .contains("vanished in the plan→execute window")
    );
    assert!(mapping.is_empty());
}
// ---- execute-layer harness: a counting spawn stub drives the
// converge spawn/restore paths without a real runtime ----

/// Spawn stub for the execution-layer tests: counts calls, returns
/// a fresh entry per call, and fails on demand so both spawn
/// outcomes are drivable.
fn spawn_stub(
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    fail: bool,
) -> crate::lifecycle::SpawnFn {
    std::sync::Arc::new(move |_args: crate::lifecycle::SpawnArgs| {
        calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let fresh = crate::test_helpers::make_entry_with_rx(None, "stub-token".to_string());
        let crate::state::AgentEntry {
            identity, agent, ..
        } = fresh.0;
        let _keep_rx_open = fresh.1;
        Box::pin(async move {
            if fail {
                anyhow::bail!("stub spawn failure");
            }
            Ok((agent, identity))
        })
    })
}

#[tokio::test]
async fn converge_spawn_row_spawns_binds_the_mapping_and_reports() {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let state = crate::test_helpers::make_state_with_spawn(spawn_stub(calls.clone(), false));
    let declaration = declaration("[[role]]\nname = \"dev\"\nprofile_set = \"default\"\n");
    let plan = converge_plan(&declaration, &[], &[]);
    let root = body("root");
    // AgentConfig::load resolves the derived workspace, so the
    // directory must exist before the row executes.
    std::fs::create_dir_all(root.workspace_root.join("team").join("dev")).unwrap();
    // spawn_subagent re-reads the supervisor from the registry.
    let (root_entry, _rx) = crate::test_helpers::make_entry_with_rx(None, "root-token".to_string());
    state.registry.write().await.register(
        root.id.clone(),
        crate::state::RegistryEntry::Live(root_entry),
    );
    let mut mapping = Vec::new();
    let (results, aborted) = execute_converge(
        &state,
        &plan,
        &mut mapping,
        &Some(root),
        false,
        "2026-01-01T00:00:00Z",
    )
    .await;
    assert!(!aborted, "results: {results:?}");
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].outcome, TeamRowOutcome::Applied);
    let id = results[0]
        .agent_id
        .clone()
        .expect("spawn reports the new id");
    assert_eq!(mapping.len(), 1);
    assert_eq!(mapping[0].role, "dev");
    assert_eq!(mapping[0].id, id);
}

#[tokio::test]
async fn converge_spawn_failure_fails_loudly_and_stops_the_batch() {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let state = crate::test_helpers::make_state_with_spawn(spawn_stub(calls.clone(), true));
    let declaration = declaration("[[role]]\nname = \"dev\"\nprofile_set = \"default\"\n");
    let plan = converge_plan(&declaration, &[], &[]);
    let root = body("root");
    std::fs::create_dir_all(root.workspace_root.join("team").join("dev")).unwrap();
    let (root_entry, _rx) = crate::test_helpers::make_entry_with_rx(None, "root-token".to_string());
    state.registry.write().await.register(
        root.id.clone(),
        crate::state::RegistryEntry::Live(root_entry),
    );
    let mut mapping = Vec::new();
    let (results, aborted) = execute_converge(
        &state,
        &plan,
        &mut mapping,
        &Some(root),
        false,
        "2026-01-01T00:00:00Z",
    )
    .await;
    assert!(aborted);
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].outcome, TeamRowOutcome::Failed);
    // The stub's cause is logged, not surfaced: `ApiError::internal`
    // sanitizes the row detail to the generic message.
    assert!(results[0].detail.contains("internal error"));
    assert!(!results[0].detail.contains("stub spawn failure"));
    assert!(mapping.is_empty());
}

#[tokio::test]
async fn converge_restore_degrades_into_a_fresh_spawn_and_rebinds() {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let state = crate::test_helpers::make_state_with_spawn(spawn_stub(calls.clone(), false));
    // The degraded fallback spawns under the registry's live root.
    let mut root_entry = crate::test_helpers::make_entry_with_rx(None, "root-token".to_string()).0;
    let root_ws_guard = dev_tempdir("conv-root");
    let root_ws = root_ws_guard.path().to_path_buf();
    root_entry.identity.config.workspace_root = root_ws.clone();
    let root_id = AgentId::random();
    state
        .registry
        .write()
        .await
        .register(root_id, crate::state::RegistryEntry::Live(root_entry));
    std::fs::create_dir_all(root_ws.join("team").join("dev")).unwrap();
    let old = AgentId::random();
    let declaration = declaration("[[role]]\nname = \"dev\"\nprofile_set = \"default\"\n");
    let lock = vec![lock_pair("dev", &old, true)];
    let plan = converge_plan(&declaration, &lock, &[]);
    // The recorded body never reached the inactive area: the restore
    // degrades into a fresh spawn instead of resurrecting it.
    let mut mapping = vec![TeamLockEntry {
        role: "dev".to_string(),
        id: old.clone(),
        converged_at: "2025-12-01T00:00:00Z".to_string(),
    }];
    let root: Option<LiveBody> = None;
    let (results, aborted) = execute_converge(
        &state,
        &plan,
        &mut mapping,
        &root,
        false,
        "2026-01-01T00:00:00Z",
    )
    .await;
    assert!(!aborted, "results: {results:?}");
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].outcome, TeamRowOutcome::Applied);
    assert!(results[0].detail.contains("degraded into a fresh spawn"));
    assert!(
        results[0]
            .notes
            .iter()
            .any(|n| n.contains("was not reused"))
    );
    let new_id = results[0]
        .agent_id
        .clone()
        .expect("degraded spawn reports the new id");
    assert_ne!(new_id, old);
    assert_eq!(mapping[0].id, new_id);
    assert!(state.registry.read().await.get(&old).is_none());
}
