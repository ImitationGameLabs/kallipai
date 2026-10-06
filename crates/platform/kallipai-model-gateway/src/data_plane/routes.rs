//! The data-plane routing: tagma-identity authentication and profile
//! selection, all decided once in `request_filter`.
//!
//! Everything that can answer without an upstream answers in that first
//! phase: every 4xx gate on the forwarding path (unknown profile,
//! visibility domain, parking, family mismatch) is an axum-shaped
//! error written through the bridge (`wire`). The forwarding selection
//! lands a [`Selection`] in the request context; the peer and rewrite
//! phases read it from there.

use axum::http::{HeaderMap, Method};
use axum::response::IntoResponse;
use pingora::prelude::HttpPeer;
use sea_orm::EntityTrait;

use crate::db;
pub(crate) use crate::distribution::TagmaIdentity;
use crate::distribution::visibility;
use crate::forward::{self, dialect};
use crate::metrics::ForwardOutcome;
use crate::registry;
use crate::routes::ApiError;
use crate::secret;
use crate::state::AppState;

use super::context::{Route, Selection};

/// The parsed upstream prefix: the peer parts plus the rewrite
/// inputs.
pub(crate) struct ParsedPrefix {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub base_path: String,
    /// The URL authority verbatim (the Host header value).
    pub authority: String,
}
/// Parse a credential's endpoint prefix into the peer parts and the
/// rewrite inputs: `https://api.example.com/v1` parses with tls on,
/// the host `api.example.com`, the default port 443, and the base
/// path `/v1`. Bracketed IPv6 literals parse; a bracket-less
/// authority with several colons does not -- that text is a malformed
/// host, and splitting it at the last colon would silently misread an
/// IPv6 address.
pub(crate) fn parse_endpoint_prefix(prefix: &str) -> Option<ParsedPrefix> {
    let (tls, rest) = prefix
        .strip_prefix("https://")
        .map(|r| (true, r))
        .or_else(|| prefix.strip_prefix("http://").map(|r| (false, r)))?;
    let (authority, base_path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_owned()),
        None => (rest, String::new()),
    };
    let default_port = if tls { 443 } else { 80 };
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        let end = stripped.find(']')?;
        let host = &stripped[..end];
        let port = match stripped[end + 1..].strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None => default_port,
        };
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => (h, p.parse().ok()?),
            // Bare multi-colon authority: malformed, never a split guess.
            Some(_) => return None,
            None => (authority, default_port),
        }
    };
    Some(ParsedPrefix {
        tls,
        host: host.to_owned(),
        port,
        authority: authority.to_owned(),
        base_path,
    })
}

/// The path/method dispatch, decided once. The `/admin` family is a 404
/// on this plane including the bare prefix: the management face owns the
/// whole namespace on its own listener, so an `/admin` hit on the public
/// port is a client mistake, never something to forward. A known path
/// with the wrong method is a 405, not a bare 404.
pub(crate) fn route_of(method: &Method, path: &str) -> Route {
    if path == "/admin" || path.starts_with("/admin/") {
        return Route::Admin;
    }
    if method == Method::POST {
        match path {
            "/chat/completions" => {
                return Route::Forward(dialect::Endpoint::ChatCompletions);
            }
            "/responses" => return Route::Forward(dialect::Endpoint::Responses),
            "/messages" => return Route::Forward(dialect::Endpoint::Messages),
            _ => {}
        }
    }
    if method == Method::GET && path == "/health" {
        return Route::Health;
    }
    let known =
        path == "/health" || matches!(path, "/chat/completions" | "/responses" | "/messages");
    if known {
        return Route::MethodNotAllowed;
    }
    Route::NotFound
}

/// The tagma-identity authentication chain: the presenting bearer to
/// the platform identity. The axum extractor wraps the same resolution
/// (the handlers keep their extraction shape); the data plane calls it
/// directly with the session's headers.
pub(crate) async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<TagmaIdentity, ApiError> {
    TagmaIdentity::from_headers(state, headers).await
}

/// A selection-gate rejection: the client's rendered answer plus the
/// outcome class the observability face counts the request under.
/// `request_filter` renders the response and stages the outcome; the
/// `logging` phase is the single terminal record point.
pub(crate) struct GateError {
    pub(crate) outcome: ForwardOutcome,
    response: Box<axum::response::Response>,
}

impl GateError {
    /// The routing family: the request never reached an upstream (the
    /// 404/405/400 vocabulary).
    pub(crate) fn route(error: ApiError) -> Self {
        Self::gate(ForwardOutcome::GatedRoute, error)
    }

    /// The visibility domain rejected the target (403).
    pub(crate) fn visibility(error: ApiError) -> Self {
        Self::gate(ForwardOutcome::GatedVisibility, error)
    }

    /// The target is parked: a platform state, not an error (403).
    pub(crate) fn parking(error: ApiError) -> Self {
        Self::gate(ForwardOutcome::GatedParking, error)
    }

    /// A gateway-side failure on the forward path (5xx): store errors,
    /// missing credentials, unparseable endpoint prefixes.
    pub(crate) fn internal(error: ApiError) -> Self {
        Self::gate(ForwardOutcome::ProxyError, error)
    }

    fn gate(outcome: ForwardOutcome, error: ApiError) -> Self {
        Self {
            outcome,
            response: Box::new(error.into_response()),
        }
    }

    pub(crate) fn into_response(self) -> axum::response::Response {
        *self.response
    }
}

/// The forwarding selection: the explicit override, the visibility
/// contract on it, the parking gate, the family gate, and the
/// credential fetch. The outcome is
/// everything the later phases need, so no phase after
/// `request_filter` touches the registry again. The error side is a
/// [`GateError`]: the gate's `ApiError` travels to the client through
/// the bridge, and the outcome class travels to the observability
/// face.
pub(crate) async fn select_route(
    state: &AppState,
    identity: &TagmaIdentity,
    headers: &HeaderMap,
    endpoint: dialect::Endpoint,
) -> Result<Selection, GateError> {
    // Profile resolution: the explicit override, authorized under the
    // visibility domain -- then the parking gate. Availability is a
    // second gate orthogonal to authorization: a parked profile inside
    // the domain is still not forwardable.
    let served = match headers
        .get(forward::PROFILE_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        Some(explicit) => {
            // The row: the id is globally unique; the grant decides
            // what this identity may see of it, the same vocabulary as
            // the distribution face: a catalog row outside the domain
            // is 403, a user-space row 404 (no existence oracle across
            // accounts).
            let mut rows = registry::profiles_by_id(&state.db, explicit)
                .await
                .map_err(|e| GateError::internal(db::map_db_err(e)))?;
            let Some(profile) = rows.pop() else {
                return Err(GateError::route(ApiError::not_found("no such profile")));
            };
            let served = registry::served_profile(&state.db, profile)
                .await
                .map_err(|e| GateError::internal(db::map_db_err(e)))?
                .ok_or_else(|| GateError::route(ApiError::not_found("no such profile")))?;
            let grant = visibility::profile_grant(
                &state.db,
                &identity.viewer,
                &served.provider_owner,
                explicit,
            )
            .await
            .map_err(|e| GateError::internal(db::map_db_err(e)))?;
            match grant {
                None if served.provider_owner == registry::CATALOG_OWNER => {
                    return Err(GateError::visibility(ApiError::forbidden(
                        "profile is outside the visibility domain",
                    )));
                }
                None => {
                    return Err(GateError::route(ApiError::not_found("no such profile")));
                }
                Some(_) => {}
            }
            if served.profile.parked {
                return Err(GateError::parking(ApiError::forbidden("profile is parked")));
            }
            served
        }
        None => {
            // The selected collection's default set: the single
            // store the pull face and this forward share.
            let Some(pointer) =
                registry::gateway_selection::Entity::find_by_id(identity.tagma_id.clone())
                    .one(&state.db)
                    .await
                    .map_err(|e| GateError::internal(db::map_db_err(e)))?
            else {
                return Err(GateError::route(ApiError::not_found(
                    "no collection selected",
                )));
            };
            let Some(collection) = registry::collection::Entity::find_by_id((
                pointer.owner.clone(),
                pointer.collection_name.clone(),
            ))
            .one(&state.db)
            .await
            .map_err(|e| GateError::internal(db::map_db_err(e)))?
            else {
                return Err(GateError::route(ApiError::not_found(
                    "the selected collection no longer exists",
                )));
            };
            let Some(default_set) = collection.default_set_name.clone() else {
                return Err(GateError::route(ApiError::not_found(
                    "the selected collection has no default set",
                )));
            };
            let served = registry::set_head_for_families(
                &state.db,
                &collection.owner,
                &default_set,
                endpoint.families(),
            )
            .await
            .map_err(|e| GateError::internal(db::map_db_err(e)))?
            .ok_or_else(|| {
                GateError::route(ApiError::not_found(
                    "the selected collection's default set has no member serving this endpoint",
                ))
            })?;
            // The domain gate is set-level, unlike the override
            // path's profile-level grant check: the default set
            // itself must be inside the identity's domain, or the
            // forward is denied -- the default deployment must not
            // become a fail-open side door around the visibility
            // domain.
            if !visibility::set_visible(&state.db, &identity.viewer, &default_set)
                .await
                .map_err(|e| GateError::internal(db::map_db_err(e)))?
            {
                return Err(GateError::visibility(ApiError::forbidden(
                    "default profile is outside the visibility domain",
                )));
            }
            if served.profile.parked {
                return Err(GateError::parking(ApiError::forbidden("profile is parked")));
            }
            served
        }
    };
    // The family gate: an endpoint speaks a fixed set of wire families,
    // and a profile outside the set is a client configuration error, not
    // a best-effort forward (the wire path and the credential shape
    // would both be guesses). The 400 names both sides of the mismatch.
    if !endpoint.families().contains(&served.family.as_str()) {
        return Err(GateError::route(ApiError::bad_request(format!(
            "profile family {:?} cannot serve this endpoint (serves {:?})",
            served.family,
            endpoint.families()
        ))));
    }
    let profile_id = served.profile.profile_id.clone();
    let credential = secret::credential_for_profile(&state.db, &served.profile)
        .await
        .map_err(|e| GateError::internal(db::map_db_err(e)))?
        .ok_or_else(|| {
            GateError::internal(ApiError::internal(format_args!(
                "no provider credential for profile {profile_id}"
            )))
        })?;
    let parsed = parse_endpoint_prefix(credential.endpoint_prefix()).ok_or_else(|| {
        GateError::internal(ApiError::internal(format_args!(
            "the upstream endpoint prefix for profile {profile_id} does not parse"
        )))
    })?;
    // SNI is the host: the certificate the upstream presents names the
    // host we dialed, and no per-credential SNI field exists.
    let peer = HttpPeer::new(
        (parsed.host.as_str(), parsed.port),
        parsed.tls,
        parsed.host.clone(),
    );
    let auth = dialect::auth_style(&served.family);
    Ok(Selection {
        peer,
        credential,
        auth,
        api_family: served.family.clone(),
        base_path: parsed.base_path,
        authority: parsed.authority,
    })
}
