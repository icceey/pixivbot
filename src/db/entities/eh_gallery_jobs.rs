use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Deserialize, Serialize)]
#[sea_orm(table_name = "eh_gallery_jobs")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub gid: i64,
    pub token: String,
    pub download_mode: String,
    pub resolution: String,
    pub title: String,
    pub status: String,
    pub telegraph_status: String,
    pub telegraph_required: bool,
    pub file_size: i64,
    pub gp_cost: i64,
    pub zip_path: Option<String>,
    pub telegraph_url: Option<String>,
    pub error: Option<String>,
    pub retry_count: i32,
    pub next_retry_at: Option<DateTime>,
    pub cleanup_status: String,
    pub cleanup_started_at: Option<DateTime>,
    pub cleanup_error: Option<String>,
    pub cleanup_next_retry_at: Option<DateTime>,
    pub created_at: DateTime,
    pub started_at: Option<DateTime>,
    pub completed_at: Option<DateTime>,
    pub background_download_status: Option<String>,
    pub background_download_started_at: Option<DateTime>,
    pub background_download_next_retry_at: Option<DateTime>,
    pub background_download_attempt_count: i32,
    pub background_download_error: Option<String>,
    pub telegraph_rewrite_data: Option<String>,
    pub telegraph_rewrite_status: Option<String>,
    pub telegraph_rewrite_after: Option<DateTime>,
    pub telegraph_rewrite_started_at: Option<DateTime>,
    pub telegraph_rewrite_next_retry_at: Option<DateTime>,
    pub telegraph_rewrite_retry_count: i32,
    pub telegraph_rewrite_error: Option<String>,
    pub telegraph_rewritten_at: Option<DateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::eh_download_queue::Entity")]
    Deliveries,
    #[sea_orm(has_many = "super::eh_gp_spend_attempts::Entity")]
    GpSpendAttempts,
    #[sea_orm(has_many = "super::eh_download_completions::Entity")]
    DownloadCompletions,
}

impl Related<super::eh_download_queue::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Deliveries.def()
    }
}

impl Related<super::eh_gp_spend_attempts::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::GpSpendAttempts.def()
    }
}

impl Related<super::eh_download_completions::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::DownloadCompletions.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
