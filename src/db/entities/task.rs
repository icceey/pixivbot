use chrono::{DateTime, Utc};
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "tasks")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    #[sea_orm(column_type = "String(Some(20))")]
    pub r#type: String, // "author" or "ranking"
    pub value: String, // Pixiv ID or ranking mode
    pub interval_sec: i32,
    pub next_poll_at: DateTime<Utc>,
    pub last_polled_at: Option<DateTime<Utc>>,
    pub latest_data: Option<Value>, // JSON for storing latest_data
    pub created_by: Option<i64>, // User ID who created the task
    pub updated_by: Option<i64>, // User ID who updated the task
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "crate::db::entities::subscription::Entity")]
    Subscription,
}

impl Related<crate::db::entities::subscription::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Subscription.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}