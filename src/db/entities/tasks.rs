use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::types::TaskType;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Deserialize, Serialize)]
#[sea_orm(table_name = "tasks")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub r#type: TaskType,
    // Note: (type, value) has a composite unique index in the database
    pub value: String, // author_id, ranking_mode
    #[sea_orm(indexed)]
    pub next_poll_at: DateTime,
    pub last_polled_at: Option<DateTime>,
    pub author_name: Option<String>, // 作者名字（仅 type="author" 时有值）
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
