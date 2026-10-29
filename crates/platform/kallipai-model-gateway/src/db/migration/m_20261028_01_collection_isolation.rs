//! Collections become the unit the registry is carved into: a profile
//! lives in exactly one collection, and a set name is unique inside its
//! own collection instead of across the store.
//!
//! Three tables are rebuilt (`profiles`, `profile_sets`, `set_members`):
//! the target shape cannot be reached by `ALTER` alone, because two
//! primary keys change columns and both `set_members` foreign keys gain
//! one. `collections` and `gateway_selections` are untouched -- both
//! already name a collection, and `collections.default_set_name` is a
//! name inside its own collection by definition.
//!
//! Inbound foreign keys, enumerated before anything is dropped (the
//! check that decides the drop order): the only references into the
//! three rebuilt tables are `set_members`' own two, `fk_set_members_set`
//! and `fk_set_members_profile`. Nothing else
//! in the schema points at them, and `gateway_selections` /
//! `management_events` carry no foreign key at all. Drop order is
//! therefore `set_members` -> `profile_sets` -> `profiles`: the member
//! table goes first, so the other two lose their last inbound edge, and
//! `profile_sets` before `profiles` matches the init migration's own
//! `down()` order -- one dependency direction in the chain, not two.
//!
//! The rebuilt tables' internal foreign keys name the `*_new` tables
//! (`fk_set_members_set` -> `profile_sets_new`, `fk_set_members_profile`
//! -> `profiles_new`). Postgres records a foreign key by OID, so the
//! rename carries the constraint onto the real table; writing the real
//! names here instead would let the new tables' own inbound edges block
//! the `DROP TABLE` of the old ones.
//!
//! `fk_profiles_provider` is rebuilt verbatim (columns, `ON UPDATE
//! CASCADE`, `ON DELETE RESTRICT`): losing it would turn "a provider
//! still serving a profile cannot be deleted" into a silent success.
//!
//! Two edges carry user-visible cascades and are rebuilt with `ON
//! DELETE CASCADE` spelled out: `set_members` -> `profiles` /
//! `profile_sets` (a moved profile's member rows vanish with its source
//! row; deleting a collection takes its sets' members) and `profiles`
//! -> `collections` (deleting a collection deletes the profiles in it).
//! Without the action Postgres would not fail loudly -- it would leave
//! rows the readers cannot place.
//!
//! No separate `(owner, collection_name)` index backs the `set_members`
//! foreign keys: both referenced column lists are prefixes of the
//! rebuilt primary keys (`profiles` `(owner, collection_name,
//! profile_id)`, `profile_sets` `(owner, collection_name, name)`), so
//! the primary key indexes serve them and no extra index is created.
//!
//! Data movement (`up`):
//!
//! - A space that carries profiles but no collection gets a collection
//!   named `default`, with `default_set_name` left `NULL`. This is a
//!   one-time, user-visible row: the user face lets a space create
//!   profiles without ever creating a collection, and a profile without
//!   a collection has no home. The anchor stays `NULL` on purpose --
//!   naming a set that does not exist would only move the failure into
//!   the data face.
//! - A profile's home collection is the smallest collection name among
//!   the sets it is a member of; a profile in no set lands in its
//!   space's smallest collection. "Smallest" is arbitrary: it gives a
//!   memberless row a deterministic home, and `move` relocates it later.
//!   The home row keeps the original `profile_id`, so external
//!   references (tagma mirrors, audit JSON, operator scripts) keep
//!   pointing at a live row.
//! - Every other collection the profile is a member of gets a copy
//!   carrying the same fields and the same `profile_id`; a copy whose id
//!   is already taken in that collection takes `-2`, `-3`, ... The
//!   member rows follow their collection, so a set keeps pointing at the
//!   row inside its own collection, which is what the composite foreign
//!   keys then enforce for every future write too.
//! - `parked` obeys the container's invariant after the move: a row that
//!   is a set member is not unassigned. The insert derives the flag from
//!   the membership list instead of trusting the stored column, because
//!   old data maintained the invariant incrementally (`flip_parked`);
//!   copying a stale `true` onto a served row would turn a served
//!   profile back into an unassigned row.
//!
//! `down` is a merging rollback, not a byte-for-byte undo. Every row a
//! space holds for one profile id merges back into one; the merged flag
//! is `bool_and(parked)`, so a row any collection served stays served and
//! only a row every copy had parked returns unassigned -- copies and rows
//! created later in another collection alike. A mixed store is reachable
//! (one collection parks a copy where the id is not served while another
//! serves it), and `bool_and` is what keeps that row served on the way
//! out. A copy that had to take a
//! suffix stays a row of its own
//! (its id differs). Set names colliding anywhere in the store take a
//! `--<collection>` suffix, and the owning collection's `default_set_name`
//! follows the rename. Two spaces using one profile id have no place in
//! the old shape -- its index is on the id alone -- so `down` refuses
//! that store and names the conflicting ids instead of dying on the
//! index build. The cost: fields a copy had edited are folded into the
//! home row, and a set that had to take the suffix answers to its new
//! name from then on -- nothing keeps the old name as an alias.

use sea_orm::Statement;
use sea_orm_migration::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// The collection a space that carries profiles but none of its own
/// gains, so every profile has a home.
const HOME_COLLECTION: &str = "default";
const HOME_COLLECTION_DESCRIPTION: &str = "created by the collection isolation migration";

#[derive(DeriveMigrationName)]
pub struct Migration;

/// A `profiles` row in the pre-migration shape.
#[derive(Clone, Debug)]
struct LegacyProfile {
    owner: String,
    profile_id: String,
    provider_id: String,
    model: String,
    max_context_window: Option<i64>,
    effort: Option<String>,
    modalities: Option<String>,
    parked: bool,
    store: Option<bool>,
}

/// A `profile_sets` row: it already carries its collection.
#[derive(Clone, Debug)]
struct LegacySet {
    owner: String,
    name: String,
    description: String,
    collection_name: String,
}

/// A `set_members` row; its collection comes from the set it belongs to.
#[derive(Clone, Debug)]
struct LegacyMember {
    owner: String,
    set_name: String,
    profile_id: String,
    position: i32,
}

/// A `profiles_new` row: the target shape.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TargetProfile {
    owner: String,
    collection_name: String,
    profile_id: String,
    provider_id: String,
    model: String,
    max_context_window: Option<i64>,
    effort: Option<String>,
    modalities: Option<String>,
    parked: bool,
    store: Option<bool>,
}

/// A `set_members_new` row with its collection already resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TargetMember {
    owner: String,
    collection_name: String,
    set_name: String,
    profile_id: String,
    position: i32,
}

/// Where every profile row lands, and which row each membership names.
#[derive(Debug, Default)]
struct Placement {
    profiles: Vec<TargetProfile>,
    members: Vec<TargetMember>,
}

/// The next free id for (`owner`, `collection_name`): `base` when the
/// collection does not hold it yet, else `base-2`, `base-3`, ... The
/// suffix form matches the proxy adapter's disambiguation style.
fn pick_free(
    taken: &HashSet<(String, String, String)>,
    owner: &str,
    collection_name: &str,
    base: &str,
) -> String {
    let mut suffix = 1_u32;
    loop {
        let candidate = if suffix == 1 {
            base.to_owned()
        } else {
            format!("{base}-{suffix}")
        };
        if !taken.contains(&(
            owner.to_owned(),
            collection_name.to_owned(),
            candidate.clone(),
        )) {
            return candidate;
        }
        suffix += 1;
    }
}

/// Home every profile and re-place every membership (see the module
/// comment for the rules). Pure: the caller supplies the legacy rows and
/// the collection registry, and gets back exactly the rows to insert.
fn plan_placement(
    profiles: &[LegacyProfile],
    sets: &[LegacySet],
    members: &[LegacyMember],
    collections: &[(String, String)],
) -> Result<Placement, DbErr> {
    // Per space, the collection names in ascending order.
    let mut by_owner: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (owner, name) in collections {
        by_owner
            .entry(owner.as_str())
            .or_default()
            .insert(name.as_str());
    }
    let set_collection: HashMap<(&str, &str), &str> = sets
        .iter()
        .map(|set| {
            (
                (set.owner.as_str(), set.name.as_str()),
                set.collection_name.as_str(),
            )
        })
        .collect();
    // Per profile, the collections its memberships reach, ascending.
    let mut member_collections: BTreeMap<(&str, &str), BTreeSet<&str>> = BTreeMap::new();
    for member in members {
        let collection = set_collection
            .get(&(member.owner.as_str(), member.set_name.as_str()))
            .ok_or_else(|| {
                DbErr::Migration(format!(
                    "member row ({}, {}, {}) names a set that does not exist",
                    member.owner, member.set_name, member.profile_id
                ))
            })?;
        member_collections
            .entry((member.owner.as_str(), member.profile_id.as_str()))
            .or_default()
            .insert(collection);
    }

    let mut taken: HashSet<(String, String, String)> = HashSet::new();
    let mut origin: HashMap<(String, String, String), String> = HashMap::new();
    let mut placement = Placement::default();

    // Homes first: their ids are fixed, so copies can be picked around
    // every real id of the target collection.
    let mut homes: Vec<(&LegacyProfile, String)> = Vec::new();
    for profile in profiles {
        let joined = member_collections.get(&(profile.owner.as_str(), profile.profile_id.as_str()));
        let home = match joined.and_then(|names| names.iter().next()) {
            Some(collection) => (*collection).to_owned(),
            None => by_owner
                .get(profile.owner.as_str())
                .and_then(|names| names.iter().next())
                .map(|name| (*name).to_owned())
                .ok_or_else(|| {
                    DbErr::Migration(format!(
                        "space {} carries no collection to home profile {}",
                        profile.owner, profile.profile_id
                    ))
                })?,
        };
        taken.insert((
            profile.owner.clone(),
            home.clone(),
            profile.profile_id.clone(),
        ));
        origin.insert(
            (
                profile.owner.clone(),
                home.clone(),
                profile.profile_id.clone(),
            ),
            profile.profile_id.clone(),
        );
        homes.push((profile, home));
    }
    for (profile, home) in &homes {
        let joined = member_collections.get(&(profile.owner.as_str(), profile.profile_id.as_str()));
        let served = joined.is_some_and(|names| !names.is_empty());
        placement.profiles.push(TargetProfile {
            owner: profile.owner.clone(),
            collection_name: home.clone(),
            profile_id: profile.profile_id.clone(),
            provider_id: profile.provider_id.clone(),
            model: profile.model.clone(),
            max_context_window: profile.max_context_window,
            effort: profile.effort.clone(),
            modalities: profile.modalities.clone(),
            parked: if served { false } else { profile.parked },
            store: profile.store,
        });
        let Some(joined) = joined else {
            continue;
        };
        for collection in joined.iter() {
            if *collection == home.as_str() {
                continue;
            }
            let copy_id = pick_free(&taken, &profile.owner, collection, &profile.profile_id);
            taken.insert((
                profile.owner.clone(),
                (*collection).to_owned(),
                copy_id.clone(),
            ));
            origin.insert(
                (
                    profile.owner.clone(),
                    (*collection).to_owned(),
                    profile.profile_id.clone(),
                ),
                copy_id.clone(),
            );
            placement.profiles.push(TargetProfile {
                owner: profile.owner.clone(),
                collection_name: (*collection).to_owned(),
                profile_id: copy_id,
                provider_id: profile.provider_id.clone(),
                model: profile.model.clone(),
                max_context_window: profile.max_context_window,
                effort: profile.effort.clone(),
                modalities: profile.modalities.clone(),
                parked: false,
                store: profile.store,
            });
        }
    }

    for member in members {
        let collection = set_collection[&(member.owner.as_str(), member.set_name.as_str())];
        let key = (
            member.owner.clone(),
            collection.to_owned(),
            member.profile_id.clone(),
        );
        let Some(profile_id) = origin.get(&key) else {
            return Err(DbErr::Migration(format!(
                "membership ({}, {}) points at profile {} in a collection it does not live in",
                member.owner, member.set_name, member.profile_id
            )));
        };
        placement.members.push(TargetMember {
            owner: member.owner.clone(),
            collection_name: collection.to_owned(),
            set_name: member.set_name.clone(),
            profile_id: profile_id.clone(),
            position: member.position,
        });
    }

    Ok(placement)
}

/// The downgraded name of every set row. The smallest `(owner, collection)`
/// among the rows sharing a name keeps it; the rest take
/// `name--<collection>`, with a numeric suffix when even that is taken.
/// Every bare name is reserved before any derived name is allocated, which
/// is what makes a literal name always win over a suffix that would
/// otherwise take it. The grouping ignores the owner on purpose: the new
/// shape makes a name unique inside its collection, while the old index
/// sits on the bare name, so two spaces' same-named sets collide the moment
/// that index comes back.
fn plan_set_names(sets: &[LegacySet]) -> HashMap<(String, String, String), String> {
    let mut groups: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for set in sets {
        groups
            .entry(set.name.clone())
            .or_default()
            .push((set.owner.clone(), set.collection_name.clone()));
    }
    let mut plan = HashMap::new();
    let mut taken: HashSet<String> = HashSet::new();
    let mut ordered: Vec<(String, Vec<(String, String)>)> = groups.into_iter().collect();
    for (name, holders) in ordered.iter_mut() {
        holders.sort();
        taken.insert(name.clone());
    }
    for (name, holders) in ordered {
        let mut holders = holders.into_iter();
        if let Some((owner, collection)) = holders.next() {
            plan.insert((owner, collection, name.clone()), name.clone());
        }
        for (owner, collection) in holders {
            let base = format!("{name}--{collection}");
            let mut candidate = base.clone();
            let mut suffix = 2_u32;
            while taken.contains(&candidate) {
                candidate = format!("{base}-{suffix}");
                suffix += 1;
            }
            taken.insert(candidate.clone());
            plan.insert((owner, collection, name.clone()), candidate);
        }
    }
    plan
}

/// Give every space that carries profiles a collection to home them.
/// `default_set_name` stays `NULL` (the column is simply not named).
fn seed_home_collection() -> String {
    format!(
        "INSERT INTO collections (owner, name, description) SELECT DISTINCT owner, '{HOME_COLLECTION}', '{HOME_COLLECTION_DESCRIPTION}' FROM profiles WHERE owner NOT IN (SELECT owner FROM collections)"
    )
}

/// The rebuilt tables, created alongside the old ones. Constraint names
/// are the live names on purpose: a constraint name only has to be
/// unique inside its table, and the underlying indexes are created after
/// the swap (their names are schema-wide).
const UP_TABLES: &[&str] = &[
    "CREATE TABLE profiles_new (\
         owner text NOT NULL DEFAULT 'system', \
         collection_name text NOT NULL, \
         profile_id text NOT NULL, \
         provider_id text NOT NULL, \
         model text NOT NULL, \
         max_context_window bigint, \
         effort text, \
         modalities text, \
         parked boolean NOT NULL DEFAULT false, \
         store boolean, \
         CONSTRAINT profiles_new_pkey PRIMARY KEY (owner, collection_name, profile_id), \
         CONSTRAINT fk_profiles_provider FOREIGN KEY (provider_id, owner) \
             REFERENCES providers (provider_id, owner) \
             ON UPDATE CASCADE ON DELETE RESTRICT, \
         CONSTRAINT profiles_owner_collection_name_fkey FOREIGN KEY (owner, collection_name) \
             REFERENCES collections (owner, name) ON DELETE CASCADE\
     )",
    "CREATE TABLE profile_sets_new (\
         owner text NOT NULL DEFAULT 'system', \
         collection_name text NOT NULL, \
         name text NOT NULL, \
         description text NOT NULL, \
         CONSTRAINT profile_sets_new_pkey PRIMARY KEY (owner, collection_name, name), \
         CONSTRAINT profile_sets_owner_collection_name_fkey FOREIGN KEY (owner, collection_name) \
             REFERENCES collections (owner, name) ON DELETE CASCADE\
     )",
    "CREATE TABLE set_members_new (\
         owner text NOT NULL DEFAULT 'system', \
         collection_name text NOT NULL, \
         set_name text NOT NULL, \
         profile_id text NOT NULL, \
         position integer NOT NULL, \
         CONSTRAINT set_members_new_pkey \
             PRIMARY KEY (owner, collection_name, set_name, profile_id), \
         CONSTRAINT fk_set_members_set FOREIGN KEY (owner, collection_name, set_name) \
             REFERENCES profile_sets_new (owner, collection_name, name) ON DELETE CASCADE, \
         CONSTRAINT fk_set_members_profile FOREIGN KEY (owner, collection_name, profile_id) \
             REFERENCES profiles_new (owner, collection_name, profile_id) ON DELETE CASCADE\
     )",
];

/// The old tables go child-first (see the module comment), then the new
/// ones take their names. The two `set_members` indexes come after the
/// swap: their names are schema-wide, so they cannot exist twice.
const UP_SWAP: &[&str] = &[
    "DROP TABLE set_members",
    "DROP TABLE profile_sets",
    "DROP TABLE profiles",
    "ALTER TABLE profiles_new RENAME TO profiles",
    "ALTER TABLE profile_sets_new RENAME TO profile_sets",
    "ALTER TABLE set_members_new RENAME TO set_members",
    "ALTER TABLE profiles RENAME CONSTRAINT profiles_new_pkey TO profiles_pkey",
    "ALTER TABLE profile_sets RENAME CONSTRAINT profile_sets_new_pkey TO profile_sets_pkey",
    "ALTER TABLE set_members RENAME CONSTRAINT set_members_new_pkey TO set_members_pkey",
    "CREATE UNIQUE INDEX uq_set_members_set_position \
         ON set_members (owner, collection_name, set_name, position)",
    "CREATE INDEX idx_set_members_owner_collection_set_name \
         ON set_members (owner, collection_name, set_name)",
];

/// The downgraded tables: the pre-migration shape, rebuilt alongside the
/// current one.
const DOWN_TABLES: &[&str] = &[
    "CREATE TABLE profiles_downgraded (\
         owner text NOT NULL DEFAULT 'system', \
         profile_id text NOT NULL, \
         provider_id text NOT NULL, \
         model text NOT NULL, \
         max_context_window bigint, \
         effort text, \
         modalities text, \
         parked boolean NOT NULL DEFAULT false, \
         store boolean, \
         CONSTRAINT profiles_downgraded_pkey PRIMARY KEY (owner, profile_id), \
         CONSTRAINT fk_profiles_provider FOREIGN KEY (provider_id, owner) \
             REFERENCES providers (provider_id, owner) \
             ON UPDATE CASCADE ON DELETE RESTRICT\
     )",
    "CREATE TABLE profile_sets_downgraded (\
         owner text NOT NULL DEFAULT 'system', \
         name text NOT NULL, \
         description text NOT NULL, \
         collection_name text NOT NULL, \
         CONSTRAINT profile_sets_downgraded_pkey PRIMARY KEY (owner, name), \
         CONSTRAINT profile_sets_owner_collection_name_fkey FOREIGN KEY (owner, collection_name) \
             REFERENCES collections (owner, name) ON DELETE CASCADE\
     )",
    "CREATE TABLE set_members_downgraded (\
         owner text NOT NULL DEFAULT 'system', \
         set_name text NOT NULL, \
         profile_id text NOT NULL, \
         position integer NOT NULL, \
         CONSTRAINT set_members_downgraded_pkey PRIMARY KEY (owner, set_name, profile_id), \
         CONSTRAINT fk_set_members_set FOREIGN KEY (owner, set_name) \
             REFERENCES profile_sets_downgraded (owner, name) ON DELETE CASCADE, \
         CONSTRAINT fk_set_members_profile FOREIGN KEY (owner, profile_id) \
             REFERENCES profiles_downgraded (owner, profile_id) ON DELETE CASCADE\
     )",
];

/// The merge: one row per `(owner, profile_id)`, keeping the smallest
/// collection's row (the row `up` homed there, when one exists) and the
/// merged parked flag. `DISTINCT ON` runs after the window function, so the
/// flag sees every row of the profile: `bool_and(parked)` makes the merged
/// row unassigned only when every copy was one, the invariant `up` keeps.
const DOWN_PROFILES: &str = "INSERT INTO profiles_downgraded \
     (owner, profile_id, provider_id, model, max_context_window, effort, modalities, parked, store) \
     SELECT DISTINCT ON (owner, profile_id) \
         owner, profile_id, provider_id, model, max_context_window, effort, modalities, \
         bool_and(parked) OVER (PARTITION BY owner, profile_id) AS parked, \
         store \
     FROM profiles \
     ORDER BY owner, profile_id, collection_name";

/// Old tables child-first again, then the renamed ones take their names
/// back and the old shape's indexes are rebuilt (they died with the
/// tables that carried them).
const DOWN_SWAP: &[&str] = &[
    "DROP TABLE set_members",
    "DROP TABLE profile_sets",
    "DROP TABLE profiles",
    "ALTER TABLE profiles_downgraded RENAME TO profiles",
    "ALTER TABLE profile_sets_downgraded RENAME TO profile_sets",
    "ALTER TABLE set_members_downgraded RENAME TO set_members",
    "ALTER TABLE profiles RENAME CONSTRAINT profiles_downgraded_pkey TO profiles_pkey",
    "ALTER TABLE profile_sets RENAME CONSTRAINT profile_sets_downgraded_pkey TO profile_sets_pkey",
    "ALTER TABLE set_members RENAME CONSTRAINT set_members_downgraded_pkey TO set_members_pkey",
    "CREATE UNIQUE INDEX uq_profiles_profile_id_global ON profiles (profile_id)",
    "CREATE UNIQUE INDEX uq_profile_sets_name_global ON profile_sets (name)",
    "CREATE UNIQUE INDEX uq_set_members_set_position \
         ON set_members (owner, set_name, position)",
    "CREATE INDEX idx_set_members_owner_set_name ON set_members (owner, set_name)",
];

async fn read_profiles<C: ConnectionTrait>(conn: &C) -> Result<Vec<LegacyProfile>, DbErr> {
    let rows = conn
        .query_all(Statement::from_string(
            conn.get_database_backend(),
            "SELECT owner, profile_id, provider_id, model, max_context_window, effort, \
             modalities, parked, store FROM profiles"
                .to_owned(),
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok(LegacyProfile {
                owner: row.try_get("", "owner")?,
                profile_id: row.try_get("", "profile_id")?,
                provider_id: row.try_get("", "provider_id")?,
                model: row.try_get("", "model")?,
                max_context_window: row.try_get("", "max_context_window")?,
                effort: row.try_get("", "effort")?,
                modalities: row.try_get("", "modalities")?,
                parked: row.try_get("", "parked")?,
                store: row.try_get("", "store")?,
            })
        })
        .collect()
}

async fn read_sets<C: ConnectionTrait>(conn: &C) -> Result<Vec<LegacySet>, DbErr> {
    let rows = conn
        .query_all(Statement::from_string(
            conn.get_database_backend(),
            "SELECT owner, name, description, collection_name FROM profile_sets".to_owned(),
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok(LegacySet {
                owner: row.try_get("", "owner")?,
                name: row.try_get("", "name")?,
                description: row.try_get("", "description")?,
                collection_name: row.try_get("", "collection_name")?,
            })
        })
        .collect()
}

async fn read_members<C: ConnectionTrait>(conn: &C) -> Result<Vec<LegacyMember>, DbErr> {
    let rows = conn
        .query_all(Statement::from_string(
            conn.get_database_backend(),
            "SELECT owner, set_name, profile_id, position FROM set_members".to_owned(),
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok(LegacyMember {
                owner: row.try_get("", "owner")?,
                set_name: row.try_get("", "set_name")?,
                profile_id: row.try_get("", "profile_id")?,
                position: row.try_get("", "position")?,
            })
        })
        .collect()
}

async fn read_collections<C: ConnectionTrait>(conn: &C) -> Result<Vec<(String, String)>, DbErr> {
    let rows = conn
        .query_all(Statement::from_string(
            conn.get_database_backend(),
            "SELECT owner, name FROM collections".to_owned(),
        ))
        .await?;
    rows.into_iter()
        .map(|row| Ok((row.try_get("", "owner")?, row.try_get("", "name")?)))
        .collect()
}

async fn read_target_members<C: ConnectionTrait>(conn: &C) -> Result<Vec<TargetMember>, DbErr> {
    let rows = conn
        .query_all(Statement::from_string(
            conn.get_database_backend(),
            "SELECT owner, collection_name, set_name, profile_id, position FROM set_members"
                .to_owned(),
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok(TargetMember {
                owner: row.try_get("", "owner")?,
                collection_name: row.try_get("", "collection_name")?,
                set_name: row.try_get("", "set_name")?,
                profile_id: row.try_get("", "profile_id")?,
                position: row.try_get("", "position")?,
            })
        })
        .collect()
}

/// Every `(owner, profile_id)` the isolated store holds.
async fn read_profile_ids<C: ConnectionTrait>(conn: &C) -> Result<Vec<(String, String)>, DbErr> {
    let rows = conn
        .query_all(Statement::from_string(
            conn.get_database_backend(),
            "SELECT owner, profile_id FROM profiles".to_owned(),
        ))
        .await?;
    rows.into_iter()
        .map(|row| Ok((row.try_get("", "owner")?, row.try_get("", "profile_id")?)))
        .collect()
}

/// The refusal for a store two spaces key alike, or `None` when every id
/// stays inside one space: the old shape keys a profile by its id alone, so
/// those rows have nowhere to land, and the sentence names each id with its
/// holders.
fn shared_profile_id_refusal(rows: &[(String, String)]) -> Option<String> {
    let mut owners_by_id: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (owner, profile_id) in rows {
        owners_by_id
            .entry(profile_id.clone())
            .or_default()
            .insert(owner.clone());
    }
    let listed: Vec<String> = owners_by_id
        .iter()
        .filter(|(_, owners)| owners.len() > 1)
        .map(|(profile_id, owners)| {
            let owners: Vec<&str> = owners.iter().map(String::as_str).collect();
            format!("{profile_id} ({})", owners.join(", "))
        })
        .collect();
    if listed.is_empty() {
        return None;
    }
    Some(format!(
        "the old shape keys a profile by its id alone, so two spaces cannot both hold it: {}",
        listed.join("; ")
    ))
}

fn text(value: &str) -> Value {
    Value::String(Some(Box::new(value.to_owned())))
}

fn optional_text(value: Option<&str>) -> Value {
    Value::String(value.map(|value| Box::new(value.to_owned())))
}

async fn insert_profile<C: ConnectionTrait>(conn: &C, row: &TargetProfile) -> Result<(), DbErr> {
    conn.execute(Statement::from_sql_and_values(
        conn.get_database_backend(),
        "INSERT INTO profiles_new \
         (owner, collection_name, profile_id, provider_id, model, max_context_window, \
          effort, modalities, parked, store) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        vec![
            text(&row.owner),
            text(&row.collection_name),
            text(&row.profile_id),
            text(&row.provider_id),
            text(&row.model),
            Value::BigInt(row.max_context_window),
            optional_text(row.effort.as_deref()),
            optional_text(row.modalities.as_deref()),
            Value::Bool(Some(row.parked)),
            Value::Bool(row.store),
        ],
    ))
    .await?;
    Ok(())
}

async fn insert_set<C: ConnectionTrait>(conn: &C, row: &LegacySet) -> Result<(), DbErr> {
    conn.execute(Statement::from_sql_and_values(
        conn.get_database_backend(),
        "INSERT INTO profile_sets_new (owner, collection_name, name, description) \
         VALUES ($1, $2, $3, $4)",
        vec![
            text(&row.owner),
            text(&row.collection_name),
            text(&row.name),
            text(&row.description),
        ],
    ))
    .await?;
    Ok(())
}

async fn insert_member<C: ConnectionTrait>(conn: &C, row: &TargetMember) -> Result<(), DbErr> {
    conn.execute(Statement::from_sql_and_values(
        conn.get_database_backend(),
        "INSERT INTO set_members_new (owner, collection_name, set_name, profile_id, position) \
         VALUES ($1, $2, $3, $4, $5)",
        vec![
            text(&row.owner),
            text(&row.collection_name),
            text(&row.set_name),
            text(&row.profile_id),
            Value::Int(Some(row.position)),
        ],
    ))
    .await?;
    Ok(())
}

async fn insert_set_downgraded<C: ConnectionTrait>(
    conn: &C,
    row: &LegacySet,
    name: &str,
) -> Result<(), DbErr> {
    conn.execute(Statement::from_sql_and_values(
        conn.get_database_backend(),
        "INSERT INTO profile_sets_downgraded (owner, name, description, collection_name) \
         VALUES ($1, $2, $3, $4)",
        vec![
            text(&row.owner),
            text(name),
            text(&row.description),
            text(&row.collection_name),
        ],
    ))
    .await?;
    Ok(())
}

async fn insert_member_downgraded<C: ConnectionTrait>(
    conn: &C,
    row: &TargetMember,
    set_name: &str,
) -> Result<(), DbErr> {
    conn.execute(Statement::from_sql_and_values(
        conn.get_database_backend(),
        "INSERT INTO set_members_downgraded (owner, set_name, profile_id, position) \
         VALUES ($1, $2, $3, $4)",
        vec![
            text(&row.owner),
            text(set_name),
            text(&row.profile_id),
            Value::Int(Some(row.position)),
        ],
    ))
    .await?;
    Ok(())
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();

        // Read the old rows first: the rebuild copies them, nothing is
        // updated in place.
        let profiles = read_profiles(conn).await?;
        let sets = read_sets(conn).await?;
        let members = read_members(conn).await?;

        conn.execute_unprepared(&seed_home_collection()).await?;
        let collections = read_collections(conn).await?;
        let placement = plan_placement(&profiles, &sets, &members, &collections)?;

        for sql in UP_TABLES {
            conn.execute_unprepared(sql).await?;
        }
        for row in &placement.profiles {
            insert_profile(conn, row).await?;
        }
        for row in &sets {
            insert_set(conn, row).await?;
        }
        for row in &placement.members {
            insert_member(conn, row).await?;
        }
        for sql in UP_SWAP {
            conn.execute_unprepared(sql).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();

        let sets = read_sets(conn).await?;
        let names = plan_set_names(&sets);
        let members = read_target_members(conn).await?;
        if let Some(refusal) = shared_profile_id_refusal(&read_profile_ids(conn).await?) {
            return Err(DbErr::Migration(refusal));
        }

        for sql in DOWN_TABLES {
            conn.execute_unprepared(sql).await?;
        }
        conn.execute_unprepared(DOWN_PROFILES).await?;

        for row in &sets {
            let key = (
                row.owner.clone(),
                row.collection_name.clone(),
                row.name.clone(),
            );
            let name = names
                .get(&key)
                .ok_or_else(|| {
                    DbErr::Migration(format!(
                        "set ({}, {}, {}) has no downgraded name",
                        row.owner, row.collection_name, row.name
                    ))
                })?
                .clone();
            insert_set_downgraded(conn, row, &name).await?;
            if name != row.name {
                // The collection's default anchor names a set inside it,
                // so it follows the rename.
                conn.execute(Statement::from_sql_and_values(
                    conn.get_database_backend(),
                    "UPDATE collections SET default_set_name = $1 \
                     WHERE owner = $2 AND name = $3 AND default_set_name = $4",
                    vec![
                        text(&name),
                        text(&row.owner),
                        text(&row.collection_name),
                        text(&row.name),
                    ],
                ))
                .await?;
            }
        }
        for row in &members {
            let key = (
                row.owner.clone(),
                row.collection_name.clone(),
                row.set_name.clone(),
            );
            let name = names.get(&key).ok_or_else(|| {
                DbErr::Migration(format!(
                    "membership ({}, {}, {}) names a set with no downgraded name",
                    row.owner, row.collection_name, row.set_name
                ))
            })?;
            insert_member_downgraded(conn, row, name).await?;
        }

        for sql in DOWN_SWAP {
            conn.execute_unprepared(sql).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(owner: &str, profile_id: &str, parked: bool) -> LegacyProfile {
        LegacyProfile {
            owner: owner.to_owned(),
            profile_id: profile_id.to_owned(),
            provider_id: "p".to_owned(),
            model: "m".to_owned(),
            max_context_window: Some(128_000),
            effort: Some("high".to_owned()),
            modalities: Some("[\"text\"]".to_owned()),
            parked,
            store: Some(true),
        }
    }

    fn set(owner: &str, collection_name: &str, name: &str) -> LegacySet {
        LegacySet {
            owner: owner.to_owned(),
            name: name.to_owned(),
            description: "d".to_owned(),
            collection_name: collection_name.to_owned(),
        }
    }

    fn member(owner: &str, set_name: &str, profile_id: &str, position: i32) -> LegacyMember {
        LegacyMember {
            owner: owner.to_owned(),
            set_name: set_name.to_owned(),
            profile_id: profile_id.to_owned(),
            position,
        }
    }

    fn collections(owner: &str, names: &[&str]) -> Vec<(String, String)> {
        names
            .iter()
            .map(|name| (owner.to_owned(), (*name).to_owned()))
            .collect()
    }

    /// A profile in two collections' sets becomes one row per collection;
    /// the home row keeps the id and the copy carries it too, because the
    /// old store's ids were globally unique.
    #[test]
    fn a_shared_profile_lands_once_per_collection() {
        let profiles = vec![profile("system", "x", false)];
        let sets = vec![set("system", "beta", "s"), set("system", "alpha", "t")];
        let members = vec![member("system", "s", "x", 0), member("system", "t", "x", 0)];
        let collections = collections("system", &["beta", "alpha"]);

        let placement =
            plan_placement(&profiles, &sets, &members, &collections).expect("plan lands");

        let rows: Vec<(&str, &str)> = placement
            .profiles
            .iter()
            .map(|row| (row.collection_name.as_str(), row.profile_id.as_str()))
            .collect();
        assert_eq!(rows, vec![("alpha", "x"), ("beta", "x")]);
        let placed: Vec<(&str, &str, i32)> = placement
            .members
            .iter()
            .map(|row| {
                (
                    row.collection_name.as_str(),
                    row.profile_id.as_str(),
                    row.position,
                )
            })
            .collect();
        assert_eq!(placed, vec![("beta", "x", 0), ("alpha", "x", 0)]);
        for row in &placement.profiles {
            assert!(!row.parked, "a served row is never unassigned");
            assert_eq!(row.provider_id, "p");
            assert_eq!(row.model, "m");
            assert_eq!(row.max_context_window, Some(128_000));
            assert_eq!(row.store, Some(true));
        }
    }

    /// An unassigned row in no set lands in the space's smallest collection and
    /// keeps its flag; a stale parked flag on a served row is cleared.
    #[test]
    fn homes_are_deterministic_and_unassigned_rows_stay_off_served() {
        let profiles = vec![
            profile("system", "unassigned-row", true),
            profile("system", "x", true),
        ];
        let sets = vec![set("system", "zeta", "s")];
        let members = vec![member("system", "s", "x", 0)];
        let collections = collections("system", &["zeta", "alpha"]);

        let placement =
            plan_placement(&profiles, &sets, &members, &collections).expect("plan lands");

        let unassigned = placement
            .profiles
            .iter()
            .find(|row| row.profile_id == "unassigned-row")
            .expect("the memberless row lands");
        assert_eq!(unassigned.collection_name, "alpha");
        assert!(unassigned.parked, "a memberless row keeps its flag");
        let served = placement
            .profiles
            .iter()
            .find(|row| row.profile_id == "x")
            .expect("the served row lands");
        assert_eq!(served.collection_name, "zeta");
        assert!(!served.parked, "membership wins over a stale flag");
    }

    /// A space with no collection of its own is refused by the planner:
    /// `up` seeds `default` first, so this is a programming error, not a
    /// data condition silently papered over.
    #[test]
    fn a_space_without_collections_is_refused() {
        let profiles = vec![profile("user-a", "x", false)];
        let err = plan_placement(&profiles, &[], &[], &collections("system", &["alpha"]))
            .expect_err("no home");
        assert!(format!("{err}").contains("no collection to home"), "{err}");
    }

    /// A membership naming a set that does not exist cannot be placed.
    #[test]
    fn a_membership_without_its_set_is_refused() {
        let profiles = vec![profile("system", "x", false)];
        let members = vec![member("system", "gone", "x", 0)];
        let err = plan_placement(&profiles, &[], &members, &collections("system", &["alpha"]))
            .expect_err("no set");
        assert!(format!("{err}").contains("names a set"), "{err}");
    }

    /// The copy id is the source id when free, and steps to `-2`, `-3`
    /// when the collection already holds it (defensive: the old ids were
    /// globally unique, so real data never reaches the suffix).
    #[test]
    fn a_taken_id_steps_to_a_suffix() {
        let mut taken = HashSet::new();
        assert_eq!(pick_free(&taken, "system", "a", "x"), "x");
        taken.insert(("system".to_owned(), "a".to_owned(), "x".to_owned()));
        assert_eq!(pick_free(&taken, "system", "a", "x"), "x-2");
        taken.insert(("system".to_owned(), "a".to_owned(), "x-2".to_owned()));
        assert_eq!(pick_free(&taken, "system", "a", "x"), "x-3");
        assert_eq!(
            pick_free(&taken, "system", "b", "x"),
            "x",
            "another collection does not carry the id"
        );
    }

    /// Downgraded set names: the smallest collection keeps the name, the
    /// rest take `--<collection>`, and a taken candidate steps to `-2`.
    #[test]
    fn downgraded_set_names_keep_the_smallest_collection() {
        let sets = vec![
            set("system", "beta", "alpha"),
            set("system", "alpha", "alpha"),
            set("system", "gamma", "alpha--beta"),
            set("system", "alpha", "only"),
        ];
        let names = plan_set_names(&sets);
        assert_eq!(
            names[&("system".to_owned(), "alpha".to_owned(), "alpha".to_owned())],
            "alpha"
        );
        assert_eq!(
            names[&("system".to_owned(), "beta".to_owned(), "alpha".to_owned())],
            "alpha--beta-2",
            "a literal alpha--beta already holds that name"
        );
        assert_eq!(
            names[&(
                "system".to_owned(),
                "gamma".to_owned(),
                "alpha--beta".to_owned()
            )],
            "alpha--beta",
            "the literal name is kept by its only collection"
        );
        assert_eq!(
            names[&("system".to_owned(), "alpha".to_owned(), "only".to_owned())],
            "only"
        );
    }

    /// The old index is on the bare name, so the grouping ignores the
    /// owner: two spaces' same-named sets get one plain name and one
    /// suffixed name, and the keeper is the smallest `(owner, collection)`.
    #[test]
    fn downgraded_set_names_span_the_spaces() {
        let sets = vec![
            set("user-a", "alpha", "shared"),
            set("system", "alpha", "shared"),
            set("user-a", "beta", "shared"),
        ];
        let names = plan_set_names(&sets);
        assert_eq!(
            names[&("system".to_owned(), "alpha".to_owned(), "shared".to_owned())],
            "shared",
            "the smallest (owner, collection) keeps the plain name"
        );
        assert_eq!(
            names[&("user-a".to_owned(), "alpha".to_owned(), "shared".to_owned())],
            "shared--alpha",
            "the other space's set takes the collection suffix"
        );
        assert_eq!(
            names[&("user-a".to_owned(), "beta".to_owned(), "shared".to_owned())],
            "shared--beta",
            "every holder past the first is suffixed"
        );
    }

    /// A profile id is per collection in the new shape and per store in the
    /// old one: rows sharing an id inside one space merge, two spaces using
    /// it have no place to land.
    #[test]
    fn the_refusal_names_the_ids_two_spaces_share() {
        let rows = vec![
            ("system".to_owned(), "shared".to_owned()),
            ("system".to_owned(), "unassigned-row".to_owned()),
            ("user-a".to_owned(), "solo".to_owned()),
            ("user-b".to_owned(), "solo".to_owned()),
        ];
        let sentence = shared_profile_id_refusal(&rows).expect("solo spans two spaces");
        assert!(
            sentence.contains("solo (user-a, user-b)"),
            "the refusal names the id and both holders: {sentence}"
        );
        assert!(
            !sentence.contains("shared"),
            "an id one space holds is not refused: {sentence}"
        );
        let one_space = vec![
            ("system".to_owned(), "shared".to_owned()),
            ("system".to_owned(), "other".to_owned()),
        ];
        assert_eq!(shared_profile_id_refusal(&one_space), None);
    }

    /// A derived name yields to a literal row that already holds it: every
    /// bare name is reserved before any suffix is handed out, so the stepper
    /// walks past the literal holders instead of taking their names.
    #[test]
    fn a_literal_suffix_shaped_name_keeps_its_own() {
        let sets = vec![
            set("system", "alpha", "alpha"),
            set("system", "beta", "alpha"),
            set("system", "gamma", "alpha--beta"),
            set("system", "delta", "alpha--beta-2"),
        ];
        let names = plan_set_names(&sets);
        assert_eq!(
            names[&("system".to_owned(), "beta".to_owned(), "alpha".to_owned())],
            "alpha--beta-3",
            "both suffix candidates belong to literal rows"
        );
        assert_eq!(
            names[&("system".to_owned(), "alpha".to_owned(), "alpha".to_owned())],
            "alpha"
        );
        assert_eq!(
            names[&(
                "system".to_owned(),
                "gamma".to_owned(),
                "alpha--beta".to_owned()
            )],
            "alpha--beta"
        );
        assert_eq!(
            names[&(
                "system".to_owned(),
                "delta".to_owned(),
                "alpha--beta-2".to_owned()
            )],
            "alpha--beta-2"
        );
    }
}
