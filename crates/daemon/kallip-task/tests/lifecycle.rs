//! Black-box lifecycle + gate tests through the public `TaskStore` API.

use std::sync::Arc;

use kallip_blob_store::LocalBackend;
use kallip_common::protocol::{TaskCheckpointRequest, TaskCreateRequest};
use kallip_task::store::TaskFilter;
use kallip_task::{ClosedReason, Error, TaskStatus, TaskStore};
use sea_orm::ConnectionTrait;

fn checkpoint(
    actor: &str,
    note: Option<&str>,
    receipt: bool,
    review: bool,
) -> TaskCheckpointRequest {
    TaskCheckpointRequest {
        actor: actor.to_string(),
        note: note.map(str::to_string),
        receipt,
        review,
        waiting: None,
    }
}

fn spec(title: &str, assignee: &str, seats: &[&str]) -> TaskCreateRequest {
    TaskCreateRequest {
        title: title.to_string(),
        creator: "root".to_string(),
        assignee: Some(assignee.to_string()),
        seats: seats.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

#[tokio::test]
async fn lifecycle_with_review_and_receipts() {
    let store = TaskStore::open_in_memory().await;
    let t = store
        .create(spec("batch", "dev", &["r1", "r2"]))
        .await
        .unwrap();
    assert_eq!(t.status, "queued");

    let t = store.start(t.id, "dev", false).await.unwrap();
    assert_eq!(t.status, "in_progress");
    assert!(t.started_at.is_some());

    // Dispatching the review round registers the roster the close gate
    // counts receipts against (r1, r2 from create).
    let t = store.dispatch(t.id, "root", None).await.unwrap();

    let t = store
        .checkpoint(t.id, checkpoint("dev", Some("wip"), false, false))
        .await
        .unwrap();
    assert_eq!(t.status, "in_progress");

    // Closing without receipts fails the close gate, naming every seat.
    let err = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::ReceiptGate { ref missing } if missing.contains("r1") && missing.contains("r2"))
    );

    let t = store
        .checkpoint(t.id, checkpoint("dev", None, false, true))
        .await
        .unwrap();
    assert_eq!(t.status, "review");

    // One receipt is still not enough.
    store
        .checkpoint(t.id, checkpoint("r1", None, true, false))
        .await
        .unwrap();
    let err = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::ReceiptGate { ref missing } if missing == "r2"));

    store
        .checkpoint(t.id, checkpoint("r2", None, true, false))
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
    let a = store.create(spec("a", "dev", &[])).await.unwrap();
    let b = store.create(spec("b", "dev", &[])).await.unwrap();
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
    let c = store.create(spec("c", "qa", &[])).await.unwrap();
    store.start(c.id, "qa", false).await.unwrap();
}

#[tokio::test]
async fn checkpoint_on_queued_is_an_invalid_transition() {
    let store = TaskStore::open_in_memory().await;
    let t = store.create(spec("x", "dev", &[])).await.unwrap();
    let err = store
        .checkpoint(t.id, checkpoint("dev", Some("wip"), false, false))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidTransition { .. }));
}

#[tokio::test]
async fn waiting_marker_is_not_a_state() {
    let store = TaskStore::open_in_memory().await;
    let t = store.create(spec("x", "dev", &[])).await.unwrap();
    store.start(t.id, "dev", false).await.unwrap();

    let mut op = checkpoint("dev", None, false, false);
    op.waiting = Some(true);
    let t = store.checkpoint(t.id, op).await.unwrap();
    assert_eq!(t.status, "in_progress", "waiting must not move the machine");
    assert_eq!(t.waiting, 1);
    assert!(t.waiting_since.is_some());

    let mut op = checkpoint("dev", None, false, false);
    op.waiting = Some(false);
    let t = store.checkpoint(t.id, op).await.unwrap();
    assert_eq!(t.waiting, 0);
    assert!(t.waiting_since.is_none());

    let (_, events) = store.get(t.id).await.unwrap();
    let names: Vec<_> = events.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"waiting_set") && names.contains(&"waiting_clear"));
}

#[tokio::test]
async fn export_json_shape_is_stable() {
    let store = TaskStore::open_in_memory().await;
    let t = store
        .create(spec("exported", "dev", &["r1"]))
        .await
        .unwrap();
    store.start(t.id, "dev", false).await.unwrap();
    store.dispatch(t.id, "root", None).await.unwrap();
    store
        .checkpoint(t.id, checkpoint("r1", None, true, false))
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
    assert_eq!(v["seats"][0], "r1");
    // ISO 8601 UTC on the machine face.
    assert!(v["created_at"].as_str().unwrap().ends_with('Z'));
    let receipts: Vec<_> = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["name"] == "receipt")
        .collect();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0]["actor"], "r1");

    // Filters partition.
    let closed = store
        .list(TaskFilter {
            status: Some(TaskStatus::Closed),
            archived: false,
            assignee: None,
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
        .create(TaskCreateRequest {
            dossier_path: Some(dossier.display().to_string()),
            ..spec("archived", "dev", &[])
        })
        .await
        .unwrap();
    store.start(t.id, "dev", false).await.unwrap();
    // Explicit zero-seat dispatch: no receipts are required at close.
    store.dispatch(t.id, "root", Some(vec![])).await.unwrap();

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

    // The pointer resolves to a real blob.
    let blob_id = kallip_blob_store::BlobId::parse(&hash).unwrap();
    assert!(blobs.stat(&blob_id).await.unwrap().is_some());

    // Round trip: extract restores the dossier content.
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
    let done = store.create(spec("done", "dev", &[])).await.unwrap();
    store.start(done.id, "dev", false).await.unwrap();
    store
        .close(done.id, "dev", ClosedReason::Completed, None, true, None)
        .await
        .unwrap();

    // Another task takes the assignee's only in_progress slot.
    let held = store.create(spec("held", "dev", &[])).await.unwrap();
    store.start(held.id, "dev", false).await.unwrap();

    // Reopening while the assignee holds another in_progress task hits
    // the serial gate; --force escapes with an auditable event.
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
async fn reopen_invalidates_prior_cycle_receipts() {
    let store = TaskStore::open_in_memory().await;
    let t = store.create(spec("cycle", "dev", &["r1"])).await.unwrap();
    store.start(t.id, "dev", false).await.unwrap();
    store.dispatch(t.id, "root", None).await.unwrap();
    store
        .checkpoint(t.id, checkpoint("r1", None, true, false))
        .await
        .unwrap();
    store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap();

    // Reopen starts a new review cycle: the old receipt no longer counts
    // and the cycle needs a fresh dispatch too.
    store.reopen(t.id, "dev", false).await.unwrap();
    let err = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::DispatchGate { id } if id == t.id));

    store.dispatch(t.id, "root", None).await.unwrap();
    let err = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::ReceiptGate { ref missing } if missing == "r1"));

    // A fresh receipt satisfies the gate again.
    store
        .checkpoint(t.id, checkpoint("r1", None, true, false))
        .await
        .unwrap();
    let t = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap();
    assert_eq!(t.status, "closed");
}

#[tokio::test]
async fn close_clears_the_waiting_marker() {
    let store = TaskStore::open_in_memory().await;
    let t = store.create(spec("waiting", "dev", &[])).await.unwrap();
    store.start(t.id, "dev", false).await.unwrap();
    let t = store
        .checkpoint(
            t.id,
            TaskCheckpointRequest {
                waiting: Some(true),
                ..checkpoint("dev", None, false, false)
            },
        )
        .await
        .unwrap();
    assert_eq!(t.waiting, 1);

    store.dispatch(t.id, "root", Some(vec![])).await.unwrap();
    let t = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap();
    assert_eq!(t.waiting, 0);
    assert!(t.waiting_since.is_none());
}

#[tokio::test]
async fn close_with_a_registered_dossier_but_no_blob_store_is_an_error() {
    let store = TaskStore::open_in_memory().await;
    let t = store
        .create(TaskCreateRequest {
            dossier_path: Some("/tmp/does-not-matter".to_string()),
            ..spec("dossier", "dev", &[])
        })
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
async fn force_close_escapes_the_receipt_gate_and_is_audited() {
    let store = TaskStore::open_in_memory().await;
    let t = store.create(spec("urgent", "dev", &["r1"])).await.unwrap();
    store.start(t.id, "dev", false).await.unwrap();
    store.dispatch(t.id, "root", None).await.unwrap();

    // Without a receipt in the current cycle the close gate refuses;
    // --force escapes it and leaves an auditable event behind.
    let err = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::ReceiptGate { ref missing } if missing == "r1"));

    store
        .close(t.id, "dev", ClosedReason::Completed, None, true, None)
        .await
        .unwrap();
    let (_, events) = store.get(t.id).await.unwrap();
    let escapes: Vec<_> = events
        .iter()
        .filter(|e| e.kind == "action" && e.name == "force_close")
        .collect();
    // Exactly one escape, naming its actor and the gates it skipped
    // (dispatch was recorded this cycle, so only the receipt gate).
    assert_eq!(escapes.len(), 1);
    assert_eq!(escapes[0].actor.as_deref(), Some("dev"));
    let payload: serde_json::Value =
        serde_json::from_str(escapes[0].payload.as_deref().unwrap()).unwrap();
    assert_eq!(payload["gates"], serde_json::json!(["receipts"]));
    assert_eq!(payload["registered_seats"], serde_json::json!(["r1"]));
}
#[tokio::test]
async fn association_windows_are_validated() {
    let store = TaskStore::open_in_memory().await;

    let err = store
        .create(TaskCreateRequest {
            inbox_id_start: Some(9),
            inbox_id_end: Some(3),
            ..spec("inverted", "dev", &[])
        })
        .await
        .unwrap_err();
    assert!(matches!(err, Error::AssociationInvalid { .. }));

    let err = store
        .create(TaskCreateRequest {
            inbox_id_start: Some(3),
            inbox_id_end: None,
            ..spec("one-sided", "dev", &[])
        })
        .await
        .unwrap_err();
    assert!(matches!(err, Error::AssociationInvalid { .. }));

    let err = store
        .create(TaskCreateRequest {
            room_seq_start: Some(1),
            room_seq_end: Some(2),
            ..spec("seq-without-room", "dev", &[])
        })
        .await
        .unwrap_err();
    assert!(matches!(err, Error::AssociationInvalid { .. }));
}

#[tokio::test]
async fn association_accepts_a_legal_window() {
    let store = TaskStore::open_in_memory().await;

    let task = store
        .create(TaskCreateRequest {
            inbox_id_start: Some(3),
            inbox_id_end: Some(9),
            ..spec("legal-window", "dev", &[])
        })
        .await
        .unwrap();
    assert_eq!(task.inbox_id_start, Some(3));
    assert_eq!(task.inbox_id_end, Some(9));
}
#[tokio::test]
async fn concurrent_starts_on_separate_pools_both_land() {
    // BEGIN IMMEDIATE semantics: the first statement of every write
    // transaction takes the write lock, so two starts for different
    // assignees queue on busy_timeout instead of a deferred snapshot
    // upgrade racing into SQLITE_BUSY.
    let dir = std::env::temp_dir().join(format!("kallip-task-conc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let a = TaskStore::open(&dir.join("tasks.sqlite")).await.unwrap();
    let b = TaskStore::open(&dir.join("tasks.sqlite")).await.unwrap();
    let ta = a.create(spec("a", "alice", &[])).await.unwrap();
    let tb = b.create(spec("b", "bob", &[])).await.unwrap();

    let (ra, rb) = tokio::join!(a.start(ta.id, "alice", false), b.start(tb.id, "bob", false));
    assert!(ra.is_ok(), "first start failed: {ra:?}");
    assert!(rb.is_ok(), "second start failed: {rb:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn annotate_records_a_note_in_any_state() {
    let store = TaskStore::open_in_memory().await;
    let t = store.create(spec("noted", "dev", &[])).await.unwrap();
    store.start(t.id, "dev", false).await.unwrap();

    let t = store
        .annotate(t.id, "dev", "mid-work observation".to_string())
        .await
        .unwrap();
    assert_eq!(t.status, "in_progress");

    store
        .close(t.id, "dev", ClosedReason::Completed, None, true, None)
        .await
        .unwrap();
    // Annotations land on closed tasks too: the trail is append-only.
    store
        .annotate(t.id, "root", "post-mortem note".to_string())
        .await
        .unwrap();
    let (_, events) = store.get(t.id).await.unwrap();
    let notes: Vec<_> = events
        .iter()
        .filter(|e| e.kind == "action" && e.name == "annotate")
        .collect();
    assert_eq!(notes.len(), 2);
}

#[tokio::test]
async fn archive_gate_rejects_open_tasks_and_force_is_audited() {
    let store = TaskStore::open_in_memory().await;
    let t = store.create(spec("open", "dev", &[])).await.unwrap();

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
    let t = store.create(spec("done", "dev", &[])).await.unwrap();
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
        })
        .await
        .unwrap();
    assert!(active.is_empty());
    let archived = store
        .list(TaskFilter {
            status: None,
            assignee: None,
            archived: true,
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
async fn close_requires_a_dispatch_in_the_cycle() {
    let store = TaskStore::open_in_memory().await;
    let t = store
        .create(spec("undispatched", "dev", &["r1"]))
        .await
        .unwrap();
    store.start(t.id, "dev", false).await.unwrap();

    let err = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::DispatchGate { id } if id == t.id));

    store.dispatch(t.id, "root", None).await.unwrap();
    store
        .checkpoint(t.id, checkpoint("r1", None, true, false))
        .await
        .unwrap();
    let t = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap();
    assert_eq!(t.status, "closed");
}

#[tokio::test]
async fn dispatch_rebases_the_receipt_boundary() {
    let store = TaskStore::open_in_memory().await;
    let t = store.create(spec("rebased", "dev", &["r1"])).await.unwrap();
    store.start(t.id, "dev", false).await.unwrap();
    store.dispatch(t.id, "root", None).await.unwrap();
    store
        .checkpoint(t.id, checkpoint("r1", None, true, false))
        .await
        .unwrap();

    // A re-dispatch starts a fresh round: the earlier receipt no longer
    // counts.
    store.dispatch(t.id, "root", None).await.unwrap();
    let err = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::ReceiptGate { ref missing } if missing == "r1"));
}

#[tokio::test]
async fn chain_op_requires_a_fresh_gate_report() {
    let store = TaskStore::open_in_memory().await;
    let t = store.create(spec("chained", "dev", &[])).await.unwrap();
    store.start(t.id, "dev", false).await.unwrap();

    let err = store
        .chain_op(t.id, "dev", "commit", Some("abc".into()), false)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::GateReportGate { id } if id == t.id));

    store
        .gate_report(t.id, "dev", "gate report before commit".to_string())
        .await
        .unwrap();
    store
        .chain_op(t.id, "dev", "commit", Some("abc".into()), false)
        .await
        .unwrap();

    // A second chain op needs its own gate report; --force escapes and
    // marks the event with the gate it skipped.
    let err = store
        .chain_op(t.id, "dev", "amend", None, false)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::GateReportGate { .. }));

    store
        .chain_op(t.id, "dev", "amend", None, true)
        .await
        .unwrap();
    let (_, events) = store.get(t.id).await.unwrap();
    let escapes: Vec<_> = events
        .iter()
        .filter(|e| e.kind == "action" && e.name == "chain_op")
        .filter(|e| {
            e.payload
                .as_deref()
                .map(|p| p.contains("\"gate\""))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(escapes.len(), 1);
}

#[tokio::test]
async fn zero_seat_dispatch_closes_without_receipts() {
    let store = TaskStore::open_in_memory().await;
    let t = store
        .create(spec("zero seats", "dev", &["r1"]))
        .await
        .unwrap();
    store.start(t.id, "dev", false).await.unwrap();

    // An explicit empty roster replaces the registered one: no seat is
    // asked for a receipt, so close passes with none filed.
    store
        .dispatch(t.id, "root", Some(Vec::new()))
        .await
        .unwrap();
    let t = store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap();
    assert_eq!(t.status, "closed");
}

#[tokio::test]
async fn dispatch_with_corrupt_stored_seats_fails_loudly() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("tasks.db");
    let store = TaskStore::open(&db_path).await.unwrap();
    let t = store
        .create(spec("corrupt seats", "dev", &["r1"]))
        .await
        .unwrap();
    store.start(t.id, "dev", false).await.unwrap();

    // Damage the stored roster out-of-band: dispatch(None) re-reads it and
    // must refuse loudly instead of folding the damaged record to zero seats.
    let raw = sea_orm::Database::connect(format!("sqlite://{}?mode=rw", db_path.display()))
        .await
        .unwrap();
    raw.execute_unprepared(&format!(
        "UPDATE tasks SET seats = '{{oops' WHERE id = {}",
        t.id
    ))
    .await
    .unwrap();

    let err = store.dispatch(t.id, "root", None).await.unwrap_err();
    match err {
        Error::CorruptRecord { id, field } => {
            assert_eq!((id, field), (t.id, "seats"));
        }
        other => panic!("expected CorruptRecord, got {other}"),
    }
}

#[tokio::test]
async fn dispatch_status_gate_accepts_only_active_states() {
    let store = TaskStore::open_in_memory().await;
    let t = store.create(spec("gated", "dev", &[])).await.unwrap();

    // Queued: not an active review cycle, dispatch is refused.
    let err = store.dispatch(t.id, "root", None).await.unwrap_err();
    assert!(matches!(err, Error::InvalidTransition { .. }), "{err}");

    store.start(t.id, "dev", false).await.unwrap();
    store.dispatch(t.id, "root", None).await.unwrap();

    // Review (moved by checkpoint) is equally dispatchable.
    store
        .checkpoint(t.id, checkpoint("dev", None, false, true))
        .await
        .unwrap();
    store.dispatch(t.id, "root", None).await.unwrap();

    // Closed: the machine is shut, dispatch is refused.
    store
        .close(t.id, "dev", ClosedReason::Completed, None, false, None)
        .await
        .unwrap();
    let err = store.dispatch(t.id, "root", None).await.unwrap_err();
    assert!(matches!(err, Error::InvalidTransition { .. }), "{err}");
}

#[tokio::test]
async fn export_all_groups_events_under_the_right_task() {
    let store = TaskStore::open_in_memory().await;
    let a = store.create(spec("alpha", "dev", &[])).await.unwrap();
    let b = store.create(spec("beta", "ops", &[])).await.unwrap();
    store.start(a.id, "dev", false).await.unwrap();
    store.start(b.id, "ops", false).await.unwrap();
    store
        .checkpoint(a.id, checkpoint("dev", Some("note on a"), false, false))
        .await
        .unwrap();
    store
        .checkpoint(b.id, checkpoint("ops", Some("note on b"), false, false))
        .await
        .unwrap();

    let all = store.export_all().await.unwrap();
    let by_id: std::collections::BTreeMap<i64, usize> =
        all.iter().map(|e| (e.id, e.events.len())).collect();
    let (a_events, b_events) = (by_id[&a.id], by_id[&b.id]);
    assert_eq!(a_events, 3, "create + start + checkpoint");
    assert_eq!(b_events, 3, "create + start + checkpoint");

    let a_export = all.iter().find(|e| e.id == a.id).unwrap();
    let trail = serde_json::to_string(&a_export.events).unwrap();
    assert!(
        trail.contains("note on a") && !trail.contains("note on b"),
        "a's export carries a's trail, not b's"
    );
}
