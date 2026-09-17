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
fn files_token_scan_precedes_restore_agents() {
    // Source-order pin: restore re-assembly fetches record bytes
    // through the boot-time token slot, so a boot that restored
    // before filling the slot would 503 every files fallback.
    // Swapping the two regions (or dropping the scan) must fail
    // here.
    let src = include_str!("main.rs");
    let scan = src
        .find("let files_token = resolve_files_token(&args)")
        .expect("token scan call site present");
    let restore = src
        .find("lifecycle::restore_agents(&state)")
        .expect("restore call site present");
    assert!(scan < restore);
}

#[test]
fn sweep_removes_a_legacy_files_token_exactly_once() {
    let prior = std::env::var_os("KALLIP_FILES_TOKEN");
    // SAFETY: test-only env edit; this test is the only reader and
    // writer of KALLIP_FILES_TOKEN in the suite (the same accepted
    // race shape as the test_helpers data-dir bootstrapping).
    unsafe {
        std::env::set_var("KALLIP_FILES_TOKEN", "legacy-token");
    }
    assert!(sweep_legacy_files_token());
    assert!(std::env::var_os("KALLIP_FILES_TOKEN").is_none());
    // Idempotent: a clean environment sweeps nothing.
    assert!(!sweep_legacy_files_token());
    // SAFETY: restore-the-prior-value counterpart of the edit above.
    unsafe {
        match prior {
            Some(value) => std::env::set_var("KALLIP_FILES_TOKEN", value),
            None => std::env::remove_var("KALLIP_FILES_TOKEN"),
        }
    }
}

#[test]
fn files_token_follows_the_primary_entry_scan() {
    let root = tempfile::tempdir().unwrap();
    let entry = |name: &str| RelayEntry {
        name: name.to_owned(),
        polis_url: "http://archeion".to_owned(),
        enrollment_code: None,
    };
    let entries = [entry("alpha"), entry("beta")];
    // No stored credentials anywhere: no token.
    assert_eq!(files_token_from_entries(&entries, root.path()), None);
    // The first entry in config order holding stored credentials
    // wins.
    std::fs::create_dir_all(root.path().join("beta")).unwrap();
    credentials::save_tagma(
        &root.path().join("beta"),
        "b-id",
        "b-token",
        "http://archeion",
    );
    assert_eq!(
        files_token_from_entries(&entries, root.path()).as_deref(),
        Some("b-token")
    );
    std::fs::create_dir_all(root.path().join("alpha")).unwrap();
    credentials::save_tagma(
        &root.path().join("alpha"),
        "a-id",
        "a-token",
        "http://archeion",
    );
    assert_eq!(
        files_token_from_entries(&entries, root.path()).as_deref(),
        Some("a-token")
    );
}

#[test]
fn boot_refuses_to_start_without_a_slug() {
    temp_env::with_vars_unset(["KALLIP_TAGMA_SLUG"], || {
        let err = boot_identity().unwrap_err();
        assert!(
            err.to_string().contains("KALLIP_TAGMA_SLUG is not set"),
            "{err:#}"
        );
    });
}

#[test]
fn boot_refuses_an_invalid_slug() {
    temp_env::with_vars([("KALLIP_TAGMA_SLUG", Some("Bad_Slug"))], || {
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
            ("KALLIP_TAGMA_SLUG", Some("e2e")),
            ("KALLIP_TAGMA_DATA_DIR", None),
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
            ("KALLIP_TAGMA_SLUG", Some("e2e")),
            (
                "KALLIP_TAGMA_DATA_DIR",
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
    temp_env::with_vars([("KALLIP_TAGMA_SLUG", Some("e2e"))], || {
        let state_root = kallip_runtime::persistence::state_dir_root().unwrap();
        assert_eq!(
            logs_target().unwrap(),
            state_root.join("tagmata").join("e2e").join("logs")
        );
    });
}

#[test]
fn logs_are_unplaceable_without_a_slug() {
    temp_env::with_vars_unset(["KALLIP_TAGMA_SLUG"], || {
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

    std::panic::set_hook(Box::new(forward_panic_to_tracing));
    let _ = std::panic::catch_unwind(|| panic!("hook probe"));

    let captured = String::from_utf8(shared.lock().unwrap().clone()).expect("log bytes are utf8");
    assert!(
        captured.contains("panic"),
        "missing panic event: {captured}"
    );
    assert!(captured.contains("hook probe"), "payload lost: {captured}");
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
    assert!(msg.contains("KALLIP_POLIS_URL"), "{msg}");
    assert!(msg.contains("first-run enrollment"), "{msg}");
    assert!(msg.contains("local-only"), "{msg}");
    // Mirror of the conflict test: no conflict markers.
    assert!(!msg.contains("ignored"), "{msg}");
    assert!(!msg.contains("re-enroll"), "{msg}");
}

/// The ignore warning names the env var, the stored identity, and the
/// delete-to-re-enroll recovery; the unknown-origin spelling says so.
#[test]
fn ignored_code_warning_names_the_recovery() {
    let recorded = ignored_code_warning(std::path::Path::new("/tmp/credentials"), true);
    assert!(
        recorded.contains("KALLIP_TAGMA_RELAY_ENROLLMENT_CODE"),
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
    );
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

    credentials::backfill_archeion_url(legacy.path(), "https://archeion.example.com");
    assert_eq!(
        std::fs::read_to_string(legacy.path().join("archeion.url")).expect("backfilled"),
        "https://archeion.example.com"
    );
    credentials::backfill_archeion_url(legacy.path(), "https://other.example.com");
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
