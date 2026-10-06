//! The visibility domain: what a presenting tagma identity may see on
//! the distribution face.
//!
//! The domain is the union of two reaches. The platform reach is the
//! published face of the platform catalog: the collections the admin
//! publishes into the platform tables, carried by the `baseline`
//! collection (published to the everyone sentinel) plus any platform
//! group the account sits in. The user reach is the account's own:
//! the sets in its own collections and the sets in user collections
//! published to the everyone audience or to one of its user groups.
//!
//! The identity carries no grant list, so an authenticated tagma sees
//! everything its account reaches and nothing more.
//!
//! Resolution is once per request: the identity extractor resolves the
//! account and its group memberships (user-side and platform-side) into
//! a [`Viewer`], and every handler on both planes reads from that.

use sea_orm::{ColumnTrait, ConnectionTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter};
use std::collections::HashSet;

use crate::registry::EVERYONE_GROUP_ID;
use crate::registry::{self, group_member, platform_group_members};

/// Whether the named collection is inside the viewer's domain. Own
/// collections are always reachable (published or not); another
/// space's collection is reachable only through a publication -- the
/// platform publications for the catalog, the user publications for
/// every other space -- to the everyone sentinel or to a group the
/// account sits in.
pub async fn collection_visible(
    db: &DatabaseConnection,
    viewer: &Viewer,
    owner: &str,
    collection_name: &str,
) -> Result<bool, DbErr> {
    if owner == viewer.account_id {
        return Ok(true);
    }
    let publications: Vec<String> = if owner == registry::CATALOG_OWNER {
        registry::platform_publications::Entity::find()
            .filter(registry::platform_publications::Column::Owner.eq(owner))
            .filter(registry::platform_publications::Column::CollectionName.eq(collection_name))
            .all(db)
            .await?
            .into_iter()
            .map(|row| row.group_id)
            .collect()
    } else {
        registry::collection_publication::Entity::find()
            .filter(registry::collection_publication::Column::Owner.eq(owner))
            .filter(registry::collection_publication::Column::CollectionName.eq(collection_name))
            .all(db)
            .await?
            .into_iter()
            .map(|row| row.group_id)
            .collect()
    };
    let groups: &[String] = if owner == registry::CATALOG_OWNER {
        &viewer.platform_memberships
    } else {
        &viewer.memberships
    };
    for group_id in publications {
        if group_id == EVERYONE_GROUP_ID || groups.contains(&group_id) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The per-request identity behind a tagma bearer.
#[derive(Clone, Debug)]
pub struct Viewer {
    /// The account the tagma is enrolled to (the enrollment read is
    /// part of the identity resolution; there is no unbound shape).
    pub account_id: String,
    /// The user-side groups the account belongs to.
    pub memberships: Vec<String>,
    /// The platform-side groups the account belongs to.
    pub platform_memberships: Vec<String>,
}

impl Viewer {
    /// Resolve the viewer: one membership query per side. The everyone
    /// audience needs no row on either side -- the predicate answers
    /// for it directly.
    pub async fn resolve(db: &DatabaseConnection, account_id: String) -> Result<Self, DbErr> {
        let memberships = group_member::Entity::find()
            .filter(group_member::Column::MemberAccount.eq(account_id.clone()))
            .all(db)
            .await?
            .into_iter()
            .map(|m| m.group_id)
            .collect();
        let platform_memberships = platform_group_members::Entity::find()
            .filter(platform_group_members::Column::MemberAccount.eq(account_id.clone()))
            .all(db)
            .await?
            .into_iter()
            .map(|m| m.group_id)
            .collect();
        Ok(Self {
            account_id,
            memberships,
            platform_memberships,
        })
    }
}

/// The set names the viewer's domain reaches. Two independent reaches,
/// each one raw join: the platform reach runs for every viewer (the
/// platform publications joined to the catalog sets they carry,
/// filtered to the everyone sentinel or the account's platform
/// memberships), and the user reach runs for a bound account (its own
/// collection sets plus the user publications it can see). The set name
/// is the join key (names are globally unique), so the domain stays
/// space-agnostic -- the handlers resolve the row behind a name.
async fn domain_set_names(
    db: &DatabaseConnection,
    viewer: &Viewer,
) -> Result<HashSet<String>, DbErr> {
    let mut names: HashSet<String> = HashSet::new();
    // The platform reach. SQL boundary: the only format! interpolation is
    // the compile-time `EVERYONE_GROUP_ID` constant; the group filters
    // bind via $1 -- no request data enters the SQL text.
    let sql = format!(
        "SELECT ps.name AS set_name \
         FROM platform_publications p \
         JOIN profile_sets ps \
           ON ps.owner = p.owner AND ps.collection_name = p.collection_name \
         WHERE p.group_id = '{EVERYONE_GROUP_ID}' OR p.group_id = ANY($1)"
    );
    let rows = db
        .query_all(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            [viewer.platform_memberships.clone().into()],
        ))
        .await?;
    for row in rows {
        if let Ok(name) = row.try_get::<String>("", "set_name") {
            names.insert(name);
        }
    }
    // The user reach: the account's own collections (the set rows it
    // owns, published or not) and the user publications the account
    // can see (everyone, or a user group it sits in). SQL boundary:
    // the format! interpolation is the same compile-time constant;
    // the account and group filters bind via $1/$2.
    let sql = format!(
        "SELECT set_name FROM ( \
           SELECT name AS set_name FROM profile_sets WHERE owner = $1 \
           UNION \
           SELECT ps.name AS set_name \
             FROM collection_publications p \
             JOIN profile_sets ps \
               ON ps.owner = p.owner AND ps.collection_name = p.collection_name \
             WHERE p.group_id = '{EVERYONE_GROUP_ID}' OR p.group_id = ANY($2) \
         ) reaches"
    );
    let rows = db
        .query_all(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            [
                viewer.account_id.clone().into(),
                viewer.memberships.clone().into(),
            ],
        ))
        .await?;
    for row in rows {
        if let Ok(name) = row.try_get::<String>("", "set_name") {
            names.insert(name);
        }
    }
    Ok(names)
}

/// Every set name the viewer's domain reaches, sorted.
pub async fn visible_set_names(
    db: &DatabaseConnection,
    viewer: &Viewer,
) -> Result<Vec<String>, DbErr> {
    let mut names: Vec<String> = domain_set_names(db, viewer).await?.into_iter().collect();
    names.sort();
    Ok(names)
}

/// Whether the named set is inside the viewer's domain.
pub async fn set_visible(
    db: &DatabaseConnection,
    viewer: &Viewer,
    set_name: &str,
) -> Result<bool, DbErr> {
    Ok(domain_set_names(db, viewer).await?.contains(set_name))
}

/// One platform catalog entry on the user face: a published platform
/// collection and the sets it carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlatformCollectionEntry {
    pub owner: String,
    pub collection_name: String,
    pub description: String,
    pub set_names: Vec<String>,
}

/// The platform catalog the viewer's platform reach carries: every
/// collection published to the everyone audience or to one
/// of the viewer's platform groups, with its sets. A collection with
/// no sets never surfaces (the inner join drops it); `baseline` is an
/// entry like any other (its sets are the every-viewer rows).
pub async fn platform_catalog(
    db: &DatabaseConnection,
    viewer: &Viewer,
) -> Result<Vec<PlatformCollectionEntry>, DbErr> {
    // SQL boundary: the only format! interpolation is the compile-time
    // `EVERYONE_GROUP_ID` constant; the group filter binds via $1 --
    // no request data enters the SQL text.
    let sql = format!(
        "SELECT DISTINCT c.owner AS owner, \
         c.name AS collection_name, c.description AS description, \
         ps.name AS set_name \
         FROM platform_publications p \
         JOIN collections c \
           ON c.owner = p.owner AND c.name = p.collection_name \
         JOIN profile_sets ps \
           ON ps.owner = c.owner AND ps.collection_name = c.name \
         WHERE p.group_id = '{EVERYONE_GROUP_ID}' OR p.group_id = ANY($1) \
         ORDER BY c.owner, c.name, ps.name"
    );
    let rows = db
        .query_all(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            [viewer.platform_memberships.clone().into()],
        ))
        .await?;
    // Rows arrive grouped by (owner, collection) and ordered within --
    // the DISTINCT collapses repeat publication rows (one collection
    // published to several audiences); the fold merges the rest.
    let mut entries: Vec<PlatformCollectionEntry> = Vec::new();
    for row in rows {
        let owner: String = row.try_get("", "owner")?;
        let collection_name: String = row.try_get("", "collection_name")?;
        let description: String = row.try_get("", "description")?;
        let set_name: String = row.try_get("", "set_name")?;
        match entries.last_mut() {
            Some(entry) if entry.owner == owner && entry.collection_name == collection_name => {
                entry.set_names.push(set_name);
            }
            _ => entries.push(PlatformCollectionEntry {
                owner,
                collection_name,
                description,
                set_names: vec![set_name],
            }),
        }
    }
    Ok(entries)
}

/// How a profile reached the face: the collection that granted it when
/// the path ran through a publication (`None` = the platform catalog).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub collection_name: Option<String>,
}

/// Whether the profile (in its own space) is inside the viewer's
/// domain, and if so through what. `Ok(None)` = invisible. The set the
/// profile sits in must itself be in the domain; the attribution is the
/// alphabetically first granting collection (the store may hold several).
pub async fn profile_grant(
    db: &DatabaseConnection,
    viewer: &Viewer,
    profile_owner: &str,
    profile_id: &str,
) -> Result<Option<Grant>, DbErr> {
    let domain = domain_set_names(db, viewer).await?;
    if domain.is_empty() {
        return Ok(None);
    }
    // The profile's own sets (a profile's member rows live in its
    // space), intersected with the domain.
    let members = registry::profile_set_names_of(db, profile_owner, profile_id).await?;
    let mut granting: Vec<String> = members
        .into_iter()
        .filter(|name| domain.contains(name))
        .collect();
    granting.sort();
    let Some(set_name) = granting.first() else {
        return Ok(None);
    };
    if profile_owner == registry::CATALOG_OWNER {
        // A catalog profile: the platform reach attributes nothing --
        // the catalog is its own space on the face.
        return Ok(Some(Grant {
            collection_name: None,
        }));
    }
    // The alphabetically first user collection in the set's space that
    // is published to a reach the viewer has and carries this set.
    // SQL boundary: the only format! interpolation is the compile-time
    // `EVERYONE_GROUP_ID` constant; the set, owner, and group filters
    // bind via $1/$2/$3 -- no request data enters the SQL text.
    let sql = format!(
        "SELECT p.collection_name AS collection_name \
         FROM collection_publications p \
         JOIN profile_sets ps \
           ON ps.owner = p.owner AND ps.collection_name = p.collection_name \
         WHERE ps.name = $1 AND ps.owner = $2 \
           AND (p.group_id = '{EVERYONE_GROUP_ID}' OR ps.owner = $2 \
              OR p.group_id = ANY($3)) \
         ORDER BY p.owner, p.collection_name \
         LIMIT 1"
    );
    let row = db
        .query_one(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            [
                set_name.clone().into(),
                profile_owner.to_owned().into(),
                viewer.memberships.clone().into(),
            ],
        ))
        .await?;
    Ok(Some(Grant {
        collection_name: row.and_then(|r| r.try_get::<String>("", "collection_name").ok()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::test_support::{migrated_test_db, seed_registry};

    async fn seeded() -> db::Db {
        let db = migrated_test_db().await;
        seed_registry(&db, "https://api.upstream.test").await;
        db
    }

    /// A bare account (no own space, no memberships) rides the everyone
    /// publications alone: the catalog baseline and any user-side
    /// everyone-published set cross to it.
    #[tokio::test]
    async fn a_bare_account_rides_the_baseline_reach() {
        let db = seeded().await;
        let viewer = Viewer::resolve(&db, "user-plain".to_owned()).await.unwrap();
        let names = visible_set_names(&db, &viewer).await.unwrap();
        assert_eq!(names, vec!["alpha", "beta", "user-set"]);
    }

    /// The bound shapes: an account reaches its own collections whether
    /// published or not, the everyone publication crosses accounts, and
    /// the group publication needs the membership row on either side of
    /// the wall.
    #[tokio::test]
    async fn bound_accounts_reach_their_publications() {
        let db = seeded().await;

        let viewer = Viewer::resolve(&db, "acc-bound".to_owned()).await.unwrap();
        let names = visible_set_names(&db, &viewer).await.unwrap();
        assert_eq!(
            names,
            vec!["alpha", "beta", "grp-set", "own-set", "user-set"]
        );

        // acc-other: the everyone publication, the user group
        // membership, and the platform group membership reach across
        // the boundary; the never-published set does not.
        let viewer = Viewer::resolve(&db, "acc-other".to_owned()).await.unwrap();
        let names = visible_set_names(&db, &viewer).await.unwrap();
        assert_eq!(
            names,
            vec![
                "alpha",
                "beta",
                "grp-set",
                "other-set",
                "pro-set",
                "user-set"
            ]
        );

        // The three reach clauses: acc-bound sees grp-set through its
        // own collection, acc-other through the membership row; the
        // never-published set stays inside its owner's space for both.
        let viewer = Viewer::resolve(&db, "acc-other".to_owned()).await.unwrap();
        assert!(
            set_visible(&db, &viewer, "grp-set").await.unwrap(),
            "the membership row makes the group publication visible"
        );
        assert!(!set_visible(&db, &viewer, "own-set").await.unwrap());
    }

    /// The platform reach is the publication join itself: a platform
    /// group member sees the group's catalog, a non-member does not,
    /// and dropping the baseline publication empties a bare account's
    /// domain -- there is no second channel to fall back to.
    #[tokio::test]
    async fn the_platform_reach_tracks_publication_rows() {
        let db = seeded().await;

        // A non-member (acc-bound sits in no platform group) lacks the
        // pro catalog; its domain is the baseline reach plus its own
        // user-side space.
        let viewer = Viewer::resolve(&db, "acc-bound".to_owned()).await.unwrap();
        assert!(
            !set_visible(&db, &viewer, "pro-set").await.unwrap(),
            "a non-member cannot see the platform group's catalog"
        );

        // A member (acc-other) sees it.
        let viewer = Viewer::resolve(&db, "acc-other".to_owned()).await.unwrap();
        assert!(
            set_visible(&db, &viewer, "pro-set").await.unwrap(),
            "the platform membership opens the group's catalog"
        );

        // A bare account rides the everyone publications alone:
        // dropping the catalog baseline removes exactly its sets, and the
        // user-side everyone publication survives as the last reach.
        db.execute_unprepared(
            "DELETE FROM platform_publications WHERE collection_name = 'baseline'",
        )
        .await
        .unwrap();
        let viewer = Viewer::resolve(&db, "user-plain".to_owned()).await.unwrap();
        let names = visible_set_names(&db, &viewer).await.unwrap();
        assert_eq!(names, vec!["user-set"], "the join is the enforcement");
    }

    /// The grant attribution: catalog profiles grant without a collection
    /// name, a publication path names its collection, the own-collection
    /// reach stays unattributed, a profile in no set is invisible, and a
    /// cross-account probe on a user-space row is invisible.
    #[tokio::test]
    async fn profile_grant_attributes_publications() {
        let db = seeded().await;

        let viewer = Viewer::resolve(&db, "user-plain".to_owned()).await.unwrap();
        let grant = profile_grant(&db, &viewer, registry::CATALOG_OWNER, "p1")
            .await
            .unwrap();
        assert_eq!(
            grant,
            Some(Grant {
                collection_name: None
            })
        );

        // A profile in no set at all is invisible to every viewer.
        let grant = profile_grant(&db, &viewer, registry::CATALOG_OWNER, "p4")
            .await
            .unwrap();
        assert_eq!(grant, None);

        let viewer = Viewer::resolve(&db, "acc-other".to_owned()).await.unwrap();
        let grant = profile_grant(&db, &viewer, "acc-other", "u2")
            .await
            .unwrap();
        // u2's only set sits in an unpublished collection: reachable
        // through the owner clause, but nothing to attribute.
        assert_eq!(
            grant,
            Some(Grant {
                collection_name: None
            })
        );

        // u1 sits in three sets; the alphabetically first granting set
        // drives the attribution (grp-set wins, its collection is
        // published to the group).
        let viewer = Viewer::resolve(&db, "acc-bound".to_owned()).await.unwrap();
        let grant = profile_grant(&db, &viewer, "acc-bound", "u1")
            .await
            .unwrap();
        assert_eq!(
            grant,
            Some(Grant {
                collection_name: Some("grp-col".to_owned())
            })
        );

        let grant = profile_grant(&db, &viewer, "acc-other", "u2")
            .await
            .unwrap();
        assert_eq!(grant, None);

        // The everyone publication attributes across accounts too:
        // u3 sits only in user-set, so the attribution is unambiguous.
        let viewer = Viewer::resolve(&db, "acc-other".to_owned()).await.unwrap();
        let grant = profile_grant(&db, &viewer, "acc-bound", "u3")
            .await
            .unwrap();
        assert_eq!(
            grant,
            Some(Grant {
                collection_name: Some("user-col".to_owned())
            })
        );
    }
}
