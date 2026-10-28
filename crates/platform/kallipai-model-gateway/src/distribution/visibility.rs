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

/// A set addressed by its full identity: a bare name answers only
/// together with its collection, so the domain holds triples and every
/// membership test is a triple comparison.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SetKey {
    pub owner: String,
    pub collection_name: String,
    pub name: String,
}

/// Read one joined row into a [`SetKey`]; a row missing a column is
/// skipped (cannot happen with the queries below, but a malformed answer
/// degrades to a smaller domain, never a panic).
fn row_set_key(row: &sea_orm::QueryResult) -> Option<SetKey> {
    let owner = row.try_get::<String>("", "owner").ok()?;
    let collection_name = row.try_get::<String>("", "collection_name").ok()?;
    let name = row.try_get::<String>("", "set_name").ok()?;
    Some(SetKey {
        owner,
        collection_name,
        name,
    })
}

/// The sets the viewer's domain reaches. Two independent reaches, each one
/// raw join: the platform reach runs for every viewer (the platform
/// publications joined to the catalog sets they carry, filtered to the
/// everyone sentinel or the account's platform memberships), and the user
/// reach runs for a bound account (its own collection sets plus the user
/// publications it can see). The join condition is the full container, so
/// a same-name set in another collection is a different key.
async fn domain_sets(db: &DatabaseConnection, viewer: &Viewer) -> Result<HashSet<SetKey>, DbErr> {
    let mut sets: HashSet<SetKey> = HashSet::new();
    // The platform reach. SQL boundary: the only format! interpolation is
    // the compile-time `EVERYONE_GROUP_ID` constant; the group filters
    // bind via $1 -- no request data enters the SQL text.
    let sql = format!(
        "SELECT ps.owner AS owner, ps.collection_name AS collection_name, \
         ps.name AS set_name \
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
        if let Some(key) = row_set_key(&row) {
            sets.insert(key);
        }
    }
    // The user reach: the account's own collections (the set rows it
    // owns, published or not) and the user publications the account
    // can see (everyone, or a user group it sits in). SQL boundary:
    // the format! interpolation is the same compile-time constant;
    // the account and group filters bind via $1/$2.
    let sql = format!(
        "SELECT owner, collection_name, set_name FROM ( \
           SELECT owner, collection_name, name AS set_name \
             FROM profile_sets WHERE owner = $1 \
           UNION \
           SELECT ps.owner AS owner, ps.collection_name AS collection_name, \
                  ps.name AS set_name \
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
        if let Some(key) = row_set_key(&row) {
            sets.insert(key);
        }
    }
    Ok(sets)
}

/// Every set the viewer's domain reaches, sorted by its full identity.
pub async fn visible_sets(db: &DatabaseConnection, viewer: &Viewer) -> Result<Vec<SetKey>, DbErr> {
    let mut sets: Vec<SetKey> = domain_sets(db, viewer).await?.into_iter().collect();
    sets.sort_by(|a, b| {
        (&a.owner, &a.collection_name, &a.name).cmp(&(&b.owner, &b.collection_name, &b.name))
    });
    Ok(sets)
}

/// Whether the set inside one collection is in the viewer's domain.
pub async fn set_visible(
    db: &DatabaseConnection,
    viewer: &Viewer,
    owner: &str,
    collection_name: &str,
    set_name: &str,
) -> Result<bool, DbErr> {
    let key = SetKey {
        owner: owner.to_owned(),
        collection_name: collection_name.to_owned(),
        name: set_name.to_owned(),
    };
    Ok(domain_sets(db, viewer).await?.contains(&key))
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

/// Whether the profile (in its own collection) is inside the viewer's
/// domain, and if so through what. `Ok(None)` = invisible. The set the
/// profile sits in must itself be in the domain -- membership never leaves
/// the collection, so the container is one read's worth of scope -- and the
/// attribution is the alphabetically first granting collection (the store
/// may hold several).
pub async fn profile_grant(
    db: &DatabaseConnection,
    viewer: &Viewer,
    profile_owner: &str,
    collection_name: &str,
    profile_id: &str,
) -> Result<Option<Grant>, DbErr> {
    let domain = domain_sets(db, viewer).await?;
    if domain.is_empty() {
        return Ok(None);
    }
    // The profile's own sets, intersected with the domain on the full key.
    let members =
        registry::profile_set_names_of(db, profile_owner, collection_name, profile_id).await?;
    let mut granting: Vec<String> = members
        .into_iter()
        .filter(|name| {
            domain.contains(&SetKey {
                owner: profile_owner.to_owned(),
                collection_name: collection_name.to_owned(),
                name: name.clone(),
            })
        })
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
    // The alphabetically first user collection in the space that is
    // published to a reach the viewer has and carries this set -- in the
    // set's own collection, so a same-name set elsewhere cannot answer.
    // SQL boundary: the only format! interpolation is the compile-time
    // `EVERYONE_GROUP_ID` constant; the set, owner, collection, and group
    // filters bind via $1/$2/$3/$4 -- no request data enters the SQL text.
    let sql = format!(
        "SELECT p.collection_name AS collection_name \
         FROM collection_publications p \
         JOIN profile_sets ps \
           ON ps.owner = p.owner AND ps.collection_name = p.collection_name \
         WHERE ps.name = $1 AND ps.owner = $2 AND ps.collection_name = $3 \
           AND (p.group_id = '{EVERYONE_GROUP_ID}' OR ps.owner = $2 \
              OR p.group_id = ANY($4)) \
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
                collection_name.to_owned().into(),
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

    /// The domain rendered as `owner/collection/set`: the full key, so a
    /// same-name set in another collection is a visible separate entry.
    async fn visible_keys(db: &db::Db, viewer: &Viewer) -> Vec<String> {
        visible_sets(db, viewer)
            .await
            .unwrap()
            .into_iter()
            .map(|k| format!("{}/{}/{}", k.owner, k.collection_name, k.name))
            .collect()
    }

    fn key(owner: &str, collection: &str, name: &str) -> SetKey {
        SetKey {
            owner: owner.to_owned(),
            collection_name: collection.to_owned(),
            name: name.to_owned(),
        }
    }

    /// A bare account (no own space, no memberships) rides the everyone
    /// publications alone: the catalog baseline and any user-side
    /// everyone-published set cross to it.
    #[tokio::test]
    async fn a_bare_account_rides_the_baseline_reach() {
        let db = seeded().await;
        let viewer = Viewer::resolve(&db, "user-plain".to_owned()).await.unwrap();
        assert_eq!(
            visible_keys(&db, &viewer).await,
            vec![
                "acc-bound/user-col/user-set",
                "system/baseline/alpha",
                "system/baseline/beta",
            ]
        );
    }

    /// The bound shapes: an account reaches its own collections whether
    /// published or not, the everyone publication crosses accounts, and
    /// the group publication needs the membership row on either side of
    /// the wall.
    #[tokio::test]
    async fn bound_accounts_reach_their_publications() {
        let db = seeded().await;

        let viewer = Viewer::resolve(&db, "acc-bound".to_owned()).await.unwrap();
        assert_eq!(
            visible_keys(&db, &viewer).await,
            vec![
                "acc-bound/grp-col/grp-set",
                "acc-bound/own-col/own-set",
                "acc-bound/user-col/user-set",
                "system/baseline/alpha",
                "system/baseline/beta",
            ]
        );

        // acc-other: the everyone publication, the user group
        // membership, and the platform group membership reach across
        // the boundary; the never-published set does not.
        let viewer = Viewer::resolve(&db, "acc-other".to_owned()).await.unwrap();
        assert_eq!(
            visible_keys(&db, &viewer).await,
            vec![
                "acc-bound/grp-col/grp-set",
                "acc-bound/user-col/user-set",
                "acc-other/other-col/other-set",
                "system/baseline/alpha",
                "system/baseline/beta",
                "system/catalog-pro/pro-set",
            ]
        );

        // The three reach clauses: acc-bound sees grp-set through its
        // own collection, acc-other through the membership row; the
        // never-published set stays inside its owner's space for both.
        let viewer = Viewer::resolve(&db, "acc-other".to_owned()).await.unwrap();
        assert!(
            set_visible(&db, &viewer, "acc-bound", "grp-col", "grp-set")
                .await
                .unwrap(),
            "the membership row makes the group publication visible"
        );
        assert!(
            !set_visible(&db, &viewer, "acc-bound", "own-col", "own-set")
                .await
                .unwrap()
        );
    }

    /// A same-name set in a reachable collection and an unreachable one:
    /// the domain holds the triple, so only the reachable twin lands (the
    /// key carries the collection, not just the name).
    #[tokio::test]
    async fn a_same_name_set_in_another_collection_is_a_different_key() {
        let db = seeded().await;
        db.execute_unprepared(
            "INSERT INTO profile_sets (owner, collection_name, name, description) \
             VALUES ('acc-bound', 'grp-col', 'own-set', 'the published twin')",
        )
        .await
        .unwrap();

        let viewer = Viewer::resolve(&db, "acc-other".to_owned()).await.unwrap();
        assert!(
            set_visible(&db, &viewer, "acc-bound", "grp-col", "own-set")
                .await
                .unwrap(),
            "the twin in the group-published collection is in the domain"
        );
        assert!(
            !set_visible(&db, &viewer, "acc-bound", "own-col", "own-set")
                .await
                .unwrap(),
            "the never-published twin of the same name stays out"
        );
        let keys = visible_sets(&db, &viewer).await.unwrap();
        assert!(
            keys.contains(&key("acc-bound", "grp-col", "own-set"))
                && !keys.contains(&key("acc-bound", "own-col", "own-set")),
            "the domain names the collection: {keys:?}"
        );
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
            !set_visible(&db, &viewer, "system", "catalog-pro", "pro-set")
                .await
                .unwrap(),
            "a non-member cannot see the platform group's catalog"
        );

        // A member (acc-other) sees it.
        let viewer = Viewer::resolve(&db, "acc-other".to_owned()).await.unwrap();
        assert!(
            set_visible(&db, &viewer, "system", "catalog-pro", "pro-set")
                .await
                .unwrap(),
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
        assert_eq!(
            visible_keys(&db, &viewer).await,
            vec!["acc-bound/user-col/user-set"],
            "the join is the enforcement"
        );
    }

    /// The grant attribution: a catalog profile grants without a
    /// collection name, a published user set names its collection, the
    /// own-collection reach stays unattributed, a profile in no set is
    /// invisible, and both the account and the collection bound the
    /// grant.
    #[tokio::test]
    async fn profile_grant_attributes_publications() {
        let db = seeded().await;

        let viewer = Viewer::resolve(&db, "user-plain".to_owned()).await.unwrap();
        let grant = profile_grant(&db, &viewer, registry::CATALOG_OWNER, "baseline", "p1")
            .await
            .unwrap();
        assert_eq!(
            grant,
            Some(Grant {
                collection_name: None
            })
        );

        // A profile in no set at all is invisible to every viewer.
        let grant = profile_grant(&db, &viewer, registry::CATALOG_OWNER, "baseline", "p4")
            .await
            .unwrap();
        assert_eq!(grant, None);

        let viewer = Viewer::resolve(&db, "acc-other".to_owned()).await.unwrap();
        let grant = profile_grant(&db, &viewer, "acc-other", "other-col", "u2")
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

        // The everyone publication attributes across accounts: u3 sits
        // only in user-set, so the attribution is unambiguous.
        let grant = profile_grant(&db, &viewer, "acc-bound", "user-col", "u3")
            .await
            .unwrap();
        assert_eq!(
            grant,
            Some(Grant {
                collection_name: Some("user-col".to_owned())
            })
        );

        // The group publication attributes through the membership row.
        let grant = profile_grant(&db, &viewer, "acc-bound", "grp-col", "u5")
            .await
            .unwrap();
        assert_eq!(
            grant,
            Some(Grant {
                collection_name: Some("grp-col".to_owned())
            })
        );

        // The never-published collection stays out of another account's
        // reach, and a cross-account probe on it grants nothing.
        let grant = profile_grant(&db, &viewer, "acc-bound", "own-col", "u4")
            .await
            .unwrap();
        assert_eq!(grant, None);

        // The owner sees its own never-published collection, with no
        // publication to attribute.
        let viewer = Viewer::resolve(&db, "acc-bound".to_owned()).await.unwrap();
        let grant = profile_grant(&db, &viewer, "acc-bound", "own-col", "u4")
            .await
            .unwrap();
        assert_eq!(
            grant,
            Some(Grant {
                collection_name: None
            })
        );
        let grant = profile_grant(&db, &viewer, "acc-bound", "user-col", "u1")
            .await
            .unwrap();
        assert_eq!(
            grant,
            Some(Grant {
                collection_name: Some("user-col".to_owned())
            })
        );

        // The collection is part of the probe: u1 lives in user-col, so
        // the same id asked about in own-col grants nothing.
        let grant = profile_grant(&db, &viewer, "acc-bound", "own-col", "u1")
            .await
            .unwrap();
        assert_eq!(grant, None);
    }
}
