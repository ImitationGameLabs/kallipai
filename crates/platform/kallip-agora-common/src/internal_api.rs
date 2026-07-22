//! Internal `ControlPlane` HTTP API wire types.
//!
//! The on-wire contract shared by `kallip-agora`'s `/internal/*` handlers (which
//! wrap its DB-backed `ControlPlane`) and `kallip-lesche`'s `HttpControlPlane`
//! client. Lives in this shared crate so the two sides cannot drift apart.
//!
//! These types are deliberately NOT the same as the public `/v1/*` surface: the
//! `/internal` API is a service-to-service boundary authenticated by a shared
//! secret, not a public route. `None` outcomes (unknown session / token / tagma)
//! are carried as HTTP `404`, not as a body variant, so the client maps status
//! directly to `Option::None` without parsing a sentinel.

use serde::{Deserialize, Serialize};

use crate::bytes::Ed25519PublicKey;
use crate::ids::{TagmaId, UserId};

// --- verify-session ---

/// `POST /internal/verify-session`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifySessionRequest {
    pub cookie: String,
}

/// `200` body: the session's owning user. (`404` = no body, maps to `None`.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifySessionResponse {
    pub user_id: UserId,
}

// --- verify-bearer ---

/// `POST /internal/verify-bearer`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyBearerRequest {
    pub token: String,
}

/// The on-wire principal for `verify_bearer`. A `User` never appears here:
/// `verify_bearer` can only resolve an `Admin` (admin token) or a `Tagma`
/// (tagma token). The session-cookie path resolves a user through
/// `verify_session`, which carries a bare `UserId`, not a `Principal`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum WirePrincipal {
    Admin,
    Tagma { tagma_id: TagmaId },
}

/// `200` body: the resolved principal. (`404` = no body, maps to `None`.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyBearerResponse {
    pub principal: WirePrincipal,
}

// --- tagma-resolvable ---

/// `POST /internal/tagma-resolvable`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagmaResolvableRequest {
    pub tagma_id: TagmaId,
    pub user_id: UserId,
}

/// `200` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagmaResolvableResponse {
    pub resolvable: bool,
}

// --- tagma-identity ---

/// `POST /internal/tagma-identity`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagmaIdentityRequest {
    pub tagma_id: TagmaId,
}

/// `200` body: the tagma's pinned key + owner. (`404` = unknown / no pinned key.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagmaIdentityResponse {
    pub pinned_public_key: Ed25519PublicKey,
    pub owner_user_id: UserId,
}

// --- tunnel-proof-ts ---

/// `POST /internal/tunnel-proof-ts`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelProofTsRequest {
    pub tagma_id: TagmaId,
    pub ts: i64,
}

/// `200` body: whether the proof timestamp advanced the high-water-mark.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelProofTsResponse {
    pub fresh: bool,
}

#[cfg(test)]
mod tests {
    //! Round-trip every wire type so a serde shape change here surfaces as a
    //! test failure before the two services drift in prod.

    use super::*;

    #[test]
    fn verify_session_round_trips() {
        let req = VerifySessionRequest {
            cookie: "sk-sess-x".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"cookie":"sk-sess-x"}"#);
        let back: VerifySessionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cookie, "sk-sess-x");

        let resp = VerifySessionResponse {
            user_id: UserId::from("u1".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: VerifySessionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.user_id, resp.user_id);
    }

    #[test]
    fn wire_principal_admin_tag_round_trips() {
        let admin = VerifyBearerResponse {
            principal: WirePrincipal::Admin,
        };
        let json = serde_json::to_string(&admin).unwrap();
        assert_eq!(json, r#"{"principal":{"kind":"admin"}}"#);
        let back: VerifyBearerResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.principal, WirePrincipal::Admin));

        let tagma = VerifyBearerResponse {
            principal: WirePrincipal::Tagma {
                tagma_id: TagmaId::from("t1".to_string()),
            },
        };
        let json = serde_json::to_string(&tagma).unwrap();
        assert_eq!(json, r#"{"principal":{"kind":"tagma","tagma_id":"t1"}}"#);
        let back: VerifyBearerResponse = serde_json::from_str(&json).unwrap();
        match back.principal {
            WirePrincipal::Tagma { tagma_id } => {
                assert_eq!(tagma_id, TagmaId::from("t1".to_string()))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn tagma_identity_round_trips() {
        let resp = TagmaIdentityResponse {
            pinned_public_key: Ed25519PublicKey(vec![1u8; 32]),
            owner_user_id: UserId::from("owner".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: TagmaIdentityResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pinned_public_key.0, resp.pinned_public_key.0);
        assert_eq!(back.owner_user_id, resp.owner_user_id);
    }

    #[test]
    fn tunnel_proof_ts_round_trips() {
        let resp = TunnelProofTsResponse { fresh: true };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"fresh":true}"#);
        let back: TunnelProofTsResponse = serde_json::from_str(&json).unwrap();
        assert!(back.fresh);
    }
}
