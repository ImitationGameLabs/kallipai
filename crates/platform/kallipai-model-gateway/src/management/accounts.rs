//! The admin account search: the member picker face over the
//! platform identity (the archeion), answering minimal identity
//! fields for a literal prefix.

use axum::Json;
use axum::extract::State;

use crate::management::AdminToken;
use crate::state::AppState;
use kallipai_common::protocol::ApiError;

/// POST /admin/accounts/search: the admin member picker's account
/// lookup. The query is a literal prefix over the account id, the
/// username, or a contact email; the answers carry the minimal identity
/// fields and never the email itself. Matching is case-sensitive.
pub async fn search_accounts(
    State(state): State<AppState>,
    _admin: AdminToken,
    Json(body): Json<AccountSearchBody>,
) -> Result<Json<AccountSearchView>, ApiError> {
    let query = body.query.trim();
    if query.is_empty() {
        return Err(ApiError::bad_request("search query must not be empty"));
    }
    let limit = body.limit.unwrap_or(20).clamp(1, 50);
    let verifier = match &state.management {
        crate::management::AdminAuth::Platform(verifier) => verifier.clone(),
        crate::management::AdminAuth::Disabled => {
            return Err(ApiError::conflict(
                "account search needs the platform identity (archeion)",
            ));
        }
    };
    let users = verifier
        .search_accounts(query, limit)
        .await
        .map_err(|_| crate::management::backend_unavailable())?;
    Ok(Json(AccountSearchView {
        users: users
            .into_iter()
            .map(|u| AccountSearchEntry {
                account_id: u.user_id.to_string(),
                username: u.username,
                display_name: u.display_name,
                disabled: u.disabled,
            })
            .collect(),
    }))
}

/// The account search request: a non-empty literal prefix and an
/// optional result cap (the platform side clamps it).
#[derive(Debug, serde::Deserialize)]
pub struct AccountSearchBody {
    pub query: String,
    pub limit: Option<u32>,
}

/// The account search answers: minimal identity fields, no email.
#[derive(Debug, serde::Serialize)]
pub struct AccountSearchView {
    pub users: Vec<AccountSearchEntry>,
}

#[derive(Debug, serde::Serialize)]
pub struct AccountSearchEntry {
    pub account_id: String,
    pub username: String,
    pub display_name: Option<String>,
    pub disabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::testkit::*;
    use axum::http::StatusCode;

    #[tokio::test]
    async fn admin_account_search_minimal_fields() {
        let state = seeded_state().await;
        let admin = Some(TEST_ADMIN_COOKIE);
        let search = |state: &AppState, body: serde_json::Value| {
            let state = state.clone();
            async move {
                send(
                    app(state),
                    cookie_req("POST", "/admin/accounts/search", admin, true, Some(body)),
                )
                .await
            }
        };
        // The id prefix channel: the whole directory, in username order.
        let (status, body) = search(&state, serde_json::json!({"query": "acct-"})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let users = body["users"].as_array().expect("users array");
        assert_eq!(users.len(), 3);
        // The minimal answer shape: exactly the four identity keys.
        let keys: Vec<&str> = users[0]
            .as_object()
            .expect("user object")
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(
            keys,
            vec!["account_id", "username", "display_name", "disabled"]
        );
        // The username prefix channel.
        let (status, body) = search(&state, serde_json::json!({"query": "al"})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["users"].as_array().expect("users").len(), 1);
        assert_eq!(body["users"][0]["username"], "alice");
        // The email prefix channel: a match key, and the address never
        // appears in the answers.
        let (status, body) = search(&state, serde_json::json!({"query": "bob@example"})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let users = body["users"].as_array().expect("users");
        assert_eq!(users.len(), 1);
        assert_eq!(users[0]["username"], "bob");
        assert!(!body.to_string().contains("bob@example"));
        // The limit clamp.
        let (status, body) =
            search(&state, serde_json::json!({"query": "acct-", "limit": 1})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["users"].as_array().expect("users").len(), 1);
        // The empty query refuses.
        let (status, body) = search(&state, serde_json::json!({"query": " "})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
}
