//! Internal `ControlPlane` HTTP API wire types.
//!
//! The on-wire contract shared by `kallipai-archeion`'s `/internal/*` handlers
//! (which wrap its DB-backed `ControlPlane`) and the data-plane services'
//! `ControlPlane` HTTP clients. Lives in this shared crate so the two sides
//! cannot drift apart.
//!
//! These types are deliberately NOT the same as the public `/v1/*` surface: the
//! `/internal` API is a service-to-service boundary authenticated by a shared
//! secret, not a public route. `None` outcomes (unknown session / token / tagma)
//! are carried as HTTP `404`, not as a body variant, so the client maps status
//! directly to `Option::None` without parsing a sentinel.

use crate::principal::Principal;
use serde::{Deserialize, Serialize};

use crate::ids::{TagmaId, TokenId, UserId};
use crate::perm::PermSet;

// --- verify-session ---

/// `POST /internal/verify-session`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifySessionRequest {
    pub cookie: String,
}

/// `200` body: the session's owning user plus the authoritative display
/// identity. (`404` = no body, maps to `None`.) The display is resolved here,
/// once per connection-open, rather than via a per-message call. An alias
/// of the trait-side [`crate::control_plane::VerifiedSession`].
pub type VerifySessionResponse = crate::control_plane::VerifiedSession;

// --- verify-bearer ---

/// `POST /internal/verify-bearer`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyBearerRequest {
    pub token: String,
}

/// The on-wire principal for `verify_bearer`: an `Admin` (admin token), a
/// `Tagma` (a tagma's credential), or a `User` (a personal access token).
/// The cookie path is unaffected: `verify_session` carries a bare `UserId`
/// and never a `Principal`, so the two ways of being a user stay distinct on
/// the wire even though both resolve to `Principal::User`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum WirePrincipal {
    Admin,
    Tagma { tagma_id: TagmaId },
    User { user_id: UserId },
}

/// Project a [`Principal`] onto the wire. Every variant is reachable: the
/// bearer path resolves an admin, a tagma credential, or a personal access
/// token (which acts as its owning user).
impl From<Principal> for WirePrincipal {
    fn from(principal: Principal) -> Self {
        match principal {
            Principal::Admin => WirePrincipal::Admin,
            Principal::Tagma(tagma_id) => WirePrincipal::Tagma { tagma_id },
            Principal::User(user_id) => WirePrincipal::User { user_id },
        }
    }
}

/// The credential a bearer resolved through, as carried on the wire: its
/// record id (so a data plane can name it in a log line) and the permission
/// set stored on that record. Absent for the admin bearer, which has no
/// record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireCredential {
    pub token_id: TokenId,
    pub perms: PermSet,
}

/// Lift a [`WirePrincipal`] back into a [`Principal`]. Total: the wire enum
/// is exactly the set the bearer path can resolve.
impl From<WirePrincipal> for Principal {
    fn from(wire: WirePrincipal) -> Self {
        match wire {
            WirePrincipal::Admin => Principal::Admin,
            WirePrincipal::Tagma { tagma_id } => Principal::Tagma(tagma_id),
            WirePrincipal::User { user_id } => Principal::User(user_id),
        }
    }
}

/// `200` body: the resolved identity plus the credential that carried it.
/// (`404` = no body, maps to `None`.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyBearerResponse {
    pub principal: WirePrincipal,
    /// `None` means this path carried no record, and only the admin bearer
    /// (a bare secret) has none: a tagma's credential and a personal access
    /// token both arrive as `Some`. Three things a reader must keep apart:
    /// `None` (the bare secret, no narrowing), an empty set (a live
    /// credential that authenticates and passes no permission gate, so its
    /// calls are refused one route at a time with `403`), and a `404` (no
    /// usable record: an unknown hash, an expired or revoked credential, a
    /// retired tagma, a disabled owner, an unknown owner kind, or a set with
    /// a string outside the vocabulary -- no credential, never half of one).
    ///
    /// A registry older than this field sends no `credential` for any bearer,
    /// so such an answer decodes to `None` and is refused by the `TryFrom`
    /// below. That closes one direction only: a data plane older than this
    /// field still reads a bearer as the whole identity, and nothing on the
    /// answering side can reach that.
    pub credential: Option<WireCredential>,
}

impl From<&crate::control_plane::VerifiedBearer> for VerifyBearerResponse {
    fn from(verified: &crate::control_plane::VerifiedBearer) -> Self {
        Self {
            principal: verified.principal.clone().into(),
            credential: verified
                .credential
                .as_ref()
                .map(|credential| WireCredential {
                    token_id: credential.token.clone(),
                    perms: credential.perms.clone(),
                }),
        }
    }
}

/// A `200` body naming a `User` or a `Tagma` without the record that carried
/// it. The registry cannot produce that shape itself: only the admin bearer
/// (a bare secret) has no record. A registry older than the `credential` field
/// produces it for every bearer, and reading that as no narrowing would hand a
/// narrow personal token the whole identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordlessBearer {
    /// The principal kind the answer named, for the failing caller's log line.
    pub kind: &'static str,
}

impl std::fmt::Display for RecordlessBearer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the registry answered a {} bearer with no credential record",
            self.kind
        )
    }
}

impl std::error::Error for RecordlessBearer {}

/// Lift a [`VerifyBearerResponse`] into a [`VerifiedBearer`], refusing the one
/// shape a same-version registry cannot produce: a user or a tagma with no
/// credential. The admin bearer stays accepted, since a bare secret is the one
/// identity with no record behind it.
impl TryFrom<VerifyBearerResponse> for crate::control_plane::VerifiedBearer {
    type Error = RecordlessBearer;

    fn try_from(response: VerifyBearerResponse) -> Result<Self, Self::Error> {
        use crate::principal::Principal;
        let principal: Principal = response.principal.into();
        let credential =
            response
                .credential
                .map(|credential| crate::control_plane::CredentialScope {
                    token: credential.token_id,
                    perms: credential.perms,
                });
        // Exhaustive on purpose: a new `Principal` variant must decide
        // here whether it can arrive without a record, rather than fall
        // into a catch-all and be refused under a wrong label.
        let recordless = match &principal {
            // A bare secret is the one identity with no record behind it.
            Principal::Admin => None,
            Principal::User(_) => credential.is_none().then_some("user"),
            Principal::Tagma(_) => credential.is_none().then_some("tagma"),
        };
        if let Some(kind) = recordless {
            return Err(RecordlessBearer { kind });
        }
        Ok(Self {
            principal,
            credential,
        })
    }
}

// --- tagma-profiles (canonical tagma fact read) ---

/// `POST /internal/tagma-profiles`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagmaProfilesRequest {
    pub tagma_ids: Vec<TagmaId>,
}

/// One tagma's facts in [`TagmaProfilesResponse`]. UNFILTERED: carries the raw
/// identity + usability state (`enrolled`/`revoked`/`owner_disabled`/key)
/// so the caller, not the registry, derives authorization. An alias of
/// the trait-side [`crate::control_plane::TagmaProfile`].
pub type TagmaProfileResponse = crate::control_plane::TagmaProfile;

/// `200` body: one entry per existing input id (unknown ids omitted). Always
/// `200` (never `404`) -- absence is expressed by omission, so the caller maps
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
/// `disabled` state so the relay derives the invite gate locally. An alias of
/// the trait-side [`crate::control_plane::UserIdentity`].
pub type UserIdentityResponse = crate::control_plane::UserIdentity;

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

// --- user-search (the admin member picker's account lookup) ---

/// `POST /internal/user-search`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSearchRequest {
    /// The literal prefix to match: a `users.id`, `username`, or
    /// `emails.address` prefix. The wildcards in a LIKE pattern are
    /// escaped server-side, so the query matches text, not the pattern
    /// language (usernames legitimately contain `_`).
    /// Matching is case-sensitive (the default Postgres `LIKE`).
    pub query: String,
    /// The result cap; the archeion clamps it to its own maximum.
    pub limit: u32,
}

/// `200` body: the matching accounts, ordered by username. The email is
/// a match key only -- the entries carry the minimal identity fields
/// and never the address itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSearchResponse {
    pub users: Vec<UserIdentityResponse>,
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

// --- enrollment-lookup (user-space enrollment set resolve) ---

/// `POST /internal/enrollment-lookup`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollmentLookupRequest {
    pub tagma_id: TagmaId,
}

/// `200` body: the tagma's owning user plus every enrolled, non-revoked
/// tagma in that user's space (the requested tagma included, sorted). (`404`
/// = no body, maps to `None`: unknown, pending, revoked, and owner-disabled
/// all collapse here -- the same population `verify-bearer` rejects, so a
/// tagma that cannot authenticate is also one that cannot be addressed.)
/// An alias of the trait-side
/// [`crate::control_plane::EnrollmentLookup`].
pub type EnrollmentLookupResponse = crate::control_plane::EnrollmentLookup;
#[cfg(test)]
mod tests {
    //! Round-trip every wire type so a serde shape change here surfaces as a
    //! test failure before the two services drift in prod.

    use super::*;
    use crate::bytes::Ed25519PublicKey;

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
            local_admin: false,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: VerifySessionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.user_id, resp.user_id);
        assert_eq!(back.username, "alice");
        assert_eq!(back.display_name.as_deref(), Some("Alice"));
    }

    #[test]
    fn user_search_round_trips() {
        let req = UserSearchRequest {
            query: "al".to_string(),
            limit: 20,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"query":"al","limit":20}"#);
        let back: UserSearchRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.query, "al");
        assert_eq!(back.limit, 20);

        let resp = UserSearchResponse {
            users: vec![UserIdentityResponse {
                user_id: UserId::from("u1".to_string()),
                username: "alice".to_string(),
                display_name: Some("Alice".to_string()),
                disabled: false,
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: UserSearchResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.users.len(), 1);
        assert_eq!(back.users[0].username, "alice");
        // The wire shape carries no email field: pin the key set.
        let entry = serde_json::to_value(&resp).unwrap();
        let keys: Vec<&str> = entry["users"][0]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(
            keys,
            vec!["user_id", "username", "display_name", "disabled"]
        );
    }
    #[test]
    fn verified_session_serializes_the_wire_key_set() {
        // VerifiedSession (aliased as VerifySessionResponse) owns the wire
        // shape; pin the 3 contract keys so a field add/drop/rename
        // surfaces here instead of drifting between the services.
        let session = VerifySessionResponse {
            user_id: UserId::from("u1".to_string()),
            username: "alice".to_string(),
            display_name: None,
            local_admin: false,
        };
        let json = serde_json::to_string(&session).unwrap();
        let obj: std::collections::BTreeSet<_> = serde_json::from_str::<serde_json::Value>(&json)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        let expected: std::collections::BTreeSet<_> =
            ["user_id", "username", "display_name", "local_admin"]
                .into_iter()
                .map(str::to_string)
                .collect();
        assert_eq!(obj, expected);
    }

    #[test]
    fn verified_session_defaults_local_admin_when_absent() {
        // Additive-field contract: a payload from an older producer (or the
        // default-constructed test shape) must deserialize with
        // local_admin = false, never fail.
        let legacy = r#"{"user_id":"u1","username":"alice","display_name":null}"#;
        let back: VerifySessionResponse = serde_json::from_str(legacy).unwrap();
        assert!(!back.local_admin);
    }

    #[test]
    fn wire_principal_admin_tag_round_trips() {
        let admin = VerifyBearerResponse {
            principal: WirePrincipal::Admin,
            credential: None,
        };
        let json = serde_json::to_string(&admin).unwrap();
        assert_eq!(json, r#"{"principal":{"kind":"admin"},"credential":null}"#);
        let back: VerifyBearerResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.principal, WirePrincipal::Admin));
        assert!(back.credential.is_none());

        let tagma = VerifyBearerResponse {
            principal: WirePrincipal::Tagma {
                tagma_id: TagmaId::from("t1".to_string()),
            },
            credential: Some(WireCredential {
                token_id: TokenId::from("c1".to_string()),
                perms: crate::perm::tagma_defaults(),
            }),
        };
        let json = serde_json::to_string(&tagma).unwrap();
        assert_eq!(
            json,
            r#"{"principal":{"kind":"tagma","tagma_id":"t1"},"credential":{"token_id":"c1","perms":["files:delivery:write","files:record:read","files:record:write"]}}"#
        );
        let back: VerifyBearerResponse = serde_json::from_str(&json).unwrap();
        match back.principal {
            WirePrincipal::Tagma { tagma_id } => {
                assert_eq!(tagma_id, TagmaId::from("t1".to_string()))
            }
            other => panic!("unexpected {other:?}"),
        }
        let credential = back.credential.expect("a tagma credential is a record");
        assert_eq!(credential.token_id, TokenId::from("c1".to_string()));
        assert_eq!(credential.perms, crate::perm::tagma_defaults());
    }

    #[test]
    fn principal_round_trips_through_the_wire_projection() {
        // Every variant projects onto the wire and lifts back losslessly:
        // the From/From pair is the only mapping site.
        let wire = WirePrincipal::from(Principal::Admin);
        assert!(matches!(Principal::from(wire), Principal::Admin));

        let tagma_id = TagmaId::from("t1".to_string());
        let wire = WirePrincipal::from(Principal::Tagma(tagma_id.clone()));
        assert!(matches!(Principal::from(wire), Principal::Tagma(id) if id == tagma_id));

        // A personal access token rides the bearer path as its owning user.
        let user_id = UserId::from("u1".to_string());
        let wire = WirePrincipal::from(Principal::User(user_id.clone()));
        assert_eq!(
            serde_json::to_string(&wire).unwrap(),
            r#"{"kind":"user","user_id":"u1"}"#
        );
        assert!(matches!(Principal::from(wire), Principal::User(id) if id == user_id));
    }

    #[test]
    fn verified_bearer_round_trips_through_the_response() {
        use crate::control_plane::{CredentialScope, VerifiedBearer};

        let verified = VerifiedBearer {
            principal: Principal::User(UserId::from("u1".to_string())),
            credential: Some(CredentialScope {
                token: TokenId::from("c1".to_string()),
                perms: crate::perm::pat_defaults(),
            }),
        };
        let response = VerifyBearerResponse::from(&verified);
        let back = VerifiedBearer::try_from(response).expect("a record is a record");
        assert!(
            matches!(back.principal, Principal::User(id) if id == UserId::from("u1".to_string()))
        );
        assert_eq!(back.credential, verified.credential);

        // The admin bearer has no record behind it.
        let admin = VerifiedBearer {
            principal: Principal::Admin,
            credential: None,
        };
        let back = VerifiedBearer::try_from(VerifyBearerResponse::from(&admin))
            .expect("the admin bearer carries no record");
        assert!(matches!(back.principal, Principal::Admin));
        assert!(back.credential.is_none());
    }
    #[test]
    fn a_bearer_with_no_record_is_refused_at_decode() {
        use crate::control_plane::VerifiedBearer;

        // The shape an older registry sends: no `credential` key at all. A
        // user or tagma bearer cannot resolve to no record, so reading that
        // as an un-narrowed identity is exactly the widening to refuse.
        for (body, kind) in [
            (r#"{"principal":{"kind":"user","user_id":"u1"}}"#, "user"),
            (r#"{"principal":{"kind":"tagma","tagma_id":"t1"}}"#, "tagma"),
        ] {
            let response: VerifyBearerResponse = serde_json::from_str(body).unwrap();
            let err =
                VerifiedBearer::try_from(response).expect_err("a recordless bearer is refused");
            assert_eq!(err.kind, kind);
        }

        // The admin bearer is the one identity with no record behind it, and
        // stays accepted.
        let admin: VerifyBearerResponse =
            serde_json::from_str(r#"{"principal":{"kind":"admin"}}"#).unwrap();
        let back = VerifiedBearer::try_from(admin).expect("the admin bearer carries no record");
        assert!(matches!(back.principal, Principal::Admin));
        assert!(back.credential.is_none());
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
    fn tagma_profile_serializes_the_wire_key_set() {
        // TagmaProfile (aliased as TagmaProfileResponse) owns the wire shape
        // now; pin the 9 contract keys so a future field add/drop/rename
        // surfaces here instead of drifting silently between the services.
        let profile = TagmaProfileResponse {
            tagma_id: TagmaId::from("t1".to_string()),
            pinned_public_key: None,
            owner_user_id: UserId::from("owner".to_string()),
            label: None,
            owner_username: "alice".to_string(),
            owner_display_name: None,
            enrolled: true,
            revoked: false,
            owner_disabled: false,
        };
        let json = serde_json::to_string(&profile).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = v.as_object().unwrap();
        let expected = [
            "tagma_id",
            "pinned_public_key",
            "owner_user_id",
            "label",
            "owner_username",
            "owner_display_name",
            "enrolled",
            "revoked",
            "owner_disabled",
        ];
        assert_eq!(obj.len(), expected.len());
        for key in expected {
            assert!(obj.contains_key(key), "missing wire key {key}");
        }
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
    fn user_identity_serializes_the_wire_key_set() {
        // UserIdentity (aliased as UserIdentityResponse) owns the wire shape
        // for BOTH reads (bulk + by-username); pin the 4 contract keys so a
        // field change surfaces here instead of drifting between services.
        let identity = UserIdentityResponse {
            user_id: UserId::from("u1".to_string()),
            username: "alice".to_string(),
            display_name: None,
            disabled: false,
        };
        let json = serde_json::to_string(&identity).unwrap();
        let obj: std::collections::BTreeSet<_> = serde_json::from_str::<serde_json::Value>(&json)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        let expected: std::collections::BTreeSet<_> =
            ["user_id", "username", "display_name", "disabled"]
                .into_iter()
                .map(str::to_string)
                .collect();
        assert_eq!(obj, expected);
    }

    #[test]
    fn tunnel_proof_ts_round_trips() {
        let resp = TunnelProofTsResponse { fresh: true };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"fresh":true}"#);
        let back: TunnelProofTsResponse = serde_json::from_str(&json).unwrap();
        assert!(back.fresh);
    }

    #[test]
    fn enrollment_lookup_round_trips() {
        let req = EnrollmentLookupRequest {
            tagma_id: TagmaId::from("tagma-abc".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"tagma_id":"tagma-abc"}"#);
        let back: EnrollmentLookupRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tagma_id, req.tagma_id);

        let resp = EnrollmentLookupResponse {
            user_id: UserId::from("user-1".to_string()),
            enrolled_tagmas: vec![
                TagmaId::from("tagma-abc".to_string()),
                TagmaId::from("tagma-xyz".to_string()),
            ],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(
            json,
            r#"{"user_id":"user-1","enrolled_tagmas":["tagma-abc","tagma-xyz"]}"#
        );
        let back: EnrollmentLookupResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.user_id, resp.user_id);
        assert_eq!(back.enrolled_tagmas, resp.enrolled_tagmas);
    }
}
