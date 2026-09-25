//! The task ledger: a coarse state machine, an append-only event trail, two
//! hard gates, and closed-task content-addressed archives, over one SQLite
//! database (`tasks.sqlite`, sibling of the other tagma stores).
//!
//! Layering: this crate owns the data plane only. Storage roots and blob
//! backends are handed in by the caller (the CLI resolves
//! `KALLIPAI_TAGMA_DATA_DIR`), so tests can point everything at a scratch
//! directory. Gates are enforced inside the transition transaction, not at
//! the CLI surface — the CLI is an entry point, the store is the law.

pub mod entities;
pub mod gates;
pub mod migration;
pub mod model;
pub mod store;

pub use entities::task::Model as Task;
pub use entities::task_event::Model as TaskEvent;
pub use kallipai_blob_store::BlobStore;
pub use kallipai_common::protocol::TaskExport;
pub use model::{ClosedReason, EventKind, TaskStatus, Transition};
pub use store::{ConfirmerResolver, TaskFilter, TaskStore};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("task {id} not found")]
    NotFound { id: i64 },

    #[error("invalid transition: task {id} is {from}, {action} needs {expected}")]
    InvalidTransition {
        id: i64,
        from: String,
        action: String,
        expected: String,
    },

    #[error(
        "serial gate: assignee {assignee} already has task {blocked_by} in \
         progress ('{title}'); --force to override (escape is recorded)"
    )]
    SerialGate {
        assignee: String,
        blocked_by: i64,
        title: String,
    },

    #[error(
        "confirmation gate: missing confirmations from registered confirmers: {missing}; \
         --force to override (escape is recorded)"
    )]
    ConfirmationGate { missing: String },
    #[error(
        "archive gate: task {id} is {status}; only closed tasks archive; --force to override (escape is recorded)"
    )]
    ArchiveGate { id: i64, status: String },

    #[error("dossier packs to {size} bytes, over the {max}-byte cap")]
    DossierTooLarge { size: usize, max: usize },
    /// A bulk payload's base64 does not decode: caller input, so the
    /// route face owes a 400, not an internal error.
    #[error("{what} is not valid base64")]
    MalformedBase64 { what: &'static str },
    /// A payload that claims to be an archive does not parse as one.
    #[error("{what} is not a valid tar archive")]
    MalformedArchive { what: &'static str },
    /// A confirm report body is not UTF-8 text: refused at write time,
    /// because the read-back path would otherwise surface it as a
    /// corrupt record (a 500) instead of the caller's mistake (a 400).
    #[error("confirm report is not valid UTF-8")]
    ReportNotUtf8,

    #[error("invalid association keys: {detail}")]
    AssociationInvalid { detail: String },
    #[error("task {id}: stored {field} is corrupt")]
    CorruptRecord { id: i64, field: &'static str },
    #[error("report is {size} bytes, over the {max}-byte limit")]
    ReportTooLarge { size: usize, max: usize },
    #[error("create gate: confirmer '{confirmer}' does not resolve to a registered identity")]
    ConfirmerUnresolved { confirmer: String },

    #[error(transparent)]
    Db(#[from] sea_orm::DbErr),

    #[error(transparent)]
    Blob(#[from] kallipai_blob_store::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Time(#[from] time::error::ComponentRange),

    #[error("{0}")]
    Other(String),
}
