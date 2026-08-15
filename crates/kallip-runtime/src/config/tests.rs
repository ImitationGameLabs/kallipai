use super::*;
use crate::tools::context::ContextUnpinTool;
use kallip_common::policy::PolicyPreset;

fn prefix(tokens: &[&str]) -> Trigger {
    Trigger::Prefix(tokens.iter().map(|t| (*t).to_owned()).collect())
}

use kallip_shell::tools::names;

#[test]
fn permission_class_ceiling_matches_tier_table() {
    // §2.3: tier 0/1 -> Normal, tier 2/3 -> Guest (the plateaus that mean depth
    // monotonicity does NOT imply ceiling monotonicity).
    assert_eq!(
        PermissionClass::ceiling_for_tier(0),
        PermissionClass::Normal
    );
    assert_eq!(
        PermissionClass::ceiling_for_tier(1),
        PermissionClass::Normal
    );
    assert_eq!(PermissionClass::ceiling_for_tier(2), PermissionClass::Guest);
    assert_eq!(PermissionClass::ceiling_for_tier(3), PermissionClass::Guest);
    // Beyond the table clamps to the last entry (Guest), like select_profile.
    assert_eq!(
        PermissionClass::ceiling_for_tier(99),
        PermissionClass::Guest
    );
}

#[test]
fn permission_class_from_str_display_round_trip() {
    use std::str::FromStr;
    // Lowercase wire/env spelling, both variants round-trip through Display.
    for class in [PermissionClass::Normal, PermissionClass::Guest] {
        let s = class.to_string();
        assert_eq!(PermissionClass::from_str(&s).unwrap(), class);
    }
    assert_eq!(PermissionClass::Normal.to_string(), "normal");
    assert_eq!(PermissionClass::Guest.to_string(), "guest");

    // FromStr is trim-free: untrimmed input is rejected (the tagma must not
    // silently accept " guest "). The env knob trims before parsing.
    assert!(PermissionClass::from_str(" guest ").is_err());
    assert!(PermissionClass::from_str("Normal").is_err());
    assert!(PermissionClass::from_str("").is_err());
    let err = PermissionClass::from_str("admin").unwrap_err();
    assert!(err.to_string().contains("admin"));
    assert!(err.to_string().contains("normal"));
}

#[test]
fn delegation_mode_from_str_display_round_trip() {
    use std::str::FromStr;
    for mode in [DelegationMode::CarveOut, DelegationMode::FullHandoff] {
        let s = mode.to_string();
        assert_eq!(DelegationMode::from_str(&s).unwrap(), mode);
    }
    assert_eq!(DelegationMode::CarveOut.to_string(), "carve_out");
    assert_eq!(DelegationMode::FullHandoff.to_string(), "full_handoff");
    // The wire producer defaults a missing field to "carve_out" itself, so
    // FromStr must NOT treat an explicit empty string as the default -- it
    // is a client bug and must be rejected (not silently become CarveOut).
    assert!(DelegationMode::from_str("").is_err());
    assert!(DelegationMode::from_str("CarveOut").is_err());
    let err = DelegationMode::from_str("handoff").unwrap_err();
    assert!(err.to_string().contains("handoff"));
    assert!(err.to_string().contains("carve_out"));
}

#[test]
fn default_system_prompt_stays_high_altitude() {
    // The base prompt must stay at agent altitude: posture and the
    // tool/round execution model. Agent identity is injected per-agent by
    // the tagma (routes/agent.rs `compose_system_prompt`); tool mechanics
    // belong in each tool's `description()` and the skill system belongs in
    // the bootstrap meta-skill the tagma appends at runtime. This guard
    // prevents tool/CLI usage from creeping back into the prompt and
    // re-duplicating those sources (drift + per-request token cost).
    let prompt = DEFAULT_SYSTEM_PROMPT;
    assert!(
        prompt.contains("# Posture"),
        "must keep the posture section"
    );
    assert!(
        prompt.contains("# Tool and round model"),
        "must keep the tool/round-model section"
    );
    assert!(prompt.contains("asynchronous"));
    for verboten in [
        names::BASH_EXEC,
        names::BG_READ,
        names::BG_KILL,
        ContextUnpinTool::NAME,
        // The approval family was migrated out of the prompt together; guard
        // the whole family, not just one member.
        "approval_redeem",
        "approval_commit",
        "approval_list",
        "approval_cancel",
    ] {
        assert!(
            !prompt.contains(verboten),
            "DEFAULT_SYSTEM_PROMPT must not embed tool/CLI usage ('{verboten}'); \
                 it belongs in the tool description or the bootstrap skill"
        );
    }
}

#[test]
fn check_context_budget_rejects_zero_window() {
    assert!(check_context_budget(0, 100, 50, 0.25).is_err());
}

#[test]
fn check_context_budget_rejects_reserve_ge_window() {
    assert!(check_context_budget(1000, 1000, 100, 0.25).is_err()); // equal
    assert!(check_context_budget(1000, 1001, 100, 0.25).is_err()); // greater
}

#[test]
fn check_context_budget_rejects_summary_exceeding_pinned() {
    // effective = 1000 − 200 = 800; pinned = 800 × 0.25 = 200.
    assert!(check_context_budget(1000, 200, 201, 0.25).is_err()); // over
    assert!(check_context_budget(1000, 200, 200, 0.25).is_ok()); // boundary ok
}

#[test]
fn set_context_window_leaves_field_unchanged_on_validation_failure() {
    // A valid baseline: window 100k → pinned = (100k − 8192) × 0.25 = 22952 ≥ summary 1200.
    let mut cfg = AgentConfig {
        prompt: None,
        system_prompt: String::new(),
        max_tool_rounds: usize::MAX,
        max_heartbeat_rounds: DEFAULT_MAX_HEARTBEAT_ROUNDS,
        max_transient_retries: DEFAULT_MAX_TRANSIENT_RETRIES,
        workspace_root: PathBuf::from("/tmp"),
        context_window_tokens: 100_000,
        output_reserve_tokens: 8_192,
        summary_max_tokens: 1_200,
        tool_timeout_secs: 120,
        skills: vec![],
        retry_policy: RetryPolicy::default(),
        pinned_budget_ratio: 0.25,
        context_thresholds: vec![50, 80],
        token_budget_warnings: vec![80, 95],
        agent_id: None,
        created_by: None,
        permissions: PermissionProfile::new(PathBuf::from("/tmp")),
        permissions_class: PermissionClass::default(),
        role: String::new(),
        description: String::new(),
        delegation_mode: DelegationMode::CarveOut,
    };
    // A 10k window → pinned = (10k − 8192) × 0.25 = 452 < summary 1200 → rejected.
    let err = cfg.set_context_window(10_000).unwrap_err();
    assert!(
        format!("{err}").contains("summary_max_tokens"),
        "got: {err}"
    );
    // Validate-before-mutate: the rejected window must NOT have been adopted.
    assert_eq!(cfg.context_window_tokens, 100_000);
    // And a valid window is adopted.
    cfg.set_context_window(200_000).unwrap();
    assert_eq!(cfg.context_window_tokens, 200_000);
}

#[test]
fn try_context_window_validates_without_mutating() {
    // The failover pre-advance probe: same rejections as `check_context_budget`, but it must
    // NOT install the window (unlike `set_context_window`). Baseline 100k is feasible.
    let cfg = AgentConfig {
        prompt: None,
        system_prompt: String::new(),
        max_tool_rounds: usize::MAX,
        max_heartbeat_rounds: DEFAULT_MAX_HEARTBEAT_ROUNDS,
        max_transient_retries: DEFAULT_MAX_TRANSIENT_RETRIES,
        workspace_root: PathBuf::from("/tmp"),
        context_window_tokens: 100_000,
        output_reserve_tokens: 8_192,
        summary_max_tokens: 1_200,
        tool_timeout_secs: 120,
        skills: vec![],
        retry_policy: RetryPolicy::default(),
        pinned_budget_ratio: 0.25,
        context_thresholds: vec![50, 80],
        token_budget_warnings: vec![80, 95],
        agent_id: None,
        created_by: None,
        permissions: PermissionProfile::new(PathBuf::from("/tmp")),
        permissions_class: PermissionClass::default(),
        role: String::new(),
        description: String::new(),
        delegation_mode: DelegationMode::CarveOut,
    };
    assert!(cfg.try_context_window(0).is_err(), "zero window rejected");
    assert!(
        cfg.try_context_window(8_000).is_err(),
        "output_reserve (8192) ≥ window rejected"
    );
    // 10k → pinned = (10k − 8192) × 0.25 = 452 < summary 1200 → infeasible (the failover skip).
    assert!(cfg.try_context_window(10_000).is_err());
    assert!(cfg.try_context_window(200_000).is_ok());
    assert_eq!(
        cfg.context_window_tokens, 100_000,
        "try_context_window must not install the window"
    );
}

#[test]
fn pinned_budget_matches_effective_times_ratio() {
    // effective = 100000 − 8192 = 91808; pinned = 91808 × 0.25 = 22952.
    let cfg = AgentConfig {
        prompt: None,
        system_prompt: String::new(),
        max_tool_rounds: usize::MAX,
        max_heartbeat_rounds: DEFAULT_MAX_HEARTBEAT_ROUNDS,
        max_transient_retries: DEFAULT_MAX_TRANSIENT_RETRIES,
        workspace_root: PathBuf::from("/tmp"),
        context_window_tokens: 100_000,
        output_reserve_tokens: 8_192,
        summary_max_tokens: 1_200,
        tool_timeout_secs: 120,
        skills: vec![],
        retry_policy: RetryPolicy::default(),
        pinned_budget_ratio: 0.25,
        context_thresholds: vec![50, 80],
        token_budget_warnings: vec![80, 95],
        agent_id: None,
        created_by: None,
        permissions: PermissionProfile::new(PathBuf::from("/tmp")),
        permissions_class: PermissionClass::default(),
        role: String::new(),
        description: String::new(),
        delegation_mode: DelegationMode::CarveOut,
    };
    assert_eq!(cfg.effective_budget(), 91_808);
    assert_eq!(cfg.pinned_budget(), 22_952);
}

#[test]
fn policy_preset_from_env_unset_returns_default() {
    temp_env::with_vars_unset(["KALLIP_POLICY_PRESET"], || {
        assert_eq!(policy_preset_from_env(), PolicyPreset::Default);
    });
}

#[test]
fn policy_preset_from_env_empty_returns_default() {
    temp_env::with_vars([("KALLIP_POLICY_PRESET", Some(""))], || {
        assert_eq!(policy_preset_from_env(), PolicyPreset::Default);
    });
}

#[test]
fn policy_preset_from_env_explicit_default() {
    temp_env::with_vars([("KALLIP_POLICY_PRESET", Some("default"))], || {
        assert_eq!(policy_preset_from_env(), PolicyPreset::Default);
    });
}

#[test]
fn policy_preset_from_env_auto() {
    temp_env::with_vars([("KALLIP_POLICY_PRESET", Some("auto"))], || {
        assert_eq!(policy_preset_from_env(), PolicyPreset::Auto);
    });
}

#[test]
fn policy_preset_from_env_allow_all() {
    temp_env::with_vars([("KALLIP_POLICY_PRESET", Some("allow-all"))], || {
        assert_eq!(policy_preset_from_env(), PolicyPreset::AllowAll);
    });
}

#[test]
fn policy_preset_from_env_whitespace_padded() {
    temp_env::with_vars([("KALLIP_POLICY_PRESET", Some("  auto  "))], || {
        assert_eq!(policy_preset_from_env(), PolicyPreset::Auto);
    });
}

#[test]
#[should_panic(expected = "KALLIP_POLICY_PRESET: invalid policy preset")]
fn policy_preset_from_env_invalid_panics() {
    temp_env::with_vars([("KALLIP_POLICY_PRESET", Some("gibberish"))], || {
        let _ = policy_preset_from_env();
    });
}

#[test]
#[should_panic(expected = "KALLIP_POLICY_PRESET: invalid policy preset")]
fn policy_preset_from_env_ask_all_no_longer_valid() {
    // `ask-all` was dropped; it must now be a fatal misconfiguration rather
    // than silently falling back.
    temp_env::with_vars([("KALLIP_POLICY_PRESET", Some("ask-all"))], || {
        let _ = policy_preset_from_env();
    });
}

#[test]
fn exec_hooks_missing_file_yields_builtin_preset() {
    let dir = std::env::temp_dir().join(format!("exec-hooks-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let rules = load_exec_hook_rules(&dir.join("exec_hooks.toml"));
    assert_eq!(rules.len(), 2);
    // BTreeMap canonical-key order: '@' sorts before letters.
    assert_eq!(rules[0].trigger, Trigger::WriteRedirect);
    assert_eq!(rules[1].trigger, prefix(&["git", "commit"]));
    assert_eq!(
        rules[1].note,
        "a git commit command just ran; were the relevant skills applied to the task, the change, and the commit message?"
    );
}

#[test]
fn exec_hooks_parse_both_value_forms_and_off() {
    let raw = "[overrides]\n\"git commit\" = \"commit skill applies\"\n\"GIT PUSH\" = { note = \"confirm the remote\" }\n\"x\" = \"off\"\n";
    let overrides = parse_exec_hook_overrides(raw).unwrap();
    // Canonical (lowercased) keys sort: "git commit" < "git push" < "x".
    let specs: Vec<_> = overrides.values().collect();
    assert_eq!(specs[0].note.as_deref(), Some("commit skill applies"));
    assert_eq!(specs[1].note.as_deref(), Some("confirm the remote"));
    assert_eq!(specs[2].note, None); // bare off → disable
    // Keys canonicalized: lowercased, whitespace-split, rejoined.
    let keys: Vec<_> = overrides.keys().collect();
    assert_eq!(keys, ["git commit", "git push", "x"]);
}

#[test]
fn exec_hooks_merge_replace_disable_add() {
    let raw = "[overrides]\n\"GIT   commit\" = \"my wording\"\n\"new cmd\" = \"added\"\n";
    let overrides = parse_exec_hook_overrides(raw).unwrap();
    // Same builtin key (case/spacing-insensitive) → replaced; new → added.
    let rules = merge_exec_hook_rules(builtin_exec_hook_rules(), overrides);
    assert_eq!(rules.len(), 3);
    // BTreeMap key order: "@write-redirect" < "git commit" < "new cmd".
    assert_eq!(rules[0].trigger, Trigger::WriteRedirect);
    assert_eq!(rules[1].trigger, prefix(&["git", "commit"]));
    assert_eq!(rules[1].note, "my wording");
    assert_eq!(rules[2].trigger, prefix(&["new", "cmd"]));
    // `off` on one builtin key removes only that rule.
    let overrides = parse_exec_hook_overrides("[overrides]\n\"git commit\" = \"off\"\n").unwrap();
    let rules = merge_exec_hook_rules(builtin_exec_hook_rules(), overrides);
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].trigger, Trigger::WriteRedirect);
}

#[test]
fn exec_hooks_bad_toml_or_wrong_shapes_fail_closed() {
    // Bad TOML.
    assert!(parse_exec_hook_overrides("[overrides").is_err());
    // Typo'd top-level table — including the v1-era [hooks] name.
    assert!(parse_exec_hook_overrides("[hook]\n\"git\" = \"n\"").is_err());
    assert!(parse_exec_hook_overrides("[hooks]\n\"git\" = \"n\"").is_err());
    // Unknown key inside the table form.
    assert!(
        parse_exec_hook_overrides("[overrides]\n\"git\" = { note = \"n\", phse = \"pre\" }")
            .is_err()
    );
    // Table form without note.
    assert!(parse_exec_hook_overrides("[overrides]\n\"git\" = {}").is_err());
    // A table note of literal "off" — disabling is the bare form's job.
    assert!(parse_exec_hook_overrides("[overrides]\n\"git\" = { note = \"off\" }").is_err());
    // Value of an unsupported type.
    assert!(parse_exec_hook_overrides("[overrides]\n\"git\" = 3").is_err());
    // A typo'd reserved pseudo-prefix key fails closed.
    assert!(parse_exec_hook_overrides("[overrides]\n\"@write-redirrect\" = \"n\"").is_err());
}

#[test]
fn exec_hooks_empty_prefix_dropped_others_kept() {
    let raw = "[overrides]\n\"\" = \"would match everything\"\n\"git commit\" = \"keep me\"\n";
    let overrides = parse_exec_hook_overrides(raw).unwrap();
    assert_eq!(overrides.len(), 1);
    assert_eq!(overrides["git commit"].note.as_deref(), Some("keep me"));
}

#[test]
fn exec_hooks_colliding_keys_warn_last_in_byte_order_wins() {
    // Two raw keys, one canonical prefix — the byte-order-last raw key wins.
    let raw = "[overrides]\n\"git commit\" = \"second\"\n\"GIT   commit\" = \"first\"\n";
    let overrides = parse_exec_hook_overrides(raw).unwrap();
    assert_eq!(overrides.len(), 1);
    assert_eq!(overrides["git commit"].note.as_deref(), Some("second"));
}

#[test]
fn exec_hooks_empty_note_dropped() {
    // Both the bare and table spellings of an empty note are dropped.
    let raw = "[overrides]\n\"a\" = \"\"\n\"b\" = { note = \"\" }\n";
    assert!(parse_exec_hook_overrides(raw).unwrap().is_empty());
}

#[test]
fn exec_hooks_non_string_table_note_names_the_type() {
    let err = parse_exec_hook_overrides("[overrides]\n\"git\" = { note = 3 }").unwrap_err();
    assert!(err.to_string().contains("invalid type"), "{err}");
    assert!(err.to_string().contains("3"), "{err}");
}

#[test]
#[should_panic(expected = "cannot read exec hook rules")]
fn exec_hooks_unreadable_path_panics() {
    // A directory read is a non-NotFound IO error → fail closed at startup.
    load_exec_hook_rules(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
}
