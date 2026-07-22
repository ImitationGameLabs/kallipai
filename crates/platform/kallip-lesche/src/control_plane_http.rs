//! The data-plane's RPC client for the registry's [`ControlPlane`].
//!
//! Each call is one HTTP POST to the agora's `/internal/*` surface, guarded by
//! a shared-secret bearer. There is deliberately NO auth cache: the relay's hot
//! paths are long-lived connections (herald tunnel, app SSE) that authenticate
//! once at open and never re-verify mid-stream, so per-request RPC volume is
//! low. A short-TTL in-process cache would add unbounded state and a
//! "freshly-issued token cached as None" hazard for ~zero benefit.
//!
//! Revocation latency is therefore bounded by the lifetime of an open
//! connection, not by a cache TTL: to force re-verification of a revoked tagma
//! or a disabled user, drop the connection (herald reconnect, app reconnect).
//! That is the v1 revocation contract. The proper future step, if per-request
//! volume ever rises enough to matter, is a JWT migration (local validation,
//! zero per-request RPC) rather than an in-process TTL map.

use std::time::Duration;

use kallip_agora_common::control_plane::{ControlPlane, ControlPlaneError, TagmaIdentity};
use kallip_agora_common::ids::{TagmaId, UserId};
use kallip_agora_common::internal_api::{
    TagmaIdentityRequest, TagmaIdentityResponse, TagmaResolvableRequest, TagmaResolvableResponse,
    TunnelProofTsRequest, TunnelProofTsResponse, VerifyBearerRequest, VerifyBearerResponse,
    VerifySessionRequest, VerifySessionResponse, WirePrincipal,
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
    ) -> Result<Option<UserId>, ControlPlaneError> {
        let resp: Option<VerifySessionResponse> = self
            .post(
                "/internal/verify-session",
                &VerifySessionRequest {
                    cookie: cookie_value.to_string(),
                },
            )
            .await?;
        Ok(resp.map(|r| r.user_id))
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
        Ok(resp.map(|r| match r.principal {
            WirePrincipal::Admin => Principal::Admin,
            WirePrincipal::Tagma { tagma_id } => Principal::Tagma(tagma_id),
        }))
    }

    async fn tagma_resolvable_by(
        &self,
        tagma_id: &TagmaId,
        user: &UserId,
    ) -> Result<bool, ControlPlaneError> {
        // The endpoint always returns 200 with a bool; a 404 (should not occur)
        // is treated conservatively as "not resolvable".
        let resp: Option<TagmaResolvableResponse> = self
            .post(
                "/internal/tagma-resolvable",
                &TagmaResolvableRequest {
                    tagma_id: tagma_id.clone(),
                    user_id: user.clone(),
                },
            )
            .await?;
        Ok(resp.map(|r| r.resolvable).unwrap_or(false))
    }

    async fn tagma_identity(
        &self,
        tagma_id: &TagmaId,
    ) -> Result<Option<TagmaIdentity>, ControlPlaneError> {
        let resp: Option<TagmaIdentityResponse> = self
            .post(
                "/internal/tagma-identity",
                &TagmaIdentityRequest {
                    tagma_id: tagma_id.clone(),
                },
            )
            .await?;
        Ok(resp.map(|r| TagmaIdentity {
            pinned_public_key: r.pinned_public_key,
            owner_user_id: r.owner_user_id,
        }))
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
    async fn verify_session_200_maps_to_some_user() {
        let (server, cp) = mocked().await;
        Mock::given(method("POST"))
            .and(path("/internal/verify-session"))
            .and(header("authorization", "Bearer internal-secret"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "user_id": "alice" })),
            )
            .mount(&server)
            .await;

        let user = cp.verify_session("sk-sess-x").await.unwrap();
        assert_eq!(user, Some(UserId::from("alice".to_string())));
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
    async fn tagma_identity_200_round_trips_key_and_owner() {
        let (server, cp) = mocked().await;
        // base64 of 32 bytes of 0x01.
        let key_b64 = base64::engine::general_purpose::STANDARD.encode([1u8; 32]);
        Mock::given(method("POST"))
            .and(path("/internal/tagma-identity"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pinned_public_key": key_b64,
                "owner_user_id": "owner"
            })))
            .mount(&server)
            .await;

        let identity = cp
            .tagma_identity(&TagmaId::from("t1".to_string()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(identity.pinned_public_key.0, vec![1u8; 32]);
        assert_eq!(identity.owner_user_id, UserId::from("owner".to_string()));
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
