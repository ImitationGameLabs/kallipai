//! The audit face: request audit rows, budget sums, and the key lifecycle
//! record surface.
//!
//! One row per forwarded request lands in `request_audits` once the
//! upstream response completes; the usage extraction decides whether token
//! counts ride along or stay NULL. A failed audit write is logged and
//! swallowed by the forwarding path: the upstream cost is already spent,
//! and failing the client's response after a successful generation would
//! invite a retry that double-bills.
//!
//! Budget numbers are BIGINT micro-units of the account currency (10^-6):
//! integer money, no floating point.

pub mod key_lifecycle_event;
pub mod management_event;
pub mod request_audit;
pub mod usage;

pub use usage::{ExtractedUsage, SseUsageScanner};

/// The audit actor recorded for static-token management changes (one
/// shared identity; a future dynamic credential writes its principal).
pub(crate) const MANAGEMENT_ACTOR: &str = "management-token";

use sea_orm::{ActiveModelTrait, ConnectionTrait, DatabaseConnection, DbErr, Set};
use sea_orm::{DatabaseBackend, Statement};
use time::OffsetDateTime;

/// Everything the forwarding path knows about a finished request.
pub(crate) struct RequestAuditRow {
    pub key_hash: Vec<u8>,
    pub tagma_id: String,
    pub profile_id: String,
    pub model: String,
    pub status_code: i32,
    pub duration_ms: i64,
    /// None = the upstream response carried no usage block.
    pub usage: Option<ExtractedUsage>,
}

/// Land one request audit row. Best-effort by contract: callers log a
/// failure and move on -- nothing about the forwarded response depends on
/// this write (see the module doc).
pub(crate) async fn record_request(
    db: &DatabaseConnection,
    row: RequestAuditRow,
) -> Result<(), DbErr> {
    let (prompt, completion, total) = match row.usage {
        Some(u) => (
            Some(u.prompt_tokens),
            Some(u.completion_tokens),
            Some(u.total_tokens),
        ),
        None => (None, None, None),
    };
    request_audit::ActiveModel {
        key_hash: Set(row.key_hash),
        tagma_id: Set(row.tagma_id),
        profile_id: Set(row.profile_id),
        model: Set(row.model),
        status_code: Set(row.status_code),
        duration_ms: Set(row.duration_ms),
        prompt_tokens: Set(prompt),
        completion_tokens: Set(completion),
        total_tokens: Set(total),
        // The pricing face lands in a later batch; until then every row
        // spends nothing and the max_budget check cannot trigger.
        cost_micros: Set(None),
        created_at: Set(OffsetDateTime::now_utc()),
        ..Default::default()
    }
    .insert(db)
    .await
    .map(|_| ())
}

/// One dimension's accumulated spend in micro-units, for the max_budget
/// check. Rows without pricing (cost_micros NULL, all of them this
/// batch) contribute nothing.
pub(crate) enum SpendBy {
    Tagma(String),
    Profile(String),
}

pub(crate) async fn spent_cost_micros(db: &DatabaseConnection, by: SpendBy) -> Result<i64, DbErr> {
    let (column, id) = match by {
        SpendBy::Tagma(v) => ("tagma_id", v),
        SpendBy::Profile(v) => ("profile_id", v),
    };
    // The column name is a code constant (never user input); the value
    // rides as a bound parameter.
    let sql = format!(
        "SELECT COALESCE(SUM(cost_micros), 0)::bigint AS spent \
         FROM request_audits WHERE {column} = $1"
    );
    // Raw SQL because SUM(bigint) returns numeric in Postgres and the
    // explicit ::bigint cast gives the driver a definite decode target
    // (the test harness uses the same raw-statement shape).
    let row = db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            sql,
            [id.into()],
        ))
        .await?
        .expect("aggregate query always returns one row");
    row.try_get("", "spent")
}

/// Record a key lifecycle transition. The single canonical write path:
/// the key admin face calls this inside the mint/revoke transaction, so
/// a lifecycle fact and the key state it describes commit or roll back
/// together (the management-face fail-closed rule: a lifecycle row
/// without its key change, or the reverse, is a corrupted trail).
pub(crate) async fn record_lifecycle_event<C: ConnectionTrait>(
    txn: &C,
    key_hash: &[u8],
    tagma_id: &str,
    event: &str,
    detail: Option<&str>,
) -> Result<(), DbErr> {
    key_lifecycle_event::ActiveModel {
        key_hash: Set(key_hash.to_vec()),
        tagma_id: Set(tagma_id.to_owned()),
        event: Set(event.to_owned()),
        detail: Set(detail.map(str::to_owned)),
        created_at: Set(OffsetDateTime::now_utc()),
        ..Default::default()
    }
    .insert(txn)
    .await
    .map(|_| ())
}
