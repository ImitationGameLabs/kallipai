//! The team domain: declarative team management.
//!
//! Read-only diagnostic face: a three-way comparison of the declaration
//! (read from the request-named path), the lock mapping the CLI folds
//! into the request, and the live registry. It reports what converge
//! WOULD decide, never what it did.
//!
//! Child modules: `status` (the three-way comparison report + lock-pair
//! parsing shared by both endpoints) and `converge` (the planned-actions
//! executor).

use crate::state::SharedState;

mod converge;
mod status;
#[cfg(test)]
mod tests;

/// The team-domain routes, mounted at /team by the root router.
pub(crate) fn router() -> axum::Router<SharedState> {
    axum::Router::new()
        .route("/status", axum::routing::get(status::team_status))
        .route("/converge", axum::routing::post(converge::team_converge))
}
