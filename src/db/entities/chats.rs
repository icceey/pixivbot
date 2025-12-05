use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::types::Tags;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Deserialize, Serialize)]
#[sea_orm(table_name = "chats")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i64,
    pub r#type: String,
    pub title: Option<String>,
    pub enabled: bool,
    pub blur_sensitive_tags: bool,
    pub excluded_tags: Tags,
    pub sensitive_tags: Tags,
    pub created_at: DateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::subscriptions::Entity")]
    Subscriptions,
}

impl Related<super::subscriptions::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Subscriptions.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
