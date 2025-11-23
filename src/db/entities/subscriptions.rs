use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Deserialize, Serialize)]
#[sea_orm(table_name = "subscriptions")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub chat_id: i64,
    pub task_id: i32,
    pub filter_tags: Option<Json>,
    pub created_at: DateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(belongs_to = "super::chats::Entity", from = "Column::ChatId", to = "super::chats::Column::Id")]
    Chat,
    #[sea_orm(belongs_to = "super::tasks::Entity", from = "Column::TaskId", to = "super::tasks::Column::Id")]
    Task,
}

impl Related<super::chats::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Chat.def()
    }
}

impl Related<super::tasks::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Task.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
