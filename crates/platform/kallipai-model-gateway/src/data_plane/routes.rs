//! The data-plane routing: tagma-identity authentication and profile
//! selection, all decided once in `request_filter`.
//!
//! Everything that can answer without an upstream answers in that first
//! phase: every 4xx gate on the forwarding path (unknown profile,
//! visibility domain, parking, family mismatch) is an axum-shaped
//! error written through the bridge (`wire`). The forwarding selection
//! lands a [`Selection`] in the request context; the peer and rewrite
//! phases read it from there.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::{HeaderMap, Method};
use axum::response::IntoResponse;
use sea_orm::EntityTrait;

use crate::db;
pub(crate) use crate::distribution::TagmaIdentity;
use crate::distribution::visibility;
use crate::forward::{self, dialect};
use crate::metrics::{ForwardOutcome, ResolveErrorClass};
use crate::registry;
use crate::routes::ApiError;
use crate::secret;
use crate::state::AppState;

use super::context::{DialParams, FailureReason, Route, Selection};
use super::resolver::{ResolveFailure, Resolver, UPSTREAM_RETRY_AFTER};

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

/// What a client reads when the upstream could not be resolved: generic
/// on purpose, since the host and the resolver's answer belong to the
/// failure line, not to the caller.
const UPSTREAM_UNRESOLVED_MESSAGE: &str =
    "the selected upstream could not be resolved; retry shortly";

/// Resolve the candidate addresses outside the forwarding worker through
/// the system resolver: every answer becomes a candidate in the order
/// the resolver returned it, each already carrying the credential
/// prefix's port, and the lookup is cached, shared with concurrent
/// lookups for the same host, and bounded.
async fn resolve_candidates(
    parsed: &ParsedPrefix,
    resolver: &Resolver,
) -> Result<Arc<[SocketAddr]>, ResolveFailure> {
    let addresses = resolver.resolve(&parsed.host, &parsed.authority).await?;
    // The cache holds addresses without a port -- its key is the host --
    // so the credential's port is joined here, once per candidate. A
    // successful answer carries at least one address, so the empty case
    // is turned into the failure rather than collected into an empty list.
    if addresses.is_empty() {
        return Err(ResolveFailure {
            class: ResolveErrorClass::NoAddresses,
            detail: "the resolver returned no address".to_string(),
        });
    }
    Ok(addresses
        .iter()
        .map(|address| SocketAddr::new(*address, parsed.port))
        .collect())
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

/// What a selection-phase refusal or a gateway-side failure travels as:
/// the client's rendered answer, the outcome class the observability
/// face counts the request under, and -- on the failure side only --
/// the reason category and readable text the failure line records.
/// `request_filter` renders the response and stages the outcome; the
/// `logging` phase is the single terminal record point.
///
/// One type carries two kinds of content. The three rejection
/// constructors ([`Self::route`], [`Self::visibility`],
/// [`Self::parking`]) set neither `reason` nor `detail`; the failure
/// constructors ([`Self::internal`], [`Self::internal_with_response`],
/// [`Self::unavailable`]) set both, and each carries its own answer:
/// a rendered 500 from [`Self::internal`], the store mapping's status
/// for [`Self::internal_with_response`], and a 503 with a `retry-after`
/// hint from [`Self::unavailable`]. The name still says `gate` because
/// the selection gates were all this type once; renaming it belongs to
/// whoever next moves this surface.
pub(crate) struct GateError {
    pub(crate) outcome: ForwardOutcome,
    /// The reason category of a failure the gateway answers itself, and
    /// the readable text behind it: the failure line carries both.
    /// Invariant: a gate refusal sets neither.
    pub(crate) reason: Option<FailureReason>,
    pub(crate) detail: Option<String>,
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

    /// A gateway-side failure on the forward path (5xx): missing
    /// credentials, unparseable endpoint prefixes. The text names
    /// the object behind the failure -- it is what the client's redacted
    /// 500 hides and what the failure line records.
    pub(crate) fn internal(reason: FailureReason, detail: impl std::fmt::Display) -> Self {
        let detail = detail.to_string();
        Self::failure(
            ForwardOutcome::InternalError,
            Some(reason),
            Some(detail.clone()),
            ApiError::internal(&detail).into_response(),
        )
    }

    /// The upstream could not be resolved: a dependency failed before any
    /// upstream was reached, so the answer says so -- 503 plus a retry
    /// hint. Nothing was sent upstream, which is what makes the retry
    /// side-effect free; the body stays generic while the failure line
    /// records the resolver's own text. It lands on `resolve_error` rather
    /// than `internal_error`, because a resolver outage is not the
    /// gateway's own bug: the run counts the two apart, and the retry
    /// hint tells the caller what the outcome tells a dashboard.
    pub(crate) fn unavailable(reason: FailureReason, detail: impl std::fmt::Display) -> Self {
        let detail = detail.to_string();
        let mut response = ApiError::unavailable(UPSTREAM_UNRESOLVED_MESSAGE).into_response();
        response.headers_mut().insert(
            axum::http::header::RETRY_AFTER,
            axum::http::HeaderValue::from(UPSTREAM_RETRY_AFTER.as_secs()),
        );
        Self::failure(
            ForwardOutcome::ResolveError,
            Some(reason),
            Some(detail),
            response,
        )
    }

    /// The same failure where the response comes from the store mapping:
    /// it renders the answer and owns the status it answers with, while
    /// the line still takes the reason and the text.
    pub(crate) fn internal_with_response(
        reason: FailureReason,
        detail: impl std::fmt::Display,
        error: ApiError,
    ) -> Self {
        Self::failure(
            ForwardOutcome::InternalError,
            Some(reason),
            Some(detail.to_string()),
            error.into_response(),
        )
    }

    fn gate(outcome: ForwardOutcome, error: ApiError) -> Self {
        Self::failure(outcome, None, None, error.into_response())
    }

    fn failure(
        outcome: ForwardOutcome,
        reason: Option<FailureReason>,
        detail: Option<String>,
        response: axum::response::Response,
    ) -> Self {
        Self {
            outcome,
            reason,
            detail,
            response: Box::new(response),
        }
    }

    pub(crate) fn into_response(self) -> axum::response::Response {
        *self.response
    }
}

/// A store failure behind the selection: the client answer keeps the
/// store mapping's status, while the failure line names the reason and
/// the sentence the mapping logged.
///
/// Two invariants hold. The response and the recorded text come from the
/// same [`sea_orm::DbErr`] -- [`db::map_db_err`] renders the answer and
/// [`db::store_error_detail`] the text -- so one failure reads the same
/// in both places. And every call site today is a read on the selection
/// path, so the mapping's unique-violation branch cannot be reached from
/// here; it stays for the handlers that write.
fn store_failure(error: sea_orm::DbErr) -> GateError {
    let detail = db::store_error_detail(&error);
    GateError::internal_with_response(FailureReason::StoreError, detail, db::map_db_err(error))
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
    resolver: &Resolver,
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
                .map_err(store_failure)?;
            let Some(profile) = rows.pop() else {
                return Err(GateError::route(ApiError::not_found("no such profile")));
            };
            let served = registry::served_profile(&state.db, profile)
                .await
                .map_err(store_failure)?
                .ok_or_else(|| GateError::route(ApiError::not_found("no such profile")))?;
            let grant = visibility::profile_grant(
                &state.db,
                &identity.viewer,
                &served.provider_owner,
                explicit,
            )
            .await
            .map_err(store_failure)?;
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
                    .map_err(store_failure)?
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
            .map_err(store_failure)?
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
            .map_err(store_failure)?
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
                .map_err(store_failure)?
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
        .map_err(store_failure)?
        .ok_or_else(|| {
            GateError::internal(
                FailureReason::MissingCredential,
                format_args!("no provider credential for profile {profile_id}"),
            )
        })?;
    let parsed = parse_endpoint_prefix(credential.endpoint_prefix()).ok_or_else(|| {
        GateError::internal(
            FailureReason::UnparseableEndpointPrefix,
            format_args!("the upstream endpoint prefix for profile {profile_id} does not parse"),
        )
    })?;
    // SNI is the host: the certificate the upstream presents names the
    // host we dialed, and no per-credential SNI field exists.
    //
    // Resolution is the selection's, not the constructor's: `HttpPeer::new`
    // on a hostname runs a blocking lookup and unwraps it, so a resolver
    // failure panicked a worker instead of answering the request. The
    // addresses resolved here are `SocketAddr`s, whose `ToSocketAddrs` impl
    // is infallible: no name lookup runs inside the constructor and it
    // cannot fail.
    let addresses = match resolve_candidates(&parsed, resolver).await {
        Ok(addresses) => addresses,
        Err(failure) => {
            return Err(GateError::unavailable(
                FailureReason::UnresolvedUpstream,
                format_args!(
                    "resolving the upstream {} for profile {profile_id} failed: {}",
                    parsed.authority, failure.detail
                ),
            ));
        }
    };
    let dial = DialParams {
        tls: parsed.tls,
        sni: parsed.host.clone(),
    };
    let auth = dialect::auth_style(&served.family);
    Ok(Selection {
        addresses,
        dial,
        credential,
        auth,
        api_family: served.family.clone(),
        base_path: parsed.base_path,
        authority: parsed.authority,
        profile_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::IpAddr;
    use std::time::Duration;

    use async_trait::async_trait;

    use crate::data_plane::resolver::Lookup;

    /// A malformed host fails as a value: a name with a space is
    /// rejected locally as a malformed name before any query leaves
    /// the host, so the answer is `lookup_error` without a name server
    /// having to be reachable.
    #[tokio::test]
    async fn a_host_that_cannot_be_a_name_fails_as_a_lookup_error() {
        let parsed = parse_endpoint_prefix("http://bad host/v1").expect("the prefix parses");
        let resolver = Resolver::system(crate::metrics::Metrics::private());
        let failure = resolve_candidates(&parsed, &resolver)
            .await
            .expect_err("no address");
        assert_eq!(failure.class, ResolveErrorClass::LookupError);
        assert!(!failure.detail.is_empty());
    }

    /// A literal address resolves to itself without a lookup.
    #[tokio::test]
    async fn a_literal_address_resolves_to_itself() {
        let parsed =
            parse_endpoint_prefix("http://203.0.113.7:8080/v1").expect("the prefix parses");
        let resolver = Resolver::system(crate::metrics::Metrics::private());
        let addresses = resolve_candidates(&parsed, &resolver)
            .await
            .expect("one address");
        assert_eq!(addresses.len(), 1);
        assert_eq!(addresses[0].to_string(), "203.0.113.7:8080");
    }

    /// Every resolver answer becomes a candidate, in the answer's own
    /// order, each joined to the credential prefix's port.
    #[tokio::test]
    async fn every_resolver_answer_becomes_a_candidate_on_the_prefix_port() {
        struct TwoAnswers;

        #[async_trait]
        impl Lookup for TwoAnswers {
            async fn lookup(&self, _host: &str) -> std::io::Result<Vec<IpAddr>> {
                Ok(vec![
                    IpAddr::from([198, 51, 100, 23]),
                    IpAddr::from([198, 51, 100, 42]),
                ])
            }
        }

        let parsed =
            parse_endpoint_prefix("https://upstream.test:8443/v1").expect("the prefix parses");
        let resolver = Resolver::with_lookup(
            crate::metrics::Metrics::private(),
            Duration::from_secs(1),
            Arc::new(TwoAnswers),
        );
        let addresses = resolve_candidates(&parsed, &resolver)
            .await
            .expect("two addresses");
        let candidates: Vec<String> = addresses
            .iter()
            .map(|address| address.to_string())
            .collect();
        assert_eq!(candidates, ["198.51.100.23:8443", "198.51.100.42:8443"]);
    }
}
