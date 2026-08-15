use super::*;

fn prefix(tokens: &[&str]) -> Trigger {
    Trigger::Prefix(tokens.iter().map(|t| (*t).to_owned()).collect())
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
