//! Local-platform admin token login (route + handler).

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use kallipai_archeion_common::control_plane::{
    LOCAL_ADMIN_PROVIDER, LOCAL_ADMIN_SUBJECT, LOCAL_ADMIN_USERNAME,
};
use kallipai_archeion_common::ids::UserId;
use kallipai_archeion_common::principal::Principal;
use kallipai_common::protocol::ApiError;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QuerySelect, SqlErr,
    TransactionError, TransactionTrait,
};
use time::OffsetDateTime;
use tracing::info;
use uuid::Uuid;

use crate::auth::AuthPrincipal;
use crate::db::entity::{external_identities, users};
use crate::db::{TxnError, map_db_err};
use crate::state::SharedState;

use super::session::{AuthFinishResponse, mint_session_row, set_cookie_response};
use super::{EXTERNAL_IDENTITY_UNIQUE_CONSTRAINT, USERNAME_UNIQUE_CONSTRAINT};

/// Marker-row discriminators binding the fixed local account that
/// [`admin_login`] creates: `(provider, subject)` is the lookup key, so these
/// are stable identities like the OAuth provider names -- never rename. The
/// username the account carries is the hardcoded `LOCAL_ADMIN_USERNAME`
/// (`admin`, in `kallipai-archeion-common`), NOT the subject: the marker pair is
/// what identifies the account, while the name doubles as a reserved
/// signup handle. The constants live in `kallipai-archeion-common` and are
/// shared with `DbControlPlane::verify_session`'s `local_admin` flag.
/// The local-platform login route. Mounted only when the boot flag
/// `KALLIPAI_ARCHEION_ADMIN_USER_LOGIN` is set (see `routes::router`); the route
/// does not exist in the default production surface.
pub fn admin_login_router() -> Router<SharedState> {
    Router::new().route("/auth/admin-login", post(admin_login))
}

/// POST /auth/admin-login: exchange the operator's `sk-admin-` token for a
/// normal User session on a fixed local account, creating that account on
/// first use. This is the fifth auth ceremony and the local-platform
/// deployment's operator entry: a User session lights up the whole
/// user-scoped surface (profiles, tagma mint/enroll) with no browser passkey
/// ceremony, which is exactly the online management experience a local stack
/// needs (see docs/en/reference/auth.md for the security boundary).
///
/// Not gated by `signup_enabled`: mounting the route is an explicit operator
/// act that pre-provisions an operator account -- the same position as the
/// OAuth creation step, where the gate also does not apply to logins. Any
/// principal other than `Admin` (wrong token, a tagma bearer, a user
/// session) is a plain 401. The plaintext token is consumed by the
/// `AuthPrincipal` extractor; it never reaches this handler and so cannot be
/// logged here.
pub(super) async fn admin_login(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Response, ApiError> {
    let Principal::Admin = principal else {
        return Err(ApiError::unauthorized("admin token required"));
    };

    let username = LOCAL_ADMIN_USERNAME.to_string();
    let session_cfg = state.session_cfg.clone();
    let outcome = state
        .db
        .transaction::<_, (String, String), TxnError>(|txn| {
            let username = username.clone();
            let session_cfg = session_cfg.clone();
            Box::pin(async move {
                let marker = external_identities::Entity::find()
                    .filter(external_identities::Column::Provider.eq(LOCAL_ADMIN_PROVIDER))
                    .filter(external_identities::Column::Subject.eq(LOCAL_ADMIN_SUBJECT))
                    .lock_exclusive()
                    .one(txn)
                    .await?;
                let now = OffsetDateTime::now_utc();

                let user_id = if let Some(row) = marker {
                    // LOGIN: reuse the account a prior admin-login created.
                    // The caller holds the admin token, so an explicit
                    // disabled-refusal (vs the OAuth arm's generic 401) leaks
                    // nothing the caller does not already control.
                    let user = users::Entity::find_by_id(row.user_id.clone())
                        .lock_exclusive()
                        .one(txn)
                        .await?
                        .ok_or_else(|| {
                            TxnError::Api(ApiError::internal(
                                "local admin marker row dangles (user row missing)",
                            ))
                        })?;
                    if user.disabled_at.is_some() {
                        return Err(TxnError::Api(ApiError::unauthorized(
                            "local admin account is disabled",
                        )));
                    }
                    let mut am: external_identities::ActiveModel = row.into();
                    am.last_used_at = Set(Some(now));
                    am.update(txn).await?;
                    UserId::from(user.id)
                } else {
                    let existing = users::Entity::find()
                    // FIRST LOGIN: create the fixed account. A real signup
                    // holding `admin` cannot exist on a fresh deploy (the
                    // reserved list refuses it at every signup rail); this
                    // collision only fires on a legacy row from before the
                    // list, and it is not a race to retry.
                        .filter(users::Column::Username.eq(username.clone()))
                        .lock_exclusive()
                        .one(txn)
                        .await?;
                    if existing.is_some() {
                        return Err(TxnError::Api(ApiError::conflict(
                            "username for the local admin account is already taken by a legacy account; disable or remove it via the admin surface",
                        )));
                    }
                    let user_id = UserId::random();
                    users::ActiveModel {
                        id: Set(user_id.to_string()),
                        username: Set(username),
                        display_name: Set(None),
                        created_at: Set(now),
                        disabled_at: Set(None),
                    }
                    .insert(txn)
                    .await?;
                    external_identities::ActiveModel {
                        id: Set(Uuid::new_v4()),
                        user_id: Set(user_id.to_string()),
                        provider: Set(LOCAL_ADMIN_PROVIDER.to_string()),
                        subject: Set(LOCAL_ADMIN_SUBJECT.to_string()),
                        display_name: Set(None),
                        created_at: Set(now),
                        // First use IS this login.
                        last_used_at: Set(Some(now)),
                    }
                    .insert(txn)
                    .await?;
                    user_id
                };

                let set_cookie = mint_session_row(txn, &user_id, &session_cfg, now).await?;
                Ok((user_id.to_string(), set_cookie))
            })
        })
        .await;
    // Discriminate the sub-ms double-first-login race the same way
    // `register_finish` discriminates its username race: the FOR UPDATE
    // pre-checks win the common case; a simultaneous insert loses to a
    // unique index and surfaces as a clean 409 (a retry then takes the
    // login arm). Any other unique violation stays a 500 via `map_db_err`.
    let (user_id, set_cookie) = match outcome {
        Ok(v) => v,
        Err(TransactionError::Transaction(TxnError::Api(e))) => return Err(e),
        Err(TransactionError::Transaction(TxnError::Db(e)))
        | Err(TransactionError::Connection(e)) => {
            if let Some(SqlErr::UniqueConstraintViolation(msg)) = e.sql_err()
                && (msg.contains(USERNAME_UNIQUE_CONSTRAINT)
                    || msg.contains(EXTERNAL_IDENTITY_UNIQUE_CONSTRAINT))
            {
                return Err(ApiError::conflict(
                    "concurrent first admin-login; retry the request",
                ));
            }
            return Err(map_db_err(e));
        }
    };

    info!(user = %user_id, "admin-login: minted local admin session");
    Ok(set_cookie_response(
        &set_cookie,
        AuthFinishResponse { user_id },
        StatusCode::OK,
    ))
}
