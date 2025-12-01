use crate::bot::notifier::Notifier;
use crate::db::repo::Repo;
use crate::pixiv::client::PixivClient;
use crate::pixiv::downloader::Downloader;
use crate::utils::{html, markdown};
use chrono::Local;
use rand::Rng;
use serde_json::json;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::Bot;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

pub struct SchedulerEngine {
    repo: Arc<Repo>,
    pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
    notifier: Notifier,
    #[allow(dead_code)]
    downloader: Arc<Downloader>,
    tick_interval_sec: u64,
    min_task_interval_sec: u64,
    max_task_interval_sec: u64,
    sensitive_tags: Vec<String>,
}

impl SchedulerEngine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: Arc<Repo>,
        pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
        bot: Bot,
        downloader: Arc<Downloader>,
        tick_interval_sec: u64,
        min_task_interval_sec: u64,
        max_task_interval_sec: u64,
        sensitive_tags: Vec<String>,
    ) -> Self {
        Self {
            repo,
            pixiv_client,
            notifier: Notifier::new(bot, downloader.clone()),
            downloader,
            tick_interval_sec,
            min_task_interval_sec,
            max_task_interval_sec,
            sensitive_tags,
        }
    }

    /// Main scheduler loop - runs indefinitely
    pub async fn run(&self) {
        info!("🚀 Scheduler engine started");

        loop {
            // Wait for tick interval before checking for tasks
            sleep(Duration::from_secs(self.tick_interval_sec)).await;

            if let Err(e) = self.tick().await {
                error!("Scheduler tick error: {}", e);
            }
        }
    }

    /// Single tick - fetch and execute one pending task
    async fn tick(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Get one pending task
        let tasks = self.repo.get_pending_tasks(1).await?;

        if tasks.is_empty() {
            return Ok(());
        }

        let task = &tasks[0];
        info!(
            "⚙️  Executing task [{}] {} {}",
            task.id, task.r#type, task.value
        );

        // Execute based on task type
        let result = match task.r#type.as_str() {
            "author" => self.execute_author_task(task).await,
            "ranking" => self.execute_ranking_task(task).await,
            _ => {
                warn!("Unknown task type: {}", task.r#type);
                Ok(())
            }
        };

        // Note: task's next_poll_at is updated inside execute_*_task methods
        // We only log errors here, no need to update task again
        if let Err(e) = result {
            error!("Task execution failed: {}", e);

            // On error, still update the poll time to avoid immediate retry
            let random_interval_sec =
                rand::rng().random_range(self.min_task_interval_sec..=self.max_task_interval_sec);
            let next_poll = Local::now() + chrono::Duration::seconds(random_interval_sec as i64);

            self.repo.update_task_after_poll(task.id, next_poll).await?;
        }

        Ok(())
    }

    /// Execute author subscription task
    async fn execute_author_task(
        &self,
        task: &crate::db::entities::tasks::Model,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let author_id: u64 = task.value.parse()?;

        // Get latest illusts
        let pixiv = self.pixiv_client.read().await;
        let illusts = pixiv.get_user_illusts(author_id, 10).await?;
        drop(pixiv);

        if illusts.is_empty() {
            info!("No illusts found for author {}", author_id);
            let random_interval_sec =
                rand::rng().random_range(self.min_task_interval_sec..=self.max_task_interval_sec);
            let next_poll = Local::now() + chrono::Duration::seconds(random_interval_sec as i64);
            self.repo.update_task_after_poll(task.id, next_poll).await?;
            return Ok(());
        }

        // Get all subscriptions for this task
        let subscriptions = self.repo.list_subscriptions_by_task(task.id).await?;

        if subscriptions.is_empty() {
            info!("No subscriptions for author task {}", task.id);
            let random_interval_sec =
                rand::rng().random_range(self.min_task_interval_sec..=self.max_task_interval_sec);
            let next_poll = Local::now() + chrono::Duration::seconds(random_interval_sec as i64);
            self.repo.update_task_after_poll(task.id, next_poll).await?;
            return Ok(());
        }

        // Process each subscription with its own push state
        for subscription in subscriptions {
            let chat_id = ChatId(subscription.chat_id);

            // Get chat settings
            let chat = match self.repo.get_chat(subscription.chat_id).await {
                Ok(Some(chat)) => chat,
                Ok(None) => {
                    info!("Chat {} not found, skipping", chat_id);
                    continue;
                }
                Err(e) => {
                    error!("Failed to get chat {}: {}", chat_id, e);
                    continue;
                }
            };

            // Check if chat is enabled or if it's an admin/owner private chat
            let should_notify = if chat.enabled {
                true
            } else {
                match self.repo.get_user(subscription.chat_id).await {
                    Ok(Some(user)) if user.role.is_admin() => true,
                    _ => {
                        info!("Skipping notification to disabled chat {}", chat_id);
                        false
                    }
                }
            };

            if !should_notify {
                continue;
            }

            // Get this subscription's push state
            let last_illust_id = subscription
                .latest_data
                .as_ref()
                .and_then(|data| data.get("latest_illust_id"))
                .and_then(|v| v.as_u64());

            // Check for pending illust (partially sent)
            let pending_illust: Option<(u64, Vec<usize>, usize)> = subscription
                .latest_data
                .as_ref()
                .and_then(|data| data.get("pending_illust"))
                .and_then(|p| {
                    let id = p.get("id")?.as_u64()?;
                    let sent_pages: Vec<usize> = p
                        .get("sent_pages")?
                        .as_array()?
                        .iter()
                        .filter_map(|v| v.as_u64().map(|n| n as usize))
                        .collect();
                    let total_pages = p.get("total_pages")?.as_u64()? as usize;
                    Some((id, sent_pages, total_pages))
                });

            // 保存 pending id 用于后续过滤
            let pending_id_to_skip = pending_illust.as_ref().map(|(id, _, _)| *id);

            // Find new illusts for this subscription
            let new_illusts: Vec<_> = if let Some(last_id) = last_illust_id {
                illusts
                    .iter()
                    .take_while(|illust| illust.id != last_id)
                    .collect()
            } else {
                // First run for this subscription - only send the latest one
                illusts.iter().take(1).collect()
            };

            if new_illusts.is_empty() && pending_illust.is_none() {
                continue;
            }

            info!(
                "Found {} new illusts for subscription {} (chat {}), pending: {:?}",
                new_illusts.len(),
                subscription.id,
                chat_id,
                pending_illust.as_ref().map(|(id, _, _)| id)
            );

            // 记录最新的 illust id（用于在过滤后没有内容时也更新）
            let newest_illust_id = new_illusts.first().map(|i| i.id);

            // Apply subscription tag filters
            let mut filtered_illusts =
                self.apply_tag_filters_ref(&new_illusts, &subscription.filter_tags);

            // Apply chat-level excluded tags
            filtered_illusts =
                self.apply_chat_excluded_tags_ref(filtered_illusts, &chat.excluded_tags);

            // 如果过滤后没有内容且没有 pending，更新 latest_illust_id
            if filtered_illusts.is_empty() && pending_illust.is_none() {
                if let Some(newest_id) = newest_illust_id {
                    let updated_data = json!({
                        "latest_illust_id": newest_id,
                        "last_check": Local::now().to_rfc3339(),
                    });
                    if let Err(e) = self
                        .repo
                        .update_subscription_latest_data(subscription.id, Some(updated_data))
                        .await
                    {
                        error!(
                            "Failed to update subscription {} latest_data: {}",
                            subscription.id, e
                        );
                    }
                }
                continue;
            }

            // 首先处理 pending illust（如果有）
            if let Some((pending_id, sent_pages, total_pages)) = pending_illust {
                // 找到对应的 illust
                if let Some(illust) = illusts.iter().find(|i| i.id == pending_id) {
                    info!(
                        "Resuming pending illust {} ({}/{} pages sent)",
                        pending_id,
                        sent_pages.len(),
                        total_pages
                    );

                    let has_spoiler = chat.blur_sensitive_tags && self.has_sensitive_tags(illust);

                    // 获取所有图片 URL
                    let all_urls = illust.get_all_image_urls();

                    // 只发送尚未成功的页
                    let pending_pages: Vec<usize> = (0..all_urls.len())
                        .filter(|i| !sent_pages.contains(i))
                        .collect();

                    if pending_pages.is_empty() {
                        // 所有页都已发送，标记为完成
                        let updated_data = json!({
                            "latest_illust_id": pending_id,
                            "last_check": Local::now().to_rfc3339(),
                        });
                        if let Err(e) = self
                            .repo
                            .update_subscription_latest_data(subscription.id, Some(updated_data))
                            .await
                        {
                            error!(
                                "Failed to update subscription {} latest_data: {}",
                                subscription.id, e
                            );
                        }
                    } else {
                        // 发送剩余页
                        let pending_urls: Vec<String> = pending_pages
                            .iter()
                            .filter_map(|&i| all_urls.get(i).cloned())
                            .collect();

                        let caption = format!(
                            "🎨 {} \\(continued, {}/{} remaining\\)\nby *{}*\n\n🔗 [来源](https://pixiv\\.net/artworks/{})",
                            markdown::escape(&illust.title),
                            pending_urls.len(),
                            total_pages,
                            markdown::escape(&illust.user.name),
                            illust.id
                        );

                        let send_result = self
                            .notifier
                            .notify_with_images(chat_id, &pending_urls, Some(&caption), has_spoiler)
                            .await;

                        // 合并已发送的页索引
                        let mut all_sent: Vec<usize> = sent_pages.clone();
                        for &idx in &send_result.succeeded_indices {
                            if let Some(&page_idx) = pending_pages.get(idx) {
                                all_sent.push(page_idx);
                            }
                        }
                        all_sent.sort();
                        all_sent.dedup();

                        if all_sent.len() == total_pages {
                            // 全部完成
                            let updated_data = json!({
                                "latest_illust_id": pending_id,
                                "last_check": Local::now().to_rfc3339(),
                            });
                            if let Err(e) = self
                                .repo
                                .update_subscription_latest_data(
                                    subscription.id,
                                    Some(updated_data),
                                )
                                .await
                            {
                                error!(
                                    "Failed to update subscription {} latest_data: {}",
                                    subscription.id, e
                                );
                            }
                        } else {
                            // 仍有失败，更新 pending 状态
                            let updated_data = json!({
                                "latest_illust_id": last_illust_id,
                                "pending_illust": {
                                    "id": pending_id,
                                    "sent_pages": all_sent,
                                    "total_pages": total_pages,
                                },
                                "last_check": Local::now().to_rfc3339(),
                            });
                            if let Err(e) = self
                                .repo
                                .update_subscription_latest_data(
                                    subscription.id,
                                    Some(updated_data),
                                )
                                .await
                            {
                                error!(
                                    "Failed to update subscription {} latest_data: {}",
                                    subscription.id, e
                                );
                            }
                            // 有 pending 未完成，暂停处理新 illusts
                            continue;
                        }

                        sleep(Duration::from_millis(2000)).await;
                    }
                } else {
                    // pending illust 不在当前 API 返回中（可能太旧了）
                    // 放弃这个 pending，清除状态，让程序继续处理新的 illusts
                    warn!(
                        "Pending illust {} not found in current API response, abandoning",
                        pending_id
                    );
                    // 保留 last_illust_id，清除 pending
                    if let Some(last_id) = last_illust_id {
                        let updated_data = json!({
                            "latest_illust_id": last_id,
                            "last_check": Local::now().to_rfc3339(),
                        });
                        if let Err(e) = self
                            .repo
                            .update_subscription_latest_data(subscription.id, Some(updated_data))
                            .await
                        {
                            error!(
                                "Failed to update subscription {} latest_data: {}",
                                subscription.id, e
                            );
                        }
                    }
                }
            }

            // 过滤掉已经处理过的 pending illust（如果有的话）
            let filtered_illusts: Vec<_> = filtered_illusts
                .into_iter()
                .filter(|i| Some(i.id) != pending_id_to_skip)
                .collect();

            // 处理新的 illusts
            for illust in filtered_illusts {
                let page_info = if illust.is_multi_page() {
                    format!(" \\({} photos\\)", illust.page_count)
                } else {
                    String::new()
                };

                // Check if this illust has sensitive tags for spoiler
                let has_spoiler = chat.blur_sensitive_tags && self.has_sensitive_tags(illust);

                let tags = self.format_tags(illust);

                let caption = format!(
                    "🎨 {}{}\nby *{}* \\(ID: `{}`\\)\n\n👀 {} \\| ❤️ {} \\| 🔗 [来源](https://pixiv\\.net/artworks/{}){}", 
                    markdown::escape(&illust.title),
                    page_info,
                    markdown::escape(&illust.user.name),
                    illust.user.id,
                    illust.total_view,
                    illust.total_bookmarks,
                    illust.id,
                    tags
                );

                // 获取所有图片URL (支持单图和多图)
                let image_urls = illust.get_all_image_urls();
                let total_pages = image_urls.len();

                let send_result = self
                    .notifier
                    .notify_with_images(chat_id, &image_urls, Some(&caption), has_spoiler)
                    .await;

                if send_result.is_complete_success() {
                    // 发送成功，更新 subscription 的 latest_data
                    let updated_data = json!({
                        "latest_illust_id": illust.id,
                        "last_check": Local::now().to_rfc3339(),
                    });

                    if let Err(e) = self
                        .repo
                        .update_subscription_latest_data(subscription.id, Some(updated_data))
                        .await
                    {
                        error!(
                            "Failed to update subscription {} latest_data: {}",
                            subscription.id, e
                        );
                    }
                } else if send_result.is_complete_failure() {
                    error!(
                        "Failed to send illust {} to chat {}, will retry next poll",
                        illust.id, chat_id
                    );
                    // 完全失败，不更新 latest_data，下次会重试
                    break; // 停止处理这个 subscription 的后续 illusts
                } else {
                    // 部分成功，记录 pending 状态
                    warn!(
                        "Partially sent illust {} to chat {} ({}/{} pages)",
                        illust.id,
                        chat_id,
                        send_result.succeeded_indices.len(),
                        total_pages
                    );

                    let updated_data = json!({
                        "latest_illust_id": last_illust_id,
                        "pending_illust": {
                            "id": illust.id,
                            "sent_pages": send_result.succeeded_indices,
                            "total_pages": total_pages,
                        },
                        "last_check": Local::now().to_rfc3339(),
                    });

                    if let Err(e) = self
                        .repo
                        .update_subscription_latest_data(subscription.id, Some(updated_data))
                        .await
                    {
                        error!(
                            "Failed to update subscription {} latest_data: {}",
                            subscription.id, e
                        );
                    }
                    // 有 pending，停止处理后续 illusts
                    break;
                }

                // Small delay between messages
                sleep(Duration::from_millis(2000)).await;
            }
        }

        // Update task's next poll time
        let random_interval_sec =
            rand::rng().random_range(self.min_task_interval_sec..=self.max_task_interval_sec);
        let next_poll = Local::now() + chrono::Duration::seconds(random_interval_sec as i64);
        self.repo.update_task_after_poll(task.id, next_poll).await?;

        Ok(())
    }

    /// Execute ranking subscription task
    async fn execute_ranking_task(
        &self,
        task: &crate::db::entities::tasks::Model,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mode = &task.value;

        // Get ranking
        let pixiv = self.pixiv_client.read().await;
        let illusts = pixiv.get_ranking(mode, None, 10).await?;
        drop(pixiv);

        if illusts.is_empty() {
            info!("No ranking illusts found for mode {}", mode);
            // Update task poll time
            self.repo
                .update_task_after_poll(task.id, Local::now() + chrono::Duration::seconds(86400))
                .await?;
            return Ok(());
        }

        info!("Found {} ranking illusts for mode {}", illusts.len(), mode);

        // Get all subscriptions
        let subscriptions = self.repo.list_subscriptions_by_task(task.id).await?;

        if subscriptions.is_empty() {
            info!("No subscriptions for ranking task {}", task.id);
            self.repo
                .update_task_after_poll(task.id, Local::now() + chrono::Duration::seconds(86400))
                .await?;
            return Ok(());
        }

        // Process each subscription with its own push state
        for subscription in subscriptions {
            let chat_id = ChatId(subscription.chat_id);

            // Get chat settings
            let chat = match self.repo.get_chat(subscription.chat_id).await {
                Ok(Some(chat)) => chat,
                Ok(None) => {
                    info!("Chat {} not found, skipping", chat_id);
                    continue;
                }
                Err(e) => {
                    error!("Failed to get chat {}: {}", chat_id, e);
                    continue;
                }
            };

            // Check if chat is enabled or if it's an admin/owner private chat
            let should_notify = if chat.enabled {
                true
            } else {
                match self.repo.get_user(subscription.chat_id).await {
                    Ok(Some(user)) if user.role.is_admin() => true,
                    _ => {
                        info!("Skipping notification to disabled chat {}", chat_id);
                        false
                    }
                }
            };

            if !should_notify {
                continue;
            }

            // Get this subscription's previously pushed illust IDs
            let pushed_ids: Vec<u64> = subscription
                .latest_data
                .as_ref()
                .and_then(|data| data.get("pushed_ids"))
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_u64()).collect())
                .unwrap_or_default();

            // Filter out illusts that have already been pushed to this subscription
            let new_illusts: Vec<&crate::pixiv_client::Illust> = illusts
                .iter()
                .filter(|illust| !pushed_ids.contains(&illust.id))
                .collect();

            if new_illusts.is_empty() {
                info!(
                    "No new ranking illusts for subscription {} (chat {})",
                    subscription.id, chat_id
                );
                continue;
            }

            info!(
                "Found {} new ranking illusts for subscription {} (chat {})",
                new_illusts.len(),
                subscription.id,
                chat_id
            );

            // Collect new illust IDs (will be used to track what was successfully sent)
            let new_ids: Vec<u64> = new_illusts.iter().map(|i| i.id).collect();

            // Apply subscription-level tag filters (include/exclude from filter_tags)
            let filtered_illusts: Vec<&crate::pixiv_client::Illust> =
                self.apply_tag_filters_ref(&new_illusts, &subscription.filter_tags);

            // Apply chat-level excluded tags filter
            let filtered_illusts: Vec<&crate::pixiv_client::Illust> =
                self.apply_chat_excluded_tags(filtered_illusts, &chat.excluded_tags);

            if filtered_illusts.is_empty() {
                info!("No illusts to send to chat {} after filtering", chat_id);
                // 即使过滤后没有要发送的，也更新 pushed_ids（因为这些已被处理）
                let mut all_pushed_ids = pushed_ids.clone();
                all_pushed_ids.extend(new_ids);
                if all_pushed_ids.len() > 100 {
                    let skip_count = all_pushed_ids.len() - 100;
                    all_pushed_ids = all_pushed_ids.into_iter().skip(skip_count).collect();
                }
                let updated_data = json!({
                    "pushed_ids": all_pushed_ids,
                    "last_check": Local::now().to_rfc3339(),
                });
                if let Err(e) = self
                    .repo
                    .update_subscription_latest_data(subscription.id, Some(updated_data))
                    .await
                {
                    error!(
                        "Failed to update subscription {} latest_data: {}",
                        subscription.id, e
                    );
                }
                continue;
            }

            // Build title to prepend to first image caption
            let title = format!(
                "📊 *{} Ranking* \\- {} new\\!\n\n",
                markdown::escape(&mode.replace('_', " ").to_uppercase()),
                filtered_illusts.len()
            );

            // Check if any illust has sensitive tags for spoiler
            let has_spoiler = chat.blur_sensitive_tags
                && filtered_illusts
                    .iter()
                    .any(|illust| self.has_sensitive_tags(illust));

            // Prepare image URLs, captions, and corresponding illust IDs
            let mut image_urls: Vec<String> = Vec::new();
            let mut captions: Vec<String> = Vec::new();
            let mut illust_ids: Vec<u64> = Vec::new();

            for (index, illust) in filtered_illusts.iter().enumerate() {
                // Get image URL
                let image_url =
                    if let Some(original_url) = &illust.meta_single_page.original_image_url {
                        original_url.clone()
                    } else {
                        illust.image_urls.large.clone()
                    };
                image_urls.push(image_url);
                illust_ids.push(illust.id);

                // Build caption for this image
                let tags = self.format_tags(illust);

                let base_caption = format!(
                    "{}\\.  {}\nby *{}* \\(ID: `{}`\\)\n\n❤️ {} \\| 🔗 [来源](https://pixiv\\.net/artworks/{}){}",
                    index + 1,
                    markdown::escape(&illust.title),
                    markdown::escape(&illust.user.name),
                    illust.user.id,
                    illust.total_bookmarks,
                    illust.id,
                    tags
                );

                // Prepend title to first image caption
                let caption = if index == 0 {
                    format!("{}{}", title, base_caption)
                } else {
                    base_caption
                };
                captions.push(caption);
            }

            // Send as media group with individual captions
            let send_result = self
                .notifier
                .notify_with_individual_captions(chat_id, &image_urls, &captions, has_spoiler)
                .await;

            // 根据发送结果更新 pushed_ids
            let successfully_sent_ids: Vec<u64> = send_result
                .succeeded_indices
                .iter()
                .filter_map(|&idx| illust_ids.get(idx).copied())
                .collect();

            if send_result.is_complete_failure() {
                error!(
                    "Failed to send ranking to chat {}, will retry next poll",
                    chat_id
                );
                // 完全失败，不更新 pushed_ids，下次会重试
                continue;
            }

            // 更新 pushed_ids（只添加成功发送的）
            let mut all_pushed_ids = pushed_ids.clone();
            all_pushed_ids.extend(successfully_sent_ids);

            // Keep only the last 100 IDs to prevent unbounded growth
            if all_pushed_ids.len() > 100 {
                let skip_count = all_pushed_ids.len() - 100;
                all_pushed_ids = all_pushed_ids.into_iter().skip(skip_count).collect();
            }

            let updated_data = json!({
                "pushed_ids": all_pushed_ids,
                "last_check": Local::now().to_rfc3339(),
            });

            if let Err(e) = self
                .repo
                .update_subscription_latest_data(subscription.id, Some(updated_data))
                .await
            {
                error!(
                    "Failed to update subscription {} latest_data: {}",
                    subscription.id, e
                );
            }

            if send_result.has_failures() {
                warn!(
                    "Partially sent ranking to chat {} ({}/{} images)",
                    chat_id,
                    send_result.succeeded_indices.len(),
                    send_result.total
                );
            }
        }

        // Update task's next poll time
        self.repo
            .update_task_after_poll(task.id, Local::now() + chrono::Duration::seconds(86400))
            .await?;

        Ok(())
    }

    /// Apply tag filters to illusts (for owned values)
    #[allow(dead_code)]
    fn apply_tag_filters<'a>(
        &self,
        illusts: &'a [crate::pixiv_client::Illust],
        filter_tags: &Option<serde_json::Value>,
    ) -> Vec<&'a crate::pixiv_client::Illust> {
        let Some(filters) = filter_tags else {
            return illusts.iter().collect();
        };

        let include_tags: Vec<String> = filters
            .get("include")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(html::normalize_tag))
                    .collect()
            })
            .unwrap_or_default();

        let exclude_tags: Vec<String> = filters
            .get("exclude")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(html::normalize_tag))
                    .collect()
            })
            .unwrap_or_default();

        illusts
            .iter()
            .filter(|illust| {
                let illust_tags: Vec<String> = illust
                    .tags
                    .iter()
                    .map(|tag| html::normalize_tag(&tag.name))
                    .collect();

                // Check exclude tags first (must not contain any - normalized match)
                if !exclude_tags.is_empty() {
                    for exclude_tag in &exclude_tags {
                        if illust_tags.iter().any(|t| t == exclude_tag) {
                            return false;
                        }
                    }
                }

                // Check include tags (must contain at least one if specified - normalized match)
                if !include_tags.is_empty() {
                    for include_tag in &include_tags {
                        if illust_tags.iter().any(|t| t == include_tag) {
                            return true;
                        }
                    }
                    return false;
                }

                true
            })
            .collect()
    }

    /// Apply tag filters to illusts (for reference values)
    fn apply_tag_filters_ref<'a>(
        &self,
        illusts: &[&'a crate::pixiv_client::Illust],
        filter_tags: &Option<serde_json::Value>,
    ) -> Vec<&'a crate::pixiv_client::Illust> {
        let Some(filters) = filter_tags else {
            return illusts.to_vec();
        };

        let include_tags: Vec<String> = filters
            .get("include")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(html::normalize_tag))
                    .collect()
            })
            .unwrap_or_default();

        let exclude_tags: Vec<String> = filters
            .get("exclude")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(html::normalize_tag))
                    .collect()
            })
            .unwrap_or_default();

        illusts
            .iter()
            .filter(|illust| {
                let illust_tags: Vec<String> = illust
                    .tags
                    .iter()
                    .map(|tag| html::normalize_tag(&tag.name))
                    .collect();

                // Check exclude tags first (must not contain any - normalized match)
                if !exclude_tags.is_empty() {
                    for exclude_tag in &exclude_tags {
                        if illust_tags.iter().any(|t| t == exclude_tag) {
                            return false;
                        }
                    }
                }

                // Check include tags (must contain at least one if specified - normalized match)
                if !include_tags.is_empty() {
                    for include_tag in &include_tags {
                        if illust_tags.iter().any(|t| t == include_tag) {
                            return true;
                        }
                    }
                    return false;
                }

                true
            })
            .copied()
            .collect()
    }

    /// Apply chat-level excluded tags filter (normalized match, case-insensitive)
    fn apply_chat_excluded_tags<'a>(
        &self,
        illusts: Vec<&'a crate::pixiv_client::Illust>,
        chat_excluded_tags: &Option<serde_json::Value>,
    ) -> Vec<&'a crate::pixiv_client::Illust> {
        let Some(tags) = chat_excluded_tags else {
            return illusts;
        };

        let excluded: Vec<String> =
            if let Ok(tag_array) = serde_json::from_value::<Vec<String>>(tags.clone()) {
                tag_array.iter().map(|s| html::normalize_tag(s)).collect()
            } else {
                return illusts;
            };

        if excluded.is_empty() {
            return illusts;
        }

        illusts
            .into_iter()
            .filter(|illust| {
                let illust_tags: Vec<String> = illust
                    .tags
                    .iter()
                    .map(|tag| html::normalize_tag(&tag.name))
                    .collect();

                // Must not contain any excluded tag (normalized match)
                for exclude_tag in &excluded {
                    if illust_tags.iter().any(|t| t == exclude_tag) {
                        return false;
                    }
                }

                true
            })
            .collect()
    }

    /// Apply chat-level excluded tags filter for reference values
    fn apply_chat_excluded_tags_ref<'a>(
        &self,
        illusts: Vec<&'a crate::pixiv_client::Illust>,
        chat_excluded_tags: &Option<serde_json::Value>,
    ) -> Vec<&'a crate::pixiv_client::Illust> {
        // Same implementation as apply_chat_excluded_tags
        self.apply_chat_excluded_tags(illusts, chat_excluded_tags)
    }

    /// Check if illust contains sensitive tags (normalized match, case-insensitive)
    fn has_sensitive_tags(&self, illust: &crate::pixiv_client::Illust) -> bool {
        let illust_tags: Vec<String> = illust
            .tags
            .iter()
            .map(|tag| html::normalize_tag(&tag.name))
            .collect();

        for sensitive_tag in &self.sensitive_tags {
            let sensitive_normalized = html::normalize_tag(sensitive_tag);
            if illust_tags.iter().any(|t| t == &sensitive_normalized) {
                return true;
            }
        }

        false
    }

    /// Format tags for display (no blur on tags, blur is on images)
    fn format_tags(&self, illust: &crate::pixiv_client::Illust) -> String {
        let tag_names: Vec<&str> = illust.tags.iter().map(|t| t.name.as_str()).collect();
        let formatted = html::format_tags(&tag_names);

        if formatted.is_empty() {
            return String::new();
        }

        let escaped: Vec<String> = formatted
            .iter()
            .map(|t| format!("\\#{}", markdown::escape(t)))
            .collect();

        format!("\n\n{}", escaped.join("  "))
    }
}
