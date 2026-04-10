use sea_orm::FromJsonQueryResult;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromJsonQueryResult)]
#[serde(tag = "type", content = "state")]
pub enum SubscriptionState {
    Author(AuthorState),
    Ranking(RankingState),
    BooruTag(BooruTagState),
    BooruPool(BooruPoolState),
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

/// State for booru tag subscriptions.
///
/// Tracks the latest seen post ID and maintains a queue for empty-tag subscriptions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BooruTagState {
    /// The latest post ID that has been processed
    pub latest_post_id: u64,
    /// Queue of posts waiting to be sent (for empty-tag "subscribe to all" mode).
    /// Uses drain-first model: posts are drained from front before polling for new ones.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_queue: Vec<QueuedBooruPost>,
    /// Retry count for failed pushes
    #[serde(default)]
    pub retry_count: u8,
}

/// State for booru pool subscriptions.
///
/// Tracks which posts in the pool have been sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BooruPoolState {
    /// Set of post IDs that have already been pushed
    pub pushed_post_ids: Vec<u64>,
    /// Retry count for failed pushes
    #[serde(default)]
    pub retry_count: u8,
}

/// A queued booru post with full data for pending delivery.
///
/// Stores complete post data so we don't need to re-fetch from the API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedBooruPost {
    pub id: u64,
    pub tags: String,
    pub score: i32,
    pub fav_count: i32,
    pub file_url: Option<String>,
    pub sample_url: Option<String>,
    pub preview_url: Option<String>,
    pub rating: String,
    pub width: u32,
    pub height: u32,
    pub source: Option<String>,
}
