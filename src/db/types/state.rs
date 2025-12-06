use sea_orm::FromJsonQueryResult;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromJsonQueryResult)]
#[serde(tag = "type", content = "state")]
pub enum SubscriptionState {
    Author(AuthorState),
    Ranking(RankingState),
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
