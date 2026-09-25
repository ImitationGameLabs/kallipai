use super::*;

#[test]
fn agent_meta_round_trips() {
    let meta = AgentMeta {
        workspace_root: PathBuf::from("/app"),
        created_by: None,
        role: "researcher".into(),
        description: "gathers sources".into(),
        profile_set: Some("research".into()),
        permissions_class: crate::config::PermissionClass::Guest,
        delegation_mode: crate::config::DelegationMode::CarveOut,
    };
    let json = serde_json::to_string(&meta).unwrap();
    let back: AgentMeta = serde_json::from_str(&json).unwrap();
    assert_eq!(back.workspace_root, PathBuf::from("/app"));
    assert_eq!(back.role, "researcher");
    assert_eq!(back.description, "gathers sources");
    assert_eq!(back.profile_set.as_deref(), Some("research"));
    assert_eq!(
        back.permissions_class,
        crate::config::PermissionClass::Guest
    );
    assert_eq!(
        back.delegation_mode,
        crate::config::DelegationMode::CarveOut
    );
    // The snake_case spelling round-trips for FullHandoff too.
    let fh = AgentMeta {
        delegation_mode: crate::config::DelegationMode::FullHandoff,
        ..meta
    };
    let back2: AgentMeta = serde_json::from_str(&serde_json::to_string(&fh).unwrap()).unwrap();
    assert_eq!(
        back2.delegation_mode,
        crate::config::DelegationMode::FullHandoff
    );
}

#[test]
fn agent_meta_pins_on_disk_enum_spellings() {
    // The two AgentMeta enums serialize in DIFFERENT cases by design:
    // PermissionClass keeps a PascalCase persisted form ("Guest"), while
    // DelegationMode uses snake_case ("full_handoff"). Pin both spellings
    // so a future "normalize the cases" refactor knows exactly what it
    // breaks (legacy meta.json files on disk carry these literals).
    let meta = AgentMeta {
        workspace_root: PathBuf::from("/app"),
        created_by: None,
        role: String::new(),
        description: String::new(),
        profile_set: None,
        permissions_class: crate::config::PermissionClass::Guest,
        delegation_mode: crate::config::DelegationMode::FullHandoff,
    };
    let json = serde_json::to_string(&meta).unwrap();
    assert!(
        json.contains(r#""permissions_class":"Guest""#),
        "PermissionClass persists PascalCase; got: {json}"
    );
    assert!(
        json.contains(r#""delegation_mode":"full_handoff""#),
        "DelegationMode persists snake_case; got: {json}"
    );
}

#[test]
fn agent_meta_loads_legacy_file_without_optional_fields() {
    // A meta.json written before optional fields existed still restores.
    let legacy = r#"{
        "workspace_root": "/app",
        "created_by": null
    }"#;
    let meta: AgentMeta = serde_json::from_str(legacy).unwrap();
    assert_eq!(meta.workspace_root, PathBuf::from("/app"));
    // String fields default to empty; enum fields default to their enum default.
    assert_eq!(meta.role, "");
    assert_eq!(meta.description, "");
    assert_eq!(
        meta.delegation_mode,
        crate::config::DelegationMode::CarveOut
    );
}
