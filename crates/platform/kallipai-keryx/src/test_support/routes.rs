//! Shared test fixtures for the route tests: an in-memory `SharedConvState`
//! wired to a mock registry (the identity oracle), plus principal
//! constructors. The keryx keeps projections in memory, so there is no
//! per-test database to provision.

use std::sync::Arc;
use std::time::Duration;

use kallipai_archeion_common::ids::{TagmaId, UserId};
use kallipai_archeion_common::principal::Principal;

use crate::auth::AuthPrincipal;
use crate::state::SharedConvState;
use crate::test_support::MockControlPlane;

pub fn as_user(u: &UserId) -> AuthPrincipal {
    AuthPrincipal(Principal::User(u.clone()))
}

pub fn as_tagma(t: &TagmaId) -> AuthPrincipal {
    AuthPrincipal(Principal::Tagma(t.clone()))
}

/// Route-test state: the in-memory pair, no durable store involved.
pub async fn mem_state() -> (SharedConvState, Arc<MockControlPlane>) {
    crate::test_support::make_state(60, Duration::from_secs(10))
}
