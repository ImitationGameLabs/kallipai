//! The data-plane's RPC client for the registry's [`ControlPlane`].
//!
//! Each call is one HTTP POST to the agora's `/internal/*` surface, guarded by
//! a shared-secret bearer. There is deliberately NO auth cache: the relay's hot
//! paths are long-lived connections (tagma tunnel, app SSE) that authenticate
//! once at open and never re-verify mid-stream, so per-request RPC volume is
//! low. A short-TTL in-process cache would add unbounded state and a
//! "freshly-issued token cached as None" hazard for ~zero benefit.
//!
//! Revocation latency is therefore bounded by the lifetime of an open
//! connection, not by a cache TTL: to force re-verification of a revoked tagma
//! or a disabled user, drop the connection (tagma reconnect, app reconnect).
//! That is the v1 revocation contract. The proper future step, if per-request
//! volume ever rises enough to matter, is a JWT migration (local validation,
//! zero per-request RPC) rather than an in-process TTL map.

use std::time::Duration;

use kallip_agora_common::control_plane::{
    ControlPlane, ControlPlaneError, TagmaProfile, UserIdentity, VerifiedSession,
};
use kallip_agora_common::ids::{TagmaId, UserId};
use kallip_agora_common::internal_api::{
    TagmaProfilesRequest, TagmaProfilesResponse, TunnelProofTsRequest, TunnelProofTsResponse,
    UserIdentitiesRequest, UserIdentitiesResponse, UserIdentityByUsernameRequest,
    UserIdentityResponse, VerifyBearerRequest, VerifyBearerResponse, VerifySessionRequest,
    VerifySessionResponse,
};
use kallip_agora_common::principal::Principal;

/// Per-call timeout for an `/internal/*` round-trip. These are tiny JSON
/// request/response pairs against a same-host registry; a 10s ceiling is a
/// generous backstop, not the expected latency.
const INTERNAL_TIMEOUT: Duration = Duration::from_secs(10);

/// A reqwest-backed [`ControlPlane`] calling the agora's `/internal/*` API.
#[derive(Clone)]
pub struct HttpControlPlane {
    /// Agora internal root (e.g. `http://127.0.0.1:7100`); `/internal/...` is
    /// appended per call.
    base_url: String,
    /// Plaintext shared secret sent as `Authorization: Bearer <token>`.
    token: String,
    http: reqwest::Client,
}

impl HttpControlPlane {
    /// `base_url` is the agora's internal root; `token` is the plaintext shared
    /// secret that must match the agora's `KALLIP_AGORA_INTERNAL_TOKEN`.
    pub fn new(base_url: String, token: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(INTERNAL_TIMEOUT)
            .build()
            .expect("build reqwest client");
        Self {
            base_url,
            token,
            http,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// POST `body` to `path`; map `200` -> `Some(deserialized)`, `404` ->
    /// `None`, any other status or transport error -> `Backend`.
    async fn post<Req, Resp>(
        &self,
        path: &str,
        body: &Req,
    ) -> Result<Option<Resp>, ControlPlaneError>
    where
        Req: serde::Serialize,
        Resp: serde::de::DeserializeOwned,
    {
        let resp = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|e| ControlPlaneError::Backend(e.to_string()))?;
        match resp.status().as_u16() {
            200 => resp
                .json::<Resp>()
                .await
                .map(Some)
                .map_err(|e| ControlPlaneError::Backend(e.to_string())),
            404 => Ok(None),
            status => Err(ControlPlaneError::Backend(format!(
                "agora {path} returned HTTP {status}"
            ))),
        }
    }
}

#[async_trait::async_trait]
impl ControlPlane for HttpControlPlane {
    async fn verify_session(
        &self,
        cookie_value: &str,
    ) -> Result<Option<VerifiedSession>, ControlPlaneError> {
        let resp: Option<VerifySessionResponse> = self
            .post(
                "/internal/verify-session",
                &VerifySessionRequest {
                    cookie: cookie_value.to_string(),
                },
            )
            .await?;
        // The wire body aliases VerifiedSession: no field mapping.
        Ok(resp)
    }

    async fn verify_bearer(&self, token: &str) -> Result<Option<Principal>, ControlPlaneError> {
        let resp: Option<VerifyBearerResponse> = self
            .post(
                "/internal/verify-bearer",
                &VerifyBearerRequest {
                    token: token.to_string(),
                },
            )
            .await?;
        // The wire enum is the bearer-reachable subset of Principal; the
        // From impl (in internal_api) is the single mapping site.
        Ok(resp.map(|r| Principal::from(r.principal)))
    }

    async fn tagma_profiles(
        &self,
        tagma_ids: &[TagmaId],
    ) -> Result<Vec<TagmaProfile>, ControlPlaneError> {
        // Always-200 endpoint: a 404 (should not occur) degrades to an empty
        // result so the relay renders prefix-only handles for every input id.
        let resp: Option<TagmaProfilesResponse> = self
            .post(
                "/internal/tagma-profiles",
                &TagmaProfilesRequest {
                    tagma_ids: tagma_ids.to_vec(),
                },
            )
            .await?;
        // The wire entry type aliases TagmaProfile, so the response body
        // deserializes straight into the trait type -- no field mapping.
        Ok(resp.map(|r| r.profiles).unwrap_or_default())
    }

    async fn user_identities(
        &self,
        user_ids: &[UserId],
    ) -> Result<Vec<UserIdentity>, ControlPlaneError> {
        let resp: Option<UserIdentitiesResponse> = self
            .post(
                "/internal/user-identities",
                &UserIdentitiesRequest {
                    user_ids: user_ids.to_vec(),
                },
            )
            .await?;
        // The wire entry aliases UserIdentity: no field mapping.
        Ok(resp.map(|r| r.users).unwrap_or_default())
    }

    async fn user_identity_by_username(
        &self,
        username: &str,
    ) -> Result<Option<UserIdentity>, ControlPlaneError> {
        // Singular handle resolve: 200 -> the identity (carrying the user_id the
        // invite gate reads back out); 404 -> None (unknown / disabled /
        // malformed all collapse on the registry side).
        let resp: Option<UserIdentityResponse> = self
            .post(
                "/internal/user-identity-by-username",
                &UserIdentityByUsernameRequest {
                    username: username.to_string(),
                },
            )
            .await?;
        Ok(resp)
    }

    async fn bump_tunnel_proof_ts(
        &self,
        tagma_id: &TagmaId,
        ts: i64,
    ) -> Result<bool, ControlPlaneError> {
        let resp: Option<TunnelProofTsResponse> = self
            .post(
                "/internal/tunnel-proof-ts",
                &TunnelProofTsRequest {
                    tagma_id: tagma_id.clone(),
                    ts,
                },
            )
            .await?;
        Ok(resp.map(|r| r.fresh).unwrap_or(false))
    }
}

#[cfg(test)]
mod tests {
    //! Stand up a wiremock agora `/internal/*` and assert each `ControlPlane`
    //! method maps request shape + HTTP status to the trait's `Option`/`bool`
    //! contract. No cache exists, so there is no cache behavior to test.

    use super::*;
    use base64::Engine;
    use kallip_agora_common::control_plane::ControlPlane;
    use kallip_agora_common::ids::{TagmaId, UserId};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn mocked() -> (MockServer, HttpControlPlane) {
        let server = MockServer::start().await;
        let cp = HttpControlPlane::new(server.uri(), "internal-secret".to_string());
        (server, cp)
    }

    #[tokio::test]
    async fn verify_session_200_maps_to_verified_session() {
        let (server, cp) = mocked().await;
        Mock::given(method("POST"))
            .and(path("/internal/verify-session"))
            .and(header("authorization", "Bearer internal-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "user_id": "alice", "username": "alice", "display_name": "Alice" }),
            ))
            .mount(&server)
            .await;

        let session = cp.verify_session("sk-sess-x").await.unwrap().unwrap();
        assert_eq!(session.user_id, UserId::from("alice".to_string()));
        assert_eq!(session.username, "alice");
        assert_eq!(session.display_name.as_deref(), Some("Alice"));
    }

    #[tokio::test]
    async fn verify_session_404_maps_to_none() {
        let (server, cp) = mocked().await;
        Mock::given(method("POST"))
            .and(path("/internal/verify-session"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        assert!(cp.verify_session("bogus").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn verify_bearer_maps_tagma_principal() {
        let (server, cp) = mocked().await;
        Mock::given(method("POST"))
            .and(path("/internal/verify-bearer"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "principal": { "kind": "tagma", "tagma_id": "t1" } }),
            ))
            .mount(&server)
            .await;

        let principal = cp.verify_bearer("sk-tagma-y").await.unwrap().unwrap();
        assert!(matches!(
            principal,
            Principal::Tagma(id) if id == TagmaId::from("t1".to_string())
        ));
    }

    #[tokio::test]
    async fn tagma_profiles_decodes_rich_facts() {
        let (server, cp) = mocked().await;
        // base64 of 32 bytes of 0x01.
        let key_b64 = base64::engine::general_purpose::STANDARD.encode([1u8; 32]);
        Mock::given(method("POST"))
            .and(path("/internal/tagma-profiles"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "profiles": [{
                    "tagma_id": "t1",
                    "pinned_public_key": key_b64,
                    "owner_user_id": "owner",
                    "label": "Laptop",
                    "owner_username": "alice",
                    "owner_display_name": null,
                    "enrolled": true,
                    "revoked": false,
                    "owner_disabled": false
                }]
            })))
            .mount(&server)
            .await;

        let profiles = cp
            .tagma_profiles(&[TagmaId::from("t1".to_string())])
            .await
            .unwrap();
        assert_eq!(profiles.len(), 1);
        let p = &profiles[0];
        assert_eq!(p.tagma_id, TagmaId::from("t1".to_string()));
        assert_eq!(p.pinned_public_key.as_ref().unwrap().0, vec![1u8; 32]);
        assert_eq!(p.owner_user_id, UserId::from("owner".to_string()));
        assert_eq!(p.label.as_deref(), Some("Laptop"));
        assert_eq!(p.owner_username, "alice");
        assert!(p.enrolled && !p.revoked && !p.owner_disabled);
    }

    #[tokio::test]
    async fn user_identities_decodes_rich_facts() {
        let (server, cp) = mocked().await;
        Mock::given(method("POST"))
            .and(path("/internal/user-identities"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "users": [{
                    "user_id": "u1",
                    "username": "alice",
                    "display_name": "Alice",
                    "disabled": false
                }]
            })))
            .mount(&server)
            .await;

        let users = cp
            .user_identities(&[UserId::from("u1".to_string())])
            .await
            .unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].user_id, UserId::from("u1".to_string()));
        assert_eq!(users[0].username, "alice");
        assert_eq!(users[0].display_name.as_deref(), Some("Alice"));
        assert!(!users[0].disabled);
    }

    #[tokio::test]
    async fn user_identity_by_username_200_maps_to_identity() {
        let (server, cp) = mocked().await;
        Mock::given(method("POST"))
            .and(path("/internal/user-identity-by-username"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_id": "u1",
                "username": "alice",
                "display_name": null,
                "disabled": false
            })))
            .mount(&server)
            .await;

        let resolved = cp
            .user_identity_by_username("alice")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.user_id, UserId::from("u1".to_string()));
        assert_eq!(resolved.username, "alice");
        assert!(resolved.display_name.is_none());
        assert!(!resolved.disabled);
    }

    #[tokio::test]
    async fn user_identity_by_username_404_maps_to_none() {
        let (server, cp) = mocked().await;
        Mock::given(method("POST"))
            .and(path("/internal/user-identity-by-username"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        assert!(
            cp.user_identity_by_username("nobody")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn bump_tunnel_proof_ts_returns_fresh_flag() {
        let (server, cp) = mocked().await;
        Mock::given(method("POST"))
            .and(path("/internal/tunnel-proof-ts"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "fresh": true })),
            )
            .mount(&server)
            .await;

        assert!(
            cp.bump_tunnel_proof_ts(&TagmaId::from("t1".to_string()), 123)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn non_2xx_non_404_is_backend_error() {
        let (server, cp) = mocked().await;
        Mock::given(method("POST"))
            .and(path("/internal/verify-session"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        assert!(cp.verify_session("x").await.is_err());
    }
}
