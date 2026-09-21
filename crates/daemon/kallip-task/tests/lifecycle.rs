//! Black-box lifecycle + gate tests through the public `TaskStore` API.

use std::sync::Arc;

use kallip_blob_store::LocalBackend;
use kallip_common::protocol::{REPORT_MAX_BYTES, TaskConfirmRequest, TaskCreateRequest};
use kallip_task::store::TaskFilter;
use kallip_task::{ClosedReason, ConfirmerResolver, Error, TaskStatus, TaskStore};
use sea_orm::ConnectionTrait;

fn confirm_req(note: Option<&str>, file: Option<&str>) -> TaskConfirmRequest {
    TaskConfirmRequest {
        note: note.map(str::to_string),
        file: file.map(str::to_string),
    }
}

fn spec(title: &str, assignee: &str, require: &[&str]) -> TaskCreateRequest {
    TaskCreateRequest {
        title: title.to_string(),
        assignee: Some(assignee.to_string()),
        require: require.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

/// No registry in these tests: confirmer names stay names (create with an
/// empty resolver only works for an empty roster).
fn no_resolver() -> ConfirmerResolver {
    Arc::new(|_| None)
}

/// The canonical test roster: r1 and r2 map to agent ids, anything else
/// (ghost confirmers) fails to resolve.
fn roster_resolver() -> ConfirmerResolver {
    Arc::new(|name| match name {
        "r1" => Some("agent-r1".to_string()),
        "r2" => Some("agent-r2".to_string()),
        _ => None,
    })
}

/// The create verb is the resolver's only caller now; every test that
/// registers a roster goes through this helper.
async fn create(
    store: &TaskStore,
    title: &str,
    assignee: &str,
    require: &[&str],
    resolver: ConfirmerResolver,
) -> kallip_task::Task {
    store
        .create(spec(title, assignee, require), "root", resolver)
        .await
        .unwrap()
}

#[tokio::test]
async fn lifecycle_with_review_and_confirmations() {
    let store = TaskStore::open_in_memory().await;
    let t = create(&store, "batch", "dev", &["r1", "r2"], roster_resolver()).await;
    assert_eq!(t.status, "queued");

    let t = store.start(t.id, "dev", false).await.unwrap();
    assert_eq!(t.status, "in_progress");
    assert!(t.started_at.is_some());

    let t = store
        .confirm(t.id, "dev", confirm_req(Some("wip"), None))
        .await
        .unwrap();
    assert_eq!(t.status, "in_progress");

    // Closing without confirmations fails the gate, naming every id.
    let err = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::ConfirmationGate { ref missing }
                if missing.contains("agent-r1") && missing.contains("agent-r2")
        ),
        "{err}"
    );

    let t = store.review(t.id, "dev").await.unwrap();
    assert_eq!(t.status, "review");

    // One confirmation is still not enough.
    store
        .confirm(t.id, "agent-r1", confirm_req(None, None))
        .await
        .unwrap();
    let err = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::ConfirmationGate { ref missing } if missing == "agent-r2"),
        "{err}"
    );

    store
        .confirm(t.id, "agent-r2", confirm_req(None, None))
        .await
        .unwrap();
    let t = store
        .close(
            t.id,
            "dev",
            ClosedReason::Completed,
            Some("landed".to_string()),
            false,
            None,
        )
        .await
        .unwrap();
    assert_eq!(t.status, "closed");
    assert_eq!(t.closed_reason.as_deref(), Some("completed"));
    assert_eq!(t.close_summary.as_deref(), Some("landed"));
    assert!(t.ended_at.is_some());

    let t = store.reopen(t.id, "root", false).await.unwrap();
    assert_eq!(t.status, "in_progress");
    assert!(t.ended_at.is_none() && t.closed_reason.is_none());
}

#[tokio::test]
async fn serial_gate_blocks_second_in_progress_and_forces_through() {
    let store = TaskStore::open_in_memory().await;
    let resolver = roster_resolver();
    let a = create(&store, "a", "dev", &[], resolver.clone()).await;
    let b = create(&store, "b", "dev", &[], resolver.clone()).await;
    store.start(a.id, "dev", false).await.unwrap();

    let err = store.start(b.id, "dev", false).await.unwrap_err();
    match err {
        Error::SerialGate { blocked_by, .. } => assert_eq!(blocked_by, a.id),
        other => panic!("expected serial gate, got {other:?}"),
    }

    // --force escapes, and the escape lands in the trail.
    store.start(b.id, "dev", true).await.unwrap();
    let (_, events) = store.get(b.id).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "action" && e.name == "force_start")
    );

    // A different assignee is not blocked.
    let c = create(&store, "c", "qa", &[], resolver).await;
    store.start(c.id, "qa", false).await.unwrap();
}

#[tokio::test]
async fn start_gate_blocks_by_assignee_and_a_claim_fills_the_assignee() {
    let store = TaskStore::open_in_memory().await;

    // The gate blocks on the assignee's held task, not the actor's:
    // bob starts alice's pre-assigned task while alice holds one.
    let held = create(&store, "held", "alice", &[], roster_resolver()).await;
    let assigned = create(&store, "assigned", "alice", &[], roster_resolver()).await;
    store.start(held.id, "alice", false).await.unwrap();
    let err = store.start(assigned.id, "bob", false).await.unwrap_err();
    match err {
        Error::SerialGate { blocked_by, .. } => assert_eq!(blocked_by, held.id),
        other => panic!("expected serial gate, got {other:?}"),
    }

    // An unassigned task's assignee is filled by the claiming actor.
    let req = TaskCreateRequest {
        title: "unassigned".into(),
        assignee: None,
        ..Default::default()
    };
    let t = store.create(req, "root", roster_resolver()).await.unwrap();
    let t = store.start(t.id, "carol", false).await.unwrap();
    assert_eq!(t.assignee, Some("carol".into()));
}

#[tokio::test]
async fn confirm_on_queued_paused_or_closed_is_an_invalid_transition() {
    let store = TaskStore::open_in_memory().await;
    let resolver = roster_resolver();

    // queued
    let t = create(&store, "q", "dev", &[], resolver.clone()).await;
    let err = store
        .confirm(t.id, "dev", confirm_req(Some("wip"), None))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidTransition { .. }));

    // paused
    let t = create(&store, "p", "dev", &[], resolver.clone()).await;
    store.start(t.id, "dev", false).await.unwrap();
    store.pause(t.id, "dev").await.unwrap();
    let err = store
        .confirm(t.id, "dev", confirm_req(Some("wip"), None))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidTransition { .. }));

    // closed
    let t = create(&store, "d", "dev", &[], resolver).await;
    store.start(t.id, "dev", false).await.unwrap();
    store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap();
    let err = store
        .confirm(t.id, "dev", confirm_req(Some("wip"), None))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidTransition { .. }));
}

#[tokio::test]
async fn pause_moves_to_paused_without_a_gate_and_resume_returns() {
    let store = TaskStore::open_in_memory().await;
    let t = create(&store, "pausable", "dev", &[], roster_resolver()).await;
    store.start(t.id, "dev", false).await.unwrap();

    let t = store.pause(t.id, "dev").await.unwrap();
    assert_eq!(t.status, "paused");

    // Notes still land while paused: the trail is not the machine.
    store
        .note(t.id, "dev", "parked mid-work".to_string())
        .await
        .unwrap();

    let t = store.resume(t.id, "dev", false).await.unwrap();
    assert_eq!(t.status, "in_progress");
    let (_, events) = store.get(t.id).await.unwrap();
    let names: Vec<_> = events.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"pause") && names.contains(&"resume"));
}

#[tokio::test]
async fn resume_runs_the_serial_gate_and_force_is_audited() {
    let store = TaskStore::open_in_memory().await;
    let resolver = roster_resolver();
    let t = create(&store, "resumable", "dev", &[], resolver.clone()).await;
    store.start(t.id, "dev", false).await.unwrap();
    store.pause(t.id, "dev").await.unwrap();

    // While resumable is paused, another task takes the assignee's
    // only in_progress slot: resuming now hits the serial gate.
    let held = create(&store, "held", "dev", &[], resolver).await;
    store.start(held.id, "dev", false).await.unwrap();

    let err = store.resume(t.id, "dev", false).await.unwrap_err();
    match err {
        Error::SerialGate { blocked_by, .. } => assert_eq!(blocked_by, held.id),
        other => panic!("expected serial gate, got {other:?}"),
    }
    store.resume(t.id, "dev", true).await.unwrap();
    let (_, events) = store.get(t.id).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "action" && e.name == "force_resume")
    );
}

#[tokio::test]
async fn resume_does_not_rebase_the_confirmation_window() {
    let store = TaskStore::open_in_memory().await;
    let t = create(&store, "window", "dev", &["r1"], roster_resolver()).await;
    store.start(t.id, "dev", false).await.unwrap();
    store
        .confirm(t.id, "agent-r1", confirm_req(None, None))
        .await
        .unwrap();

    // A pause/resume pair opens no new cycle: the pre-pause confirmation
    // still satisfies the close gate.
    store.pause(t.id, "dev").await.unwrap();
    store.resume(t.id, "dev", false).await.unwrap();
    let t = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap();
    assert_eq!(t.status, "closed");
}

#[tokio::test]
async fn export_json_shape_is_stable() {
    let store = TaskStore::open_in_memory().await;
    let t = create(&store, "exported", "dev", &["r1"], roster_resolver()).await;
    store.start(t.id, "dev", false).await.unwrap();
    store
        .confirm(t.id, "agent-r1", confirm_req(None, None))
        .await
        .unwrap();
    store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap();

    let export = store.export(t.id).await.unwrap();
    let v = serde_json::to_value(&export).unwrap();
    assert_eq!(v["status"], "closed");
    assert_eq!(v["closed_reason"], "completed");
    assert_eq!(v["assignee"], "dev");
    assert_eq!(v["confirmers"][0], "agent-r1");
    assert!(v["created_at"].as_str().unwrap().ends_with('Z'));
    let confirms: Vec<_> = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["name"] == "confirm")
        .collect();
    assert_eq!(confirms.len(), 1);
    assert_eq!(confirms[0]["actor"], "agent-r1");

    let closed = store
        .list(TaskFilter {
            status: Some(TaskStatus::Closed),
            archived: false,
            assignee: None,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(closed.len(), 1);
}

#[tokio::test]
async fn close_archives_dossier_content_addressed() {
    let store = TaskStore::open_in_memory().await;
    let tmp = tempfile::tempdir().unwrap();
    let dossier = tmp.path().join("dossier");
    std::fs::create_dir_all(dossier.join("notes")).unwrap();
    std::fs::write(dossier.join("plan.md"), "# plan\n").unwrap();
    std::fs::write(dossier.join("notes/a.md"), "a").unwrap();

    let t = store
        .create(
            TaskCreateRequest {
                dossier_path: Some(dossier.display().to_string()),
                ..spec("archived", "dev", &[])
            },
            "root",
            roster_resolver(),
        )
        .await
        .unwrap();
    store.start(t.id, "dev", false).await.unwrap();

    let blob_root = tempfile::tempdir().unwrap();
    let blobs: Arc<dyn kallip_blob_store::BlobStore> =
        Arc::new(LocalBackend::new(blob_root.path().join("blobs")));

    let t = store
        .close(
            t.id,
            "dev",
            ClosedReason::Completed,
            None,
            false,
            Some(blobs.clone()),
        )
        .await
        .unwrap();
    let hash = t.archive_hash.clone().expect("archive hash pointer set");

    let blob_id = kallip_blob_store::BlobId::parse(&hash).unwrap();
    assert!(blobs.stat(&blob_id).await.unwrap().is_some());

    let out = tempfile::tempdir().unwrap();
    kallip_task::archive::extract(blobs.as_ref(), &blob_id, &out.path().join("out"))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(out.path().join("out/plan.md")).unwrap(),
        "# plan\n"
    );
}

#[tokio::test]
async fn reopen_runs_the_serial_gate_and_force_is_audited() {
    let store = TaskStore::open_in_memory().await;
    let resolver = roster_resolver();
    let done = create(&store, "done", "dev", &[], resolver.clone()).await;
    store.start(done.id, "dev", false).await.unwrap();
    store
        .close(done.id, "dev", ClosedReason::Completed, None, true, None)
        .await
        .unwrap();

    let held = create(&store, "held", "dev", &[], resolver).await;
    store.start(held.id, "dev", false).await.unwrap();

    let err = store.reopen(done.id, "dev", false).await.unwrap_err();
    match err {
        Error::SerialGate { blocked_by, .. } => assert_eq!(blocked_by, held.id),
        other => panic!("expected serial gate, got {other:?}"),
    }
    store.reopen(done.id, "dev", true).await.unwrap();
    let (_, events) = store.get(done.id).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "action" && e.name == "force_reopen")
    );
}

#[tokio::test]
async fn reopen_invalidates_prior_cycle_confirmations() {
    let store = TaskStore::open_in_memory().await;
    let t = create(&store, "cycle", "dev", &["r1"], roster_resolver()).await;
    store.start(t.id, "dev", false).await.unwrap();
    store
        .confirm(t.id, "agent-r1", confirm_req(None, None))
        .await
        .unwrap();
    store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap();

    // Reopen starts a new confirmation cycle: the old confirmation no
    // longer counts.
    store.reopen(t.id, "dev", false).await.unwrap();
    let err = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::ConfirmationGate { ref missing } if missing == "agent-r1"),
        "{err}"
    );

    // A fresh confirmation satisfies the gate again.
    store
        .confirm(t.id, "agent-r1", confirm_req(None, None))
        .await
        .unwrap();
    let t = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap();
    assert_eq!(t.status, "closed");
}

#[tokio::test]
async fn close_with_a_registered_dossier_but_no_blob_store_is_an_error() {
    let store = TaskStore::open_in_memory().await;
    let t = store
        .create(
            TaskCreateRequest {
                dossier_path: Some("/tmp/does-not-matter".to_string()),
                ..spec("dossier", "dev", &[])
            },
            "root",
            roster_resolver(),
        )
        .await
        .unwrap();
    store.start(t.id, "dev", false).await.unwrap();
    let err = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::ArchiveNoBlobStore { .. }));
}

#[tokio::test]
async fn force_close_escapes_the_confirmation_gate_and_is_audited() {
    let store = TaskStore::open_in_memory().await;
    let t = create(&store, "urgent", "dev", &["r1"], roster_resolver()).await;
    store.start(t.id, "dev", false).await.unwrap();

    let err = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::ConfirmationGate { ref missing } if missing == "agent-r1"),
        "{err}"
    );

    store
        .close(t.id, "dev", ClosedReason::Completed, None, true, None)
        .await
        .unwrap();
    let (_, events) = store.get(t.id).await.unwrap();
    let escapes: Vec<_> = events
        .iter()
        .filter(|e| e.kind == "action" && e.name == "force_close")
        .collect();
    assert_eq!(escapes.len(), 1);
    assert_eq!(escapes[0].actor.as_deref(), Some("dev"));
    let payload: serde_json::Value =
        serde_json::from_str(escapes[0].payload.as_deref().unwrap()).unwrap();
    assert_eq!(payload["gates"], serde_json::json!(["confirmations"]));
    assert_eq!(
        payload["registered_confirmers"],
        serde_json::json!(["agent-r1"])
    );
}

#[tokio::test]
async fn association_windows_are_validated() {
    let store = TaskStore::open_in_memory().await;

    let err = store
        .create(
            TaskCreateRequest {
                inbox_id_start: Some(9),
                inbox_id_end: Some(3),
                ..spec("inverted", "dev", &[])
            },
            "root",
            no_resolver(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::AssociationInvalid { .. }));

    let err = store
        .create(
            TaskCreateRequest {
                inbox_id_start: Some(3),
                inbox_id_end: None,
                ..spec("one-sided", "dev", &[])
            },
            "root",
            no_resolver(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::AssociationInvalid { .. }));

    let err = store
        .create(
            TaskCreateRequest {
                room_seq_start: Some(1),
                room_seq_end: Some(2),
                ..spec("seq-without-room", "dev", &[])
            },
            "root",
            no_resolver(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::AssociationInvalid { .. }));
}

#[tokio::test]
async fn association_accepts_a_legal_window() {
    let store = TaskStore::open_in_memory().await;

    let task = store
        .create(
            TaskCreateRequest {
                inbox_id_start: Some(3),
                inbox_id_end: Some(9),
                ..spec("legal-window", "dev", &[])
            },
            "root",
            no_resolver(),
        )
        .await
        .unwrap();
    assert_eq!(task.inbox_id_start, Some(3));
    assert_eq!(task.inbox_id_end, Some(9));
}
use kallip_testkit::DevDir;

fn dev_tempdir(label: &str) -> DevDir {
    DevDir::new(label)
}

#[tokio::test]
async fn concurrent_starts_on_separate_pools_both_land() {
    // BEGIN IMMEDIATE semantics: the first statement of every write
    // transaction takes the write lock, so two starts for different
    // assignees queue on busy_timeout instead of a deferred snapshot
    // upgrade racing into SQLITE_BUSY.
    let dir = dev_tempdir("task-conc");
    let a = TaskStore::open(&dir.join("tasks.sqlite")).await.unwrap();
    let b = TaskStore::open(&dir.join("tasks.sqlite")).await.unwrap();
    let ta = a
        .create(spec("a", "alice", &[]), "root", no_resolver())
        .await
        .unwrap();
    let tb = b
        .create(spec("b", "bob", &[]), "root", no_resolver())
        .await
        .unwrap();

    let (ra, rb) = tokio::join!(a.start(ta.id, "alice", false), b.start(tb.id, "bob", false));
    assert!(ra.is_ok(), "first start failed: {ra:?}");
    assert!(rb.is_ok(), "second start failed: {rb:?}");
}

#[tokio::test]
async fn note_records_a_note_in_any_state() {
    let store = TaskStore::open_in_memory().await;
    let t = create(&store, "noted", "dev", &[], roster_resolver()).await;
    store.start(t.id, "dev", false).await.unwrap();

    let t = store
        .note(t.id, "dev", "mid-work observation".to_string())
        .await
        .unwrap();
    assert_eq!(t.status, "in_progress");

    store
        .close(t.id, "dev", ClosedReason::Completed, None, true, None)
        .await
        .unwrap();
    // Notes land on closed tasks too: the trail is append-only.
    store
        .note(t.id, "root", "post-mortem note".to_string())
        .await
        .unwrap();
    let (_, events) = store.get(t.id).await.unwrap();
    let notes: Vec<_> = events
        .iter()
        .filter(|e| e.kind == "action" && e.name == "note")
        .collect();
    assert_eq!(notes.len(), 2);
}

#[tokio::test]
async fn create_refuses_when_a_confirmer_name_resolves_to_nothing() {
    let store = TaskStore::open_in_memory().await;
    let err = store
        .create(spec("ghost", "dev", &["ghost"]), "root", no_resolver())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::ConfirmerUnresolved { ref confirmer } if confirmer == "ghost"));
}

#[tokio::test]
async fn archive_gate_rejects_open_tasks_and_force_is_audited() {
    let store = TaskStore::open_in_memory().await;
    let t = create(&store, "open", "dev", &[], roster_resolver()).await;

    let err = store.archive_task(t.id, "root", false).await.unwrap_err();
    assert!(
        matches!(err, Error::ArchiveGate { id, ref status } if id == t.id && status == "queued")
    );

    store.archive_task(t.id, "root", true).await.unwrap();
    let (_, events) = store.get(t.id).await.unwrap();
    let archive = events
        .iter()
        .find(|e| e.kind == "action" && e.name == "archive")
        .unwrap();
    let payload: serde_json::Value =
        serde_json::from_str(archive.payload.as_deref().unwrap()).unwrap();
    assert_eq!(payload["gate"], "archive_requires_closed");
    assert_eq!(payload["status"], "queued");
}

#[tokio::test]
async fn archived_tasks_leave_the_default_view() {
    let store = TaskStore::open_in_memory().await;
    let t = create(&store, "done", "dev", &[], roster_resolver()).await;
    store.start(t.id, "dev", false).await.unwrap();
    store
        .close(t.id, "dev", ClosedReason::Completed, None, true, None)
        .await
        .unwrap();
    store.archive_task(t.id, "root", false).await.unwrap();

    let active = store
        .list(TaskFilter {
            status: None,
            assignee: None,
            archived: false,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(active.is_empty());
    let archived = store
        .list(TaskFilter {
            status: None,
            assignee: None,
            archived: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].archived, 1);
    assert!(archived[0].archived_at.is_some());

    // The machine face carries the partition fields.
    let export = store.export(t.id).await.unwrap();
    let v = serde_json::to_value(&export).unwrap();
    assert_eq!(v["archived"], true);
    assert!(v["archived_at"].as_str().unwrap().ends_with('Z'));
}

#[tokio::test]
async fn close_with_a_damaged_stored_roster_fails_loudly() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("tasks.db");
    let store = TaskStore::open(&db_path).await.unwrap();
    let t = store
        .create(spec("corrupt roster", "dev", &[]), "root", no_resolver())
        .await
        .unwrap();
    store.start(t.id, "dev", false).await.unwrap();

    // Damage the stored roster out-of-band: close re-reads it and must
    // refuse loudly instead of folding the damaged record to zero
    // confirmers.
    let raw = sea_orm::Database::connect(format!("sqlite://{}?mode=rw", db_path.display()))
        .await
        .unwrap();
    raw.execute_unprepared(&format!(
        "UPDATE tasks SET confirmers = '{{oops' WHERE id = {}",
        t.id
    ))
    .await
    .unwrap();

    let err = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap_err();
    match err {
        Error::CorruptRecord { id, field } => {
            assert_eq!((id, field), (t.id, "confirmers"));
        }
        other => panic!("expected CorruptRecord, got {other}"),
    }
}

#[tokio::test]
async fn export_all_groups_events_under_the_right_task() {
    let store = TaskStore::open_in_memory().await;
    let a = create(&store, "alpha", "dev", &[], roster_resolver()).await;
    let b = create(&store, "beta", "ops", &[], roster_resolver()).await;
    store.start(a.id, "dev", false).await.unwrap();
    store.start(b.id, "ops", false).await.unwrap();
    store
        .note(a.id, "dev", "note on a".to_string())
        .await
        .unwrap();
    store
        .note(b.id, "ops", "note on b".to_string())
        .await
        .unwrap();

    let all = store.export_all().await.unwrap();
    let by_id: std::collections::BTreeMap<i64, usize> =
        all.iter().map(|e| (e.id, e.events.len())).collect();
    let (a_events, b_events) = (by_id[&a.id], by_id[&b.id]);
    assert_eq!(a_events, 3, "create + start + note");
    assert_eq!(b_events, 3, "create + start + note");

    let a_export = all.iter().find(|e| e.id == a.id).unwrap();
    let trail = serde_json::to_string(&a_export.events).unwrap();
    assert!(
        trail.contains("note on a") && !trail.contains("note on b"),
        "a's export carries a's trail, not b's"
    );
}

#[tokio::test]
async fn list_orders_newest_first_windows_and_pages() {
    use kallip_common::protocol::TaskTimeAxis;

    let store = TaskStore::open_in_memory().await;
    let resolver = roster_resolver();
    let a = create(&store, "a-first", "dev", &[], resolver.clone()).await;
    let b = create(&store, "b-second", "dev", &[], resolver.clone()).await;
    let c = create(&store, "c-third", "dev", &[], resolver).await;
    // Close b and c so the closed axis has both values and NULLs
    // (close is only legal from in_progress|review, so start them first).
    store.start(c.id, "dev", false).await.unwrap();
    store
        .close(c.id, "dev", ClosedReason::Completed, None, true, None)
        .await
        .unwrap();
    // The closed axis sorts on ended_at (second precision); a real gap
    // makes the order assertion meaningful instead of tie-dependent.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    store.start(b.id, "dev", false).await.unwrap();
    store
        .close(b.id, "dev", ClosedReason::Completed, None, true, None)
        .await
        .unwrap();

    async fn all(store: &TaskStore, time: TaskTimeAxis) -> Vec<kallip_task::Task> {
        store
            .list(TaskFilter {
                time,
                ..Default::default()
            })
            .await
            .unwrap()
    }

    let ids: Vec<i64> = all(&store, TaskTimeAxis::Updated)
        .await
        .iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(ids, vec![b.id, c.id, a.id], "updated axis is newest-first");
    let ids: Vec<i64> = all(&store, TaskTimeAxis::Closed)
        .await
        .iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(ids, vec![b.id, c.id, a.id], "closed desc, then NULLs last");

    let windowed = store
        .list(TaskFilter {
            time: TaskTimeAxis::Closed,
            since: Some(0),
            ..Default::default()
        })
        .await
        .unwrap();
    let ids: Vec<i64> = windowed.iter().map(|t| t.id).collect();
    assert_eq!(
        ids,
        vec![b.id, c.id],
        "the NULL row drops out of the window"
    );
    let future = store
        .list(TaskFilter {
            time: TaskTimeAxis::Closed,
            since: Some(i64::MAX / 2),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(future.is_empty(), "a future window matches nothing");

    let page = store
        .list(TaskFilter {
            limit: Some(1),
            offset: Some(1),
            ..Default::default()
        })
        .await
        .unwrap();
    let ids: Vec<i64> = page.iter().map(|t| t.id).collect();
    assert_eq!(ids, vec![c.id], "offset skips the newest, limit takes one");
}

// ---------------------------------------------------------------------------
// Actor attribution and confirmation reports (task 25/26): the acting agent
// comes from the caller's identity, the close gate compares ids fixed at
// create, and confirmations may carry a report body.
// ---------------------------------------------------------------------------

async fn confirmed_task(store: &TaskStore, name: &str, id: &str) -> kallip_task::Task {
    let resolver: ConfirmerResolver = {
        let (name, id) = (name.to_string(), id.to_string());
        Arc::new(move |n| (n == name).then(|| id.clone()))
    };
    let t = create(store, "gate", "dev", &[name], resolver).await;
    store.start(t.id, "dev", false).await.unwrap();
    t
}

/// The close gate compares the confirmation's actor against the ids fixed
/// at create. Filing as the agent id passes.
#[tokio::test]
async fn close_gate_accepts_a_confirmation_filed_as_the_agent_id() {
    let store = TaskStore::open_in_memory().await;
    let t = confirmed_task(&store, "reviewer-c", "agent-1").await;
    store
        .confirm(t.id, "agent-1", confirm_req(None, None))
        .await
        .unwrap();
    let t = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap();
    assert_eq!(t.status, "closed");
}

/// A confirmation from an outside actor is recorded but does not count
/// toward the gate: the roster is fixed at create.
#[tokio::test]
async fn confirm_from_an_outside_actor_does_not_count() {
    let store = TaskStore::open_in_memory().await;
    let t = confirmed_task(&store, "reviewer-c", "agent-1").await;
    store
        .confirm(t.id, "mystery", confirm_req(None, None))
        .await
        .unwrap();
    let (_, events) = store.get(t.id).await.unwrap();
    assert!(
        events.iter().any(|e| e.kind == "action"
            && e.name == "confirm"
            && e.actor.as_deref() == Some("mystery")),
        "the outside confirmation is recorded"
    );

    let err = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::ConfirmationGate { ref missing } if missing == "agent-1"),
        "{err}"
    );
}

/// The confirmation payload carries note and file independently; both
/// absent means no payload at all.
#[tokio::test]
async fn confirm_payload_shapes_follow_the_optional_fields() {
    let store = TaskStore::open_in_memory().await;
    let cases = [
        (
            Some("with both"),
            Some("r1"),
            serde_json::json!({"note": "with both", "file": "r1"}),
        ),
        (
            Some("note only"),
            None,
            serde_json::json!({"note": "note only"}),
        ),
        (None, Some("r2"), serde_json::json!({"file": "r2"})),
    ];
    for (i, (note, file, want)) in cases.into_iter().enumerate() {
        let t = store
            .create(
                spec("payload", &format!("dev{i}"), &[]),
                "root",
                roster_resolver(),
            )
            .await
            .unwrap();
        store.start(t.id, "dev", false).await.unwrap();
        store
            .confirm(
                t.id,
                "dev",
                TaskConfirmRequest {
                    note: note.map(str::to_string),
                    file: file.map(str::to_string),
                },
            )
            .await
            .unwrap();
        let (_, events) = store.get(t.id).await.unwrap();
        let payload: serde_json::Value = events
            .iter()
            .rfind(|e| e.kind == "action" && e.name == "confirm")
            .and_then(|e| e.payload.as_deref())
            .map(|p| serde_json::from_str(p).unwrap())
            .unwrap();
        assert_eq!(payload, want);
    }

    // Neither field: no payload at all.
    let t = create(&store, "payload", "dev", &[], roster_resolver()).await;
    store.start(t.id, "dev", false).await.unwrap();
    store
        .confirm(t.id, "dev", confirm_req(None, None))
        .await
        .unwrap();
    let (_, events) = store.get(t.id).await.unwrap();
    let payload = events
        .iter()
        .rfind(|e| e.kind == "action" && e.name == "confirm")
        .and_then(|e| e.payload.clone());
    assert!(payload.is_none());
}

/// A report over the shared cap is refused before anything is written.
#[tokio::test]
async fn confirm_report_over_cap_is_refused() {
    let store = TaskStore::open_in_memory().await;
    let t = create(&store, "big", "dev", &[], roster_resolver()).await;
    store.start(t.id, "dev", false).await.unwrap();
    let err = store
        .confirm(
            t.id,
            "dev",
            TaskConfirmRequest {
                note: None,
                file: Some("x".repeat(REPORT_MAX_BYTES + 1)),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        Error::ReportTooLarge { size, max }
            if size == REPORT_MAX_BYTES + 1 && max == REPORT_MAX_BYTES
    ));
}

/// The lightweight row reports confirmation-report presence, and the
/// envelope total follows the filter, not the page.
#[tokio::test]
async fn list_page_reports_has_reports_and_total_by_filter() {
    let store = TaskStore::open_in_memory().await;
    let resolver = roster_resolver();
    let plain = create(&store, "plain", "dev", &[], resolver.clone()).await;
    let reported = create(&store, "reported", "dev", &[], resolver.clone()).await;
    store.start(reported.id, "dev", false).await.unwrap();
    store
        .confirm(
            reported.id,
            "dev",
            TaskConfirmRequest {
                note: None,
                file: Some("body".into()),
            },
        )
        .await
        .unwrap();
    let done = create(&store, "done", "ops", &[], resolver).await;
    store.start(done.id, "ops", false).await.unwrap();
    store
        .close(done.id, "ops", ClosedReason::Completed, None, true, None)
        .await
        .unwrap();

    let has = |page: &kallip_common::protocol::TaskListPage, id: i64| {
        page.rows.iter().find(|r| r.id == id).map(|r| r.has_reports)
    };

    let active = store
        .list_page(TaskFilter {
            archived: false,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(active.total, 3);
    assert_eq!(
        has(&active, reported.id),
        Some(true),
        "confirmation had a report"
    );
    assert_eq!(has(&active, plain.id), Some(false));

    let closed = store
        .list_page(TaskFilter {
            archived: false,
            status: Some(TaskStatus::Closed),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(closed.total, 1, "the status filter moves the total");
    assert_eq!(closed.rows[0].id, done.id);
    assert_eq!(has(&closed, done.id), Some(false));
}
