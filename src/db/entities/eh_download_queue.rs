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
    /// Comma-separated owning subscription ids for scheduler-created entries.
    /// Direct entries and older rows keep this null.
    #[sea_orm(nullable)]
    pub subscription_ids: Option<String>,
    /// Comma-separated owning subscription ids that requested Telegraph delivery.
    /// Subset of `subscription_ids`; direct entries keep this null.
    #[sea_orm(nullable)]
    pub telegraph_subscription_ids: Option<String>,
    /// "pending", "downloading", "done", "failed"
    pub status: String,
    #[sea_orm(default = 0)]
    pub file_size: i64,
    /// Compatibility/display metadata for the most recent successful archive
    /// download (0 for free / unlocked downloads). The append-only
    /// `eh_gp_spend_attempts` ledger calculates rolling GP budgets.
    #[sea_orm(default = 0)]
    pub gp_cost: i64,
    #[sea_orm(nullable)]
    pub error: Option<String>,
    #[sea_orm(default = 0)]
    pub retry_count: i32,
    pub created_at: DateTime,
    #[sea_orm(nullable)]
    pub started_at: Option<DateTime>,
    #[sea_orm(nullable)]
    pub completed_at: Option<DateTime>,
    /// Local path to the downloaded ZIP file (set by download stage).
    #[sea_orm(nullable)]
    pub zip_path: Option<String>,
    /// Telegraph page URL (set by upload stage, only for telegraph=true entries).
    #[sea_orm(nullable)]
    pub telegraph_url: Option<String>,
    /// Earliest time to retry this entry (for backoff).
    #[sea_orm(nullable)]
    pub next_retry_at: Option<DateTime>,
    /// Timestamp when the archive ZIP was sent to the chat (publish stage).
    #[sea_orm(nullable)]
    pub archive_sent_at: Option<DateTime>,
    /// Timestamp when the Telegraph link was sent to the chat (publish stage).
    #[sea_orm(nullable)]
    pub telegraph_sent_at: Option<DateTime>,
    #[sea_orm(nullable)]
    pub background_download_status: Option<String>,
    #[sea_orm(nullable)]
    pub background_download_started_at: Option<DateTime>,
    #[sea_orm(nullable)]
    pub background_download_next_retry_at: Option<DateTime>,
    #[sea_orm(default = 0)]
    pub background_download_attempt_count: i32,
    #[sea_orm(nullable)]
    pub background_download_error: Option<String>,
    #[sea_orm(nullable)]
    pub telegraph_rewrite_data: Option<String>,
    #[sea_orm(nullable)]
    pub telegraph_rewrite_status: Option<String>,
    #[sea_orm(nullable)]
    pub telegraph_rewrite_after: Option<DateTime>,
    #[sea_orm(nullable)]
    pub telegraph_rewrite_started_at: Option<DateTime>,
    #[sea_orm(nullable)]
    pub telegraph_rewrite_next_retry_at: Option<DateTime>,
    #[sea_orm(default = 0)]
    pub telegraph_rewrite_retry_count: i32,
    #[sea_orm(nullable)]
    pub telegraph_rewrite_error: Option<String>,
    #[sea_orm(nullable)]
    pub telegraph_rewritten_at: Option<DateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
