//! End-to-end: the CLI's file client against a real files service (the
//! real boot migration on a test Postgres, the real router, real HTTP)
//! with a mini archeion `/internal` mock. The client is driven function-level
//! through `kallip::file`; the binary-invoke path (arg parsing to exit
//! code) is compose-smoke territory.

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use kallip::file::FilesClient;
use kallip_archeion_common::control_plane::EnrollmentLookup;
use kallip_archeion_common::ids::{TagmaId, UserId};
use kallip_archeion_common::internal_api::{
    EnrollmentLookupRequest, VerifyBearerRequest, VerifyBearerResponse, WirePrincipal,
};
use kallip_files::LocalBackend;
use kallip_files::auth::FilesControlPlane;
use kallip_files::gc::GcConfig;
use kallip_files::metadata::connect_and_migrate;
use kallip_files::state::{AppState, FilesConfig};
use tempfile::TempDir;

/// The mock registry: bearer tokens -> wire principals, tagmas -> their
/// enrollment facts. Two routes only -- `verify-bearer` and
/// `enrollment-lookup` -- which is the entire `/internal` surface the
/// files service reaches on these paths: the send flow's user-identity
/// read serves the U->U delivery, unreachable with a tagma bearer.
#[derive(Clone, Default)]
struct Registry {
    bearer: Arc<Mutex<HashMap<String, WirePrincipal>>>,
    enrollment: Arc<Mutex<HashMap<TagmaId, EnrollmentLookup>>>,
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
        Some(principal) => (
            StatusCode::OK,
            axum::Json(VerifyBearerResponse { principal }),
        )
            .into_response(),
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
    bearers.insert(
        t1_token.clone(),
        WirePrincipal::Tagma {
            tagma_id: t1.clone(),
        },
    );
    bearers.insert(
        t2_token.clone(),
        WirePrincipal::Tagma {
            tagma_id: t2.clone(),
        },
    );
    bearers.insert(
        t3_token.clone(),
        WirePrincipal::Tagma {
            tagma_id: t3.clone(),
        },
    );
    bearers.insert(admin_token.clone(), WirePrincipal::Admin);
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
    let archeion_url = spawn_mini_archeion(registry).await;

    let db_url = common::test_db_url().await;
    let db = connect_and_migrate(&db_url)
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
        axum::serve(listener, kallip_files::state::router(state))
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
