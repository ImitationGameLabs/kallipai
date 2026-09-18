//! Shared router state.

use std::sync::Arc;

use crate::db::Db;
use crate::management::ManagementAuth;
use crate::quota::QuotaLedger;
use crate::secret::KeyCache;

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
    /// The in-memory quota ledger (rpm/tpm window counters), shared by
    /// the forwarding path. Process-local by ruling: restarts reset the
    /// windows; the database holds the audit rows and the budget sum.
    pub quota: Arc<QuotaLedger>,
    /// The management credential (the admin face's authenticator). The
    /// proxy-key family never flows through here: distribution handlers
    /// see this trait object but cannot resolve a proxy key with it, and
    /// the secret module never sees the management credential.
    pub management: Arc<dyn ManagementAuth>,
    /// The process-local resolved-key cache for the distribution
    /// read path. Management writes clear it after commit (see
    /// [`KeyCache::clear`]); the store stays the source of truth.
    pub key_cache: std::sync::Arc<KeyCache>,
}
