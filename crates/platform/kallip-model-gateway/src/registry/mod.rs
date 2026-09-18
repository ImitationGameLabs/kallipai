//! The registry domain: model profiles, their ordered sets, the parking
//! resource, and registry-level metadata.
//!
//! This is the distribution face's data: no credential columns live here
//! (upstream credentials and proxy keys are the secret domain's tables, see
//! [`crate::secret`]). Everything in this module is safe to serve -- the
//! sanitization contract is structural: the entities below simply do not
//! carry secret material.
//!
//! `position` in `set_members` is failover semantics (the first member
//! active deployment, `FailoverState` walks the Vec); every read path
//! returns members verbatim in position order and must never reorder.

pub mod profile;
pub mod profile_set;
pub mod registry_meta;
pub mod set_member;

use sea_orm::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, QueryOrder};

use profile::Entity as Profiles;
use profile_set::Entity as ProfileSets;
use set_member::Entity as SetMembers;

/// The registry_meta key holding the default set marker (decision 8: the
/// default lives at the registry level, not on keys).
pub const DEFAULT_SET_KEY: &str = "default_set";

/// Fetch one profile by id.
pub async fn profile_by_id<C: ConnectionTrait>(
    db: &C,
    profile_id: &str,
) -> Result<Option<profile::Model>, DbErr> {
    Profiles::find_by_id(profile_id).one(db).await
}

/// Fetch one set by name.
pub async fn set_by_name<C: ConnectionTrait>(
    db: &C,
    name: &str,
) -> Result<Option<profile_set::Model>, DbErr> {
    ProfileSets::find_by_id(name).one(db).await
}

/// The sets among `names` that exist, in arbitrary order (the /sets list
/// surface: allowed set names from the key, intersected with reality).
pub async fn sets_existing_among(
    db: &DatabaseConnection,
    names: &[String],
) -> Result<Vec<profile_set::Model>, DbErr> {
    if names.is_empty() {
        return Ok(vec![]);
    }
    ProfileSets::find()
        .filter(profile_set::Column::Name.is_in(names.to_vec()))
        .all(db)
        .await
}

/// Whether `profile_id` is a member of any of `allowed_set_names`.
pub async fn profile_in_authorized_sets(
    db: &DatabaseConnection,
    profile_id: &str,
    allowed_set_names: &[String],
) -> Result<bool, DbErr> {
    if allowed_set_names.is_empty() {
        return Ok(false);
    }
    let found = SetMembers::find()
        .filter(set_member::Column::ProfileId.eq(profile_id))
        .filter(set_member::Column::SetName.is_in(allowed_set_names.to_vec()))
        .one(db)
        .await?;
    Ok(found.is_some())
}

/// The members of `set_name` in `position` order, joined with their profiles.
/// A member row whose profile vanished cannot exist (the FK cascades), so
/// the two-step read cannot lose rows.
pub async fn set_members_ordered(
    db: &DatabaseConnection,
    set_name: &str,
) -> Result<Vec<profile::Model>, DbErr> {
    let members = SetMembers::find()
        .filter(set_member::Column::SetName.eq(set_name))
        .order_by_asc(set_member::Column::Position)
        .all(db)
        .await?;
    let ids: Vec<String> = members.into_iter().map(|m| m.profile_id).collect();
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let mut profiles = Profiles::find()
        .filter(profile::Column::ProfileId.is_in(ids.clone()))
        .all(db)
        .await?;
    // Restore the position order the second query lost.
    profiles.sort_by_key(|p| ids.iter().position(|id| *id == p.profile_id));
    Ok(profiles)
}

/// The parking resource: profiles flagged as parked drafts (no set
/// membership is the editing-time property; the flag is the served truth).
pub async fn parked_profiles(db: &DatabaseConnection) -> Result<Vec<profile::Model>, DbErr> {
    Profiles::find()
        .filter(profile::Column::Parked.eq(true))
        .all(db)
        .await
}

/// The registry-level default set marker.
pub async fn default_set_name<C: ConnectionTrait>(db: &C) -> Result<Option<String>, DbErr> {
    let row = registry_meta::Entity::find_by_id(DEFAULT_SET_KEY)
        .one(db)
        .await?;
    Ok(row.map(|m| m.value))
}

/// The head profile (the set's first member) of `set_name` -- the
/// forwarding default target.
pub async fn set_head(
    db: &DatabaseConnection,
    set_name: &str,
) -> Result<Option<profile::Model>, DbErr> {
    let member = SetMembers::find()
        .filter(set_member::Column::SetName.eq(set_name))
        .order_by_asc(set_member::Column::Position)
        .one(db)
        .await?;
    match member {
        Some(m) => profile_by_id(db, &m.profile_id).await,
        None => Ok(None),
    }
}
