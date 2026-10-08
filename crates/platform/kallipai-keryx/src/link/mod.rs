//! The link domain: every surface whose peer is a tagma.
//!
//! The tunnel (a tagma's only inbound channel), the upstream event
//! intake, the state/projection/status read faces, the manage proxy,
//! the user-tagma E2E conversations, and the files-notify internal
//! face. Grouping them keeps every tagma-facing concern in one place.
mod conversations;
mod internal;
mod manage_proxy;
mod signal;
mod state;
mod status;
mod tunnel;
mod upstream;

use axum::Router;

use crate::state::SharedConvState;

/// The link domain's data-plane routes.
pub(crate) fn router() -> Router<SharedConvState> {
    Router::new()
        .merge(conversations::router())
        .merge(manage_proxy::router())
        .merge(state::router())
        .merge(upstream::router())
        .merge(tunnel::router())
}

/// The service-to-service `/internal/*` surface (files notify). Its
/// mount point and token guard live in [`crate::routes`].
pub(crate) fn internal_router(state: SharedConvState) -> Router {
    internal::router(state)
}

// Test-only seam: the events face replays status pushes through the
// link domain's relay to drive its SSE tests.
#[cfg(test)]
pub(crate) use status::relay_status;
