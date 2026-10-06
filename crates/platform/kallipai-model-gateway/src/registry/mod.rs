//! The registry domain: the provider pool, model profiles, their ordered
//! sets, collections, groups, and the parking resource.
//!
//! This is the distribution face's data: no credential columns live here
//! (the key half of the pool is the secret domain's `provider_credentials`,
//! see [`crate::secret`]). Everything in this module is safe to serve --
//! the sanitization contract is structural: the entities below simply do
//! not carry secret material.
//!
//! Ownership keys the registry: every table is `(owner, name/id)` primary
//! keyed, and the profile -> provider reference is a composite foreign
//! key, so a cross-space reference cannot exist in the store. Names and
//! profile ids are additionally globally unique (the
//! `uq_profile_sets_name_global` / `uq_profiles_profile_id_global`
//! indexes), so the by-name and by-id lookups behind the management
//! face's `?owner=` addressing resolve to one row; the write paths
//! still refuse a duplicate early with a 409.
//!
//! `position` in `set_members` is failover semantics (the first member
//! active deployment, `FailoverState` walks the Vec); every read path
//! returns members verbatim in position order and must never reorder.
//!
//! The wire family lives on the provider row (family is pool-level
//! state), so serving reads
//! return [`ServedProfile`] (profile joined with family) rather than a
//! bare profile row.

/// The reserved audience every bound account can publish to and read
/// through: the visibility predicates answer true for this id without
/// consulting any member list (the audience is the predicate, not a
/// row set). Both group tables host the row as a stable foreign-key
/// target.
pub const EVERYONE_GROUP_ID: &str = "everyone";
// The collections/groups family: the user-face entities (the /user
// routes are their consumer); storage lives in the same migrations as
// the rest of the registry.
pub mod collection;
pub mod collection_publication;
pub mod gateway_selection;
pub mod group;
pub mod group_member;
pub mod platform_group_members;
pub mod platform_groups;
pub mod platform_publications;
pub mod profile;
pub mod profile_set;
pub mod provider;
pub mod set_member;

use sea_orm::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, QueryOrder};
use std::collections::{HashMap, HashSet};

use profile::Entity as Profiles;
use profile_set::Entity as ProfileSets;
use provider::Entity as Providers;
use set_member::Entity as SetMembers;

/// The platform catalog owner: the public space the distribution face
/// serves (`owner = 'system'` is the catalog read family's filter).
pub const CATALOG_OWNER: &str = "system";

/// A profile joined with its provider's wire family: everything the
/// serving paths need in one shape.
#[derive(Clone, Debug)]
pub struct ServedProfile {
    pub profile: profile::Model,
    /// The provider's wire family (the dispatch set
    /// `crate::forward::dialect::WIRE_FAMILIES` admits).
    pub family: String,
    /// The space whose credential serves this profile -- the profile's
    /// own owner (the composite FK pins the provider to the same space).
    /// Carried so the audit row's snapshot needs no second lookup.
    pub provider_owner: String,
}

impl std::ops::Deref for ServedProfile {
    type Target = profile::Model;
    fn deref(&self) -> &profile::Model {
        &self.profile
    }
}

/// The provider row behind a profile (the family and base URL holder).
async fn provider_of<C: ConnectionTrait>(
    db: &C,
    owner: &str,
    provider_id: &str,
) -> Result<Option<provider::Model>, DbErr> {
    Providers::find_by_id((owner.to_owned(), provider_id.to_owned()))
        .one(db)
        .await
}

/// Join a profile with its provider's family. `None` = the provider
/// vanished without the FK noticing (cannot happen; treated as unserved).
/// Public: the management face's single-row read shares this join.
pub async fn served_profile<C: ConnectionTrait>(
    db: &C,
    profile: profile::Model,
) -> Result<Option<ServedProfile>, DbErr> {
    let Some(provider) = provider_of(db, &profile.owner, &profile.provider_id).await? else {
        return Ok(None);
    };
    Ok(Some(ServedProfile {
        family: provider.family,
        provider_owner: profile.owner.clone(),
        profile,
    }))
}

/// Served views for the given profile rows, in input order, under ONE
/// provider query for the whole batch. Rows whose provider vanished
/// are skipped: the profiles FK carries RESTRICT, so a provider row
/// cannot vanish under a live profile; the skip stays defensive and
/// never panics.
pub async fn served_many<C: ConnectionTrait>(
    db: &C,
    profiles: Vec<profile::Model>,
) -> Result<Vec<ServedProfile>, DbErr> {
    let owners: HashSet<String> = profiles.iter().map(|p| p.owner.clone()).collect();
    let ids: HashSet<String> = profiles.iter().map(|p| p.provider_id.clone()).collect();
    let rows = Providers::find()
        .filter(provider::Column::Owner.is_in(owners))
        .filter(provider::Column::ProviderId.is_in(ids))
        .all(db)
        .await?;
    let by_key: HashMap<(String, String), provider::Model> = rows
        .into_iter()
        .map(|p| ((p.owner.clone(), p.provider_id.clone()), p))
        .collect();
    Ok(profiles
        .into_iter()
        .filter_map(|profile| {
            let provider = by_key
                .get(&(profile.owner.clone(), profile.provider_id.clone()))?
                .clone();
            Some(ServedProfile {
                family: provider.family,
                provider_owner: profile.owner.clone(),
                profile,
            })
        })
        .collect())
}

// -- id lookups ---------------------------------------------------------------

/// Every profile row with this id, across spaces (at most one row: the
/// profile id is globally unique). The management face's id-only
/// addressing resolves through this read; the write paths refuse the
/// second row that would break the index.
pub async fn profiles_by_id<C: ConnectionTrait>(
    db: &C,
    profile_id: &str,
) -> Result<Vec<profile::Model>, DbErr> {
    Profiles::find()
        .filter(profile::Column::ProfileId.eq(profile_id))
        .all(db)
        .await
}

/// The profile in one space (the precise read for owner-scoped paths).
pub async fn profile_by_id<C: ConnectionTrait>(
    db: &C,
    owner: &str,
    profile_id: &str,
) -> Result<Option<profile::Model>, DbErr> {
    Profiles::find_by_id((owner.to_owned(), profile_id.to_owned()))
        .one(db)
        .await
}

/// Every set row with this name, across spaces (at most one: names are
/// globally unique).
pub async fn sets_by_name<C: ConnectionTrait>(
    db: &C,
    name: &str,
) -> Result<Vec<profile_set::Model>, DbErr> {
    ProfileSets::find()
        .filter(profile_set::Column::Name.eq(name))
        .all(db)
        .await
}

/// The set in one space.
pub async fn set_by_id<C: ConnectionTrait>(
    db: &C,
    owner: &str,
    name: &str,
) -> Result<Option<profile_set::Model>, DbErr> {
    ProfileSets::find_by_id((owner.to_owned(), name.to_owned()))
        .one(db)
        .await
}

/// The provider row for a space-scoped provider id (the management face's
/// provider reads).
pub async fn provider_by_id<C: ConnectionTrait>(
    db: &C,
    owner: &str,
    provider_id: &str,
) -> Result<Option<provider::Model>, DbErr> {
    provider_of(db, owner, provider_id).await
}

/// The set names `profile_id` belongs to in `profile_owner`'s space
/// (the visibility predicate's member read: the caller intersects
/// with the viewer's visibility domain).
pub async fn profile_set_names_of<C: ConnectionTrait>(
    db: &C,
    profile_owner: &str,
    profile_id: &str,
) -> Result<Vec<String>, DbErr> {
    Ok(SetMembers::find()
        .filter(set_member::Column::Owner.eq(profile_owner.to_owned()))
        .filter(set_member::Column::ProfileId.eq(profile_id.to_owned()))
        .all(db)
        .await?
        .into_iter()
        .map(|m| m.set_name)
        .collect())
}

/// The members of `(owner, set_name)` in `position` order, joined with
/// their profiles and provider families. A member row whose profile
/// vanished cannot exist (the FK cascades), so the two-step read cannot
/// lose rows; a vanished provider is skipped (cannot happen, but a
/// corrupted store degrades to a shorter order, never a panic).
pub async fn set_members_ordered(
    db: &DatabaseConnection,
    owner: &str,
    set_name: &str,
) -> Result<Vec<ServedProfile>, DbErr> {
    let members = SetMembers::find()
        .filter(set_member::Column::Owner.eq(owner))
        .filter(set_member::Column::SetName.eq(set_name))
        .order_by_asc(set_member::Column::Position)
        .all(db)
        .await?;
    let ids: Vec<String> = members.into_iter().map(|m| m.profile_id).collect();
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let mut profiles = Profiles::find()
        .filter(profile::Column::Owner.eq(owner))
        .filter(profile::Column::ProfileId.is_in(ids.clone()))
        .all(db)
        .await?;
    // Restore the position order the second query lost.
    profiles.sort_by_key(|p| ids.iter().position(|id| *id == p.profile_id));
    served_many(db, profiles).await
}

/// The head profile of `owner`'s set `set_name` among the families a
/// generation endpoint serves: the first member in position order
/// whose provider family is in `families`. Position order is the
/// failover order, so the walk preserves it -- a family-mismatched
/// head must not blind the endpoint to servable members behind it.
pub async fn set_head_for_families(
    db: &DatabaseConnection,
    owner: &str,
    set_name: &str,
    families: &[&str],
) -> Result<Option<ServedProfile>, DbErr> {
    for served in set_members_ordered(db, owner, set_name).await? {
        if families.contains(&served.family.as_str()) {
            return Ok(Some(served));
        }
    }
    Ok(None)
}

// -- the platform-catalog read family ---------------------------------------
//
// The distribution face's view: every read filters `owner = 'system'`, so
// a private row is outside this face by construction (in the query, not a
// post-filter). The management face reads the owner-agnostic functions
// above; these are the distribution face's only registry entry points.

/// The catalog parking resource: parked drafts the distribution face
/// serves (a private-space draft joins the catalog only through a set).
pub async fn catalog_parked_profiles(db: &DatabaseConnection) -> Result<Vec<ServedProfile>, DbErr> {
    let parked = Profiles::find()
        .filter(profile::Column::Parked.eq(true))
        .filter(profile::Column::Owner.eq(CATALOG_OWNER))
        .all(db)
        .await?;
    served_many(db, parked).await
}
