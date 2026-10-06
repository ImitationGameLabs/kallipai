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
) -> Result<TagmaIdentity, Box<axum::response::Response>> {
    TagmaIdentity::from_headers(state, headers)
        .await
        .map_err(|error| Box::new(error.into_response()))
}

/// The forwarding selection: the explicit override, the visibility
/// contract on it, the parking gate, the family gate, and the
/// credential fetch. The outcome is
/// everything the later phases need, so no phase after
/// `request_filter` touches the registry again. The error side is the
/// rendered response: the handler gates speak `ApiError` and travel to
/// the client through the bridge.
pub(crate) async fn select_route(
    state: &AppState,
    identity: &TagmaIdentity,
    headers: &HeaderMap,
    endpoint: dialect::Endpoint,
) -> Result<Selection, Box<axum::response::Response>> {
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
                .map_err(|e| Box::new(db::map_db_err(e).into_response()))?;
            let Some(profile) = rows.pop() else {
                return Err(Box::new(
                    ApiError::not_found("no such profile").into_response(),
                ));
            };
            let served = registry::served_profile(&state.db, profile)
                .await
                .map_err(|e| Box::new(db::map_db_err(e).into_response()))?
                .ok_or_else(|| Box::new(ApiError::not_found("no such profile").into_response()))?;
            let grant = visibility::profile_grant(
                &state.db,
                &identity.viewer,
                &served.provider_owner,
                explicit,
            )
            .await
            .map_err(|e| Box::new(db::map_db_err(e).into_response()))?;
            match grant {
                None if served.provider_owner == registry::CATALOG_OWNER => {
                    return Err(Box::new(
                        ApiError::forbidden("profile is outside the visibility domain")
                            .into_response(),
                    ));
                }
                None => {
                    return Err(Box::new(
                        ApiError::not_found("no such profile").into_response(),
                    ));
                }
                Some(_) => {}
            }
            if served.profile.parked {
                return Err(Box::new(
                    ApiError::forbidden("profile is parked").into_response(),
                ));
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
                    .map_err(|e| Box::new(db::map_db_err(e).into_response()))?
            else {
                return Err(Box::new(
                    ApiError::not_found("no collection selected").into_response(),
                ));
            };
            let Some(collection) = registry::collection::Entity::find_by_id((
                pointer.owner.clone(),
                pointer.collection_name.clone(),
            ))
            .one(&state.db)
            .await
            .map_err(|e| Box::new(db::map_db_err(e).into_response()))?
            else {
                return Err(Box::new(
                    ApiError::not_found("the selected collection no longer exists").into_response(),
                ));
            };
            let Some(default_set) = collection.default_set_name.clone() else {
                return Err(Box::new(
                    ApiError::not_found("the selected collection has no default set")
                        .into_response(),
                ));
            };
            let served = registry::set_head_for_families(
                &state.db,
                &collection.owner,
                &default_set,
                endpoint.families(),
            )
            .await
            .map_err(|e| Box::new(db::map_db_err(e).into_response()))?
            .ok_or_else(|| {
                Box::new(
                    ApiError::not_found(
                        "the selected collection's default set has no member serving this endpoint",
                    )
                    .into_response(),
                )
            })?;
            // The domain gate is set-level, unlike the override
            // path's profile-level grant check: the default set
            // itself must be inside the identity's domain, or the
            // forward is denied -- the default deployment must not
            // become a fail-open side door around the visibility
            // domain.
            if !visibility::set_visible(&state.db, &identity.viewer, &default_set)
                .await
                .map_err(|e| Box::new(db::map_db_err(e).into_response()))?
            {
                return Err(Box::new(
                    ApiError::forbidden("default profile is outside the visibility domain")
                        .into_response(),
                ));
            }
            if served.profile.parked {
                return Err(Box::new(
                    ApiError::forbidden("profile is parked").into_response(),
                ));
            }
            served
        }
    };
    // The family gate: an endpoint speaks a fixed set of wire families,
    // and a profile outside the set is a client configuration error, not
    // a best-effort forward (the wire path and the credential shape
    // would both be guesses). The 400 names both sides of the mismatch.
    if !endpoint.families().contains(&served.family.as_str()) {
        return Err(Box::new(
            ApiError::bad_request(format!(
                "profile family {:?} cannot serve this endpoint (serves {:?})",
                served.family,
                endpoint.families()
            ))
            .into_response(),
        ));
    }
    let profile_id = served.profile.profile_id.clone();
    let credential = secret::credential_for_profile(&state.db, &served.profile)
        .await
        .map_err(|e| Box::new(db::map_db_err(e).into_response()))?
        .ok_or_else(|| {
            Box::new(
                ApiError::internal(format_args!(
                    "no provider credential for profile {profile_id}"
                ))
                .into_response(),
            )
        })?;
    let parsed = parse_endpoint_prefix(credential.endpoint_prefix()).ok_or_else(|| {
        Box::new(
            ApiError::internal(format_args!(
                "the upstream endpoint prefix for profile {profile_id} does not parse"
            ))
            .into_response(),
        )
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
        base_path: parsed.base_path,
        authority: parsed.authority,
    })
}
