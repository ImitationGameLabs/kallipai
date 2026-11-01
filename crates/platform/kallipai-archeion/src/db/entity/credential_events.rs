//! `credential_events` entity — the audit trail of one credential record.
//!
//! One row per lifecycle action that changes something: `create` carries the
//! starting set in `added`, a rescope carries what it granted and withdrew, and
//! `reset` writes both empty because only the hash changes. A rescope that
//! changes nothing writes no row. A `mint` row records that the credential
//! asked for an enrollment code: `target_id` names the pending tagma it
//! created. `actor_user_id` is the user who acted; NULL when no user did: a
//! row the migration carried, or the create a tagma writes for itself as it
//! enrolls.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "credential_events")]
pub struct Model {
    #[sea_orm(primary_key)]
    #[sea_orm(column_type = "Text")]
    pub id: String,
    #[sea_orm(column_type = "Text")]
    pub token_id: String,
    #[sea_orm(column_type = "TimestampWithTimeZone")]
    pub at: OffsetDateTime,
    #[sea_orm(column_type = "Text")]
    pub actor_user_id: Option<String>,
    /// `create` / `reset` / `perms` / `mint`.
    #[sea_orm(column_type = "Text")]
    pub kind: String,
    pub added: Json,
    /// What a `mint` acted on: the pending tagma id. NULL for the kinds that
    /// act on the credential row itself.
    #[sea_orm(column_type = "Text")]
    pub target_id: Option<String>,
    pub removed: Json,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::access_tokens::Entity",
        from = "Column::TokenId",
        to = "super::access_tokens::Column::Id"
    )]
    Token,
}

impl Related<super::access_tokens::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Token.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
