use super::*;
use serial_test::serial;

#[test]
fn polis_toml_rejects_the_relay_table_name() {
    // Rename guard: a file kept from the old relays.toml shape
    // must fail loudly naming the rename point, not parse as an
    // empty config (deny_unknown_fields at both levels).
    let err = toml::from_str::<PolisFile>(
        "[[relay]]\nname = \"main\"\nurl = \"https://api.example.com\"\n",
    )
    .err()
    .expect("the old table name must be rejected");
    assert!(format!("{err}").contains("unknown field `relay`"), "{err}");
}

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
        token: token.map(|t| t.to_owned()),
    };
    inner.platforms = std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(vec![
        face("alpha", None),
        face("blank", Some("   ")),
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
    assert_eq!(access[0].token, "b-token");
    assert_eq!(access[1].name, "gamma");
    assert_eq!(access[1].origin, "http://gamma");
    assert_eq!(access[1].token, "g-token");
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
    assert_eq!(params.token, "token-1");
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
    assert_eq!(params.token, "token-1");
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
fn boot_derives_the_instance_roots_from_the_slug() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_str().unwrap().to_owned();
    temp_env::with_vars(
        [
            ("KALLIPAI_TAGMA_SLUG", Some("e2e")),
            ("KALLIPAI_TAGMA_DATA_DIR", None),
            ("XDG_DATA_HOME", Some(home.as_str())),
            ("XDG_CONFIG_HOME", Some(home.as_str())),
            ("XDG_STATE_HOME", Some(home.as_str())),
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
fn boot_honors_the_data_dir_override() {
    let tmp = tempfile::tempdir().unwrap();
    let override_dir = tmp.path().join("override");
    temp_env::with_vars(
        [
            ("KALLIPAI_TAGMA_SLUG", Some("e2e")),
            (
                "KALLIPAI_TAGMA_DATA_DIR",
                Some(override_dir.to_str().unwrap()),
            ),
        ],
        || {
            let roots = instance_roots_from_env().unwrap();
            assert_eq!(roots.data, override_dir);
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
            code: "sk-enroll-test".to_string()
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
    assert!(matches!(plan, EnrollEntry::Fresh { code } if code == "sk-code"));
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
/// exactly one owner-only `runtime.json` carrying the process pid
/// and the actually bound (`:0`) port.
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
