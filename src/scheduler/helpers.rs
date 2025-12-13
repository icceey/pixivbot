use crate::bot::notifier::{BatchSendResult, Notifier};
use crate::db::repo::Repo;
use crate::db::types::RankingState;
use crate::utils::{sensitive, tag};
use anyhow::{Context, Result};
use pixiv_client::Illust;
use teloxide::prelude::*;
use teloxide::utils::markdown;
use tracing::info;

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
    image_size: pixiv_client::ImageSize,
) -> Result<PushResult> {
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
