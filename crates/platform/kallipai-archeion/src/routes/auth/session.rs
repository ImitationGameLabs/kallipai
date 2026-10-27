//! Session minting and the response shapes the finish handlers share.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use kallipai_archeion_common::ids::UserId;
use kallipai_common::authtoken::MintedToken;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, DatabaseTransaction, DbErr};
use serde::Serialize;
use time::OffsetDateTime;

use crate::db::entity::sessions;
use crate::session::{SessionCfg, build_set_cookie};
use crate::token::SESSION;

#[derive(Serialize, Debug)]
pub(crate) struct CeremonyBeginResponse<T: Serialize> {
    pub ceremony_id: String,
    pub options: T,
}

#[derive(Serialize)]
pub(super) struct AuthFinishResponse {
    pub(super) user_id: String,
}

/// Mint a session for `user_id` inside `txn`: generate a fresh `sk-sess-`
/// token, insert the `sessions` row (seeding `authed_at = now` so a freshly
/// signed-in user can immediately add a passkey), and return the `Set-Cookie`
/// header value. Shared by [`register_finish`](super::register::register_finish),
/// [`finalize_login_and_mint_session`](super::login::finalize_login_and_mint_session),
/// and the OAuth login/create paths. The caller owns the surrounding txn
/// (ceremony lock/delete, disabled recheck, optional passkey advance) and
/// builds the response from the returned header.
pub(crate) async fn mint_session_row(
    txn: &DatabaseTransaction,
    user_id: &UserId,
    session_cfg: &SessionCfg,
    now: OffsetDateTime,
) -> Result<String, DbErr> {
    let session = MintedToken::generate(SESSION).expect("getrandom failed");
    let session_hash = session.hash().as_bytes().to_vec();
    let set_cookie = build_set_cookie(session_cfg, session.secret());
    sessions::ActiveModel {
        token_hash: Set(session_hash),
        user_id: Set(user_id.to_string()),
        created_at: Set(now),
        expires_at: Set(now + session_cfg.ttl),
        // A fresh login/signup is the step-up that authorizes adding a device;
        // seed `authed_at` (consumed on use by add-passkey begin).
        authed_at: Set(Some(now)),
    }
    .insert(txn)
    .await?;
    Ok(set_cookie)
}

/// Build a JSON response carrying `body` and a `Set-Cookie` header built from
/// `set_cookie`, with the given status. Shared by the session-minting finishes:
/// register/pair pass `CREATED` (a credential + session were created), login
/// passes `OK` (an existing credential was authenticated).
pub(crate) fn set_cookie_response<T: Serialize>(
    set_cookie: &str,
    body: T,
    status: StatusCode,
) -> Response {
    let mut resp = Json(body).into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(set_cookie) {
        resp.headers_mut()
            .append(axum::http::header::SET_COOKIE, value);
    }
    *resp.status_mut() = status;
    resp
}
