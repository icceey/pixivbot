use sea_orm::FromJsonQueryResult;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromJsonQueryResult)]
#[serde(tag = "type", content = "state")]
pub enum SubscriptionState {
    Author(AuthorState),
    Ranking(RankingState),
    EhGallery(EhGalleryState),
    EhSearch(EhSearchState),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorState {
    pub latest_illust_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_illust: Option<PendingIllust>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankingState {
    pub pushed_ids: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_illust: Option<PendingIllust>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingIllust {
    pub illust_id: u64,
    pub sent_pages: Vec<usize>,
    pub total_pages: usize,
    pub retry_count: u8,
}

/// E-Hentai 画廊订阅状态
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EhGalleryState {
    /// 当前订阅的画廊 gid
    pub current_gid: u64,
    /// 当前订阅的画廊 token
    pub current_token: String,
    /// 最后检查时间 (Unix timestamp)
    #[serde(default)]
    pub last_checked_at: Option<i64>,
}

/// E-Hentai 搜索订阅状态
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EhSearchState {
    /// 已推送的画廊 gid 列表
    /// 注意: 引擎实现应限制数量避免无限增长
    pub pushed_gids: Vec<u64>,
    /// 最后检查时间 (Unix timestamp)
    #[serde(default)]
    pub last_checked_at: Option<i64>,
}
