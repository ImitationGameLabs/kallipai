//! The self-service enrollment-code mint: `POST /tagmata`.
//!
//! One DTO shared by the route and the CLI client, so the fields a caller needs
//! in hand after a mint (the code, the pending tagma's id, and the two
//! timestamps) cannot drift between the two sides.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MintEnrollmentCodeResponse {
    /// `sk-enroll-...` plaintext, returned once. The archeion keeps only its
    /// hash and the masked form.
    pub code: String,
    /// The pending tagma's id. Stable across the enroll transition, and the
    /// handle an operator needs to revoke an unused code.
    pub id: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}
