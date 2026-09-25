//! Inbox wire types for listing, reading, and summarizing agent messages.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// A message in an agent's inbox (the read-model).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxEntry {
    pub id: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub source: String,
    pub body: String,
    pub status: String,
}

/// Summary counts for an agent's inbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxSummary {
    pub total: usize,
    pub unread: usize,
}

/// Query params for `GET /agents/{id}/inbox`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct InboxListQuery {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}
