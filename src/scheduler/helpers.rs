use crate::bot::notifier::{
    BatchSendResult, ContinuationNumbering, DownloadButtonConfig, Notifier,
};
use crate::db::entities::{chats, subscriptions};
use crate::db::repo::Repo;
use crate::db::types::{
    AuthorState, BooruRankingState, BooruTagState, EhTagState, RankingState, SubscriptionState,
    TagFilter,
};
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
    Success { illust_id: u64 },
    /// Some pages failed, need to retry
    Partial {
        illust_id: u64,
        sent_pages: Vec<usize>,
        total_pages: usize,
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

pub fn booru_ranking_subscription_state(
    subscription: &subscriptions::Model,
) -> Option<BooruRankingState> {
    match &subscription.latest_data {
        Some(SubscriptionState::BooruRanking(state)) => Some(state.clone()),
        _ => None,
    }
}

pub fn eh_tag_subscription_state(subscription: &subscriptions::Model) -> Option<EhTagState> {
    match &subscription.latest_data {
        Some(SubscriptionState::EhTag(state)) => Some(state.clone()),
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

/// Shared author/tag sender. Rankings intentionally use their own send path.
pub async fn process_illust_push(
    repo: &Repo,
    notifier: &Notifier,
    pixiv: &Arc<RwLock<PixivClient>>,
    chat: &chats::Model,
    illust: &Illust,
    subscription_id: i32,
    image_size: pixiv_client::ImageSize,
) -> Result<PushResult> {
    let _guard = repo.lock_pixiv_push(chat.id).await;
    let progress = repo.pixiv_push_progress(chat.id, illust.id).await?;
    if progress.done {
        info!(
            "Skipping previously pushed illust {} for chat {}",
            illust.id, chat.id
        );
        return Ok(PushResult::Success {
            illust_id: illust.id,
        });
    }
    let mut sent_pages = progress.sent_pages;
    sent_pages.sort_unstable();
    sent_pages.dedup();

    // For ugoira works, delegate to the specialized handler
    if illust.is_ugoira() {
        let result = process_ugoira_push(notifier, pixiv, chat, illust).await?;
        if result.is_complete_failure() {
            return Ok(PushResult::Failure {
                illust_id: illust.id,
            });
        }
        let message_id = result
            .first_message_id
            .context("Missing acknowledged animation message ID")?;
        repo.save_pixiv_push_progress(
            chat.id,
            illust.id,
            &[],
            true,
            Some((subscription_id, message_id)),
        )
        .await?;
        return Ok(PushResult::Success {
            illust_id: illust.id,
        });
    }

    let chat_id = ChatId(chat.id);
    let all_urls = illust.get_all_image_urls_with_size(image_size);
    let total_pages = all_urls.len();
    anyhow::ensure!(total_pages > 0, "Illustration has no pages");

    let has_spoiler = sensitive::should_blur(chat, illust);
    let download_config = DownloadButtonConfig::for_pixiv_chat(illust.id, chat);
    // Send one original page group at a time and commit its acknowledgement
    // before starting another. Keep the chat lock for the entire work.
    for batch_start in (0..total_pages).step_by(caption::MAX_PER_GROUP) {
        let batch_end = (batch_start + caption::MAX_PER_GROUP).min(total_pages);
        let attempted: Vec<_> = (batch_start..batch_end)
            .filter(|page| !sent_pages.contains(page))
            .collect();
        if attempted.is_empty() {
            continue;
        }
        let urls: Vec<_> = attempted
            .iter()
            .map(|&page| all_urls[page].clone())
            .collect();
        let caption = if sent_pages.is_empty() {
            caption::build_illust_caption(illust)
        } else {
            caption::build_continuation_caption(illust, sent_pages.len(), total_pages)
        };
        let result = notifier
            .notify_with_images_and_button_and_continuation(
                chat_id,
                &urls,
                Some(&caption),
                has_spoiler,
                &download_config,
                ContinuationNumbering::new(
                    batch_start / caption::MAX_PER_GROUP + 1,
                    total_pages.div_ceil(caption::MAX_PER_GROUP),
                ),
            )
            .await;
        if !result.succeeded_indices.is_empty() {
            for &index in &result.succeeded_indices {
                sent_pages.push(
                    *attempted
                        .get(index)
                        .context("Invalid acknowledged page index")?,
                );
            }
            sent_pages.sort_unstable();
            sent_pages.dedup();
            let message_id = result
                .first_message_id
                .context("Missing acknowledged batch message ID")?;
            let done = (0..total_pages).all(|page| sent_pages.contains(&page));
            repo.save_pixiv_push_progress(
                chat.id,
                illust.id,
                &sent_pages,
                done,
                Some((subscription_id, message_id)),
            )
            .await?;
            if done {
                return Ok(PushResult::Success {
                    illust_id: illust.id,
                });
            }
        }
        if !result.is_complete_success() {
            return Ok(if sent_pages.is_empty() {
                PushResult::Failure {
                    illust_id: illust.id,
                }
            } else {
                PushResult::Partial {
                    illust_id: illust.id,
                    sent_pages,
                    total_pages,
                }
            });
        }
    }
    // Imported author state may already contain every page.
    repo.save_pixiv_push_progress(chat.id, illust.id, &[], true, None)
        .await?;
    Ok(PushResult::Success {
        illust_id: illust.id,
    })
}

/// Push a ugoira (animated) illust as an MP4 animation
async fn process_ugoira_push(
    notifier: &Notifier,
    pixiv: &Arc<RwLock<PixivClient>>,
    chat: &chats::Model,
    illust: &Illust,
) -> Result<BatchSendResult> {
    let chat_id = ChatId(chat.id);

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
    let has_spoiler = sensitive::should_blur(chat, illust);

    // Build download button config
    let download_config = DownloadButtonConfig::for_pixiv_chat(illust.id, chat);

    // Send ugoira as MP4 animation
    Ok(notifier
        .notify_ugoira(
            chat_id,
            &metadata.zip_urls.medium,
            metadata.frames,
            Some(&caption),
            has_spoiler,
            &download_config,
        )
        .await)
}
