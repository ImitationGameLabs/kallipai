//! The spawn pipeline: authorize → validate → harvest and compose the
//! launch environment → resolve the instance roots → register the record →
//! detach-exec via the helper → wait for the instance's self-written
//! runtime.json, rolling back the record on timeout.
//!
//! The record area is the registry: a spawn registers `<slug>.json`
//! before launching and deregisters it on failure, so collision checks
//! and discovery read the same authority. The instance's roots are not handed
//! down as separate keys: the composed env carries the deployment's root
//! variables verbatim, and both sides resolve the three roots from those same
//! pairs with one shared function (`kallipai_daemon_common::roots`).
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

pub use error::{OverlapInput, SpawnError};
pub use launch::spawn;

pub(crate) use authorize::{authorized, delegate_keyless_rejection};
pub(crate) use env::{deployment_root_vars, validate_user_env};
pub(crate) use error::deny_guidance;
pub(crate) use harvest::Harvest;
#[cfg(test)]
pub(crate) use identity::ResolvedUser;
pub(crate) use identity::{
    LaunchIdentity, OwnerInference, cached_passwd_identity, ensure_not_root_inplace,
    identity_from_record, infer_owner_decision, passwd_by_name, passwd_by_uid, path_owner_uid,
    require_root_for_drop, resolve_launch_identity,
};
pub(crate) use launch::{
    exe_runnable, launch, launch_harvest, note_roots, now_unix, overlaps, prepare_launch,
    prepare_launch_with, same_tree,
};

#[cfg(test)]
mod tests;
