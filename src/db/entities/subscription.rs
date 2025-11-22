use chrono::{DateTime, Utc};
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "subscriptions")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub chat_id: i64,
    pub task_id: i32,
    pub filter_tags: Option<Value>, // JSON for filter configuration
    pub created_at: DateTime<Utc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::db::entities::chat::Entity",
        from = "Column::ChatId",
        to = "crate::db::entities::chat::Column::Id"
    )]
    Chat,
    #[sea_orm(
        belongs_to = "crate::db::entities::task::Entity",
        from = "Column::TaskId",
        to = "crate::db::entities::task::Column::Id"
    )]
    Task,
}

impl Related<crate::db::entities::chat::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Chat.def()
    }
}

impl Related<crate::db::entities::task::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Task.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}