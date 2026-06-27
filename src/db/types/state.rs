use chrono::{DateTime, Utc};
use sea_orm::FromJsonQueryResult;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FromJsonQueryResult)]
#[serde(tag = "type", content = "state")]
pub enum SubscriptionState {
    Author(AuthorState),
    Ranking(RankingState),
    BooruTag(BooruTagState),
    BooruPool(BooruPoolState),
    BooruRanking(BooruRankingState),
    EhTag(EhTagState),
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
    /// Posts below filter threshold awaiting fav/score ripening. GC uses local `first_seen`
    /// (not server `created_at`) for uniform behaviour across Danbooru/Moebooru/Gelbooru.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hot_posts: Vec<HotPost>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotPost {
    pub id: u64,
    pub first_seen: DateTime<Utc>,
    /// `true` once pushed; dedup within grace window while cursor is held back.
    #[serde(default)]
    pub pushed: bool,
    /// Send-failure counter; abandons after threshold to prevent infinite retries.
    #[serde(default)]
    pub attempts: u8,
}

impl BooruTagState {
    pub fn cleared(latest_post_id: u64) -> Self {
        Self {
            latest_post_id,
            pending_queue: Vec::new(),
            retry_count: 0,
            hot_posts: Vec::new(),
        }
    }

    /// Create state with the front item removed from the queue (successful send or skip).
    ///
    /// Panics if the queue is empty.
    pub fn popped_front(&self) -> Self {
        Self {
            latest_post_id: self.latest_post_id,
            pending_queue: self.pending_queue[1..].to_vec(),
            retry_count: 0,
            hot_posts: self.hot_posts.clone(),
        }
    }

    /// Create state with retry count incremented (failed send).
    pub fn with_retry_increment(&self) -> Self {
        Self {
            latest_post_id: self.latest_post_id,
            pending_queue: self.pending_queue.clone(),
            retry_count: self.retry_count.saturating_add(1),
            hot_posts: self.hot_posts.clone(),
        }
    }

    /// Determine whether the pending queue should be abandoned.
    /// Returns `true` if retry is disabled (`max_retry_count <= 0`) or retry limit is exhausted.
    pub fn should_abandon_queue(&self, max_retry_count: i32) -> bool {
        max_retry_count <= 0 || (self.retry_count as i32) >= max_retry_count
    }
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BooruRankingState {
    pub pushed_ids: Vec<u64>,
    #[serde(default)]
    pub retry_count: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_post: Option<QueuedBooruPost>,
    /// Per-post send-failure counters; abandons after threshold (mirrors HotPost.attempts).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_attempts: Vec<(u64, u8)>,
}

impl BooruRankingState {
    /// Drop the front of `pushed_ids` until length <= cap.
    ///
    /// **Ordering invariant**: `pushed_ids` must be in push-chronological order
    /// (oldest push at index 0, newest at the end) so that the front entries
    /// dropped here are truly the oldest-pushed ones. The scheduler maintains
    /// this invariant by using an order-preserving dedup (`retain`) instead of
    /// `sort_unstable() + dedup()` before calling this method.
    pub fn trim_pushed(&mut self, cap: usize) {
        if self.pushed_ids.len() > cap {
            let drop = self.pushed_ids.len() - cap;
            self.pushed_ids.drain(0..drop);
        }
    }
}

/// State for e-hentai tag subscriptions.
///
/// Tracks which gallery GIDs have been sent (dedup) and the latest posted
/// timestamp for cursor-based polling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EhTagState {
    /// GIDs that have already been pushed to the chat.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pushed_gids: Vec<u64>,
    /// Unix timestamp of the newest gallery processed (cursor).
    #[serde(default)]
    pub latest_posted_ts: i64,
}

impl EhTagState {
    pub fn cleared(latest_posted_ts: i64) -> Self {
        Self {
            pushed_gids: Vec::new(),
            latest_posted_ts,
        }
    }

    /// Add a GID to the pushed set (dedup, preserves insertion order).
    pub fn add_pushed_gid(&mut self, gid: u64) {
        if !self.pushed_gids.contains(&gid) {
            self.pushed_gids.push(gid);
        }
    }

    /// Drop the front of `pushed_gids` until length <= cap.
    pub fn trim_pushed(&mut self, cap: usize) {
        if self.pushed_gids.len() > cap {
            let drop = self.pushed_gids.len() - cap;
            self.pushed_gids.drain(0..drop);
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_queued_post(id: u64) -> QueuedBooruPost {
        QueuedBooruPost {
            id,
            tags: "test".to_string(),
            score: 0,
            fav_count: 0,
            file_url: Some(format!("https://example.com/{}.jpg", id)),
            sample_url: None,
            preview_url: None,
            rating: "s".to_string(),
            width: 100,
            height: 100,
            source: None,
        }
    }

    #[test]
    fn test_booru_tag_state_cleared() {
        let state = BooruTagState::cleared(42);
        assert_eq!(state.latest_post_id, 42);
        assert!(state.pending_queue.is_empty());
        assert_eq!(state.retry_count, 0);
    }

    #[test]
    fn test_booru_tag_state_popped_front() {
        let state = BooruTagState {
            latest_post_id: 100,
            pending_queue: vec![
                make_queued_post(1),
                make_queued_post(2),
                make_queued_post(3),
            ],
            retry_count: 2,
            hot_posts: Vec::new(),
        };
        let popped = state.popped_front();
        assert_eq!(popped.latest_post_id, 100);
        assert_eq!(popped.pending_queue.len(), 2);
        assert_eq!(popped.pending_queue[0].id, 2);
        assert_eq!(popped.pending_queue[1].id, 3);
        assert_eq!(popped.retry_count, 0);
    }

    #[test]
    fn test_booru_tag_state_with_retry_increment() {
        let state = BooruTagState {
            latest_post_id: 100,
            pending_queue: vec![make_queued_post(1)],
            retry_count: 2,
            hot_posts: Vec::new(),
        };
        let retried = state.with_retry_increment();
        assert_eq!(retried.latest_post_id, 100);
        assert_eq!(retried.pending_queue.len(), 1);
        assert_eq!(retried.retry_count, 3);
    }

    #[test]
    fn test_booru_tag_state_with_retry_increment_saturates() {
        let state = BooruTagState {
            latest_post_id: 100,
            pending_queue: vec![make_queued_post(1)],
            retry_count: u8::MAX,
            hot_posts: Vec::new(),
        };
        let retried = state.with_retry_increment();
        assert_eq!(retried.retry_count, u8::MAX);
    }

    #[test]
    fn test_should_abandon_queue_retry_disabled() {
        let state = BooruTagState {
            latest_post_id: 100,
            pending_queue: vec![make_queued_post(1)],
            retry_count: 0,
            hot_posts: Vec::new(),
        };
        // max_retry_count <= 0 means retry disabled
        assert!(state.should_abandon_queue(0));
        assert!(state.should_abandon_queue(-1));
    }

    #[test]
    fn test_should_abandon_queue_retry_exhausted() {
        let state = BooruTagState {
            latest_post_id: 100,
            pending_queue: vec![make_queued_post(1)],
            retry_count: 3,
            hot_posts: Vec::new(),
        };
        assert!(state.should_abandon_queue(3));
        assert!(state.should_abandon_queue(2));
        assert!(!state.should_abandon_queue(4));
    }

    #[test]
    fn test_should_abandon_queue_retries_remaining() {
        let state = BooruTagState {
            latest_post_id: 100,
            pending_queue: vec![make_queued_post(1)],
            retry_count: 1,
            hot_posts: Vec::new(),
        };
        assert!(!state.should_abandon_queue(3));
        assert!(!state.should_abandon_queue(2));
    }

    #[test]
    fn test_eh_tag_state_cleared() {
        let state = EhTagState::cleared(12345);
        assert_eq!(state.latest_posted_ts, 12345);
        assert!(state.pushed_gids.is_empty());
    }

    #[test]
    fn test_eh_tag_state_add_pushed_gid_dedup() {
        let mut state = EhTagState::cleared(100);
        state.add_pushed_gid(1);
        state.add_pushed_gid(2);
        state.add_pushed_gid(1); // duplicate
        assert_eq!(state.pushed_gids, vec![1, 2]);
    }

    #[test]
    fn test_eh_tag_state_trim_pushed() {
        let mut state = EhTagState {
            pushed_gids: vec![1, 2, 3, 4, 5],
            latest_posted_ts: 100,
        };
        state.trim_pushed(3);
        assert_eq!(state.pushed_gids, vec![3, 4, 5]);
    }
}
