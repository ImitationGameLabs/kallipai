//! Shared router state.

use std::sync::Arc;

use crate::db::Db;
use crate::management::AdminAuth;
use crate::secret::IdentityCache;

/// State shared by every route handler: one cloneable handle to the durable
/// store. Credential material never travels through this type -- handlers on
/// the distribution face receive only this store handle, and the secret face
/// resolves credentials inside the `secret` module.
#[derive(Clone)]
pub struct AppState {
    /// The durable store handle. Credential rows live behind the secret
    /// module's resolve/credential functions -- no handler queries
    /// credential tables through this handle directly.
    pub db: Db,
    /// The gateway's own base URL, echoed as `base_url` in every
    /// distributed profile (the proxy self reference).
    pub public_base_url: String,
    /// The admin face's authentication source. The distribution read
    /// path resolves its tagma bearers through the same verifier (see
    /// [`crate::secret`]), but only the admin principal -- or the
    /// local-admin session -- carries management rights: a tagma
    /// bearer never opens the /admin face.
    pub management: AdminAuth,
    /// The process-local identity cache for the distribution read
    /// path. Pure TTL staleness (the authority is the truth; see
    /// [`IdentityCache`]).
    pub identity_cache: Arc<IdentityCache>,
    /// The macro-observability collectors the data plane writes from
    /// its phase boundaries and the management plane renders at
    /// `/metrics` (see [`crate::metrics`]).
    pub metrics: crate::metrics::Metrics,
}
