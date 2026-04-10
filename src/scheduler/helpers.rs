use crate::bot::notifier::{
    BatchSendResult, ContinuationNumbering, DownloadButtonConfig, Notifier,
};
use crate::db::entities::{chats, subscriptions};
use crate::db::repo::Repo;
use crate::db::types::{AuthorState, BooruTagState, RankingState, SubscriptionState, TagFilter};
use crate::pixiv::client::PixivClient;
use crate::utils::{caption, sensitive};
use anyhow::{Context, Result};
use pixiv_client::Illust;
use std::sync::Arc;
use teloxide::prelude::*;
use tokio::sync::RwLock;
use tracing::info;

pub const INTER_SUBSCRIPTION_DELAY_MS: u64 = 2000;

/// Result of processing a single illust push
#[derive(Debug)]
pub enum PushResult {
    /// All pages sent successfully
    Success {
        illust_id: u64,
        first_message_id: Option<i32>,
    },
    /// Some pages failed, need to retry
    Partial {
        illust_id: u64,
        sent_pages: Vec<usize>,
        total_pages: usize,
        first_message_id: Option<i32>,
    },
    /// Complete failure, retry later
    Failure { illust_id: u64 },
}

/// Context for processing a single author subscription
pub struct AuthorContext<'a> {
    pub subscription: &'a crate::db::entities::subscriptions::Model,
    pub chat: crate::db::entities::chats::Model,
    pub subscription_state: Option<crate::db::types::AuthorState>,
}

/// Context for processing a single ranking subscription
pub struct RankingContext<'a> {
    pub subscription: &'a crate::db::entities::subscriptions::Model,
    pub chat: crate::db::entities::chats::Model,
    pub subscription_state: Option<RankingState>,
}

pub fn author_subscription_state(subscription: &subscriptions::Model) -> Option<AuthorState> {
    match &subscription.latest_data {
        Some(SubscriptionState::Author(state)) => Some(state.clone()),
        _ => None,
    }
}

pub fn ranking_subscription_state(subscription: &subscriptions::Model) -> Option<RankingState> {
    match &subscription.latest_data {
        Some(SubscriptionState::Ranking(state)) => Some(state.clone()),
        _ => None,
    }
}

pub fn booru_tag_subscription_state(subscription: &subscriptions::Model) -> Option<BooruTagState> {
    match &subscription.latest_data {
        Some(SubscriptionState::BooruTag(state)) => Some(state.clone()),
        _ => None,
    }
}

pub fn apply_subscription_tag_filter<'a>(
    subscription: &subscriptions::Model,
    chat: &chats::Model,
    illusts: impl IntoIterator<Item = &'a Illust>,
) -> Vec<&'a Illust> {
    let chat_filter = TagFilter::from_excluded_tags(&chat.excluded_tags);
    let combined_filter = subscription.filter_tags.merged(&chat_filter);
    combined_filter.filter(illusts)
}

pub async fn save_first_message_record(
    repo: &Repo,
    chat_id: ChatId,
    subscription_id: i32,
    first_message_id: Option<i32>,
    illust_id: Option<i64>,
) {
    let Some(msg_id) = first_message_id else {
        return;
    };

    if let Err(e) = repo
        .save_message(chat_id.0, msg_id, subscription_id, illust_id)
        .await
    {
        tracing::warn!("Failed to save message record: {:#}", e);
    }
}

/// Get chat and check if should notify (enabled or admin)
pub async fn get_chat_if_should_notify(
    repo: &Repo,
    chat_id: i64,
) -> Result<Option<crate::db::entities::chats::Model>> {
    let chat = repo.get_chat(chat_id).await.context("Failed to get chat")?;

    let Some(chat) = chat else {
        info!("Chat {} not found, skipping", chat_id);
        return Ok(None);
    };

    if chat.enabled {
        return Ok(Some(chat));
    }

    // Check if admin/owner
    match repo.get_user(chat_id).await {
        Ok(Some(user)) if user.role.is_admin() => Ok(Some(chat)),
        _ => {
            info!("Skipping notification to disabled chat {}", chat_id);
            Ok(None)
        }
    }
}

/// Generic push executor: Send specific illust pages (excluding already sent pages)
pub async fn process_illust_push(
    notifier: &Notifier,
    pixiv: &Arc<RwLock<PixivClient>>,
    ctx: &AuthorContext<'_>,
    illust: &Illust,
    already_sent_pages: &[usize],
    image_size: pixiv_client::ImageSize,
) -> Result<PushResult> {
    // For ugoira works, delegate to the specialized handler
    if illust.is_ugoira() {
        return process_ugoira_push(notifier, pixiv, ctx, illust).await;
    }

    let chat_id = ChatId(ctx.subscription.chat_id);
    let all_urls = illust.get_all_image_urls_with_size(image_size);
    let total_pages = all_urls.len();

    // Calculate pages to send
    let pages_to_send: Vec<usize> = (0..total_pages)
        .filter(|i| !already_sent_pages.contains(i))
        .collect();

    if pages_to_send.is_empty() {
        return Ok(PushResult::Success {
            illust_id: illust.id,
            first_message_id: None,
        });
    }

    let urls_to_send: Vec<String> = pages_to_send
        .iter()
        .filter_map(|&i| all_urls.get(i).cloned())
        .collect();

    // Prepare caption
    let caption = if already_sent_pages.is_empty() {
        caption::build_illust_caption(illust)
    } else {
        caption::build_continuation_caption(illust, already_sent_pages.len(), total_pages)
    };

    // Check spoiler setting
    let has_spoiler = sensitive::should_blur(&ctx.chat, illust);

    // Build download button config
    // Skip download button for channel chats (channels don't support inline buttons)
    let download_config = DownloadButtonConfig::for_chat(illust.id, &ctx.chat);

    // Send images with download button
    let continuation_numbering = (!already_sent_pages.is_empty()).then(|| {
        ContinuationNumbering::new(
            (already_sent_pages.len() / caption::MAX_PER_GROUP) + 1,
            total_pages.div_ceil(caption::MAX_PER_GROUP),
        )
    });

    let send_result = notifier
        .notify_with_images_and_button_and_continuation(
            chat_id,
            &urls_to_send,
            Some(&caption),
            has_spoiler,
            &download_config,
            continuation_numbering.unwrap_or_else(|| {
                ContinuationNumbering::new(1, total_pages.div_ceil(caption::MAX_PER_GROUP))
            }),
        )
        .await;

    // Map send result to PushResult
    let result = map_send_result_to_push_result(
        illust.id,
        send_result,
        already_sent_pages,
        &pages_to_send,
        total_pages,
    );

    Ok(result)
}

/// Map BatchSendResult to PushResult
fn map_send_result_to_push_result(
    illust_id: u64,
    send_result: BatchSendResult,
    already_sent: &[usize],
    attempted_pages: &[usize],
    total_pages: usize,
) -> PushResult {
    let first_message_id = send_result.first_message_id;

    if send_result.is_complete_success() {
        // All attempted pages succeeded
        let mut all_sent = already_sent.to_vec();
        all_sent.extend(attempted_pages);
        all_sent.sort();
        all_sent.dedup();

        if all_sent.len() == total_pages {
            PushResult::Success {
                illust_id,
                first_message_id,
            }
        } else {
            // Should not happen, but handle gracefully
            PushResult::Partial {
                illust_id,
                sent_pages: all_sent,
                total_pages,
                first_message_id,
            }
        }
    } else if send_result.is_complete_failure() {
        PushResult::Failure { illust_id }
    } else {
        // Partial success
        let mut all_sent = already_sent.to_vec();
        for &idx in &send_result.succeeded_indices {
            if let Some(&page_idx) = attempted_pages.get(idx) {
                all_sent.push(page_idx);
            }
        }
        all_sent.sort();
        all_sent.dedup();

        PushResult::Partial {
            illust_id,
            sent_pages: all_sent,
            total_pages,
            first_message_id,
        }
    }
}

/// Push a ugoira (animated) illust as an MP4 animation
async fn process_ugoira_push(
    notifier: &Notifier,
    pixiv: &Arc<RwLock<PixivClient>>,
    ctx: &AuthorContext<'_>,
    illust: &Illust,
) -> Result<PushResult> {
    let chat_id = ChatId(ctx.subscription.chat_id);

    // Fetch ugoira metadata (ZIP URL + frame delays)
    let pixiv_guard = pixiv.read().await;
    let metadata = pixiv_guard
        .get_ugoira_metadata(illust.id)
        .await
        .context("Failed to fetch ugoira metadata")?;
    drop(pixiv_guard);

    // Prepare caption (same format as regular illusts, with 🎞️ indicator)
    let caption = caption::build_ugoira_caption(illust);

    // Check spoiler setting
    let has_spoiler = sensitive::should_blur(&ctx.chat, illust);

    // Build download button config
    let download_config = DownloadButtonConfig::for_chat(illust.id, &ctx.chat);

    // Send ugoira as MP4 animation
    let send_result = notifier
        .notify_ugoira(
            chat_id,
            &metadata.zip_urls.medium,
            metadata.frames,
            Some(&caption),
            has_spoiler,
            &download_config,
        )
        .await;

    // Ugoira is a single item, so treat it simply
    if send_result.is_complete_failure() {
        Ok(PushResult::Failure {
            illust_id: illust.id,
        })
    } else {
        Ok(PushResult::Success {
            illust_id: illust.id,
            first_message_id: send_result.first_message_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_subscription_tag_filter, author_subscription_state, ranking_subscription_state,
        INTER_SUBSCRIPTION_DELAY_MS,
    };
    use crate::db::entities::{chats, subscriptions};
    use crate::db::types::{AuthorState, RankingState, SubscriptionState, TagFilter, Tags};
    use pixiv_client::Illust;
    use serde_json::json;

    fn make_chat(excluded_tags: &[&str]) -> chats::Model {
        chats::Model {
            id: 1,
            r#type: "private".to_string(),
            title: Some("chat".to_string()),
            enabled: true,
            blur_sensitive_tags: false,
            excluded_tags: Tags(excluded_tags.iter().map(|t| t.to_string()).collect()),
            sensitive_tags: Tags::default(),
            created_at: chrono::Utc::now().naive_utc(),
            allow_without_mention: false,
        }
    }

    fn make_subscription(
        latest_data: Option<SubscriptionState>,
        filter_tags: TagFilter,
    ) -> subscriptions::Model {
        subscriptions::Model {
            id: 1,
            chat_id: 1,
            task_id: 1,
            filter_tags,
            booru_filter: None,
            latest_data,
            created_at: chrono::Utc::now().naive_utc(),
        }
    }

    fn make_illust(id: u64, tags: &[&str]) -> Illust {
        serde_json::from_value(json!({
            "id": id,
            "title": format!("illust-{id}"),
            "type": "illust",
            "image_urls": {
                "square_medium": "square",
                "medium": "medium",
                "large": "large",
                "original": "original"
            },
            "caption": "",
            "restrict": 0,
            "user": {
                "id": 67890,
                "name": "Author",
                "account": "author"
            },
            "tags": tags.iter().map(|name| json!({ "name": name, "translated_name": null })).collect::<Vec<_>>(),
            "create_date": "2026-01-01T00:00:00+00:00",
            "page_count": 1,
            "width": 100,
            "height": 100,
            "sanity_level": 2,
            "x_restrict": 0,
            "series": null,
            "meta_single_page": { "original_image_url": "original" },
            "meta_pages": [],
            "total_view": 1,
            "total_bookmarks": 2,
            "is_bookmarked": false,
            "visible": true,
            "is_muted": false,
            "total_comments": 0
        }))
        .unwrap()
    }

    #[test]
    fn author_subscription_state_extracts_only_author_state() {
        let author = AuthorState {
            latest_illust_id: 42,
            pending_illust: None,
        };
        let subscription = make_subscription(
            Some(SubscriptionState::Author(author.clone())),
            TagFilter::default(),
        );

        assert_eq!(author_subscription_state(&subscription), Some(author));
        assert_eq!(
            ranking_subscription_state(&subscription),
            None,
            "author state must not be exposed as ranking state"
        );
    }

    #[test]
    fn ranking_subscription_state_extracts_only_ranking_state() {
        let ranking = RankingState {
            pushed_ids: vec![1, 2, 3],
            pending_illust: None,
        };
        let subscription = make_subscription(
            Some(SubscriptionState::Ranking(ranking.clone())),
            TagFilter::default(),
        );

        assert_eq!(ranking_subscription_state(&subscription), Some(ranking));
        assert_eq!(
            author_subscription_state(&subscription),
            None,
            "ranking state must not be exposed as author state"
        );
    }

    #[test]
    fn apply_subscription_tag_filter_merges_subscription_and_chat_rules() {
        let subscription = make_subscription(None, TagFilter::parse_from_args(&["+cat"]));
        let chat = make_chat(&["R-18"]);
        let keep = make_illust(1, &["cat"]);
        let drop_by_chat = make_illust(2, &["cat", "R-18"]);
        let drop_by_subscription = make_illust(3, &["dog"]);

        let filtered = apply_subscription_tag_filter(
            &subscription,
            &chat,
            [&keep, &drop_by_chat, &drop_by_subscription],
        );

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, keep.id);
    }

    #[test]
    fn inter_subscription_delay_constant_stays_two_seconds() {
        assert_eq!(INTER_SUBSCRIPTION_DELAY_MS, 2000);
    }
}
