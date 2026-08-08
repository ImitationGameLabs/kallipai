//! Admin (operator) DTOs shared by `kallip-agora` (the server) and
//! `kallip-agora-client` (the admin CLI's HTTP client). Defining the admin wire
//! contract in one place means the server and the client cannot drift apart.
//!
//! These are deliberately separate from the public `control` / `message` surface:
//! admin types are operator-facing and evolve with the admin tooling, not with
//! the relay/tagma contract.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Page-size + opaque-cursor query shared by all paginated admin list endpoints.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PageQuery {
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub cursor: Option<String>,
}

/// A page of items plus an opaque cursor for the next page (`None` on the last
/// page). The cursor is server-opaque; clients pass it back verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

// ---------------------------------------------------------------------------
// enrollment codes (operator mint of a pending tagma on a user's behalf)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateEnrollmentCodeRequest {
    pub user_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateEnrollmentCodeResponse {
    /// `sk-enroll-...` single-use, short-TTL; returned once.
    pub code: String,
}

// ---------------------------------------------------------------------------
// users
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSummary {
    /// `UserId` (UUID), as a hyphenated string. Bare `String` (not the
    /// `UserId` newtype) because the admin wire surface takes raw path params.
    pub id: String,
    pub username: String,
    /// Primary contact address, if the user linked one. `None` when the user
    /// has no email (email is an optional, settings-linked channel).
    pub primary_email: Option<String>,
    pub display_name: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub disabled_at: Option<OffsetDateTime>,
}

/// PATCH body for `PATCH /v1/admin/users/{id}`. `disabled = true` disables the
/// account; `false` re-enables it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateUserRequest {
    pub disabled: bool,
}

// ---------------------------------------------------------------------------
// passkeys
// ---------------------------------------------------------------------------

/// A WebAuthn credential summary. Omits the `credential` JSONB and `cred_id`
/// bytes as least-exposure for the API surface — neither is a secret (the
/// `credential` carries only the public key; `cred_id` is a per-RP opaque handle
/// sent in plaintext on every ceremony), but a list view has no need for them.
/// The `passkeys` table holds only live credentials, so there is no status
/// field; revoked history lives in a separate `passkey_revocations` audit table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PasskeySummary {
    /// The passkey id (UUID), as a hyphenated string.
    pub id: String,
    /// User-supplied device label (may be empty for legacy rows; the UI falls
    /// back to a generic name).
    pub label: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When this passkey was last used. Seeded to the enrollment instant and
    /// stamped on every subsequent sign-in.
    #[serde(with = "time::serde::rfc3339")]
    pub last_used_at: OffsetDateTime,
    /// Whether this credential was enrolled via the discoverable (resident-key)
    /// flow -- gates the "passwordless sign-in" UI affordance.
    pub discoverable: bool,
}
