//! GET /: the listing face (the CLI `file ls` dependency). The SQL
//! prefix is derived from the caller's identity alone, matched literally
//! (the LIKE metacharacters are escaped), and only narrows the candidate
//! set; authorization itself is the per-row matrix decision -- the
//! same `user_can` / `tagma_can` functions the single-record routes go
//! through -- so a listing can never serve a row the record routes would
//! not. The admin principal is refused up front (row 8: no content face).

use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;

use super::tagma_facts;
use crate::acl::{self, Action, SpacePath};
use crate::auth::AuthPrincipal;
use crate::state::AppState;
use kallip_archeion_common::principal::Principal;
use kallip_common::protocol::ApiError;

/// Which slice of the caller's own space to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListSpace {
    /// The caller's private face: a user's whole space, a tagma's private
    /// region (its `inbox/` included).
    Private,
    /// The user-space shared region.
    Shared,
}

impl ListSpace {
    /// Parse the `space` query value.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "self" => Some(Self::Private),
            "shared" => Some(Self::Shared),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// `self` (the caller's private face) or `shared`.
    pub space: String,
    /// Optional narrowing prefix, relative to the selected slice (e.g.
    /// `inbox/`), appended to the identity-derived prefix. A value starting
    /// with `/` is rejected: the client narrows, never re-points.
    pub prefix: Option<String>,
    /// Page size; the server clamps to [`MAX_LIST_LIMIT`].
    pub limit: Option<u64>,
}

/// One listed record as served. `created_at` is RFC 3339.
#[derive(Debug, Serialize)]
pub struct FileEntryView {
    pub id: uuid::Uuid,
    pub path: String,
    /// Blob size in bytes, read in the same transaction as the rows.
    pub size: i64,
    pub created_at: String,
}

/// The server-side page cap: a client can ask for fewer, never more. This
/// is the anti-unbounded-enumeration bound; a default keeps a bare `ls`
/// small too.
pub const MAX_LIST_LIMIT: u64 = 500;

/// Page size when the client does not name one.
const DEFAULT_LIMIT: u64 = 200;

/// Apply the pinned clamp. Unit-pinned in `list_tests` via the same
/// arithmetic the route uses.
pub fn clamp_limit(requested: Option<u64>) -> u64 {
    requested.unwrap_or(DEFAULT_LIMIT).min(MAX_LIST_LIMIT)
}

/// GET /?space=self|shared&prefix=&limit=
pub async fn list_files(
    State(state): State<AppState>,
    AuthPrincipal(principal): AuthPrincipal,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<FileEntryView>>, ApiError> {
    // Row 8 parity with the other content routes: the admin gets the same
    // shape of denial, not an empty listing that hides it.
    if matches!(principal, Principal::Admin) {
        return Err(ApiError::forbidden("admin cannot list content"));
    }
    let space = ListSpace::parse(&query.space)
        .ok_or_else(|| ApiError::bad_request("space must be one of: self, shared"))?;
    let rel_prefix = match &query.prefix {
        Some(p) if p.starts_with('/') => {
            return Err(ApiError::bad_request(
                "prefix must be relative to the selected space",
            ));
        }
        Some(p) => p.clone(),
        None => String::new(),
    };

    // Tagma facts, resolved once per request (the send flow's shape) and
    // reused for both the grant prefix and every per-row decision below.
    let facts = match &principal {
        Principal::Tagma(tagma) => Some(tagma_facts(&state, tagma).await?),
        _ => None,
    };

    // The identity-derived grant prefix. For a user this is their own
    // space (the row-1 scope); for a tagma it is one region of their
    // space's enrollment (rows 2/3).
    let grant_prefix = match (&principal, facts.as_ref()) {
        (Principal::User(user), _) => {
            let base = format!("/users/{}/", user.as_ref());
            match space {
                ListSpace::Private => base,
                ListSpace::Shared => format!("{base}shared/"),
            }
        }
        (Principal::Tagma(tagma), Some(enrollment)) => {
            let base = format!("/users/{}/", enrollment.space_user);
            match space {
                ListSpace::Private => format!("{base}tagmas/{}/", tagma.as_ref()),
                ListSpace::Shared => format!("{base}shared/"),
            }
        }
        // The admin was rejected above; a tagma always carries facts.
        _ => unreachable!(),
    };
    let sql_prefix = format!("{grant_prefix}{rel_prefix}");

    let rows = crate::metadata::repo::list_records_with_sizes(
        &state.db,
        &sql_prefix,
        clamp_limit(query.limit),
    )
    .await
    .map_err(ApiError::internal)?;

    let mut out = Vec::with_capacity(rows.len());
    for (record, size) in rows {
        // A stored path that no longer parses is corruption (the parser
        // accepted it at write time): 500, never a silent skip.
        let path = SpacePath::parse(&record.space_path)
            .ok_or_else(|| ApiError::internal("record has a malformed space path"))?;
        // The per-row decision is the same matrix call the single-record
        // routes make. With a correctly derived prefix it never filters
        // anything; it exists so the listing can only ever narrow, never
        // widen, what GET /{id} would serve.
        let allowed = match (&principal, facts.as_ref()) {
            (Principal::User(user), _) => acl::user_can(user.as_ref(), &path, Action::Read),
            (Principal::Tagma(tagma), Some(row_facts)) => {
                acl::tagma_can(tagma.as_ref(), &path, Action::Read, row_facts)
            }
            // A tagma always carries facts (resolved above); anything else
            // is denied defensively.
            _ => false,
        };
        if !allowed {
            continue;
        }
        let created_at = record
            .created_at
            .format(&Rfc3339)
            .unwrap_or_else(|_| record.created_at.to_string());
        out.push(FileEntryView {
            id: record.id,
            path: record.space_path,
            size,
            created_at,
        });
    }
    Ok(Json(out))
}
