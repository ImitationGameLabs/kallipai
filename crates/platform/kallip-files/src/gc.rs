//! Garbage collection for the metadata catalog and the blob store.
//!
//! Two mechanisms, complementary by design:
//!
//! - [`sweep`] reclaims catalog rows whose refcount has been zero for at
//!   least [`GcConfig::grace`] (stamped `freed_at` by
//!   [`crate::metadata::repo::remove_record`]), then unlinks their blob
//!   files. The grace span is the primary race guard: a same-content
//!   re-upload that lands within it re-registers the row (clearing
//!   `freed_at`), so by unlink time the row is live again and the re-check
//!   skips the file. The re-check between a reclaimed row's deletion and
//!   its unlink still has a residual micro-window -- a re-upload whose
//!   register commits exactly there would leave *a row with no file*,
//!   which is real data loss for that content, not a disk leak. The grace
//!   period makes hitting that window a matter of a re-upload stalling
//!   longer than `grace` between storing its bytes and registering them,
//!   not of ordinary timing; the window itself stays open (no claim
//!   protocol), and [`reconcile`] exists to surface exactly this class of
//!   drift -- it is a detector, not a repair path.
//! - [`reconcile`] audits both directions of the catalog-vs-store
//!   correspondence and reports drift; it deletes nothing. Catalog rows
//!   without files surface as [`ReconcileReport::missing_blobs`] (user
//!   visible: reads of that content fail), files without rows as
//!   [`ReconcileReport::orphan_files`] (invisible to users, cost disk).
//!   The periodic driver for both lives with the service binary (it
//!   arrives with the HTTP layer).

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use crate::metadata::Db;
use kallip_blob_store::blob_path;
use kallip_blob_store::{BlobId, BlobStore};
use sea_orm::{ConnectionTrait, DbErr, EntityTrait, Statement};
use time::OffsetDateTime;
use tracing::warn;

/// Knobs for the periodic GC driver.
#[derive(Debug, Clone)]
pub struct GcConfig {
    /// Maximum catalog rows reclaimed per sweep pass.
    pub batch: u32,
    /// Delay between sweep passes (the driver sleeps this between runs).
    pub interval: Duration,
    /// How long a row's refcount must have been zero (its `freed_at`
    /// stamp) before the sweep may reclaim it and unlink the file. The
    /// primary guard against unlinking a blob a same-content re-upload
    /// just re-registered; see the module docs for the residual window.
    pub grace: Duration,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            batch: 128,
            interval: Duration::from_secs(60),
            grace: Duration::from_secs(60),
        }
    }
}

/// Errors from GC passes: metadata access, blob-store access, or the
/// filesystem walk behind [`reconcile`].
#[derive(Debug, thiserror::Error)]
pub enum GcError {
    #[error(transparent)]
    Db(#[from] DbErr),
    #[error(transparent)]
    Store(#[from] kallip_blob_store::Error),
    #[error("blob store walk failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Result of one sweep pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Catalog rows deleted (all had refcount zero).
    pub catalog_reclaimed: u32,
    /// Blob files actually unlinked.
    pub unlinked: u32,
    /// Rows re-created by a concurrent registration between the batch
    /// delete and the re-check; their files were (correctly) left alone.
    pub skipped_live: u32,
}

/// Result of the unlink phase of a sweep.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UnlinkReport {
    pub unlinked: u32,
    pub skipped_live: u32,
}

/// Reclaim one batch of fully-graced zero-refcount catalog rows (the
/// caller supplies the cutoff: `now - grace`). Exposed separately from
/// [`sweep`] so tests can interleave a re-upload between the phases.
pub(crate) async fn reclaim_batch(
    db: &Db,
    batch: u32,
    cutoff: OffsetDateTime,
) -> Result<Vec<BlobId>, GcError> {
    let backend = db.get_database_backend();
    let rows = db
        .query_all(Statement::from_sql_and_values(
            backend,
            "DELETE FROM blob_rows r \
             WHERE r.blob_id IN \
             (SELECT c.blob_id FROM blob_rows c \
              WHERE c.refcount = 0 AND c.freed_at <= $1 LIMIT $2) \
             RETURNING r.blob_id",
            [cutoff.into(), i64::from(batch).into()],
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            let hex = row.try_get::<String>("", "blob_id")?;
            BlobId::parse(&hex)
                .map_err(|e| DbErr::Custom(format!("catalog row held malformed id: {e}")).into())
        })
        .collect()
}

/// Unlink the blob files of freshly reclaimed rows -- unless the row came
/// back: a re-check per id keeps a live row's file on disk (a re-upload
/// that re-registered within the grace span, or in the residual window
/// the module docs describe). Returns how many files were unlinked and
/// how many ids were skipped because their row was live again.
pub(crate) async fn unlink_freed(
    db: &Db,
    store: &dyn BlobStore,
    reclaimed: &[BlobId],
) -> Result<UnlinkReport, GcError> {
    let mut report = UnlinkReport::default();
    for id in reclaimed {
        let backend = db.get_database_backend();
        let live = db
            .query_one(Statement::from_sql_and_values(
                backend,
                "SELECT 1 FROM blob_rows WHERE blob_id = $1",
                [id.as_str().into()],
            ))
            .await?
            .is_some();
        if live {
            warn!(
                blob_id = %id,
                "gc: row re-registered before unlink; keeping blob file"
            );
            report.skipped_live += 1;
            continue;
        }
        store.delete(id).await?;
        report.unlinked += 1;
    }
    Ok(report)
}

/// Run one sweep pass: reclaim rows whose zero state outlasted the grace
/// period, then unlink their files behind the per-id re-check guard.
pub async fn sweep(
    db: &Db,
    store: &dyn BlobStore,
    config: &GcConfig,
) -> Result<SweepReport, GcError> {
    let cutoff = OffsetDateTime::now_utc() - config.grace;
    let reclaimed = reclaim_batch(db, config.batch, cutoff).await?;
    let mut report = SweepReport {
        catalog_reclaimed: reclaimed.len() as u32,
        ..Default::default()
    };
    let UnlinkReport {
        unlinked,
        skipped_live,
    } = unlink_freed(db, store, &reclaimed).await?;
    report.unlinked = unlinked;
    report.skipped_live = skipped_live;
    Ok(report)
}

/// Drift found by a [`reconcile`] audit. Both lists are findings, not
/// actions: reconcile never deletes.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Catalog rows whose blob file is missing from the store.
    pub missing_blobs: Vec<BlobId>,
    /// Blob files in the store with no catalog row.
    pub orphan_files: Vec<BlobId>,
}

/// Audit the catalog against the blob store, both directions, in one pass:
/// one catalog query, one filesystem walk, one `stat` per row.
pub async fn reconcile(db: &Db, root: &Path) -> Result<ReconcileReport, GcError> {
    let rows = crate::metadata::models::blob_rows::Entity::find()
        .all(db)
        .await?;
    let mut cataloged = HashSet::with_capacity(rows.len());
    for row in &rows {
        cataloged.insert(row.blob_id.as_str());
    }

    let mut report = ReconcileReport::default();
    for row in &rows {
        let id = BlobId::parse(&row.blob_id)
            .map_err(|e| DbErr::Custom(format!("catalog row held malformed id: {e}")))?;
        if !tokio::fs::try_exists(blob_path(root, &id))
            .await
            .unwrap_or(false)
        {
            warn!(blob_id = %row.blob_id, "reconcile: catalog row has no blob file");
            report.missing_blobs.push(id);
        }
    }

    for id in blob_files_on_disk(root).await? {
        if !cataloged.contains(id.as_str()) {
            warn!(blob_id = %id, "reconcile: blob file has no catalog row");
            report.orphan_files.push(id);
        }
    }
    Ok(report)
}

/// Every blob file currently in the store, parsed into ids. Files whose
/// names are not valid blob ids are warned about and skipped (they cannot
/// be content the catalog references).
/// A missing `blobs/` directory enumerates as empty: the per-row `stat`
/// above already reports every catalog row as missing, so the audit
/// loses nothing and a not-yet-initialized store root stays auditable.
async fn blob_files_on_disk(root: &Path) -> Result<Vec<BlobId>, std::io::Error> {
    let blobs = root.join("blobs");
    let mut out = Vec::new();
    let Ok(mut buckets) = tokio::fs::read_dir(&blobs).await else {
        return Ok(out);
    };
    while let Some(bucket) = buckets.next_entry().await? {
        if !bucket.file_type().await?.is_dir() {
            continue;
        }
        let mut entries = tokio::fs::read_dir(bucket.path()).await?;
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            match BlobId::parse(&name) {
                Ok(id) => out.push(id),
                Err(_) => warn!(file = %name, "reconcile: non-blob file in blob store"),
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::repo::{register_upload, remove_record};
    use crate::test_helpers::migrated_test_db;
    use kallip_blob_store::LocalBackend;

    fn id_from(content: &[u8]) -> BlobId {
        use sha2::{Digest, Sha256};
        let digest: [u8; 32] = Sha256::digest(content).into();
        BlobId::from_digest(digest)
    }

    async fn store_with(root: &Path, content: &[u8]) -> (BlobId, LocalBackend) {
        let store = LocalBackend::new(root);
        let mut reader = std::io::Cursor::new(content);
        let id = store.put(&mut reader).await.expect("put blob").id;
        (id, store)
    }

    #[tokio::test]
    async fn sweep_unlinks_blobs_whose_last_record_went_away() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let root = dir.path();
        let (id, store) = store_with(root, b"gc me").await;
        let db = migrated_test_db().await;

        let record = register_upload(&db, &id, 5, "/t/x", "owner", None)
            .await
            .expect("register");
        let out = remove_record(&db, record)
            .await
            .expect("remove")
            .expect("exists");
        assert!(out.blob_freed);

        // grace already elapsed: the freed stamp is older than the span.
        let config = GcConfig {
            grace: Duration::ZERO,
            ..Default::default()
        };
        let report = sweep(&db, &store, &config).await.expect("sweep");
        assert_eq!(
            report,
            SweepReport {
                catalog_reclaimed: 1,
                unlinked: 1,
                skipped_live: 0
            }
        );
        assert!(
            store.stat(&id).await.expect("stat").is_none(),
            "file must be gone"
        );
    }

    #[tokio::test]
    async fn sweep_leaves_referenced_blobs_alone() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let root = dir.path();
        let (id, store) = store_with(root, b"kept").await;
        let db = migrated_test_db().await;

        register_upload(&db, &id, 4, "/t/live", "owner", None)
            .await
            .expect("register");

        let report = sweep(&db, &store, &GcConfig::default())
            .await
            .expect("sweep");
        assert_eq!(report, SweepReport::default());
        assert!(store.stat(&id).await.expect("stat").is_some());
    }

    #[tokio::test]
    async fn unlink_skips_files_re_registered_mid_sweep() {
        // The race sweep's re-check guards against: reclaim_batch removes a
        // zero-refcount row, then a concurrent upload re-registers the same
        // content before the unlink phase runs. Interleave that upload here
        // and the file must survive with its (live) row.
        let dir = tempfile::tempdir().expect("tmpdir");
        let root = dir.path();
        let (id, store) = store_with(root, b"racing").await;
        let db = migrated_test_db().await;

        let record = register_upload(&db, &id, 6, "/t/a", "owner", None)
            .await
            .expect("register");
        let out = remove_record(&db, record)
            .await
            .expect("remove")
            .expect("exists");
        assert!(out.blob_freed);

        // Cutoff now: the just-stamped freed_at qualifies for reclaim.
        let cutoff = OffsetDateTime::now_utc();
        let reclaimed = reclaim_batch(&db, 16, cutoff).await.expect("reclaim");
        assert_eq!(reclaimed, vec![id.clone()]);

        // The "concurrent" upload lands between the two phases.
        register_upload(&db, &id, 6, "/t/b", "owner", None)
            .await
            .expect("re-register");

        let report = unlink_freed(&db, &store, &reclaimed).await.expect("unlink");
        assert_eq!(
            report,
            UnlinkReport {
                unlinked: 0,
                skipped_live: 1
            }
        );
        assert!(
            store.stat(&id).await.expect("stat").is_some(),
            "live row's file must not be unlinked"
        );
        let row = crate::metadata::models::blob_rows::Entity::find_by_id(id.as_str())
            .one(&db)
            .await
            .expect("query")
            .expect("row is live");
        assert_eq!(row.refcount, 1);
        assert!(
            row.freed_at.is_none(),
            "re-registration must clear the grace stamp"
        );
        assert_eq!(id_from(b"racing"), id);
    }

    #[tokio::test]
    async fn sweep_spares_rows_still_inside_the_grace_window() {
        // A removal stamps freed_at now; a sweep whose grace spans that
        // stamp must reclaim nothing yet -- the window a re-upload needs.
        let dir = tempfile::tempdir().expect("tmpdir");
        let root = dir.path();
        let (id, store) = store_with(root, b"young").await;
        let db = migrated_test_db().await;

        let record = register_upload(&db, &id, 5, "/t/y", "owner", None)
            .await
            .expect("register");
        remove_record(&db, record)
            .await
            .expect("remove")
            .expect("exists");

        let config = GcConfig {
            grace: Duration::from_secs(3600),
            ..Default::default()
        };
        let report = sweep(&db, &store, &config).await.expect("sweep");
        assert_eq!(report, SweepReport::default());
        assert!(store.stat(&id).await.expect("stat").is_some(), "file kept");
        let row = crate::metadata::models::blob_rows::Entity::find_by_id(id.as_str())
            .one(&db)
            .await
            .expect("query")
            .expect("row kept until grace elapses");
        assert!(row.freed_at.is_some(), "stamp set when the count hit zero");
    }

    #[tokio::test]
    async fn reconcile_reports_rows_without_files() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let db = migrated_test_db().await;

        // Registered but the store lost the file (or it never landed).
        let id = id_from(b"lost content");
        register_upload(&db, &id, 12, "/t/gone", "owner", None)
            .await
            .expect("register");

        let report = reconcile(&db, dir.path()).await.expect("reconcile");
        assert_eq!(report.missing_blobs, vec![id]);
        assert!(report.orphan_files.is_empty());
    }

    #[tokio::test]
    async fn reconcile_reports_files_without_rows() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let root = dir.path();
        let (orphan, _store) = store_with(root, b"orphaned content").await;
        let (referenced, store) = store_with(root, b"referenced content").await;
        let db = migrated_test_db().await;

        register_upload(&db, &referenced, 18, "/t/kept", "owner", None)
            .await
            .expect("register");

        // A stray non-blob file is skipped (warned), never reported.
        let stray_bucket = root.join("blobs").join(orphan.bucket());
        std::fs::create_dir_all(&stray_bucket).expect("mkdir stray bucket");
        std::fs::write(stray_bucket.join("stray"), b"?").expect("write stray file");

        let report = reconcile(&db, root).await.expect("reconcile");
        assert_eq!(report.orphan_files, vec![orphan]);
        assert!(report.missing_blobs.is_empty());
        assert!(
            store.stat(&referenced).await.expect("stat").is_some(),
            "referenced file untouched"
        );
    }
}
