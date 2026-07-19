use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Deserialize, Serialize)]
#[sea_orm(table_name = "eh_gp_spend_attempts")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    #[sea_orm(nullable)]
    pub queue_id: Option<i32>,
    pub gid: i64,
    pub gp_cost: i64,
    pub created_at: DateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::eh_download_queue::Entity",
        from = "Column::QueueId",
        to = "super::eh_download_queue::Column::Id",
        on_delete = "SetNull"
    )]
    Queue,
}

impl Related<super::eh_download_queue::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Queue.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
