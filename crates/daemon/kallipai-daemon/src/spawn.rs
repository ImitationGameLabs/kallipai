//! The spawn pipeline: authorize → validate → register the record →
//! harvest the login environment → detach-exec via the helper → wait for
//! the instance's self-written runtime.json, rolling back the record on
//! timeout.
//!
//! The record area is the registry: a spawn registers `<slug>.json`
//! before launching and deregisters it on failure, so collision checks
//! and discovery read the same authority. The instance's data directory
//! is handed to the tagma explicitly (`KALLIPAI_TAGMA_DATA_DIR`) — the
//! two sides no longer assume a shared tree root.
//!
//! The pipeline lives in submodules: `authorize` (the uid matrix and the
//! delegate refusals), `identity` (the launch identity, the passwd lookups
//! and the runtime directories), `harvest` (the login-environment harvest
//! and its fallback), `env` (the environment assembly), `launch` (the record
//! checks, the detach-exec and the anchors) and `error` (the refusal shapes
//! and their guidance). The re-exports below keep the names the rest of the
//! daemon uses resolving under `crate::spawn::`; the two test-only names are
//! re-exported for the `adopt.rs` suite, which lives outside this subtree.

mod authorize;
mod env;
mod error;
mod harvest;
mod identity;
mod launch;

#[cfg(test)]
pub use error::OverlapRival;
#[cfg(test)]
pub(crate) use identity::ResolvedUser;

pub use error::{OverlapInput, SpawnError};
pub use launch::spawn;

pub(crate) use authorize::{authorized, delegate_keyless_rejection};
pub(crate) use env::validate_user_env;
pub(crate) use error::deny_guidance;
pub(crate) use identity::{
    LaunchIdentity, OwnerInference, cached_passwd_identity, ensure_not_root_inplace,
    identity_from_record, infer_owner_decision, passwd_by_name, passwd_by_uid, path_owner_uid,
    require_root_for_drop, resolve_launch_identity,
};
pub(crate) use launch::{exe_runnable, instance_data_dir, launch, now_unix, overlaps};

#[cfg(test)]
mod tests;
