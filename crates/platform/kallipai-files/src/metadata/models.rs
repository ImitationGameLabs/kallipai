//! sea-orm entities for the metadata schema. One nested module per table,
//! mirroring `migration::m_20260831_01_init`.

/// The content-addressed blob catalog. One row per stored blob; `refcount`
/// is maintained by [`crate::metadata::repo`] transactions and reclaimed by
/// [`crate::gc`].
pub mod blob_rows {
    use sea_orm::entity::prelude::*;
    use time::OffsetDateTime;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "blob_rows")]
    pub struct Model {
        /// Full blob id (`sha256-` plus the 64-char lowercase hex digest).
        #[sea_orm(primary_key, column_type = "Text")]
        pub blob_id: String,
        /// Blob size in bytes, as ingested. Content addressing makes this a
        /// function of the id, so conflict-updates keep the original value.
        pub size: i64,
        /// Number of `file_records` rows pointing at the blob.
        pub refcount: i32,
        /// First time the blob was registered in the catalog.
        #[sea_orm(column_type = "TimestampWithTimeZone")]
        pub created_at: OffsetDateTime,
        /// When the blob's reference count hit zero (the start of the GC
        /// grace window); `None` while the blob is referenced or freshly
        /// re-registered. Cleared on conflict-insert, set on the removal
        /// that reaches zero (see [`crate::metadata::repo`]).
        #[sea_orm(column_type = "TimestampWithTimeZone", nullable)]
        pub freed_at: Option<OffsetDateTime>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// One uploaded file a principal owns, pointing at its content blob.
pub mod file_records {
    use sea_orm::entity::prelude::*;
    use time::OffsetDateTime;
    use uuid::Uuid;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "file_records")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: Uuid,
        /// Namespaced path the file is reachable under (the exact lookup
        /// key; no normalization happens at this layer).
        #[sea_orm(column_type = "Text")]
        pub space_path: String,
        /// Principal that owns the record.
        #[sea_orm(column_type = "Text")]
        pub owner: String,
        /// Content blob. References `blob_rows(blob_id)` with `RESTRICT`:
        /// the catalog row must outlive the record (see the migration notes).
        #[sea_orm(column_type = "Text")]
        pub blob_id: String,
        /// How the file arrived (e.g. which transfer delivered it). `None`
        /// until the send flow fills it in.
        #[sea_orm(column_type = "Text", nullable)]
        pub provenance: Option<String>,
        #[sea_orm(column_type = "TimestampWithTimeZone")]
        pub created_at: OffsetDateTime,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// Append-only delivery log: one row per completed transfer hop.
pub mod delivery_events {
    use sea_orm::entity::prelude::*;
    use time::OffsetDateTime;
    use uuid::Uuid;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "delivery_events")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: Uuid,
        /// When the delivery completed.
        #[sea_orm(column_type = "TimestampWithTimeZone")]
        pub happened_at: OffsetDateTime,
        /// Principal the blob was sent from.
        #[sea_orm(column_type = "Text")]
        pub from_principal: String,
        /// Principal the blob was delivered to.
        #[sea_orm(column_type = "Text")]
        pub to_principal: String,
        /// Delivered content.
        #[sea_orm(column_type = "Text")]
        pub blob_id: String,
        /// Sender's `file_records` id, when the send originated from a stored
        /// record.
        pub source_record_id: Option<Uuid>,
        /// Recipient's `file_records` id, once the receiving side stores one.
        pub target_record_id: Option<Uuid>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}
