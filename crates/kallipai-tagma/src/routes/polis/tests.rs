//! Route-level tests for `/polis`: a temp data root (process-pinned by
//! `ensure_test_data_dir`), a stub platform on loopback, and the real handlers.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::extract::{Query, State};
use serde_json::{Value, json};
use serial_test::serial;

use super::{AddPolisBody, RemovePolisQuery};
use crate::auth::AuthIdentity;
use crate::state::SharedState;

/// A stub platform: answers enrollment (or refuses it) and records the paths it
/// was asked for, so a test can assert a code never reached it.
struct Stub {
    origin: String,
    refuse: Arc<AtomicBool>,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Stub {
    fn enrollments(&self) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|path| path.ends_with("/tagmata/enroll"))
            .count()
    }
}

/// What the stub's enrollment handler reads from its state: whether to refuse,
/// and the paths it has answered.
type StubState = (Arc<AtomicBool>, Arc<std::sync::Mutex<Vec<String>>>);

async fn start_stub() -> Stub {
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let refuse = Arc::new(AtomicBool::new(false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let enroll_state = (refuse.clone(), seen.clone());
    let app = axum::Router::new()
        .route(
            "/v1/archeion/tagmata/enroll",
            axum::routing::post(|State((refuse, seen)): State<StubState>| async move {
                seen.lock().unwrap().push("/tagmata/enroll".to_string());
                if refuse.load(Ordering::Relaxed) {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        Json(json!({"error": "code expired"})),
                    );
                }
                (
                    axum::http::StatusCode::OK,
                    Json(json!({"tagma_id": "tagma-1", "tagma_token": "sk-tagma-x"})),
                )
            }),
        )
        .fallback(|uri: axum::http::Uri| async move {
            (
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({"error": format!("no {}", uri.path())})),
            )
        })
        .with_state(enroll_state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Stub {
        origin: format!("http://{addr}"),
        refuse,
        seen,
    }
}

/// The operator's view of the handlers.
fn operator() -> AuthIdentity {
    AuthIdentity::operator()
}

/// One test's data root: no leftover document, and none of the entry's
/// credentials. The data root itself is process-wide (`ensure_test_data_dir`),
/// so the tests run serialized.
fn reset(name: &str) {
    crate::test_helpers::ensure_test_data_dir();
    let _ = std::fs::remove_file(crate::polis_config::polis_path().unwrap());
    let root = crate::credentials_dir().unwrap();
    let _ = std::fs::remove_dir_all(root.join(name));
}

fn write_entry(name: &str, url: &str) {
    let path = crate::polis_config::polis_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut body = std::fs::read_to_string(&path).unwrap_or_default();
    body.push_str(&format!("[[polis]]\nname = \"{name}\"\nurl = \"{url}\"\n"));
    std::fs::write(path, body).unwrap();
}

fn store_credentials(name: &str, origin: &str) {
    let dir = crate::credentials_dir().unwrap().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("tagma.id"), "stored-id").unwrap();
    std::fs::write(dir.join("tagma.token"), "stored-token").unwrap();
    std::fs::write(dir.join("archeion.url"), origin).unwrap();
}

/// A state with a root agent (relay activation asks for it) and a projector
/// (a first-run enrollment claims its ids).
async fn state_with_root() -> (SharedState, tempfile::TempDir) {
    crate::test_helpers::ensure_test_data_dir();
    let state = crate::test_helpers::make_state();
    let id = kallipai_common::agentid::AgentId::random();
    {
        let mut registry = state.registry.write().await;
        crate::test_helpers::add_root(&mut registry, &id);
    }
    let (_, _, dir) = crate::test_helpers::install_projector(&state).await;
    (state, dir)
}

fn body(name: &str, url: &str, code: Option<&str>) -> AddPolisBody {
    AddPolisBody {
        name: name.to_string(),
        url: url.to_string(),
        code: code.map(str::to_owned),
    }
}

fn forget() -> Query<RemovePolisQuery> {
    Query(RemovePolisQuery { forget: true })
}

fn keep() -> Query<RemovePolisQuery> {
    Query(RemovePolisQuery { forget: false })
}

#[tokio::test]
#[serial]
async fn get_lists_entries_with_their_disk_state() {
    reset("alpha-list");
    reset("beta-list");
    let (state, _dir) = state_with_root().await;
    let stub = start_stub().await;
    write_entry("alpha-list", &stub.origin);
    write_entry("beta-list", &stub.origin);
    store_credentials("alpha-list", &stub.origin);

    let Json(body) = super::list_polis(State(state.clone()), operator())
        .await
        .unwrap();
    let rows = body["entries"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{body}");
    assert_eq!(rows[0]["name"], "alpha-list");
    assert_eq!(rows[0]["enrolled"], true);
    assert_eq!(rows[0]["tagma_id"], "stored-id");
    assert_eq!(rows[0]["relay"], false);
    assert_eq!(rows[0]["gateway"], false);
    assert_eq!(rows[1]["enrolled"], false);
    assert_eq!(rows[1]["tagma_id"], Value::Null);
}

#[tokio::test]
#[serial]
async fn post_with_a_code_enrolls_writes_the_file_and_starts_the_relay() {
    reset("alpha-add");
    let (state, _dir) = state_with_root().await;
    let stub = start_stub().await;

    let (status, Json(response)) = super::add_polis(
        State(state.clone()),
        operator(),
        Json(body("alpha-add", &stub.origin, Some("code-1"))),
    )
    .await
    .unwrap();

    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert_eq!(response["entry"]["enrolled"], true);
    assert_eq!(response["entry"]["tagma_id"], "tagma-1");
    assert!(
        response["note"].as_str().unwrap().contains("next start"),
        "{response}"
    );
    assert_eq!(stub.enrollments(), 1);
    let entries = crate::polis_config::read_entries().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].polis_url, stub.origin);
    assert!(state.relay("alpha-add").is_some(), "the relay is installed");
    let raw = std::fs::read_to_string(crate::polis_config::polis_path().unwrap()).unwrap();
    assert!(
        !raw.contains("code-1"),
        "the code is spent, not stored: {raw}"
    );
    let Json(listed) = super::list_polis(State(state.clone()), operator())
        .await
        .unwrap();
    assert_eq!(
        response["entry"], listed["entries"][0],
        "the response row is the file's row, not one assembled for the reply"
    );
    let _ = super::stop_relay(&state, "alpha-add").await;
}

#[tokio::test]
#[serial]
async fn post_without_a_code_or_stored_credentials_is_refused_and_writes_nothing() {
    reset("alpha-nocode");
    let (state, _dir) = state_with_root().await;
    let stub = start_stub().await;

    let error = super::add_polis(
        State(state.clone()),
        operator(),
        Json(body("alpha-nocode", &stub.origin, None)),
    )
    .await
    .unwrap_err();

    assert_eq!(error.status, 400, "{error:?}");
    assert!(
        error.message.contains("kallipctl polis token set"),
        "{error:?}"
    );
    assert!(!crate::polis_config::polis_path().unwrap().exists());
    assert_eq!(stub.enrollments(), 0);
}

#[tokio::test]
#[serial]
async fn post_without_a_code_reuses_stored_credentials_and_rebinds_the_origin() {
    reset("alpha-rebind");
    let (state, _dir) = state_with_root().await;
    let stub = start_stub().await;
    store_credentials("alpha-rebind", "http://old.example.test");

    let (status, Json(response)) = super::add_polis(
        State(state.clone()),
        operator(),
        Json(body("alpha-rebind", &stub.origin, None)),
    )
    .await
    .unwrap();

    assert_eq!(status, axum::http::StatusCode::CREATED, "{response}");
    assert_eq!(stub.enrollments(), 0, "the stored token is reused");
    let entries = crate::polis_config::read_entries().unwrap();
    assert_eq!(entries[0].polis_url, stub.origin);
    let stored =
        crate::credentials::load_tagma(&crate::credentials_dir().unwrap().join("alpha-rebind"))
            .unwrap();
    assert_eq!(stored.token, "stored-token");
    let _ = super::stop_relay(&state, "alpha-rebind").await;
}

#[tokio::test]
#[serial]
async fn post_with_a_code_for_an_enrolled_entry_is_refused_and_spends_no_code() {
    reset("alpha-double");
    let (state, _dir) = state_with_root().await;
    let stub = start_stub().await;
    write_entry("alpha-double", &stub.origin);
    store_credentials("alpha-double", &stub.origin);

    let error = super::add_polis(
        State(state.clone()),
        operator(),
        Json(body("alpha-double", &stub.origin, Some("code-2"))),
    )
    .await
    .unwrap_err();

    assert_eq!(error.status, 409, "{error:?}");
    assert!(error.message.contains("already enrolled"), "{error:?}");
    assert_eq!(stub.enrollments(), 0);
}

#[tokio::test]
#[serial]
async fn post_that_rebinds_the_pinned_entry_is_refused() {
    reset("alpha-pin");
    let (state, _dir) = state_with_root().await;
    let stub = start_stub().await;
    write_entry("alpha-pin", &stub.origin);
    store_credentials("alpha-pin", &stub.origin);
    crate::settings::save_profile_source_polis(Some(&stub.origin)).unwrap();

    let error = super::add_polis(
        State(state.clone()),
        operator(),
        Json(body("alpha-pin", "http://moved.example.test", None)),
    )
    .await
    .unwrap_err();
    crate::settings::save_profile_source_polis(None).unwrap();

    assert_eq!(error.status, 409, "{error:?}");
    assert!(error.message.contains("pinned"), "{error:?}");
}

#[tokio::test]
#[serial]
async fn post_against_an_unparsable_file_is_refused_with_its_bytes_untouched() {
    reset("alpha-broken");
    let (state, _dir) = state_with_root().await;
    let stub = start_stub().await;
    let path = crate::polis_config::polis_path().unwrap();
    let broken = "[[polis]]\nname = \"alpha-broken\"\n";
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, broken).unwrap();

    let error = super::add_polis(
        State(state.clone()),
        operator(),
        Json(body("alpha-broken", &stub.origin, Some("code-3"))),
    )
    .await
    .unwrap_err();

    assert_eq!(error.status, 500, "{error:?}");
    assert!(
        error.message.contains("could not be read") && error.message.contains("fix the file"),
        "{error:?}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
    assert_eq!(
        stub.enrollments(),
        0,
        "no code is spent on a file we cannot rewrite"
    );
}

#[tokio::test]
#[serial]
async fn post_when_enrollment_fails_writes_nothing_and_leaves_no_credentials() {
    reset("alpha-refused");
    let (state, _dir) = state_with_root().await;
    let stub = start_stub().await;
    stub.refuse.store(true, Ordering::Relaxed);

    let error = super::add_polis(
        State(state.clone()),
        operator(),
        Json(body("alpha-refused", &stub.origin, Some("code-4"))),
    )
    .await
    .unwrap_err();

    assert_eq!(error.status, 400, "{error:?}");
    assert!(
        error.message.contains("refused the enrollment code"),
        "{error:?}"
    );
    assert!(!crate::polis_config::polis_path().unwrap().exists());
    assert!(
        !crate::credentials_dir()
            .unwrap()
            .join("alpha-refused")
            .exists()
    );
    assert!(state.relay("alpha-refused").is_none());
}

/// A connector for a route test's relay slot (nothing on the other end).
fn relay_handle(state: &SharedState, name: &str, origin: &str) -> crate::relay::RelayHandle {
    let client = kallipai_lesche_client::LescheClient::builder(
        &kallipai_common::polis::service_base(origin, "lesche"),
        "tok",
    )
    .build()
    .expect("lesche client");
    crate::relay::RelayHandle::new(
        client,
        name.to_string(),
        kallipai_archeion_common::ids::TagmaId::from("t1".to_string()),
        "Tagma".into(),
        kallipai_e2ee::DeviceKey::generate(),
        kallipai_common::agentid::AgentId::random(),
        std::sync::Arc::downgrade(state),
    )
}

#[tokio::test]
#[serial]
async fn post_when_the_file_commit_fails_stops_the_relay_and_removes_fresh_credentials() {
    reset("alpha-rollback");
    let (state, _dir) = state_with_root().await;
    let stub = start_stub().await;
    // The writer stages `<file>.tmp<pid>` beside the document; a directory in
    // that place makes the staging write fail after the enrollment succeeded.
    let staged = crate::polis_config::polis_path()
        .unwrap()
        .with_file_name(format!("polis.toml.tmp{}", std::process::id()));
    std::fs::create_dir_all(&staged).unwrap();

    let error = super::add_polis(
        State(state.clone()),
        operator(),
        Json(body("alpha-rollback", &stub.origin, Some("code-5"))),
    )
    .await
    .unwrap_err();

    assert_eq!(error.status, 500, "{error:?}");
    assert_eq!(stub.enrollments(), 1, "the enrollment did happen");
    assert!(
        state.relay("alpha-rollback").is_none(),
        "the relay was stopped"
    );
    assert!(
        !crate::credentials_dir()
            .unwrap()
            .join("alpha-rollback")
            .exists(),
        "credentials this call created were removed"
    );
    assert!(!crate::polis_config::polis_path().unwrap().exists());
    std::fs::remove_dir_all(&staged).unwrap();
}

#[tokio::test]
#[serial]
async fn delete_removes_the_entry_and_stops_the_relay() {
    reset("alpha-del");
    reset("beta-del");
    let (state, _dir) = state_with_root().await;
    let stub = start_stub().await;
    write_entry("alpha-del", &stub.origin);
    write_entry("beta-del", &stub.origin);
    crate::test_helpers::install_relay(
        &state,
        "alpha-del",
        relay_handle(&state, "alpha-del", &stub.origin),
    );

    let Json(response) = super::remove_polis(
        State(state.clone()),
        axum::extract::Path("alpha-del".to_string()),
        keep(),
        operator(),
    )
    .await
    .unwrap();

    assert_eq!(response["relay"], "stopped", "{response}");
    assert_eq!(response["credentials"], "kept");
    assert!(state.relay("alpha-del").is_none());
    let entries = crate::polis_config::read_entries().unwrap();
    let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, ["beta-del"]);
}

#[tokio::test]
#[serial]
async fn delete_of_the_pinned_entry_is_refused() {
    reset("alpha-delpin");
    let (state, _dir) = state_with_root().await;
    let stub = start_stub().await;
    write_entry("alpha-delpin", &stub.origin);
    crate::settings::save_profile_source_polis(Some(&stub.origin)).unwrap();

    let error = super::remove_polis(
        State(state.clone()),
        axum::extract::Path("alpha-delpin".to_string()),
        keep(),
        operator(),
    )
    .await
    .unwrap_err();
    crate::settings::save_profile_source_polis(None).unwrap();

    assert_eq!(error.status, 409, "{error:?}");
    assert!(error.message.contains("pinned"), "{error:?}");
    assert_eq!(crate::polis_config::read_entries().unwrap().len(), 1);
}

#[tokio::test]
#[serial]
async fn delete_with_forget_removes_the_credentials_directory() {
    reset("alpha-forget");
    let (state, _dir) = state_with_root().await;
    let stub = start_stub().await;
    write_entry("alpha-forget", &stub.origin);
    store_credentials("alpha-forget", &stub.origin);

    let Json(response) = super::remove_polis(
        State(state.clone()),
        axum::extract::Path("alpha-forget".to_string()),
        forget(),
        operator(),
    )
    .await
    .unwrap();

    assert_eq!(response["credentials"], "deleted", "{response}");
    assert!(
        !crate::credentials_dir()
            .unwrap()
            .join("alpha-forget")
            .exists()
    );
    assert!(
        !crate::polis_config::polis_path().unwrap().exists(),
        "the last entry's removal takes the document with it"
    );
}

#[tokio::test]
#[serial]
async fn post_twice_without_a_code_is_already_attached() {
    reset("alpha-idem");
    let (state, _dir) = state_with_root().await;
    let stub = start_stub().await;
    let request = || Json(body("alpha-idem", &stub.origin, Some("code-6")));
    let _ = super::add_polis(State(state.clone()), operator(), request())
        .await
        .unwrap();
    let before = std::fs::read_to_string(crate::polis_config::polis_path().unwrap()).unwrap();

    let (status, Json(response)) = super::add_polis(
        State(state.clone()),
        operator(),
        Json(body("alpha-idem", &stub.origin, None)),
    )
    .await
    .unwrap();

    assert_eq!(status, axum::http::StatusCode::OK, "{response}");
    assert!(
        response["note"]
            .as_str()
            .unwrap()
            .contains("already attached"),
        "{response}"
    );
    let after = std::fs::read_to_string(crate::polis_config::polis_path().unwrap()).unwrap();
    assert_eq!(before, after, "the file is untouched");
    assert_eq!(stub.enrollments(), 1, "no second enrollment");
    assert!(
        state.relay("alpha-idem").is_some(),
        "the relay is still installed"
    );
    let unchanged = response["note"].as_str().unwrap();
    assert!(
        unchanged.contains("its relay are unchanged"),
        "the running relay is reported as unchanged: {response}"
    );
    let _ = super::stop_relay(&state, "alpha-idem").await;
}

#[tokio::test]
#[serial]
async fn post_without_a_code_starts_a_relay_that_was_not_running() {
    reset("alpha-detached");
    let (state, _dir) = state_with_root().await;
    let stub = start_stub().await;
    write_entry("alpha-detached", &stub.origin);
    store_credentials("alpha-detached", &stub.origin);
    let before = std::fs::read_to_string(crate::polis_config::polis_path().unwrap()).unwrap();
    assert!(state.relay("alpha-detached").is_none());

    let (status, Json(response)) = super::add_polis(
        State(state.clone()),
        operator(),
        Json(body("alpha-detached", &stub.origin, None)),
    )
    .await
    .unwrap();

    assert_eq!(status, axum::http::StatusCode::OK, "{response}");
    let note = response["note"].as_str().unwrap();
    assert!(note.contains("started now"), "{response}");
    assert_eq!(response["entry"]["relay"], true, "{response}");
    assert!(state.relay("alpha-detached").is_some());
    let after = std::fs::read_to_string(crate::polis_config::polis_path().unwrap()).unwrap();
    assert_eq!(before, after, "the file already had the entry");
    assert_eq!(stub.enrollments(), 0, "stored credentials are reused");
    let _ = super::stop_relay(&state, "alpha-detached").await;
}

#[tokio::test]
#[serial]
async fn a_sugar_boot_refuses_the_write_faces_and_lists_its_entry() {
    reset("default");
    crate::test_helpers::ensure_test_data_dir();
    let stub = start_stub().await;
    let mut inner = match Arc::try_unwrap(crate::test_helpers::make_state()) {
        Ok(inner) => inner,
        Err(_) => panic!("make_state hands out the sole reference"),
    };
    inner.polis_sugar = true;
    inner.platforms = Arc::new(arc_swap::ArcSwap::from_pointee(vec![
        crate::state::PlatformFace {
            name: "default".to_string(),
            origin: stub.origin.clone(),
            base: kallipai_common::polis::service_base(&stub.origin, "model-gateway"),
            credential: None,
        },
    ]));
    let state: SharedState = Arc::new(inner);
    {
        let mut registry = state.registry.write().await;
        crate::test_helpers::add_root(&mut registry, &kallipai_common::agentid::AgentId::random());
    }
    let _ = crate::test_helpers::install_projector(&state).await;

    let Json(listing) = super::list_polis(State(state.clone()), operator())
        .await
        .unwrap();
    let rows = listing["entries"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "the boot's own entry is listed: {listing}");
    assert_eq!(rows[0]["name"], "default");
    assert_eq!(rows[0]["gateway"], true);
    assert_eq!(listing["sugar"], true);

    let added = super::add_polis(
        State(state.clone()),
        operator(),
        Json(body("extra", &stub.origin, Some("code-7"))),
    )
    .await
    .unwrap_err();
    assert_eq!(added.status, 409, "{added:?}");
    assert!(added.message.contains("KALLIPAI_POLIS_URL"), "{added:?}");

    let removed = super::remove_polis(
        State(state.clone()),
        axum::extract::Path("default".to_string()),
        keep(),
        operator(),
    )
    .await
    .unwrap_err();
    assert_eq!(removed.status, 409, "{removed:?}");
    assert!(
        !crate::polis_config::polis_path().unwrap().exists(),
        "neither write face creates the file"
    );
}

/// A rebind of a running entry must not leave the previous connector running:
/// same name, new origin, credentials already stored.
#[tokio::test]
#[serial]
async fn post_that_rebinds_a_running_entry_stops_the_old_relay() {
    reset("alpha-rebind");
    let (state, _dir) = state_with_root().await;
    let stub = start_stub().await;
    write_entry("alpha-rebind", "https://old.example.test");
    store_credentials("alpha-rebind", "https://old.example.test");

    let old_cancel = tokio_util::sync::CancellationToken::new();
    let finished = Arc::new(AtomicBool::new(false));
    let observer = old_cancel.clone();
    let flag = finished.clone();
    let join = tokio::spawn(async move {
        observer.cancelled().await;
        flag.store(true, Ordering::SeqCst);
    });
    let displaced = state.claim_relay_slot(
        "alpha-rebind",
        crate::state::ActiveRelay {
            handle: relay_handle(&state, "alpha-rebind", &stub.origin),
            join,
            cancel: old_cancel.clone(),
        },
    );
    assert!(displaced.is_none());

    let (status, Json(response)) = super::add_polis(
        State(state.clone()),
        operator(),
        Json(body("alpha-rebind", &stub.origin, None)),
    )
    .await
    .unwrap();

    assert_eq!(status, axum::http::StatusCode::CREATED, "{response}");
    assert!(
        old_cancel.is_cancelled(),
        "the displaced connector's token is cancelled"
    );
    assert!(
        finished.load(Ordering::SeqCst),
        "and its task was awaited to completion before the call returned"
    );
    assert!(state.relay("alpha-rebind").is_some(), "the new relay is up");
    assert_eq!(stub.enrollments(), 0, "stored credentials are reused");
    let _ = super::stop_relay(&state, "alpha-rebind").await;
}

/// The two write faces share one lock: whatever order an overlapping POST and
/// DELETE take, the end state is one of the two consistent ones, never "entry
/// without relay". This pins the shape of that outcome, not the lock that
/// produces it, so it stays green if the serialization moves.
#[tokio::test]
#[serial]
async fn concurrent_post_and_delete_leave_a_consistent_state() {
    reset("alpha-race");
    let (state, _dir) = state_with_root().await;
    let stub = start_stub().await;

    let adding = super::add_polis(
        State(state.clone()),
        operator(),
        Json(body("alpha-race", &stub.origin, Some("code-race"))),
    );
    let removing = super::remove_polis(
        State(state.clone()),
        axum::extract::Path("alpha-race".to_owned()),
        keep(),
        operator(),
    );
    let (added, removed) = tokio::join!(adding, removing);

    // At least one of them must have succeeded: DELETE first sees no entry.
    assert!(
        added.is_ok() || matches!(removed, Err(ref error) if error.status == 404),
        "the pair cannot both be no-ops"
    );
    let on_disk = crate::polis_config::read_entries()
        .unwrap()
        .iter()
        .any(|entry| entry.name == "alpha-race");
    let running = state.relay("alpha-race").is_some();
    assert_eq!(
        on_disk, running,
        "the file and the relay map agree (entry={on_disk}, relay={running})"
    );
    let _ = super::stop_relay(&state, "alpha-race").await;
}

/// Removing the last entry deletes the document: an empty `[[polis]]` array is
/// a boot error, while a missing file means a local-only instance.
#[tokio::test]
#[serial]
async fn delete_of_the_last_entry_removes_the_document() {
    reset("alpha-last");
    let (state, _dir) = state_with_root().await;
    let stub = start_stub().await;
    write_entry("alpha-last", &stub.origin);

    let Json(response) = super::remove_polis(
        State(state.clone()),
        axum::extract::Path("alpha-last".to_owned()),
        keep(),
        operator(),
    )
    .await
    .unwrap();

    assert_eq!(response["removed"], true);
    assert!(
        !crate::polis_config::polis_path().unwrap().exists(),
        "no zero-entry document is left behind"
    );
}

/// A failure before the platform is reached is not a refusal: the entry's
/// credentials path is occupied by a file, so the relay directory cannot be
/// made and no enrollment code is spent.
#[tokio::test]
#[serial]
async fn post_that_cannot_prepare_the_relay_reports_a_local_failure() {
    reset("alpha-local");
    let (state, _dir) = state_with_root().await;
    let stub = start_stub().await;
    let blocked = crate::credentials_dir().unwrap().join("alpha-local");
    std::fs::create_dir_all(blocked.parent().unwrap()).unwrap();
    std::fs::write(&blocked, b"not a directory").unwrap();

    let error = super::add_polis(
        State(state.clone()),
        operator(),
        Json(body("alpha-local", &stub.origin, Some("code-local"))),
    )
    .await
    .unwrap_err();

    assert_eq!(error.status, 500, "{error:?}");
    assert_eq!(error.code.as_deref(), Some("polis_activation_failed"));
    assert_eq!(stub.enrollments(), 0, "nothing reached the platform");
    assert!(!crate::polis_config::polis_path().unwrap().exists());
}

/// Every activation verdict maps to the status the caller can act on, and the
/// document-removal failure is not reported as an unreadable document.
#[test]
fn activation_failures_map_to_their_own_status() {
    let dir = tempfile::TempDir::new().unwrap();
    let entry_dir = dir.path().join("creds");
    std::fs::create_dir_all(&entry_dir).unwrap();

    let refused = super::activation_error(
        "alpha",
        &entry_dir,
        true,
        crate::ActivationFailure::Refused(anyhow::anyhow!("code expired")),
    );
    assert_eq!(refused.status, 400, "{refused:?}");
    assert!(refused.message.contains("refused the enrollment code"));

    let not_stored = super::activation_error(
        "alpha",
        &entry_dir,
        true,
        crate::ActivationFailure::CredentialsNotStored {
            tagma_id: "tagma-9".to_owned(),
            error: anyhow::anyhow!("disk full"),
        },
    );
    assert_eq!(not_stored.status, 500, "{not_stored:?}");
    assert_eq!(
        not_stored.code.as_deref(),
        Some("polis_enrollment_not_stored")
    );
    assert!(
        not_stored.message.contains("tagma-9"),
        "{}",
        not_stored.message
    );
    assert!(
        not_stored.message.contains("kallipctl polis revoke"),
        "the sentence names the way out: {}",
        not_stored.message
    );

    let unconfirmed = super::activation_error(
        "alpha",
        &entry_dir,
        true,
        crate::ActivationFailure::Unconfirmed(anyhow::anyhow!("connection refused")),
    );
    assert_eq!(unconfirmed.status, 502, "{unconfirmed:?}");
    assert!(
        unconfirmed.message.contains("no usable answer"),
        "{}",
        unconfirmed.message
    );
    assert!(
        unconfirmed.message.contains("polis status alpha"),
        "the sentence names the check: {}",
        unconfirmed.message
    );

    let connector = super::activation_error(
        "alpha",
        &entry_dir,
        true,
        crate::ActivationFailure::Connector(anyhow::anyhow!("root agent missing")),
    );
    assert_eq!(connector.status, 502, "{connector:?}");
    assert!(
        connector.message.contains("connector did not start"),
        "{}",
        connector.message
    );

    let not_removed = super::config_error(crate::polis_config::PolisConfigError::NotRemoved(
        "/data/polis.toml: device or resource busy".to_owned(),
    ));
    assert_eq!(not_removed.status, 500, "{not_removed:?}");
    assert_eq!(not_removed.code.as_deref(), Some("polis_file_not_removed"));
    assert!(
        not_removed.message.contains("still there") && not_removed.message.contains("left running"),
        "the sentence names the state left behind: {}",
        not_removed.message
    );

    let fresh_dir = dir.path().join("fresh");
    std::fs::create_dir_all(&fresh_dir).unwrap();
    let local = super::activation_error(
        "alpha",
        &fresh_dir,
        false,
        crate::ActivationFailure::Local(anyhow::anyhow!("permission denied")),
    );
    assert_eq!(local.status, 500, "{local:?}");
    assert_eq!(local.code.as_deref(), Some("polis_activation_failed"));
    assert!(
        !fresh_dir.exists(),
        "a directory this call made is removed when it stored nothing"
    );
}

/// Every face is operator-only: an agent identity gets 403 before any state is
/// read or changed.
#[tokio::test]
#[serial]
async fn every_face_refuses_an_agent_identity() {
    reset("alpha-forbidden");
    let (state, _dir) = state_with_root().await;
    let agent = || {
        AuthIdentity::test_new(crate::auth::Identity::Agent {
            id: kallipai_common::agentid::AgentId::random(),
        })
    };

    let listed = super::list_polis(State(state.clone()), agent()).await;
    assert_eq!(listed.unwrap_err().status, 403);

    let added = super::add_polis(
        State(state.clone()),
        agent(),
        Json(body("alpha-forbidden", "https://one.example.test", None)),
    )
    .await;
    assert_eq!(added.unwrap_err().status, 403);

    let removed = super::remove_polis(
        State(state.clone()),
        axum::extract::Path("alpha-forbidden".to_owned()),
        keep(),
        agent(),
    )
    .await;
    assert_eq!(removed.unwrap_err().status, 403);
    assert!(!crate::polis_config::polis_path().unwrap().exists());
}

/// A connector that cannot start reports 502 and leaves the entry in place: the
/// state here has no root agent, which is the first thing activation asks for.
/// No stub stands in for the failure.
#[tokio::test]
#[serial]
async fn post_that_cannot_start_the_connector_reports_it_and_keeps_the_entry() {
    reset("alpha-noconnector");
    crate::test_helpers::ensure_test_data_dir();
    let state = crate::test_helpers::make_state();
    write_entry("alpha-noconnector", "https://one.example.test");
    store_credentials("alpha-noconnector", "https://one.example.test");

    let error = super::add_polis(
        State(state.clone()),
        operator(),
        Json(body("alpha-noconnector", "https://one.example.test", None)),
    )
    .await
    .unwrap_err();

    assert_eq!(error.status, 502, "{error:?}");
    assert!(
        error.message.contains("connector did not start"),
        "{}",
        error.message
    );
    assert!(state.relay("alpha-noconnector").is_none());
    let entries = crate::polis_config::read_entries().unwrap();
    assert_eq!(entries.len(), 1, "the entry survives a failed activation");
}
