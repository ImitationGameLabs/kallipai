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

/// `200` body: the session's owning user plus the authoritative display
/// identity. (`404` = no body, maps to `None`.) The display is resolved here,
/// once per connection-open, rather than via a per-message call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifySessionResponse {
    pub user_id: UserId,
    pub username: String,
    pub display_name: Option<String>,
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

// --- tagma-profiles (canonical tagma fact read) ---

/// `POST /internal/tagma-profiles`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagmaProfilesRequest {
    pub tagma_ids: Vec<TagmaId>,
}

/// One tagma's facts in [`TagmaProfilesResponse`]. UNFILTERED: carries the raw
/// identity + usability state (`enrolled`/`revoked`/`owner_disabled`/key) so the
/// relay, not the registry, derives authorization. The wire shape of
/// [`crate::control_plane::TagmaProfile`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagmaProfileResponse {
    pub tagma_id: TagmaId,
    pub pinned_public_key: Option<Ed25519PublicKey>,
    pub owner_user_id: UserId,
    pub label: Option<String>,
    pub owner_username: String,
    pub owner_display_name: Option<String>,
    pub enrolled: bool,
    pub revoked: bool,
    pub owner_disabled: bool,
}

/// `200` body: one entry per existing input id (unknown ids omitted). Always
/// `200` (never `404`) -- absence is expressed by omission, so the relay maps
/// status straight to the result list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagmaProfilesResponse {
    pub profiles: Vec<TagmaProfileResponse>,
}

// --- user-identities (canonical user fact read) ---

/// `POST /internal/user-identities`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserIdentitiesRequest {
    pub user_ids: Vec<UserId>,
}

/// One user's facts in [`UserIdentitiesResponse`]. UNFILTERED: carries the raw
/// `disabled` state so the relay derives the invite gate locally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserIdentityResponse {
    pub user_id: UserId,
    pub username: String,
    pub display_name: Option<String>,
    pub disabled: bool,
}

/// `200` body: one entry per existing input user (unknown ids omitted). Always
/// `200` (never `404`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserIdentitiesResponse {
    pub users: Vec<UserIdentityResponse>,
}

// --- user-identity-by-username (singular handle resolve) ---

/// `POST /internal/user-identity-by-username`. The invite gate's handle ->
/// identity resolve. Carries a BARE handle (the caller strips any `@` sigil
/// first); the registry normalizes + validates it. `200` body is
/// [`UserIdentityResponse`] (same shape as the bulk read); an unknown /
/// malformed handle is `404` with no body, which the client maps to `None` --
/// matching the `None`-as-404 convention of `verify-session` / `verify-bearer`,
/// not the omission convention of the bulk reader, because this lookup is
/// singular.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserIdentityByUsernameRequest {
    pub username: String,
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
            username: "alice".to_string(),
            display_name: Some("Alice".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: VerifySessionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.user_id, resp.user_id);
        assert_eq!(back.username, "alice");
        assert_eq!(back.display_name.as_deref(), Some("Alice"));
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
    fn tagma_profiles_round_trips() {
        let req = TagmaProfilesRequest {
            tagma_ids: vec![TagmaId::from("t1".to_string())],
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"tagma_ids":["t1"]}"#);
        let back: TagmaProfilesRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tagma_ids, req.tagma_ids);

        // Rich shape: key + owner + display + raw usability facts.
        let resp = TagmaProfilesResponse {
            profiles: vec![TagmaProfileResponse {
                tagma_id: TagmaId::from("t1".to_string()),
                pinned_public_key: Some(Ed25519PublicKey(vec![1u8; 32])),
                owner_user_id: UserId::from("owner".to_string()),
                label: Some("Laptop".to_string()),
                owner_username: "alice".to_string(),
                owner_display_name: None,
                enrolled: true,
                revoked: false,
                owner_disabled: false,
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: TagmaProfilesResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.profiles.len(), 1);
        let p = &back.profiles[0];
        assert_eq!(p.tagma_id, TagmaId::from("t1".to_string()));
        assert_eq!(p.pinned_public_key.as_ref().unwrap().0, vec![1u8; 32]);
        assert_eq!(p.owner_user_id, UserId::from("owner".to_string()));
        assert_eq!(p.label.as_deref(), Some("Laptop"));
        assert_eq!(p.owner_username, "alice");
        assert!(p.owner_display_name.is_none());
        assert!(p.enrolled && !p.revoked && !p.owner_disabled);

        // A pending tagma (no key) round-trips with `pinned_public_key: null`.
        let pending = TagmaProfileResponse {
            tagma_id: TagmaId::from("t2".to_string()),
            pinned_public_key: None,
            owner_user_id: UserId::from("owner".to_string()),
            label: None,
            owner_username: "alice".to_string(),
            owner_display_name: None,
            enrolled: false,
            revoked: false,
            owner_disabled: false,
        };
        let json = serde_json::to_string(&pending).unwrap();
        assert!(json.contains(r#""pinned_public_key":null"#));
        let back: TagmaProfileResponse = serde_json::from_str(&json).unwrap();
        assert!(back.pinned_public_key.is_none());
    }

    #[test]
    fn user_identities_round_trips() {
        let req = UserIdentitiesRequest {
            user_ids: vec![UserId::from("u1".to_string())],
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"user_ids":["u1"]}"#);
        let back: UserIdentitiesRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.user_ids, req.user_ids);

        let resp = UserIdentitiesResponse {
            users: vec![UserIdentityResponse {
                user_id: UserId::from("u1".to_string()),
                username: "alice".to_string(),
                display_name: Some("Alice".to_string()),
                disabled: false,
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: UserIdentitiesResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.users.len(), 1);
        assert_eq!(back.users[0].user_id, UserId::from("u1".to_string()));
        assert_eq!(back.users[0].username, "alice");
        assert_eq!(back.users[0].display_name.as_deref(), Some("Alice"));
        assert!(!back.users[0].disabled);
    }

    #[test]
    fn user_identity_by_username_round_trips() {
        // A raw handle (with a leading `@`, as a client might forward) is
        // carried verbatim; the registry normalizes, not the wire.
        let req = UserIdentityByUsernameRequest {
            username: "@alice".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"username":"@alice"}"#);
        let back: UserIdentityByUsernameRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.username, "@alice");

        // The 200 body reuses the bulk reader's identity shape verbatim.
        let resp = UserIdentityResponse {
            user_id: UserId::from("u1".to_string()),
            username: "alice".to_string(),
            display_name: None,
            disabled: false,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: UserIdentityResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.user_id, UserId::from("u1".to_string()));
        assert_eq!(back.username, "alice");
        assert!(back.display_name.is_none());
        assert!(!back.disabled);
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
