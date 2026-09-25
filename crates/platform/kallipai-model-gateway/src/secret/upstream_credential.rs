//! The `upstream_credentials` entity: per-profile provider endpoint prefix
//! and API key. Never selected by distribution paths -- only the secret
//! module reads this table.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "upstream_credentials")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub profile_id: String,
    /// The upstream URL prefix before the wire path (`/chat/completions`
    /// is appended by the forwarding path): e.g. `https://api.deepseek.com`
    /// or an openai-compatible root including its `/v1` where the provider
    /// routes there.
    pub upstream_base_url: String,
    pub upstream_api_key: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
