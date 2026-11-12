//! End-to-end: the CLI's file client against a real files service (the
//! real boot migration on a test Postgres, the real router, real HTTP)
//! with a mini archeion `/internal` mock. The client is driven function-level
//! through `kallip::file`; the binary-invoke path (arg parsing to exit
//! code) is compose-smoke territory.

mod common;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use kallip::file::FilesClient;
use kallipai_archeion_common::control_plane::EnrollmentLookup;
use kallipai_archeion_common::ids::{TagmaId, UserId};
use kallipai_archeion_common::internal_api::{
    EnrollmentLookupRequest, VerifyBearerRequest, VerifyBearerResponse, WireCredential,
    WirePrincipal,
};
use kallipai_archeion_common::perm::PermSet;
use kallipai_common::telemetry::test_observability::{ObservedSpan, Recorder};
use kallipai_files::LocalBackend;
use kallipai_files::auth::FilesControlPlane;
use kallipai_files::gc::GcConfig;
use kallipai_files::metadata::connect_and_migrate;
use kallipai_files::state::{AppState, FilesConfig};
use tempfile::TempDir;

/// The mock registry: bearer tokens -> wire principals, tagmas -> their
/// enrollment facts. Two routes only -- `verify-bearer` and
/// `enrollment-lookup` -- which is the entire `/internal` surface the
/// files service reaches on these paths: the send flow's user-identity
/// read serves the U->U delivery, unreachable with a tagma bearer.
#[derive(Clone, Default)]
struct Registry {
    /// Bearer -> the identity it resolves to plus the record it carries (the
    /// admin bearer has none, every other bearer is a record).
    bearer: Arc<Mutex<HashMap<String, BearerRecord>>>,
    enrollment: Arc<Mutex<HashMap<TagmaId, EnrollmentLookup>>>,
}

/// What a bearer resolves to: the identity, plus the permission record it
/// carries. `None` for the admin bearer, whose rights are its own, and for
/// the recordless shape an older registry sends, which a data plane refuses.
type BearerRecord = (WirePrincipal, Option<PermSet>);

impl Registry {
    /// Mint a personal access token for `user`: the plaintext comes back once,
    /// the store keeps the record it resolves to. This is the create step the
    /// CLI's bearer comes from; every call after it carries the plaintext.
    fn mint_pat(&self, user: &UserId, perms: &[&str]) -> String {
        let plaintext = format!("sk-pat-{}", uuid::Uuid::new_v4());
        let perms =
            PermSet::from_known(perms.iter().map(|p| (*p).to_owned())).expect("known perms");
        self.bearer.lock().expect("lock").insert(
            plaintext.clone(),
            (
                WirePrincipal::User {
                    user_id: user.clone(),
                },
                Some(perms),
            ),
        );
        plaintext
    }

    /// The shape a registry older than the credential field sends: the user's
    /// identity with no record at all. A data plane meeting it must refuse the
    /// call rather than read the absent record as no narrowing.
    fn mint_recordless_pat(&self, user: &UserId) -> String {
        let plaintext = format!("sk-pat-{}", uuid::Uuid::new_v4());
        self.bearer.lock().expect("lock").insert(
            plaintext.clone(),
            (
                WirePrincipal::User {
                    user_id: user.clone(),
                },
                None,
            ),
        );
        plaintext
    }
}

/// The mock ignores the internal shared secret the client presents (the
/// real archeion verifies it); these e2e tests exercise the files-side
/// behavior, not the secret check.
async fn spawn_mini_archeion(registry: Registry) -> String {
    let app = axum::Router::new()
        .route(
            "/internal/verify-bearer",
            axum::routing::post(verify_bearer_route),
        )
        .route(
            "/internal/enrollment-lookup",
            axum::routing::post(enrollment_lookup_route),
        )
        .with_state(registry);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{addr}")
}

async fn verify_bearer_route(
    State(registry): State<Registry>,
    axum::Json(request): axum::Json<VerifyBearerRequest>,
) -> axum::response::Response {
    let found = registry
        .bearer
        .lock()
        .expect("lock")
        .get(&request.token)
        .cloned();
    match found {
        Some((principal, perms)) => {
            // Every resolved bearer is a record, except the recordless shape
            // an older registry sends; this route serves it verbatim so the
            // service's refusal is exercised.
            let credential = perms.map(|perms| WireCredential {
                token_id: kallipai_archeion_common::ids::TokenId::from(request.token.clone()),
                perms,
            });
            (
                StatusCode::OK,
                axum::Json(VerifyBearerResponse {
                    principal,
                    credential,
                }),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn enrollment_lookup_route(
    State(registry): State<Registry>,
    axum::Json(request): axum::Json<EnrollmentLookupRequest>,
) -> axum::response::Response {
    let found = registry
        .enrollment
        .lock()
        .expect("lock")
        .get(&request.tagma_id)
        .cloned();
    match found {
        Some(lookup) => (StatusCode::OK, axum::Json(lookup)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

struct World {
    files_url: String,
    user1: UserId,
    t1: TagmaId,
    t2: TagmaId,
    t3: TagmaId,
    t1_token: String,
    t2_token: String,
    t3_token: String,
    admin_token: String,
    /// A personal access token for user1 (files read + write), and one that
    /// carries reads only.
    pat_token: String,
    read_only_pat: String,
    /// The shape an older registry sends: a personal token with no record.
    recordless_pat: String,
    /// Keeps the blob root alive for the whole test.
    _blob_dir: TempDir,
}

async fn spawn_world() -> World {
    let blob_dir = TempDir::new().expect("blob dir");
    let user1 = UserId::from(uuid::Uuid::new_v4().to_string());
    let user2 = UserId::from(uuid::Uuid::new_v4().to_string());
    let t1 = TagmaId::from(uuid::Uuid::new_v4().to_string());
    let t2 = TagmaId::from(uuid::Uuid::new_v4().to_string());
    let t3 = TagmaId::from(uuid::Uuid::new_v4().to_string());
    let t1_token = "sk-tagma-e2e-t1".to_owned();
    let t2_token = "sk-tagma-e2e-t2".to_owned();
    let t3_token = "sk-tagma-e2e-t3".to_owned();
    let admin_token = "sk-admin-e2e".to_owned();

    // t1 and t2 are enrolled in user1's space; t3 in user2's.
    let mut bearers = HashMap::new();
    let tagma_record = || {
        Some(
            PermSet::from_known(
                kallipai_archeion_common::perm::TAGMA_DEFAULTS
                    .iter()
                    .map(|p| (*p).to_owned()),
            )
            .expect("known perms"),
        )
    };
    bearers.insert(
        t1_token.clone(),
        (
            WirePrincipal::Tagma {
                tagma_id: t1.clone(),
            },
            tagma_record(),
        ),
    );
    bearers.insert(
        t2_token.clone(),
        (
            WirePrincipal::Tagma {
                tagma_id: t2.clone(),
            },
            tagma_record(),
        ),
    );
    bearers.insert(
        t3_token.clone(),
        (
            WirePrincipal::Tagma {
                tagma_id: t3.clone(),
            },
            tagma_record(),
        ),
    );
    bearers.insert(admin_token.clone(), (WirePrincipal::Admin, None));
    let space1 = vec![t1.clone(), t2.clone()];
    let mut enrollments = HashMap::new();
    for tagma in [&t1, &t2] {
        enrollments.insert(
            tagma.clone(),
            EnrollmentLookup {
                user_id: user1.clone(),
                enrolled_tagmas: space1.clone(),
            },
        );
    }
    enrollments.insert(
        t3.clone(),
        EnrollmentLookup {
            user_id: user2.clone(),
            enrolled_tagmas: vec![t3.clone()],
        },
    );
    let registry = Registry {
        bearer: Arc::new(Mutex::new(bearers)),
        enrollment: Arc::new(Mutex::new(enrollments)),
    };
    // The CLI's own bearer: a personal token minted for user1, carrying the
    // two files strings its calls need.
    let pat_token = registry.mint_pat(
        &user1,
        &[
            kallipai_archeion_common::perm::FILES_RECORD_READ,
            kallipai_archeion_common::perm::FILES_RECORD_WRITE,
        ],
    );
    // A second one, reads only: the record's set is what the service honors.
    let read_only_pat =
        registry.mint_pat(&user1, &[kallipai_archeion_common::perm::FILES_RECORD_READ]);
    // The mixed-version shape: a personal token the registry answers without
    // any record at all.
    let recordless_pat = registry.mint_recordless_pat(&user1);
    let archeion_url = spawn_mini_archeion(registry).await;

    let db_url = common::test_db_url().await;
    let db = connect_and_migrate(&db_url, std::time::Duration::from_millis(100))
        .await
        .expect("connect and migrate");
    let state = AppState {
        db,
        blob: LocalBackend::arc(blob_dir.path()),
        blob_root: blob_dir.path().to_path_buf(),
        control: Arc::new(FilesControlPlane::new(
            archeion_url,
            "sk-internal-e2e".to_owned(),
        )),
        notify: None,
        config: Arc::new(FilesConfig {
            max_body_bytes: 1024 * 1024,
            degrade_fail_soft: false,
            cors_origins: String::new(),
            gc: GcConfig {
                batch: 128,
                interval: std::time::Duration::from_secs(3600),
                grace: std::time::Duration::from_secs(3600),
            },
        }),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, kallipai_files::state::router(state))
            .await
            .expect("serve");
    });
    World {
        files_url: format!("http://{addr}"),
        user1,
        t1,
        t2,
        t3,
        t1_token,
        t2_token,
        t3_token,
        admin_token,
        pat_token,
        read_only_pat,
        recordless_pat,
        _blob_dir: blob_dir,
    }
}

/// A local scratch file with known content, removed with the TempDir.
fn scratch_file(dir: &TempDir, name: &str, content: &[u8]) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, content).expect("write scratch file");
    path
}

#[tokio::test]
async fn put_ls_get_roundtrips_through_the_real_service() {
    let world = spawn_world().await;
    let client = FilesClient::new(world.files_url.clone(), world.t1_token.clone());
    let blob_dir = TempDir::new().expect("scratch dir");
    let content = b"report body \xf0\x9f\x93\x84 bytes";
    let local = scratch_file(&blob_dir, "report.txt", content);
    let target = format!(
        "/users/{}/tagmas/{}/report.txt",
        world.user1.as_ref(),
        world.t1.as_ref()
    );

    let put = client.put_file(&target, &local).await.expect("put");
    // The space path is the identity of the record; the blob id names the
    // content-addressed copy.
    assert_eq!(put.blob_id.len(), "sha256-".len() + 64);

    let rows = client
        .list_files("self", Some("report"), None)
        .await
        .expect("ls");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path, target);
    assert_eq!(rows[0].size, content.len() as i64);

    let downloaded = client.get_file(put.record_id).await.expect("get");
    assert_eq!(downloaded, content);
}

/// The process's one recorder.
///
/// Read from the helper: `recorder` publishes the process-wide W3C propagator
/// and the tracer provider, and those are last-writer-wins, so two recorders
/// built inside one run take each other's handles. One recorder for the binary
/// keeps them in place, and installing its subscriber globally, once, covers
/// every thread instead of one.
///
/// Measured here, two counts from two shapes. Built per test with a thread-local
/// subscriber, this binary failed two of forty runs. With one recorder shared and
/// its subscriber still thread-local, a targeted repeat failed three runs in
/// three, spans a test had made missing from its exporter. With the one recorder
/// installed globally, forty of forty passed. Why the thread-local shapes lose
/// spans is not established, so this file rests on the measured shape rather than
/// on an account of it.
///
/// Two premises the compiler does not enforce. The service's span is created on
/// the test's own thread -- `#[tokio::test]` runs a current-thread runtime, so
/// the task serving the upload is polled there -- and that is what lets a test
/// claim the traces that started on its thread; a multi-thread runtime would
/// take the claim away silently. And the recorder holds every test's spans, so
/// an assertion reads its own trace and never claims that some span is absent.
fn trace_recorder() -> &'static Recorder {
    use kallipai_common::telemetry::test_observability::recorder;

    static SHARED: OnceLock<Recorder> = OnceLock::new();
    SHARED.get_or_init(|| {
        let (recorder, subscriber) = recorder("kallip");
        tracing::subscriber::set_global_default(subscriber)
            .expect("the process-wide subscriber is installed once");
        recorder
    })
}

/// The running test's thread name: the tag that tells its spans from a
/// sibling test's in the shared recorder.
fn test_thread_name() -> String {
    std::thread::current()
        .name()
        .expect("a test is polled on a named thread")
        .to_owned()
}

/// What the running test exported so far.
///
/// A span created on the test's thread pins the trace. One trace's spans are not
/// promised to be created on one thread, so the trace, not the creating thread,
/// is what carries a test's spans together. A sibling test's traces never enter
/// this set.
fn own_spans(recorder: &Recorder, test: &str) -> Vec<ObservedSpan> {
    let all = recorder.observed();
    let mine: HashSet<String> = all
        .iter()
        .filter(|span| span.attribute("thread.name") == Some(test))
        .map(|span| span.trace_id.clone())
        .collect();
    all.into_iter()
        .filter(|span| mine.contains(&span.trace_id))
        .collect()
}

/// Wait until the recorder holds one of this test's spans with this name and
/// kind, then hand back everything the test exported: a service's span ends on
/// its own task, after the call returns, so counting spans would race the
/// export.
async fn spans_once(name: &str, kind: &str) -> Vec<ObservedSpan> {
    use kallipai_common::telemetry::test_observability::spans_when;

    let recorder = trace_recorder();
    let test = test_thread_name();
    spans_when(
        recorder,
        &format!("a {kind} span named {name} created on this test's thread"),
        |spans| {
            spans.iter().any(|span| {
                span.name == name
                    && span.kind == kind
                    && span.attribute("thread.name") == Some(test.as_str())
            })
        },
    )
    .await;
    own_spans(recorder, &test)
}

/// A put made inside a traced command keeps that command's trace: the
/// files service answers inside the span of the upload instead of opening
/// a trace of its own. The service mounts the server-span layer itself;
/// what this proves is the CLI's client putting the header on the wire.
#[tokio::test]
async fn a_put_carries_the_context_of_the_command_it_serves() {
    use kallipai_common::telemetry::test_observability::{client_span_of, span_of};
    use tracing::Instrument as _;

    let world = spawn_world().await;
    let scratch = TempDir::new().expect("scratch dir");
    let local = scratch_file(&scratch, "report.txt", b"payload");
    let target = format!(
        "/users/{}/tagmas/{}/report.txt",
        world.user1.as_ref(),
        world.t1.as_ref()
    );

    trace_recorder();
    let client = FilesClient::new(world.files_url.clone(), world.t1_token.clone());
    let command = kallipai_common::telemetry::request_span(
        "PUT",
        "/files",
        "/files",
        &axum::http::HeaderMap::new(),
    );
    async { client.put_file(&target, &local).await }
        .instrument(command.clone())
        .await
        .expect("the upload lands");
    // A span is exported when it is dropped, so the command span has to end
    // for the assertions to find it.
    drop(command);

    let spans = spans_once("PUT /", "server").await;
    let asked = span_of(&spans, "PUT /files", "server");
    let call = client_span_of(&spans, "PUT", "/");
    let answered = span_of(&spans, "PUT /", "server");
    assert_eq!(
        call.parent_span_id.as_deref(),
        Some(asked.span_id.as_str()),
        "the call hangs under the command that made it"
    );
    assert_eq!(
        answered.parent_span_id.as_deref(),
        Some(call.span_id.as_str()),
        "the service answered inside the span of the call"
    );
    assert_eq!(
        answered.trace_id, call.trace_id,
        "one trace spans the upload, not a second one at the service"
    );
    assert_eq!(
        call.trace_id, asked.trace_id,
        "and that trace is the command's"
    );
}

/// The same upload on a bare client: nothing puts a header on the wire, so
/// the service starts a trace of its own. Without this, the test above
/// would pass on a header read back at the caller.
#[tokio::test]
async fn a_put_that_carries_no_header_leaves_the_service_its_own_trace() {
    use kallipai_common::telemetry::test_observability::span_of;
    use tracing::Instrument as _;

    let world = spawn_world().await;
    let scratch = TempDir::new().expect("scratch dir");
    let local = scratch_file(&scratch, "report.txt", b"payload");
    let body = std::fs::read(&local).expect("read scratch file");
    let target = format!(
        "/users/{}/tagmas/{}/report.txt",
        world.user1.as_ref(),
        world.t1.as_ref()
    );

    trace_recorder();
    let command = kallipai_common::telemetry::request_span(
        "PUT",
        "/files",
        "/files",
        &axum::http::HeaderMap::new(),
    );
    let bare = reqwest::Client::new();
    let response = async {
        bare.put(&world.files_url)
            .query(&[("path", target.as_str())])
            .header("authorization", format!("Bearer {}", world.t1_token))
            .header(reqwest::header::CONTENT_LENGTH, body.len())
            .body(body)
            .send()
            .await
    }
    .instrument(command.clone())
    .await
    .expect("the service answers");
    assert_eq!(response.status().as_u16(), 201);
    // A span is exported when it is dropped, so the command span has to end
    // for the assertions to find it.
    drop(command);

    let spans = spans_once("PUT /", "server").await;
    let asked = span_of(&spans, "PUT /files", "server");
    let answered = span_of(&spans, "PUT /", "server");
    assert_eq!(
        answered.parent_span_id, None,
        "a request with no header is a root at the service"
    );
    assert_ne!(
        answered.trace_id, asked.trace_id,
        "the service did not join the command's trace"
    );
}

#[tokio::test]
async fn send_lands_in_the_target_inbox_and_is_discoverable_there() {
    let world = spawn_world().await;
    let sender = FilesClient::new(world.files_url.clone(), world.t1_token.clone());
    let recipient = FilesClient::new(world.files_url.clone(), world.t2_token.clone());
    let blob_dir = TempDir::new().expect("scratch dir");
    let content = b"team notes";
    let local = scratch_file(&blob_dir, "team.txt", content);
    let shared = format!("/users/{}/shared/team.txt", world.user1.as_ref());

    let put = sender.put_file(&shared, &local).await.expect("put");
    let sent = sender
        .send_file(put.record_id, None, Some(world.t2.as_ref()))
        .await
        .expect("send");
    assert_eq!(
        sent.path,
        format!(
            "/users/{}/tagmas/{}/inbox/team.txt",
            world.user1.as_ref(),
            world.t2.as_ref()
        )
    );

    // The recipient discovers the delivery with its own listing (the inbox
    // is inside its region prefix) and reads it back byte-for-byte.
    let rows = recipient.list_files("self", None, None).await.expect("ls");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path, sent.path);
    let fetched = recipient.get_file(sent.record_id).await.expect("get");
    assert_eq!(fetched, content);
}

#[tokio::test]
async fn cross_space_records_are_refused_and_never_listed() {
    let world = spawn_world().await;
    let t1 = FilesClient::new(world.files_url.clone(), world.t1_token.clone());
    let t3 = FilesClient::new(world.files_url.clone(), world.t3_token.clone());
    let blob_dir = TempDir::new().expect("scratch dir");
    let local = scratch_file(&blob_dir, "secret.txt", b"secret");
    let target = format!(
        "/users/{}/tagmas/{}/secret.txt",
        world.user1.as_ref(),
        world.t1.as_ref()
    );

    let put = t1.put_file(&target, &local).await.expect("put");

    // The foreign tagma gets the same denial a direct GET would give, and
    // its listings -- private or shared -- never carry the other space's
    // rows.
    let err = t3.get_file(put.record_id).await.expect_err("403");
    assert!(err.to_string().contains("403"), "{err}");
    let rows = t3.list_files("self", None, None).await.expect("ls");
    assert!(rows.is_empty());
    let rows = t3.list_files("shared", None, None).await.expect("ls");
    assert!(rows.is_empty());

    // A cross-space send is refused server-side (same-space-only rule):
    // t1 cannot deliver into t3's inbox.
    let err = t1
        .send_file(put.record_id, None, Some(world.t3.as_ref()))
        .await
        .expect_err("cross-space send refused");
    assert!(err.to_string().contains("403"), "{err}");
}

#[tokio::test]
async fn admin_bearer_has_no_listing_face() {
    let world = spawn_world().await;
    let admin = FilesClient::new(world.files_url.clone(), world.admin_token.clone());
    let err = admin
        .list_files("shared", None, None)
        .await
        .expect_err("403");
    assert!(err.to_string().contains("403"), "{err}");
}

#[tokio::test]
async fn an_unrecognized_bearer_is_rejected_before_any_path_logic() {
    let world = spawn_world().await;
    let ghost = FilesClient::new(world.files_url.clone(), "sk-tagma-e2e-ghost".to_owned());
    let err = ghost.list_files("self", None, None).await.expect_err("401");
    assert!(err.to_string().contains("401"), "{err}");
}

/// A registry that predates the credential field answers every bearer with no
/// record at all. The files service must fail closed instead of reading the
/// absent record as no narrowing: the answer is a permanent 500 carrying
/// `auth_contract_violation`, which no retry clears.
#[tokio::test]
async fn a_bearer_that_carries_no_record_is_refused() {
    let world = spawn_world().await;
    let client = FilesClient::new(world.files_url.clone(), world.recordless_pat.clone());
    let err = client
        .list_files("self", None, None)
        .await
        .expect_err("a recordless bearer is not a caller");
    assert!(err.to_string().contains("500"), "{err}");

    // The failure kind rides the body, not the client's message: ask the
    // service directly for the code a caller can branch on.
    let response = reqwest::Client::new()
        .get(world.files_url.clone())
        .query(&[("space", "self")])
        .bearer_auth(&world.recordless_pat)
        .send()
        .await
        .expect("call the files service");
    assert_eq!(response.status(), 500);
    let body: serde_json::Value = response.json().await.expect("error body");
    assert_eq!(body["error"]["code"], "auth_contract_violation", "{body}");
}

/// The CLI's bearer is a personal access token: the test mints one (the
/// plaintext comes back once) and every call after that carries only the
/// plaintext, through the real files service. It stores, lists and reads in its
/// owner's space; a second token minted with reads only is refused the store,
/// so the record's set -- not the token's shape -- decides.
#[tokio::test]
async fn a_personal_token_drives_the_cli_in_its_owners_space() {
    let world = spawn_world().await;
    let client = FilesClient::new(world.files_url.clone(), world.pat_token.clone());
    let blob_dir = TempDir::new().expect("scratch dir");
    let content = b"pat-driven bytes";
    let local = scratch_file(&blob_dir, "pat.txt", content);
    let target = format!("/users/{}/shared/pat.txt", world.user1.as_ref());

    let put = client
        .put_file(&target, &local)
        .await
        .expect("put with a personal token");

    let rows = client
        // Relative to the space root, so the shared region is part of it.
        .list_files("self", Some("shared/pat"), None)
        .await
        .expect("ls");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path, target);
    assert_eq!(rows[0].size, content.len() as i64);

    let downloaded = client.get_file(put.record_id).await.expect("get");
    assert_eq!(downloaded, content);

    // Reads only: the read half still answers, the write half is refused.
    let read_only = FilesClient::new(world.files_url.clone(), world.read_only_pat.clone());
    assert_eq!(
        read_only.get_file(put.record_id).await.expect("read half"),
        content
    );
    let refused = read_only
        .put_file(
            &format!("/users/{}/shared/refused.txt", world.user1.as_ref()),
            &local,
        )
        .await
        .expect_err("a reads-only token cannot store");
    let message = refused.to_string();
    assert!(
        message.contains("403") && message.contains("files:record:write"),
        "{message}"
    );
}
