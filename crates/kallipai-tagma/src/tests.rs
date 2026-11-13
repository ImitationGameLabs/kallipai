use super::*;
use serial_test::serial;

#[test]
fn files_access_lists_enrolled_faces_in_config_order() {
    let mut inner = match std::sync::Arc::try_unwrap(crate::test_helpers::make_state()) {
        Ok(inner) => inner,
        Err(_) => panic!("make_state hands out the sole reference"),
    };
    let face = |name: &str, token: Option<&str>| state::PlatformFace {
        name: name.to_owned(),
        origin: format!("http://{name}"),
        base: format!("http://{name}/v1/model-gateway"),
        credential: token.map(kallipai_common::credential::Credential::fixed),
    };
    inner.platforms = std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(vec![
        face("alpha", None),
        face("beta", Some("b-token")),
        face("gamma", Some("g-token")),
    ]));
    let state = std::sync::Arc::new(inner);
    let access = state.files_access();
    assert_eq!(
        access.len(),
        2,
        "only enrolled faces with a credential can fetch"
    );
    assert_eq!(access[0].name, "beta");
    assert_eq!(access[0].origin, "http://beta");
    assert_eq!(access[0].credential.current(), "b-token");
    assert_eq!(access[1].name, "gamma");
    assert_eq!(access[1].origin, "http://gamma");
    assert_eq!(access[1].credential.current(), "g-token");
}

#[test]
fn gateway_params_pair_the_active_origin_with_its_enrollment_token() {
    let entry = |name: &str, url: &str| RelayEntry {
        name: name.to_owned(),
        polis_url: url.to_owned(),
        enrollment_code: None,
    };
    // Enroll one entry on disk (credentials/<name>/tagma.{id,token}).
    let root = tempfile::TempDir::new().unwrap();
    let enrolled = |root: &std::path::Path, name: &str| {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tagma.id"), "id-1").unwrap();
        std::fs::write(dir.join("tagma.token"), "token-1").unwrap();
    };
    // No relay entries: no platform face, so no gateway at all.
    assert!(gateway_params_from(&[], None).is_none());
    // Entries exist but none enrolled: the token half is missing.
    let entries = [
        entry("alpha", "https://api.example.com"),
        entry("beta", "http://api.other.test"),
    ];
    let faces = platform_faces(&entries, root.path());
    assert!(gateway_params_from(&faces, None).is_none());
    // The second entry enrolls: it is the first (and only) dual-complete
    // face, so the unpinned default resolves it and derives the base
    // from ITS origin (the one-segment shape every backend service
    // uses).
    enrolled(root.path(), "beta");
    let faces = platform_faces(&entries, root.path());
    let params = gateway_params_from(&faces, None).unwrap();
    assert_eq!(params.base, "http://api.other.test/v1/model-gateway");
    assert_eq!(params.origin, "http://api.other.test");
    assert_eq!(params.credential.current(), "token-1");
    // A pin decides even when it points at an UNENROLLED entry: no
    // silent fallback to the enrolled one — no enrollment, no gateway.
    assert!(gateway_params_from(&faces, Some("https://api.example.com")).is_none());
    // A pin matching no configured entry yields no gateway either.
    assert!(gateway_params_from(&faces, Some("https://nowhere.test")).is_none());
    // The pin enrolls later: the pin wins over the config order.
    enrolled(root.path(), "alpha");
    let faces = platform_faces(&entries, root.path());
    let params = gateway_params_from(&faces, Some("https://api.example.com")).unwrap();
    assert_eq!(params.origin, "https://api.example.com");
    assert_eq!(params.credential.current(), "token-1");
}

#[test]
fn a_relay_entry_keeps_one_credential_handle_across_its_readers() {
    // The face boot resolves carries the handle, and relay activation asks
    // the state for it instead of making a second one: a replacement through
    // one reader shows on the others, the reload source rides along, and the
    // value activation read moves the handle forward when the file moved.
    let root = tempfile::TempDir::new().unwrap();
    let dir = root.path().join("beta");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("tagma.id"), "id-1").unwrap();
    std::fs::write(dir.join("tagma.token"), "old-token").unwrap();
    let entries = [RelayEntry {
        name: "beta".to_owned(),
        polis_url: "http://api.other.test".to_owned(),
        enrollment_code: None,
    }];
    let mut inner = match std::sync::Arc::try_unwrap(crate::test_helpers::make_state()) {
        Ok(inner) => inner,
        Err(_) => panic!("make_state hands out the sole reference"),
    };
    inner.platforms = std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(platform_faces(
        &entries,
        root.path(),
    )));
    let state = std::sync::Arc::new(inner);
    // Relay activation asks the state for the handle and moves it forward:
    // the file was rewritten after the boot snapshot, so the activation read
    // wins over the value the face took.
    let relay = relay_credential(&state, "beta", &dir, "activation-token".to_owned());
    assert_eq!(relay.current(), "activation-token");
    relay.clone().replace("swapped-token".to_owned());
    assert_eq!(
        state.files_access()[0].credential.current(),
        "swapped-token"
    );
    std::fs::write(dir.join("tagma.token"), "disk-token").unwrap();
    state.files_access()[0].credential.refresh();
    assert_eq!(state.files_access()[0].credential.current(), "disk-token");
}
#[test]
fn boot_refuses_to_start_without_a_slug() {
    temp_env::with_vars_unset(["KALLIPAI_TAGMA_SLUG"], || {
        let err = boot_identity().unwrap_err();
        assert!(
            err.to_string().contains("KALLIPAI_TAGMA_SLUG is not set"),
            "{err:#}"
        );
    });
}

#[test]
fn boot_refuses_an_invalid_slug() {
    temp_env::with_vars([("KALLIPAI_TAGMA_SLUG", Some("Bad_Slug"))], || {
        let err = boot_identity().unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err:#}");
    });
}

#[test]
#[serial]
fn boot_derives_the_roots_from_home_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_str().unwrap().to_owned();
    temp_env::with_vars(
        [
            ("KALLIPAI_TAGMA_SLUG", Some("e2e")),
            ("KALLIPAI_TAGMA_DATA_DIR", None),
            ("KALLIPAI_TAGMA_CONFIG_DIR", None),
            ("KALLIPAI_TAGMA_STATE_DIR", None),
            ("XDG_DATA_HOME", None),
            ("XDG_CONFIG_HOME", None),
            ("XDG_STATE_HOME", None),
            ("HOME", Some(home.as_str())),
        ],
        || {
            let roots = instance_roots_from_env().unwrap();
            assert_eq!(
                roots.data,
                tmp.path()
                    .join(".local/share")
                    .join("kallipai")
                    .join("tagmata")
                    .join("e2e")
            );
            assert_eq!(
                roots.config,
                tmp.path()
                    .join(".config")
                    .join("kallipai")
                    .join("tagmata")
                    .join("e2e")
            );
            assert_eq!(
                roots.state,
                tmp.path().join(".local/state").join("kallipai")
            );
        },
    );
}

#[test]
#[serial]
fn boot_derives_the_roots_from_xdg_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_str().unwrap().to_owned();
    temp_env::with_vars(
        [
            ("KALLIPAI_TAGMA_SLUG", Some("e2e")),
            ("KALLIPAI_TAGMA_DATA_DIR", None),
            ("KALLIPAI_TAGMA_CONFIG_DIR", None),
            ("KALLIPAI_TAGMA_STATE_DIR", None),
            ("XDG_DATA_HOME", Some(base.as_str())),
            ("XDG_CONFIG_HOME", Some(base.as_str())),
            ("XDG_STATE_HOME", Some(base.as_str())),
            ("HOME", None),
        ],
        || {
            let roots = instance_roots_from_env().unwrap();
            let leaf = tmp.path().join("kallipai").join("tagmata").join("e2e");
            assert_eq!(roots.data, leaf);
            assert_eq!(roots.config, leaf);
            assert_eq!(roots.state, tmp.path().join("kallipai"));
        },
    );
}

#[test]
#[serial]
fn boot_lets_an_own_key_win_over_the_standard_variables() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_str().unwrap().to_owned();
    let data = tmp.path().join("own-data");
    let config = tmp.path().join("own-config");
    let state = tmp.path().join("own-state");
    temp_env::with_vars(
        [
            ("KALLIPAI_TAGMA_SLUG", Some("e2e")),
            ("KALLIPAI_TAGMA_DATA_DIR", Some(data.to_str().unwrap())),
            ("KALLIPAI_TAGMA_CONFIG_DIR", Some(config.to_str().unwrap())),
            ("KALLIPAI_TAGMA_STATE_DIR", Some(state.to_str().unwrap())),
            ("XDG_DATA_HOME", Some(base.as_str())),
            ("XDG_CONFIG_HOME", Some(base.as_str())),
            ("XDG_STATE_HOME", Some(base.as_str())),
            ("HOME", Some(base.as_str())),
        ],
        || {
            let roots = instance_roots_from_env().unwrap();
            assert_eq!(roots.data, data);
            assert_eq!(roots.config, config);
            assert_eq!(roots.state, state);
        },
    );
}

#[test]
#[serial]
fn boot_treats_an_empty_own_key_as_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_str().unwrap().to_owned();
    temp_env::with_vars(
        [
            ("KALLIPAI_TAGMA_SLUG", Some("e2e")),
            ("KALLIPAI_TAGMA_CONFIG_DIR", Some("")),
            ("XDG_CONFIG_HOME", Some(base.as_str())),
        ],
        || {
            let roots = instance_roots_from_env().unwrap();
            assert_eq!(
                roots.config,
                tmp.path().join("kallipai").join("tagmata").join("e2e")
            );
        },
    );
}

#[test]
#[serial]
fn boot_fails_fast_when_no_level_names_a_root() {
    temp_env::with_vars(
        [
            ("KALLIPAI_TAGMA_SLUG", Some("e2e")),
            ("KALLIPAI_TAGMA_DATA_DIR", None),
            ("XDG_DATA_HOME", None),
            ("HOME", None),
        ],
        || {
            let err = instance_roots_from_env().unwrap_err();
            let text = format!("{err:#}");
            for key in ["KALLIPAI_TAGMA_DATA_DIR", "XDG_DATA_HOME", "HOME"] {
                assert!(text.contains(key), "{key} missing from {text}");
            }
        },
    );
}

#[test]
#[serial]
fn logs_land_in_the_state_tree_under_the_slug() {
    // Install the process roots (like every state-backed test): log
    // placement resolves the state root through the injection and appends
    // the slug leaf itself.
    crate::test_helpers::ensure_test_data_dir();
    temp_env::with_vars([("KALLIPAI_TAGMA_SLUG", Some("e2e"))], || {
        let state_root = kallipai_adk::persistence::state_dir_root().unwrap();
        assert_eq!(
            logs_target().unwrap(),
            state_root.join("tagmata").join("e2e").join("logs")
        );
    });
}

#[test]
fn logs_are_unplaceable_without_a_slug() {
    temp_env::with_vars_unset(["KALLIPAI_TAGMA_SLUG"], || {
        assert!(logs_target().is_err(), "no slug, no log placement");
    });
}

fn stored(id: &str, origin: Option<&str>) -> credentials::StoredTagma {
    credentials::StoredTagma {
        id: id.to_string(),
        token: "token".to_string(),
        archeion_url: origin.map(str::to_string),
    }
}

/// Stored credentials and no code: the stored identity is reused.
#[test]
fn stored_credentials_are_reused_without_a_code() {
    let entry = resolve_enroll_entry(
        Some(&stored("tagma-test", Some("https://archeion.example.com"))),
        "https://archeion.example.com",
        None,
    )
    .expect("stored entry resolves");
    assert_eq!(entry, EnrollEntry::Stored);
}

/// The forwarding hook surfaces a caught panic as a `panic` error
/// event. Captured through an in-memory writer so the test has real
/// discriminating power: if the forward regresses to the default
/// stderr-only path, the buffer stays empty and the assert fires.
#[test]
#[serial]
fn panic_forward_writes_error_event() {
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::prelude::*;

    struct SharedBuf(Arc<Mutex<Vec<u8>>>);
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
        type Writer = MutexGuardWriter<'a>;
        fn make_writer(&'a self) -> Self::Writer {
            MutexGuardWriter(self.0.lock().unwrap())
        }
    }
    struct MutexGuardWriter<'a>(std::sync::MutexGuard<'a, Vec<u8>>);
    impl std::io::Write for MutexGuardWriter<'_> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.0.flush()
        }
    }

    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(SharedBuf(shared.clone()))
            .with_filter(tracing_subscriber::EnvFilter::new("error")),
    );
    let _guard = tracing::subscriber::set_default(subscriber);
    // The forward hook is process-global, so this block owns it: a hook
    // that outlived its test would swallow the panic output of every test
    // that runs after it. The guard drops back the recording sentinel
    // installed below, not the harness hook that sentinel wrapped: the
    // sentinel stays installed for the rest of the process, because taking
    // the hook out leaves no path back to the hook itself. The sentinel
    // delegates to that hook, and the restore is checked by identity below.
    // The hook type `std::panic::set_hook` takes.
    type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo) + Sync + Send + 'static>;

    struct HookGuard(Option<PanicHook>);
    impl Drop for HookGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.0.take() {
                std::panic::set_hook(previous);
            }
        }
    }

    // A recording sentinel stands in for the harness hook, so the restore
    // is checked by identity: the panic raised after the drop must reach
    // this sentinel instead of the forwarding hook. It delegates to the
    // hook it replaced, which is where the harness output goes from there
    // on. The sentinel outlives this test and counts process-wide, so it
    // counts only raises on this thread: another test's off-thread panic is
    // not one of this test's raises.
    let previous = std::panic::take_hook();
    let sentinel_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits = sentinel_hits.clone();
    let test_thread = std::thread::current().id();
    std::panic::set_hook(Box::new(move |info: &std::panic::PanicHookInfo| {
        if std::thread::current().id() == test_thread {
            hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        previous(info);
    }));
    let restore_hook = HookGuard(Some(std::panic::take_hook()));

    std::panic::set_hook(Box::new(forward_panic_to_tracing));
    let _ = std::panic::catch_unwind(|| panic!("hook probe"));

    let captured = String::from_utf8(shared.lock().unwrap().clone()).expect("log bytes are utf8");
    // A failing assert in this test can arrive with no reason at all: while
    // the forwarding hook is installed - the state a broken restore leaves
    // behind - the panic goes to the tracing buffer instead of stderr. Say
    // it on stderr before the assert fires; passing runs print nothing. The
    // write goes straight to stderr rather than through `eprintln!`, whose
    // line a process abort drops with libtest's capture buffer unflushed.
    let report = |what: String| {
        use std::io::Write as _;
        let _ = writeln!(
            std::io::stderr(),
            "panic_forward_writes_error_event: {what}"
        );
    };
    if !captured.contains("panic") {
        report(format!("no panic event in the buffer: {captured}"));
    }
    assert!(
        captured.contains("panic"),
        "missing panic event: {captured}"
    );
    if !captured.contains("hook probe") {
        report(format!("the payload never reached the buffer: {captured}"));
    }
    assert!(captured.contains("hook probe"), "payload lost: {captured}");

    // Restoring is part of the contract, not aftercare: after the drop a
    // fresh panic must reach the sentinel standing in for the hook
    // installed before this test, and must leave this test's writer alone.
    drop(restore_hook);
    let _ = std::panic::catch_unwind(|| panic!("after restore"));
    // The count is thread-scoped, so the raise above - this thread's - is
    // the only one the sentinel may have counted by now. Read it here, so a
    // filter that counts the other side fails on the spot.
    let restored_count = sentinel_hits.load(std::sync::atomic::Ordering::SeqCst);
    if restored_count != 1 {
        report(format!(
            "the sentinel counted {restored_count} raises after the restore, not one"
        ));
    }
    assert_eq!(
        restored_count, 1,
        "the raise after the restore did not count once on the installing thread"
    );
    // The raise below is this test's own, on another thread: the sentinel
    // outlives this test, so another thread's raise must leave the count
    // alone. It is not a failure. The join reports whether the raise happened,
    // so a count left alone cannot pass for a probe that never raised.
    let probe_raised = std::thread::spawn(|| panic!("sibling thread probe"))
        .join()
        .is_err();
    let after = String::from_utf8(shared.lock().unwrap().clone()).expect("log bytes are utf8");
    let sentinel_count = sentinel_hits.load(std::sync::atomic::Ordering::SeqCst);
    if sentinel_count != 1 {
        report(format!(
            "the sentinel counted {sentinel_count} raises, not one"
        ));
    }
    if after != captured {
        report(format!("the forward hook outlived the test: {after}"));
    }
    assert_eq!(
        sentinel_count, 1,
        "an off-thread raise reached the sentinel standing in for the harness hook"
    );
    if !probe_raised {
        report("the sibling probe never raised".to_string());
    }
    assert!(
        probe_raised,
        "the sibling probe raised nothing to be ignored"
    );
    assert_eq!(after, captured, "the forward hook outlived the test");
}

/// The env switch accepts 1/true case-insensitively and nothing else:
/// unset, empty, 0, yes, or padded values all keep the file default.
#[test]
fn log_to_stderr_accepts_only_1_and_true() {
    assert!(parse_log_to_stderr(Some("1")));
    assert!(parse_log_to_stderr(Some("true")));
    assert!(parse_log_to_stderr(Some("TRUE")));
    assert!(parse_log_to_stderr(Some("True")));
    assert!(!parse_log_to_stderr(None));
    assert!(!parse_log_to_stderr(Some("")));
    assert!(!parse_log_to_stderr(Some("0")));
    assert!(!parse_log_to_stderr(Some("yes")));
    assert!(!parse_log_to_stderr(Some("1 ")));
}

/// Derive covers the documented shapes: loopback passes through,
/// an unspecified v4/v6 host becomes 127.0.0.1, other IPv6 keeps brackets.
#[test]
fn derive_advertise_url_shapes() {
    assert_eq!(
        derive_advertise_url("127.0.0.1:7301", 7301).unwrap(),
        "http://127.0.0.1:7301"
    );
    assert_eq!(
        derive_advertise_url("0.0.0.0:9999", 9999).unwrap(),
        "http://127.0.0.1:9999"
    );
    assert_eq!(
        derive_advertise_url("[::1]:5555", 5555).unwrap(),
        "http://[::1]:5555"
    );
    assert_eq!(
        derive_advertise_url("[::]:5555", 5555).unwrap(),
        "http://127.0.0.1:5555"
    );
}
/// No stored credentials and a code: first-run enrollment.
#[test]
fn fresh_code_enrolls_when_no_credentials_stored() {
    let entry = resolve_enroll_entry(None, "https://archeion.example.com", Some("sk-enroll-test"))
        .expect("fresh entry resolves");
    assert_eq!(
        entry,
        EnrollEntry::Fresh {
            code: "sk-enroll-test".to_string(),
            replace: false,
        }
    );
}

/// Stored credentials + code at the same archeion — the stale
/// false-alarm shape — ignores the code instead of failing the boot.
/// The comparison is normalized origin, so trailing-slash and
/// explicit-default-port spellings of the same server still match.
#[test]
fn same_archeion_code_is_ignored_with_warning_entry() {
    for (recorded, configured) in [
        (
            "https://archeion.example.com",
            "https://archeion.example.com",
        ),
        (
            "https://archeion.example.com/",
            "https://archeion.example.com",
        ),
        (
            "https://archeion.example.com:443",
            "https://archeion.example.com",
        ),
    ] {
        let entry = resolve_enroll_entry(
            Some(&stored("tagma-test", Some(recorded))),
            configured,
            Some("sk-spent"),
        )
        .expect("same-archeion entry resolves");
        assert_eq!(
            entry,
            EnrollEntry::StoredIgnoringCode {
                address_recorded: true
            },
            "recorded {recorded}, configured {configured}"
        );
    }
}

/// An unrecorded origin (credential predates origin recording) or an
/// unparsable one on either side counts as unknown and takes the
/// same-archeion path: origin data is hygiene and must not brick a boot.
#[test]
fn unknown_origin_takes_the_same_archeion_path() {
    for (recorded, configured) in [
        (None, "https://archeion.example.com"),
        (Some("not a url"), "https://archeion.example.com"),
        (Some("https://archeion.example.com"), "not a url"),
    ] {
        let entry = resolve_enroll_entry(
            Some(&stored("tagma-test", recorded)),
            configured,
            Some("sk-spent"),
        )
        .expect("unknown-origin entry resolves");
        assert_eq!(
            entry,
            EnrollEntry::StoredIgnoringCode {
                address_recorded: false
            },
            "recorded {recorded:?}, configured {configured}"
        );
    }
}

/// Stored credentials enrolled at a different platform origin: the
/// configured origin wins and the token is kept (the rename
/// migration), with and without a code. Both spell the reorigin.
#[test]
fn different_origin_reorigins_instead_of_failing() {
    for (code, had_code) in [(Some("sk-enroll-test"), true), (None, false)] {
        let entry = resolve_enroll_entry(
            Some(&stored("tagma-test", Some("https://old.example.com"))),
            "https://new.example.com",
            code,
        )
        .expect("reorigin entry resolves");
        assert_eq!(entry, EnrollEntry::StoredReorigin { had_code });
    }
}

/// Neither credentials nor code: the message names the URL env var and
/// both exits, distinct from the conflict message.
#[test]
fn neither_credentials_nor_code_fails_fast() {
    let err = resolve_enroll_entry(None, "https://archeion.example.com", None)
        .expect_err("incomplete must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("KALLIPAI_POLIS_URL"), "{msg}");
    assert!(msg.contains("first-run enrollment"), "{msg}");
    assert!(msg.contains("local-only"), "{msg}");
    // Mirror of the conflict test: no conflict markers.
    assert!(!msg.contains("ignored"), "{msg}");
    assert!(!msg.contains("re-enroll"), "{msg}");
}

/// A stored-but-blank pair is not a stored pair: the loader reports nothing,
/// so a boot that also carries a code takes the fresh-enrollment path — the
/// same answer the daemon's scan, adopt probe and replay filter give.
#[test]
fn blank_stored_credentials_take_the_fresh_path() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("tagma.id"), " \n").unwrap();
    std::fs::write(dir.path().join("tagma.token"), "\n").unwrap();
    let stored = credentials::load_tagma(dir.path());
    assert!(stored.is_none(), "a blank pair is not an enrollment");
    let plan = resolve_enroll_entry(
        stored.as_ref(),
        "https://archeion.example.com",
        Some("sk-code"),
    )
    .expect("a code with no stored pair enrolls fresh");
    assert!(matches!(plan, EnrollEntry::Fresh { code, replace: false } if code == "sk-code"));
}

/// A blank pair with no code lands in the fail-fast branch, not a third
/// state: the message is the one a completely empty data directory produces.
#[test]
fn blank_credentials_without_a_code_fail_fast() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("tagma.id"), "").unwrap();
    std::fs::write(dir.path().join("tagma.token"), "  ").unwrap();
    let stored = credentials::load_tagma(dir.path());
    assert!(stored.is_none(), "a blank pair is not an enrollment");
    let error = resolve_enroll_entry(stored.as_ref(), "https://archeion.example.com", None)
        .expect_err("nothing to reuse and nothing to enroll with");
    let msg = format!("{error:#}");
    assert!(msg.contains("first-run enrollment"), "{msg}");
    assert!(msg.contains("local-only"), "{msg}");
}

/// The ignore warning names the env var, the stored identity, and the
/// delete-to-re-enroll recovery; the unknown-origin spelling says so.
#[test]
fn ignored_code_warning_names_the_recovery() {
    let recorded = ignored_code_warning(std::path::Path::new("/tmp/credentials"), true);
    assert!(
        recorded.contains("KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE"),
        "{recorded}"
    );
    assert!(recorded.contains("ignored"), "{recorded}");
    assert!(recorded.contains("stored identity"), "{recorded}");
    assert!(
        recorded.contains("delete the credentials directory"),
        "{recorded}"
    );
    assert!(!recorded.contains("before origin recording"), "{recorded}");

    let unknown = ignored_code_warning(std::path::Path::new("/tmp/credentials"), false);
    assert!(unknown.contains("before origin recording"), "{unknown}");
    assert!(unknown.contains("backfilled"), "{unknown}");
}

/// save/load roundtrip carries the enrollment origin; a pre-origin
/// credential (id + token only) loads with the origin absent; the
/// backfill writes the configured origin when absent and rebinds
/// (overwrites) it when the recorded origin disagrees.
#[test]
fn credential_origin_roundtrip_and_backfill() {
    let dir = tempfile::tempdir().expect("credentials tempdir");
    credentials::save_tagma(
        dir.path(),
        "tagma-1",
        "token",
        "https://archeion.example.com",
    )
    .expect("roundtrip save");
    let reloaded = credentials::load_tagma(dir.path()).expect("roundtrip loads");
    assert_eq!(reloaded.id, "tagma-1");
    assert_eq!(reloaded.token, "token");
    assert_eq!(
        reloaded.archeion_url.as_deref(),
        Some("https://archeion.example.com")
    );

    let legacy = tempfile::tempdir().expect("legacy tempdir");
    std::fs::write(legacy.path().join("tagma.id"), "tagma-2").expect("write id");
    std::fs::write(legacy.path().join("tagma.token"), "token").expect("write token");
    let legacy_stored = credentials::load_tagma(legacy.path()).expect("legacy loads");
    assert_eq!(legacy_stored.archeion_url, None);

    credentials::backfill_archeion_url(legacy.path(), "https://archeion.example.com")
        .expect("backfill writes the origin");
    assert_eq!(
        std::fs::read_to_string(legacy.path().join("archeion.url")).expect("backfilled"),
        "https://archeion.example.com"
    );
    credentials::backfill_archeion_url(legacy.path(), "https://other.example.com")
        .expect("backfill rebinds the origin");
    assert_eq!(
        std::fs::read_to_string(legacy.path().join("archeion.url")).expect("rebound"),
        "https://other.example.com"
    );
}

/// write_instance_state contract: handed a state dir, it writes
/// exactly one owner-only `runtime.json` carrying the process pid,
/// the actually bound (`:0`) port and the session the spawn put it in.
#[tokio::test]
async fn instance_state_dir_gets_runtime_json_owner_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral");
    let port = listener.local_addr().expect("local addr").port();

    write_instance_state(dir.path(), &listener).expect("write instance state");

    let text = std::fs::read_to_string(dir.path().join("runtime.json")).expect("runtime.json");
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("parse runtime.json");
    assert_eq!(parsed["pid"], serde_json::json!(std::process::id()));
    // The spawn makes the session leader an intermediate child, so the session
    // id is not this process's pid -- assert it against the kernel, not against
    // `pid`, or the test would bless exactly the mistake the leak check made.
    let sid = unsafe { libc::getsid(0) };
    assert_eq!(parsed["sid"], serde_json::json!(sid));
    assert_eq!(parsed["port"], serde_json::json!(port));

    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(dir.path().join("runtime.json"))
        .expect("runtime.json metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "runtime.json must be owner-only");
    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read state dir")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(entries.len(), 1, "exactly runtime.json, no tmp leftovers");
}

/// The identity rule `build_info` and the trace resource read, on disk: a
/// host whose first configured entry is not enrolled yet while a later one is
/// must name the tagma the boot actually serves, not report nothing.
#[test]
fn the_stored_id_follows_the_first_entry_that_has_credentials() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("credentials");
    std::fs::create_dir_all(root.join("second")).unwrap();
    credentials::save_tagma(
        &root.join("second"),
        "tagma-b",
        "token-b",
        "https://edge.example",
    )
    .unwrap();
    let entries = vec![
        RelayEntry {
            name: "first".to_owned(),
            polis_url: "https://edge.example".to_owned(),
            enrollment_code: None,
        },
        RelayEntry {
            name: "second".to_owned(),
            polis_url: "https://edge.example".to_owned(),
            enrollment_code: None,
        },
    ];
    assert_eq!(stored_id_under(&root, &entries).as_deref(), Some("tagma-b"));
    assert_eq!(
        stored_token_under(&root, &entries).as_deref(),
        Some("token-b"),
        "the token comes from the same entry as the id"
    );
}

/// An instance with no credentials still names itself in the trace resource,
/// with the same stand-in `build_info` reports: enrollment later changes a
/// value, not the shape of what a collector receives.
#[test]
fn the_trace_resource_names_an_unenrolled_instance() {
    let unenrolled = telemetry_attrs(None);
    assert_eq!(unenrolled[0], ("tagma.id", UNENROLLED_TAGMA_ID));
    assert_eq!(unenrolled[1], ("service.instance.id", UNENROLLED_TAGMA_ID));
    assert_eq!(
        unenrolled[2],
        ("service.version", env!("CARGO_PKG_VERSION"))
    );

    let enrolled = telemetry_attrs(Some("tagma-b"));
    assert_eq!(enrolled[0], ("tagma.id", "tagma-b"));
    assert_eq!(enrolled[1], ("service.instance.id", "tagma-b"));
    assert_eq!(enrolled[2], ("service.version", env!("CARGO_PKG_VERSION")));
}

/// A collector on loopback that accepts whatever an export sends.
async fn trace_collector() -> wiremock::MockServer {
    let collector = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/traces"))
        .respond_with(wiremock::ResponseTemplate::new(200))
        .mount(&collector)
        .await;
    collector
}

fn collector_config(collector: &wiremock::MockServer) -> kallipai_common::telemetry::Config {
    kallipai_common::telemetry::config_from_lookup(|name| {
        (name == kallipai_common::telemetry::ENDPOINT_VAR).then(|| collector.uri())
    })
    .expect("the collector's endpoint is the configuration")
}

/// One probe span through the real OTLP/HTTP client, flushed: what the
/// collector received is then what the layer attached.
fn export_one_span(config: &kallipai_common::telemetry::Config, token: Option<&str>) {
    use tracing_subscriber::prelude::*;

    let (layer, mut guard) =
        kallipai_common::telemetry::export_layer_authorized(Some(config), "tagma", &[], token);
    {
        let subscriber = tracing_subscriber::registry().with(layer);
        let _default = tracing::subscriber::set_default(subscriber);
        tracing::info_span!(target: kallipai_common::SPAN_TARGET, "probe").in_scope(|| {});
    }
    assert!(guard.shutdown(), "a configured guard owns a provider");
}

/// The credential the export carries is the one stored on disk: the boot's
/// token walk and the header meet at one value, so a re-enrollment that moved
/// the entry cannot leave a trace arriving under another instance's key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_stored_token_authenticates_the_trace_export() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("credentials");
    std::fs::create_dir_all(&root).unwrap();
    credentials::save_tagma(&root, "tagma-x", "token-from-disk", "https://edge.example").unwrap();
    let token = stored_token_under(&root, &[]);
    assert_eq!(
        token.as_deref(),
        Some("token-from-disk"),
        "the token half is read from the credential file, not the id beside it"
    );

    let collector = trace_collector().await;
    export_one_span(&collector_config(&collector), token.as_deref());

    let requests = collector.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "one probe, one export");
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer token-from-disk"),
        "the intake sees the fixture's token, not another file's value"
    );
}

/// A first boot has no stored credential, and the export carries no header
/// rather than borrowing whatever the environment happens to hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unenrolled_instance_exports_without_a_credential() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("credentials");
    let token = stored_token_under(&root, &[]);
    assert_eq!(token, None, "no stored entry, no token");

    let collector = trace_collector().await;
    export_one_span(&collector_config(&collector), token.as_deref());

    let requests = collector.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        1,
        "the export still leaves, it just says nothing new"
    );
    assert!(
        requests[0].headers.get("authorization").is_none(),
        "an instance with no credential sends no authorization header"
    );
}

/// The upgrade path: the enrollment is still at the root of the credentials
/// directory because the layout migration has not run, and the boot that
/// follows still has to report the id it serves.
#[test]
fn the_flat_layout_is_read_before_the_migration_runs() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("credentials");
    std::fs::create_dir_all(&root).unwrap();
    credentials::save_tagma(&root, "tagma-flat", "token", "https://edge.example").unwrap();
    assert_eq!(stored_id_under(&root, &[]).as_deref(), Some("tagma-flat"));
    assert_eq!(
        stored_token_under(&root, &[]).as_deref(),
        Some("token"),
        "the flat layout carries the pair, not the id alone"
    );
}

#[test]
fn no_credentials_anywhere_reads_as_unenrolled() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("credentials");
    assert_eq!(stored_id_under(&root, &[]), None);
}
