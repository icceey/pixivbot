use crate::bot::notifier::{BatchSendResult, Notifier};
use crate::db::repo::Repo;
use crate::db::types::{RankingState, SubscriptionState, TagFilter};
use crate::utils::{sensitive, tag};
use anyhow::{Context, Result};
use pixiv_client::Illust;
use teloxide::prelude::*;
use teloxide::utils::markdown;
use tracing::{error, info};

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
    ctx: &AuthorContext<'_>,
    illust: &Illust,
    already_sent_pages: &[usize],
) -> Result<PushResult> {
    let chat_id = ChatId(ctx.subscription.chat_id);
    let all_urls = illust.get_all_image_urls();
    let total_pages = all_urls.len();

    // Calculate pages to send
    let pages_to_send: Vec<usize> = (0..total_pages)
        .filter(|i| !already_sent_pages.contains(i))
        .collect();

    if pages_to_send.is_empty() {
        return Ok(PushResult::Success {
            illust_id: illust.id,
        });
    }

    let urls_to_send: Vec<String> = pages_to_send
        .iter()
        .filter_map(|&i| all_urls.get(i).cloned())
        .collect();

    // Prepare caption
    let caption = if already_sent_pages.is_empty() {
        // First time sending this illust
        let page_info = if illust.is_multi_page() {
            format!(" \\({} photos\\)", illust.page_count)
        } else {
            String::new()
        };
        let tags = tag::format_tags_escaped(illust);
        format!(
            "🎨 {}{}\nby *{}* \\(ID: `{}`\\)\n\n👀 {} \\| ❤️ {} \\| 🔗 [来源](https://pixiv\\.net/artworks/{}){}", 
            markdown::escape(&illust.title),
            page_info,
            markdown::escape(&illust.user.name),
            illust.user.id,
            illust.total_view,
            illust.total_bookmarks,
            illust.id,
            tags
        )
    } else {
        // Continuing from previous attempt - calculate batch numbers like normal send
        // Normal batch size is 10 (MAX_PER_GROUP in notifier)
        const MAX_PER_GROUP: usize = 10;
        let total_batches = total_pages.div_ceil(MAX_PER_GROUP);
        let current_batch = (already_sent_pages.len() / MAX_PER_GROUP) + 1;
        let tags = tag::format_tags_escaped(illust);

        format!(
            "🎨 {} \\(continued {}/{}\\)\nby *{}*\n\n🔗 [来源](https://pixiv\\.net/artworks/{}){}",
            markdown::escape(&illust.title),
            current_batch,
            total_batches,
            markdown::escape(&illust.user.name),
            illust.id,
            tags
        )
    };

    // Check spoiler setting
    let sensitive_tags = sensitive::get_chat_sensitive_tags(&ctx.chat);
    let has_spoiler =
        ctx.chat.blur_sensitive_tags && sensitive::contains_sensitive_tags(illust, sensitive_tags);

    // Send images
    let send_result = notifier
        .notify_with_images(chat_id, &urls_to_send, Some(&caption), has_spoiler)
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
    if send_result.is_complete_success() {
        // All attempted pages succeeded
        let mut all_sent = already_sent.to_vec();
        all_sent.extend(attempted_pages);
        all_sent.sort();
        all_sent.dedup();

        if all_sent.len() == total_pages {
            PushResult::Success { illust_id }
        } else {
            // Should not happen, but handle gracefully
            PushResult::Partial {
                illust_id,
                sent_pages: all_sent,
                total_pages,
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
        }
    }
}

/// Helper: Trim pushed_ids to last 100 and update state
pub async fn trim_and_update_pushed_ids(
    repo: &Repo,
    subscription_id: i32,
    mut pushed_ids: Vec<u64>,
) -> Result<()> {
    // Keep only the last 100 IDs to prevent unbounded growth
    if pushed_ids.len() > 100 {
        let skip_count = pushed_ids.len() - 100;
        pushed_ids = pushed_ids.into_iter().skip(skip_count).collect();
    }

    let new_state = RankingState {
        pushed_ids,
        pending_illust: None,
    };

    update_ranking_state(repo, subscription_id, new_state).await
}

/// Update ranking subscription state in database
async fn update_ranking_state(
    repo: &Repo,
    subscription_id: i32,
    state: RankingState,
) -> Result<()> {
    repo.update_subscription_latest_data(subscription_id, Some(SubscriptionState::Ranking(state)))
        .await?;
    Ok(())
}

/// Helper: Mark illusts as pushed (when filtered out but should be marked as processed)
pub async fn mark_ranking_illusts_as_pushed(
    repo: &Repo,
    subscription_id: i32,
    mut pushed_ids: Vec<u64>,
    new_ids: Vec<u64>,
) -> Result<()> {
    pushed_ids.extend(new_ids);
    trim_and_update_pushed_ids(repo, subscription_id, pushed_ids).await
}

/// Dispatcher: Process single ranking subscription
pub async fn process_single_ranking_sub(
    repo: &Repo,
    notifier: &Notifier,
    ctx: &RankingContext<'_>,
    illusts: &[Illust],
    mode: &str,
) -> Result<()> {
    let chat_id = ChatId(ctx.subscription.chat_id);

    // Get previously pushed IDs
    let pushed_ids = ctx
        .subscription_state
        .as_ref()
        .map(|s| s.pushed_ids.clone())
        .unwrap_or_default();

    // Find new illusts (not already pushed)
    let new_illusts: Vec<_> = illusts
        .iter()
        .filter(|i| !pushed_ids.contains(&i.id))
        .collect();

    if new_illusts.is_empty() {
        return Ok(());
    }

    info!(
        "Found {} new ranking illusts for subscription {} (chat {}): {:?}",
        new_illusts.len(),
        ctx.subscription.id,
        chat_id,
        new_illusts.iter().map(|i| i.id).collect::<Vec<_>>()
    );

    // Apply tag filters
    let chat_filter = TagFilter::from_excluded_tags(&ctx.chat.excluded_tags);
    let combined_filter = ctx.subscription.filter_tags.merged(&chat_filter);
    let filtered_illusts: Vec<&Illust> = combined_filter.filter(new_illusts.iter().copied());

    // Collect all new IDs for tracking
    let all_new_ids: Vec<u64> = new_illusts.iter().map(|i| i.id).collect();

    // If all filtered out, mark as processed and return
    if filtered_illusts.is_empty() {
        info!("No illusts to send to chat {} after filtering", chat_id);
        mark_ranking_illusts_as_pushed(repo, ctx.subscription.id, pushed_ids, all_new_ids).await?;
        return Ok(());
    }

    // *** Process ALL filtered ranking illusts in batch ***
    info!(
        "Sending {} ranking illusts to chat {}",
        filtered_illusts.len(),
        chat_id
    );

    // Build title for the batch
    let title = format!(
        "📊 *{} Ranking* \\- {} new\\!\n\n",
        markdown::escape(&mode.replace('_', " ").to_uppercase()),
        filtered_illusts.len()
    );

    // Collect all illusts data for batch sending
    let mut image_urls = Vec::new();
    let mut captions = Vec::new();
    let mut illust_ids = Vec::new();

    for (index, illust) in filtered_illusts.iter().enumerate() {
        // Get image URL (single image per ranking item)
        let image_url = if let Some(url) = &illust.meta_single_page.original_image_url {
            url.clone()
        } else {
            illust.image_urls.large.clone()
        };
        image_urls.push(image_url);
        illust_ids.push(illust.id);

        // Build caption
        let tags = tag::format_tags_escaped(illust);
        let base_caption = format!(
            "{}\nby *{}* \\(ID: `{}`\\)\n\n❤️ {} \\| 🔗 [来源](https://pixiv\\.net/artworks/{}){}",
            markdown::escape(&illust.title),
            markdown::escape(&illust.user.name),
            illust.user.id,
            illust.total_bookmarks,
            illust.id,
            tags
        );

        // Prepend title to first caption
        let caption = if index == 0 {
            format!("{}{}", title, base_caption)
        } else {
            base_caption
        };
        captions.push(caption);
    }

    // Check spoiler setting
    let sensitive_tags = sensitive::get_chat_sensitive_tags(&ctx.chat);
    let has_spoiler = ctx.chat.blur_sensitive_tags
        && filtered_illusts
            .iter()
            .any(|illust| sensitive::contains_sensitive_tags(illust, sensitive_tags));

    // Send as media group with individual captions
    let send_result = notifier
        .notify_with_individual_captions(chat_id, &image_urls, &captions, has_spoiler)
        .await;

    // Collect successfully sent illust IDs
    let successfully_sent_ids: Vec<u64> = send_result
        .succeeded_indices
        .iter()
        .filter_map(|&idx| illust_ids.get(idx).copied())
        .collect();

    if send_result.is_complete_failure() {
        error!(
            "❌ Failed to send ranking to chat {}, will retry next poll",
            chat_id
        );
        // Don't update pushed_ids, retry next tick
        return Ok(());
    }

    // Update pushed_ids with successfully sent illusts
    let mut new_pushed_ids = pushed_ids.clone();
    new_pushed_ids.extend(successfully_sent_ids);
    trim_and_update_pushed_ids(repo, ctx.subscription.id, new_pushed_ids).await?;

    if send_result.is_complete_success() {
        info!(
            "✅ Successfully sent {} ranking illusts to chat {}",
            filtered_illusts.len(),
            chat_id
        );
    } else {
        info!(
            "⚠️  Partially sent ranking to chat {} ({}/{} illusts)",
            chat_id,
            send_result.succeeded_indices.len(),
            filtered_illusts.len()
        );
    }

    Ok(())
}
