use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Download queue entry for e-hentai archives.
///
/// Entries are enqueued by EhEngine (subscription scans) or by `/edl` (direct
/// download). The EhDownloadProcessor drains pending entries with rate-limit
/// enforcement.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Deserialize, Serialize)]
#[sea_orm(table_name = "eh_download_queue")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub chat_id: i64,
    pub gid: i64,
    pub token: String,
    pub title: String,
    #[sea_orm(default = false)]
    pub telegraph: bool,
    /// "subscription" or "direct"
    pub source: String,
    /// "pending", "downloading", "done", "failed"
    pub status: String,
    #[sea_orm(default = 0)]
    pub file_size: i64,
    #[sea_orm(nullable)]
    pub error: Option<String>,
    #[sea_orm(default = 0)]
    pub retry_count: i32,
    pub created_at: DateTime,
    #[sea_orm(nullable)]
    pub started_at: Option<DateTime>,
    #[sea_orm(nullable)]
    pub completed_at: Option<DateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
