//! The four tests that re-point the process-wide INSTANCE_ROOTS
//! singleton (`set_instance_roots_for_tests`), including the
//! ensure_test_data_dir regression guard. They live in their own
//! integration-test target so the singleton mutation cannot race the
//! unit-test process.
use kallipai_adk::config::{DelegationMode, PermissionClass};
use kallipai_adk::persistence::{
    AgentMeta, InstanceRoots, data_dir_root, set_instance_roots_for_tests,
};
use kallipai_common::agentid::AgentId;
use kallipai_tagma::lifecycle::restore_agents;
use kallipai_tagma::routes::ensure_root_agent;
use kallipai_tagma::state::RegistryEntry;
use kallipai_tagma::test_helpers::{ensure_test_data_dir, make_state};
use std::path::PathBuf;

#[test]
#[serial_test::serial]
fn ensure_root_agent_refuses_to_mint_when_a_disk_root_exists() {
    // /dev/shm keeps the leaked-env window away from /tmp workspaces: this
    // test pins KALLIPAI_TAGMA_SLUG+XDG_DATA_HOME (serial only among serial tests),
    // and a /tmp-based data dir would overlap the "/tmp" workspaces other
    // concurrently-running tests use, flipping their disjointness checks.
    let tmp = tempfile::TempDir::new_in("/dev/shm").unwrap();
    let root = tmp
        .path()
        .join("kallipai")
        .join("tagmata")
        .join("disk-root")
        .join("agents")
        .join("active")
        .join("disk-root-1");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("meta.json"),
        serde_json::to_string(&AgentMeta {
            workspace_root: PathBuf::from("/ws"),
            created_by: None,
            role: "root".into(),
            description: String::new(),
            profile_set: None,
            permissions_class: PermissionClass::Normal,
            delegation_mode: DelegationMode::CarveOut,
        })
        .unwrap(),
    )
    .unwrap();
    let path = tmp.path().to_str().unwrap().to_owned();
    temp_env::with_vars(
        [
            ("KALLIPAI_TAGMA_SLUG", Some("disk-root")),
            ("XDG_DATA_HOME", Some(path.as_str())),
        ],
        || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let state = make_state();
                // Re-point the roots at the pinned identity AFTER the state
                // constructor (it installs the shared roots only when none
                // are installed or the installed pin is dangling).
                set_instance_roots_for_tests(Some(InstanceRoots {
                    data: tmp
                        .path()
                        .join("kallipai")
                        .join("tagmata")
                        .join("disk-root"),
                    config: tmp
                        .path()
                        .join("kallipai")
                        .join("tagmata")
                        .join("disk-root"),
                    state: tmp.path().join("state"),
                }));
                let err = ensure_root_agent(&state).await.unwrap_err();
                assert!(
                    err.to_string().contains("refusing to mint"),
                    "error points at the disk root: {err}"
                );
                assert!(state.registry.read().await.root_agent().is_none());
            });
        },
    );
}

#[test]
#[serial_test::serial]
fn ensure_root_agent_refuses_to_mint_when_the_agents_dir_is_unreadable() {
    // Same /dev/shm rationale as the disk-root test above. agents/active exists
    // as a regular file so read_dir fails with ENOTDIR -- the runner cannot
    // reproduce a permission-denied directory as root.
    let tmp = tempfile::TempDir::new_in("/dev/shm").unwrap();
    let agents_at = tmp
        .path()
        .join("kallipai")
        .join("tagmata")
        .join("disk-root")
        .join("agents")
        .join("active");
    std::fs::create_dir_all(agents_at.parent().unwrap()).unwrap();
    std::fs::write(agents_at, "not a directory").unwrap();
    let path = tmp.path().to_str().unwrap().to_owned();
    temp_env::with_vars(
        [
            ("KALLIPAI_TAGMA_SLUG", Some("disk-root")),
            ("XDG_DATA_HOME", Some(path.as_str())),
        ],
        || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let state = make_state();
                // Re-point the roots at the pinned identity AFTER the state
                // constructor (it installs the shared roots only when none are
                // installed or the installed pin is dangling).
                set_instance_roots_for_tests(Some(InstanceRoots {
                    data: tmp
                        .path()
                        .join("kallipai")
                        .join("tagmata")
                        .join("disk-root"),
                    config: tmp
                        .path()
                        .join("kallipai")
                        .join("tagmata")
                        .join("disk-root"),
                    state: tmp.path().join("state"),
                }));
                let err = ensure_root_agent(&state).await.unwrap_err();
                assert!(
                    err.to_string()
                        .contains("cannot verify whether a disk root exists"),
                    "error names the verification failure: {err}"
                );
                assert!(state.registry.read().await.root_agent().is_none());
            });
        },
    );
}

/// Regression guard for the roots race behind the two disk-root refusals
/// above: `ensure_test_data_dir` must leave a live pin alone and must
/// replace a pin whose fixture directory is gone with the shared tree.
#[test]
#[serial_test::serial]
fn ensure_test_data_dir_keeps_live_pins_and_replaces_dangling_ones() {
    ensure_test_data_dir();
    // A live pin: an existing directory owned by a (simulated) fixture test.
    let tmp = tempfile::TempDir::new_in("/dev/shm").unwrap();
    let pinned = tmp.path().join("kallipai").join("tagmata").join("pinned");
    std::fs::create_dir_all(&pinned).unwrap();
    set_instance_roots_for_tests(Some(InstanceRoots {
        data: pinned.clone(),
        config: pinned.clone(),
        state: tmp.path().join("state"),
    }));
    ensure_test_data_dir();
    assert_eq!(
        data_dir_root().unwrap(),
        pinned,
        "a live pin must survive ensure_test_data_dir"
    );
    // A dangling pin: the fixture directory is gone (TempDir dropped).
    drop(tmp);
    ensure_test_data_dir();
    let restored = data_dir_root().unwrap();
    assert!(
        restored.ends_with(std::path::Path::new("kallipai/tagmata/test")),
        "a dangling pin must be replaced by the shared tree, got {restored:?}"
    );
}

#[test]
#[serial_test::serial]
fn restore_agents_registers_cycle_stragglers_faulted() {
    // See the ensure-root tests above for why the data dir is
    // created under /dev/shm rather than /tmp: this test mutates
    // XDG_DATA_HOME and must not overlap concurrently-running tests'
    // /tmp workspaces.
    let tmp = tempfile::TempDir::new_in("/dev/shm").unwrap();
    let base = tmp
        .path()
        .join("kallipai")
        .join("tagmata")
        .join("cycle")
        .join("agents")
        .join("active");
    std::fs::create_dir_all(&base).unwrap();
    let a = AgentId::from("cycle-a".to_owned());
    let b = AgentId::from("cycle-b".to_owned());
    let write_meta = |id: &AgentId, created_by: Option<&AgentId>| {
        std::fs::create_dir_all(base.join(id.as_ref())).unwrap();
        std::fs::write(
            base.join(id.as_ref()).join("meta.json"),
            serde_json::to_string(&AgentMeta {
                workspace_root: std::path::PathBuf::from("/ws"),
                created_by: created_by.cloned(),
                role: String::new(),
                description: String::new(),
                profile_set: None,
                permissions_class: PermissionClass::Normal,
                delegation_mode: kallipai_adk::config::DelegationMode::CarveOut,
            })
            .unwrap(),
        )
        .unwrap();
    };
    write_meta(&a, Some(&b));
    write_meta(&b, Some(&a));
    let path = tmp.path().to_str().unwrap().to_owned();
    temp_env::with_vars(
        [
            ("KALLIPAI_TAGMA_SLUG", Some("cycle")),
            ("XDG_DATA_HOME", Some(path.as_str())),
        ],
        || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let state = make_state();
                // Re-point the roots at the pinned identity AFTER the
                // state constructor (it installs the shared roots only when
                // none are installed or the installed pin is dangling).
                set_instance_roots_for_tests(Some(InstanceRoots {
                    data: tmp.path().join("kallipai").join("tagmata").join("cycle"),
                    config: tmp.path().join("kallipai").join("tagmata").join("cycle"),
                    state: tmp.path().join("state"),
                }));
                restore_agents(&state).await.unwrap();
                let registry = state.registry.read().await;
                for id in [&a, &b] {
                    assert!(
                        matches!(registry.get(id), Some(RegistryEntry::Faulted(_))),
                        "cycle agent {id} must be registered faulted, not dropped"
                    );
                }
            });
        },
    );
}

#[test]
#[serial_test::serial]
fn minted_operator_token_persists_0600_in_credentials_dir() {
    ensure_test_data_dir();
    unsafe { std::env::remove_var("KALLIPAI_OPERATOR_TOKEN") };
    // Serial siblings share the data root: resolve once, drop the file, and
    // pin the first-boot shape (absent file + no env = mint) regardless
    // of test order.
    let (_, first) = kallipai_tagma::resolve_operator_token().unwrap();
    let path = match first {
        kallipai_tagma::OperatorTokenResolution::Minted(path)
        | kallipai_tagma::OperatorTokenResolution::Loaded(path) => path,
        kallipai_tagma::OperatorTokenResolution::Pinned => {
            panic!("the env pin was removed; the token must come from disk")
        }
    };
    std::fs::remove_file(&path).unwrap();
    let (token, resolution) = kallipai_tagma::resolve_operator_token().unwrap();
    let path = match resolution {
        kallipai_tagma::OperatorTokenResolution::Minted(path) => path,
        other => panic!("a minted token must be persisted, got {other:?}"),
    };
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "token file must be owner-only");
    let body = std::fs::read_to_string(&path).unwrap();
    let secret = body
        .lines()
        .find_map(|l| l.strip_prefix("KALLIPAI_OPERATOR_TOKEN="))
        .expect("env-file form: KALLIPAI_OPERATOR_TOKEN=<secret>");
    assert_eq!(secret, token.secret());
}

#[test]
#[serial_test::serial]
fn pinned_operator_token_is_taken_verbatim_and_not_written() {
    ensure_test_data_dir();
    unsafe { std::env::set_var("KALLIPAI_OPERATOR_TOKEN", "sk-operator-pinned-value") };
    let (token, resolution) = kallipai_tagma::resolve_operator_token().unwrap();
    unsafe { std::env::remove_var("KALLIPAI_OPERATOR_TOKEN") };
    assert_eq!(token.secret(), "sk-operator-pinned-value");
    assert!(
        matches!(resolution, kallipai_tagma::OperatorTokenResolution::Pinned),
        "a pinned token must not be re-persisted"
    );
}

#[test]
#[serial_test::serial]
fn existing_token_file_is_loaded_verbatim_across_restarts() {
    ensure_test_data_dir();
    unsafe { std::env::remove_var("KALLIPAI_OPERATOR_TOKEN") };
    // A previous run's token file survives restarts: loaded verbatim, not
    // re-minted and not rewritten (the third tier of the boot priority).
    let (_, first) = kallipai_tagma::resolve_operator_token().unwrap();
    let path = match first {
        kallipai_tagma::OperatorTokenResolution::Minted(path)
        | kallipai_tagma::OperatorTokenResolution::Loaded(path) => path,
        kallipai_tagma::OperatorTokenResolution::Pinned => {
            panic!("with the env removed the token must come from disk")
        }
    };
    std::fs::write(&path, "KALLIPAI_OPERATOR_TOKEN=sk-operator-previous\n").unwrap();
    let (token, resolution) = kallipai_tagma::resolve_operator_token().unwrap();
    assert_eq!(token.secret(), "sk-operator-previous");
    assert!(matches!(
        resolution,
        kallipai_tagma::OperatorTokenResolution::Loaded(_)
    ));
    let body = std::fs::read_to_string(&path).unwrap();
    assert_eq!(body, "KALLIPAI_OPERATOR_TOKEN=sk-operator-previous\n");
}

#[test]
#[serial_test::serial]
fn valueless_token_file_fails_the_boot_instead_of_re_minting() {
    ensure_test_data_dir();
    unsafe { std::env::remove_var("KALLIPAI_OPERATOR_TOKEN") };
    // An existing but valueless token file is a broken state, not a fresh
    // install: the boot must fail loudly rather than silently mint over it.
    let (_, first) = kallipai_tagma::resolve_operator_token().unwrap();
    let path = match first {
        kallipai_tagma::OperatorTokenResolution::Minted(path)
        | kallipai_tagma::OperatorTokenResolution::Loaded(path) => path,
        kallipai_tagma::OperatorTokenResolution::Pinned => {
            panic!("the env pin was removed; the token must come from disk")
        }
    };
    std::fs::write(&path, "OTHER=x\n").unwrap();
    let err = match kallipai_tagma::resolve_operator_token() {
        Ok(_) => panic!("a valueless token file must fail the boot"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("carries no KALLIPAI_OPERATOR_TOKEN"),
        "valueless file must be named: {err}"
    );
    // Leave a sane state for the serial siblings: the broken file is gone.
    std::fs::remove_file(&path).unwrap();
}
