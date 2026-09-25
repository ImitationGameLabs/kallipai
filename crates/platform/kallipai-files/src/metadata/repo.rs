//! Reference-counted writes over the metadata store.
//!
//! Invariants this module maintains: a blob's `refcount` equals the number
//! of `file_records` rows pointing at it, and a count never goes
//! negative. [`register_upload`] and [`remove_record`] each move both
//! sides inside one transaction. A row whose count reaches zero is
//! stamped with `freed_at` and left in place; the GC (`crate::gc`)
//! reclaims it -- and then unlinks the blob file -- only after the
//! zero state has held past the configured grace period, which gives a
//! same-content re-upload racing the removal the whole window to land.

use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DbErr, EntityTrait,
    QueryFilter, QueryOrder, QuerySelect, Statement, TransactionTrait,
};
use time::OffsetDateTime;
use uuid::Uuid;

use super::Db;
use crate::metadata::models::file_records;
use kallipai_blob_store::BlobId;

/// Outcome of removing a file record.
#[derive(Debug)]
pub struct Removal {
    /// The removed record's blob.
    pub blob_id: BlobId,
    /// True when the removal dropped the refcount to zero: the blob file
    /// has no references left and becomes sweep-eligible (the catalog row
    /// remains at zero until [`crate::gc::sweep`] reclaims it). Purely
    /// informational; no caller action is required.
    pub blob_freed: bool,
}

/// Record one upload: bump the blob's refcount (creating the catalog row on
/// first sight) and insert the file record, atomically. Returns the new
/// record id.
///
/// The refcount bump is one raw upsert-and-return: it reads the pre-update
/// value (`refcount = blob_rows.refcount + 1`), clears any stale
/// `freed_at`, and returns the new count in a single round trip. The
/// conflict-aware insert API can express the self-referential increment,
/// but as an update without the same-query return, so the raw form wins
/// on clarity and round trips. Concurrent uploads of the same blob
/// serialize on the row lock and each leave the count one higher.
pub async fn register_upload(
    db: &Db,
    blob_id: &BlobId,
    size: i64,
    space_path: &str,
    owner: &str,
    provenance: Option<&str>,
) -> Result<Uuid, DbErr> {
    let txn = db.begin().await?;
    let backend = txn.get_database_backend();
    let now = OffsetDateTime::now_utc();

    txn.query_one(Statement::from_sql_and_values(
        backend,
        "INSERT INTO blob_rows (blob_id, size, refcount, created_at) \
         VALUES ($1, $2, 1, $3) \
         ON CONFLICT (blob_id) DO UPDATE SET refcount = blob_rows.refcount + 1, freed_at = NULL \
         RETURNING refcount",
        [blob_id.as_str().into(), size.into(), now.into()],
    ))
    .await?
    .ok_or_else(|| DbErr::Custom("refcount upsert returned no row".to_owned()))?;

    let record_id = Uuid::new_v4();
    file_records::ActiveModel {
        id: Set(record_id),
        space_path: Set(space_path.to_owned()),
        owner: Set(owner.to_owned()),
        blob_id: Set(blob_id.as_str().to_owned()),
        provenance: Set(provenance.map(str::to_owned)),
        created_at: Set(now),
    }
    .insert(&txn)
    .await?;

    txn.commit().await?;
    Ok(record_id)
}

/// Append one delivery event to the log.
pub async fn record_delivery(
    db: &Db,
    from_principal: &str,
    to_principal: &str,
    blob_id: &BlobId,
    source_record_id: Option<Uuid>,
    target_record_id: Option<Uuid>,
) -> Result<Uuid, DbErr> {
    let event = crate::metadata::models::delivery_events::ActiveModel {
        id: Set(Uuid::new_v4()),
        happened_at: Set(OffsetDateTime::now_utc()),
        from_principal: Set(from_principal.to_owned()),
        to_principal: Set(to_principal.to_owned()),
        blob_id: Set(blob_id.as_str().to_owned()),
        source_record_id: Set(source_record_id),
        target_record_id: Set(target_record_id),
    }
    .insert(db)
    .await?;
    Ok(event.id)
}

/// Remove a file record and release its blob reference, atomically.
///
/// Returns `Ok(None)` when the record does not exist (nothing changed; the
/// caller decides whether that is an error). When the release drops the
/// refcount to zero, [`Removal::blob_freed`] is set and the row is
/// stamped with `freed_at`: reclaim-eligible once the zero state holds
/// past the GC grace period. A same-content re-upload landing in the
/// meantime re-registers the row, clearing `freed_at` -- which is what
/// keeps the racing removal from ever taking the file away.
pub async fn remove_record(db: &Db, record_id: Uuid) -> Result<Option<Removal>, DbErr> {
    let txn = db.begin().await?;
    let backend = txn.get_database_backend();

    let deleted = txn
        .query_one(Statement::from_sql_and_values(
            backend,
            "DELETE FROM file_records WHERE id = $1 RETURNING blob_id",
            [record_id.into()],
        ))
        .await?;
    let Some(blob) = deleted.and_then(|row| row.try_get::<String>("", "blob_id").ok()) else {
        // Unknown record: dropping the transaction leaves nothing changed.
        return Ok(None);
    };

    // The `refcount > 0` guard makes a drifted state (record removed,
    // but no catalog row to decrement) harmless rather than a negative
    // count; rows are never deleted here, only by the GC sweep.
    let freed = txn
        .query_one(Statement::from_sql_and_values(
            backend,
            "UPDATE blob_rows SET refcount = refcount - 1, \
             freed_at = CASE WHEN refcount = 1 THEN $2 ELSE freed_at END \
             WHERE blob_id = $1 AND refcount > 0 RETURNING refcount",
            [blob.as_str().into(), OffsetDateTime::now_utc().into()],
        ))
        .await?
        .and_then(|row| row.try_get::<i32>("", "refcount").ok())
        .map(|refcount| refcount == 0)
        .unwrap_or(false);

    txn.commit().await?;
    let blob_id =
        BlobId::parse(&blob).map_err(|e| DbErr::Custom(format!("malformed blob id: {e}")))?;
    Ok(Some(Removal {
        blob_id,
        blob_freed: freed,
    }))
}

/// Record one delivery: a new file record on the target path pointing at the
/// same blob (refcount + 1, zero data copy) plus the delivery event, in one
/// transaction. This is the send path's whole write -- record, reference,
/// and audit trail land together or not at all, so a delivered file is
/// never visible without its event and an event never lands without its
/// record. The write is the service acting as delivery agent: it does not
/// pass through ACL checks, because the send handler has already
/// authorized the source read and validated the target. `size` feeds the
/// catalog row's insert arm only (a delivery of a live blob always finds
/// the row present, so the arm is a drifted-state repair); the caller
/// reads it from the blob store.
#[allow(clippy::too_many_arguments)]
pub async fn register_delivery(
    db: &Db,
    source_record_id: Uuid,
    blob_id: &BlobId,
    size: i64,
    target_space_path: &str,
    target_owner: &str,
    provenance: &str,
    from_principal: &str,
    to_principal: &str,
) -> Result<(Uuid, Uuid), DbErr> {
    let txn = db.begin().await?;
    let backend = txn.get_database_backend();
    let now = OffsetDateTime::now_utc();

    txn.query_one(Statement::from_sql_and_values(
        backend,
        "INSERT INTO blob_rows (blob_id, size, refcount, created_at) \
         VALUES ($1, $2, 1, $3) \
         ON CONFLICT (blob_id) DO UPDATE SET refcount = blob_rows.refcount + 1, freed_at = NULL \
         RETURNING refcount",
        [blob_id.as_str().into(), size.into(), now.into()],
    ))
    .await?
    .ok_or_else(|| DbErr::Custom("refcount upsert returned no row".to_owned()))?;

    let record_id = Uuid::new_v4();
    file_records::ActiveModel {
        id: Set(record_id),
        space_path: Set(target_space_path.to_owned()),
        owner: Set(target_owner.to_owned()),
        blob_id: Set(blob_id.as_str().to_owned()),
        provenance: Set(Some(provenance.to_owned())),
        created_at: Set(now),
    }
    .insert(&txn)
    .await?;

    let event = crate::metadata::models::delivery_events::ActiveModel {
        id: Set(Uuid::new_v4()),
        happened_at: Set(now),
        from_principal: Set(from_principal.to_owned()),
        to_principal: Set(to_principal.to_owned()),
        blob_id: Set(blob_id.as_str().to_owned()),
        source_record_id: Set(Some(source_record_id)),
        target_record_id: Set(Some(record_id)),
    }
    .insert(&txn)
    .await?;

    txn.commit().await?;
    Ok((record_id, event.id))
}

/// List delivery events for the admin query face, oldest first, optionally
/// filtered to one blob. Read-only; the admin route caps `limit`.
pub async fn list_delivery_events(
    db: &Db,
    blob_id: Option<&str>,
    limit: u64,
) -> Result<Vec<crate::metadata::models::delivery_events::Model>, DbErr> {
    let mut query = crate::metadata::models::delivery_events::Entity::find()
        .order_by_asc(crate::metadata::models::delivery_events::Column::HappenedAt);
    if let Some(blob_id) = blob_id {
        query = query.filter(crate::metadata::models::delivery_events::Column::BlobId.eq(blob_id));
    }
    query.limit(limit).all(db).await
}

/// Build the LIKE pattern for a literal prefix: escape the metacharacters
/// (`\`, `%`, `_`) so they match verbatim (Postgres' default LIKE escape is
/// the backslash), then append the trailing `%` ourselves -- sea-orm's
/// `starts_with` does no escaping, so it cannot carry this contract.
fn like_prefix_pattern(prefix: &str) -> String {
    let mut pattern = String::with_capacity(prefix.len() + 1);
    for ch in prefix.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern.push('%');
    pattern
}

/// List records under a path prefix, path-ascending, capped. The read is
/// one transaction (records + their blob sizes), so a size always belongs
/// to the same snapshot as the record row naming it. Prefix matching is
/// literal: the LIKE metacharacters (`\`, `%`, `_`) are escaped before the
/// pattern is built, so the caller's narrowing string matches verbatim,
/// never as a pattern. This is the
/// query face of the listing route; authorization stays with the caller
/// (the route's per-row matrix decision), which keeps this function a
/// pure catalog read.
pub async fn list_records_with_sizes(
    db: &Db,
    path_prefix: &str,
    limit: u64,
) -> Result<Vec<(file_records::Model, i64)>, DbErr> {
    use crate::metadata::models::blob_rows;

    let txn = db.begin().await?;
    let records = file_records::Entity::find()
        .filter(file_records::Column::SpacePath.like(like_prefix_pattern(path_prefix)))
        .order_by_asc(file_records::Column::SpacePath)
        .limit(limit)
        .all(&txn)
        .await?;
    let blob_ids: Vec<String> = records.iter().map(|r| r.blob_id.clone()).collect();
    let sizes: std::collections::HashMap<String, i64> = blob_rows::Entity::find()
        .filter(blob_rows::Column::BlobId.is_in(blob_ids))
        .all(&txn)
        .await?
        .into_iter()
        .map(|b| (b.blob_id, b.size))
        .collect();
    let rows = records
        .into_iter()
        .map(|record| {
            // blob_rows is the RESTRICT parent of file_records.blob_id, so a
            // record's size is always in the catalog. A miss is corruption:
            // fail loud (the route's DbErr mapping turns it into a 500),
            // never a silent 0 pretending the file is empty.
            let size = sizes.get(&record.blob_id).copied().ok_or_else(|| {
                DbErr::Custom(format!("blob row missing for record {}", record.id))
            })?;
            Ok((record, size))
        })
        .collect::<Result<Vec<_>, DbErr>>()?;
    Ok(rows)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::migrated_test_db;
    use sea_orm::EntityTrait;

    fn blob(hex_tail: &str) -> BlobId {
        BlobId::parse(&format!("sha256-{hex_tail}")).expect("valid blob id")
    }

    #[tokio::test]
    async fn register_then_remove_round_trips_refcounts() {
        let db = migrated_test_db().await;
        let id = blob(&"ab".repeat(32));

        let first = register_upload(&db, &id, 5, "/t/a", "alice", Some("from T1"))
            .await
            .expect("first register");
        let second = register_upload(&db, &id, 5, "/t/b", "bob", None)
            .await
            .expect("second register");
        assert_ne!(first, second);

        let out = remove_record(&db, first)
            .await
            .expect("remove")
            .expect("exists");
        assert_eq!(out.blob_id, id);
        assert!(!out.blob_freed, "one reference left, row must stay");

        let out = remove_record(&db, second)
            .await
            .expect("remove")
            .expect("exists");
        assert!(out.blob_freed, "last reference marks the blob reclaimable");

        assert!(
            remove_record(&db, first).await.expect("remove").is_none(),
            "removing an already-removed record reports None"
        );

        let row = crate::metadata::models::blob_rows::Entity::find_by_id(id.as_str())
            .one(&db)
            .await
            .expect("query")
            .expect("catalog row still present");
        assert_eq!(row.refcount, 0, "row stays at zero until the sweep");
        assert!(
            row.freed_at.is_some(),
            "the zero state is stamped as the grace-window start"
        );
    }

    #[tokio::test]
    async fn concurrent_registrations_serialize_to_exact_refcount() {
        let db = std::sync::Arc::new(migrated_test_db().await);
        let id = blob(&"cd".repeat(32));

        // Eight racing uploads of the same blob: the single-statement upsert
        // serializes on the row lock, so no increment can be lost.
        let mut tasks = Vec::new();
        for n in 0..8 {
            let db = db.clone();
            let id = id.clone();
            tasks.push(tokio::spawn(async move {
                register_upload(&db, &id, 1, &format!("/t/{n}"), "owner", None).await
            }));
        }
        for task in tasks {
            task.await.expect("join").expect("register");
        }

        let row = crate::metadata::models::blob_rows::Entity::find_by_id(id.as_str())
            .one(&*db)
            .await
            .expect("query")
            .expect("catalog row exists");
        assert_eq!(row.refcount, 8);
        assert_eq!(row.size, 1);

        // Draining all eight records: exactly one release must observe the
        // drop to zero. `find()` order is unspecified, so count the freed
        // flags instead of assuming which space path goes last.
        let records = crate::metadata::models::file_records::Entity::find()
            .all(&*db)
            .await
            .expect("records");
        let mut freed_count = 0;
        for record in records {
            let out = remove_record(&db, record.id)
                .await
                .expect("remove")
                .expect("exists");
            freed_count += u32::from(out.blob_freed);
        }
        assert_eq!(freed_count, 1, "exactly the final release frees the blob");
        let row = crate::metadata::models::blob_rows::Entity::find_by_id(id.as_str())
            .one(&*db)
            .await
            .expect("query")
            .expect("catalog row still present");
        assert_eq!(
            row.refcount, 0,
            "drained row stays until the GC sweep reclaims it"
        );
    }

    #[tokio::test]
    async fn record_delivery_appends_to_the_log() {
        let db = migrated_test_db().await;
        let id = blob(&"ef".repeat(32));

        let event = record_delivery(&db, "alice", "bob", &id, None, None)
            .await
            .expect("record delivery");
        let rows = crate::metadata::models::delivery_events::Entity::find()
            .all(&db)
            .await
            .expect("events");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, event);
        assert_eq!(rows[0].from_principal, "alice");
        assert_eq!(rows[0].blob_id, id.as_str());
    }
}
