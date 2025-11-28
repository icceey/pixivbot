use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Deserialize, Serialize)]
#[sea_orm(table_name = "tasks")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub r#type: String, // author, ranking
    #[sea_orm(unique)]
    pub value: String, // author_id, ranking_mode
    #[sea_orm(indexed)]
    pub next_poll_at: DateTime,
    pub last_polled_at: Option<DateTime>,
    pub created_by: i64,
    pub author_name: Option<String>, // 作者名字（仅 type="author" 时有值）
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::subscriptions::Entity")]
    Subscriptions,
    #[sea_orm(
        belongs_to = "super::users::Entity",
        from = "Column::CreatedBy",
        to = "super::users::Column::Id",
        on_update = "NoAction",
        on_delete = "NoAction"
    )]
    UserCreated,
}

impl Related<super::subscriptions::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Subscriptions.def()
    }
}

impl Related<super::users::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::UserCreated.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
