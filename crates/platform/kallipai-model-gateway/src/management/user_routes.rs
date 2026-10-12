//! The user domain: the /user face behind the session cookie
//! ([`crate::management::UserSession`]). Ownership is the session's account id, so the
//! routes take no owner parameter -- the owner is always the caller,
//! and the composite foreign keys keep every reference inside that one
//! space. Writes audit into `management_events` with the account id as
//! the actor (one spelling with the admin face).
//!
//! The handlers live per resource in submodules named for the resource they
//! serve (`providers`, `profiles`, `sets`, `collections`, `groups`): four of
//! them also name the admin-face sibling they mirror, and those siblings
//! serve `/admin` while this subtree serves `/user`. `shared` holds the
//! ownership key every resource module needs; the re-exports below keep the
//! symbols the admin-face modules borrow resolving under this path.

mod collections;
mod groups;
mod profiles;
mod providers;
mod sets;
mod shared;
#[cfg(test)]
mod tests;

use self::collections::{
    create_collection, create_collection_set, delete_collection, get_collection, list_collections,
    publish_collection, set_collection_default, unpublish_collection, update_collection,
};
use self::groups::{
    create_group, delete_group, get_group, list_groups, list_platform_collections, update_group,
};
use self::profiles::{create_profile, delete_profile, list_profiles, update_profile};
use self::providers::{
    create_provider, delete_provider, delete_provider_credential, list_providers,
    put_provider_credential, update_provider,
};
use self::sets::{delete_set, get_set, list_sets, update_set};
use crate::state::AppState;
use axum::routing::{delete, get, patch, post, put};

pub(crate) use collections::{
    CascadeFace, UserCollectionCreate, UserCollectionDefaultBody, UserCollectionUpdate,
    UserPublicationBody, anchor_collection_default, collection_default_guard,
    delete_collection_cascade, user_collection_view,
};
pub(crate) use groups::{UserGroupCreate, UserGroupUpdate, UserGroupView, UserGroupViews};

// -- the route table ---------------------------------------------------------

pub(crate) fn router() -> axum::Router<AppState> {
    axum::Router::new()
        // -- providers ------------------------------------------------------
        .route("/providers", get(list_providers).post(create_provider))
        .route(
            "/providers/{provider_id}",
            patch(update_provider).delete(delete_provider),
        )
        .route(
            "/providers/{provider_id}/credential",
            put(put_provider_credential).delete(delete_provider_credential),
        )
        // -- profiles -------------------------------------------------------
        .route("/profiles", get(list_profiles).post(create_profile))
        .route(
            "/profiles/{profile_id}",
            put(update_profile).delete(delete_profile),
        )
        // -- sets -----------------------------------------------------------
        .route("/sets", get(list_sets))
        .route(
            "/sets/{name}",
            get(get_set).patch(update_set).delete(delete_set),
        )
        // -- collections ----------------------------------------------------
        .route(
            "/collections",
            get(list_collections).post(create_collection),
        )
        .route(
            "/collections/{name}",
            get(get_collection)
                .patch(update_collection)
                .delete(delete_collection),
        )
        .route("/collections/{name}/publications", post(publish_collection))
        .route(
            "/collections/{name}/publications/{group}",
            delete(unpublish_collection),
        )
        .route("/collections/{name}/sets", post(create_collection_set))
        .route("/collections/{name}/default", patch(set_collection_default))
        // -- groups ---------------------------------------------------------
        .route("/groups", get(list_groups).post(create_group))
        .route(
            "/groups/{group_id}",
            get(get_group).patch(update_group).delete(delete_group),
        )
        // -- the platform catalog --------------------------------------------
        .route("/platform-collections", get(list_platform_collections))
}
