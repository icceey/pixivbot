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
            notifier: Notifier::new(bot, downloader),
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

            self.repo
                .update_task_after_poll(task.id, next_poll, task.created_by)
                .await?;
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
            self.repo
                .update_task_after_poll(task.id, next_poll, task.created_by)
                .await?;
            return Ok(());
        }

        // Get all subscriptions for this task
        let subscriptions = self.repo.list_subscriptions_by_task(task.id).await?;

        if subscriptions.is_empty() {
            info!("No subscriptions for author task {}", task.id);
            let random_interval_sec =
                rand::rng().random_range(self.min_task_interval_sec..=self.max_task_interval_sec);
            let next_poll = Local::now() + chrono::Duration::seconds(random_interval_sec as i64);
            self.repo
                .update_task_after_poll(task.id, next_poll, task.created_by)
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
                    Ok(Some(user)) if user.role.is_admin() => {
                        info!(
                            "Chat {} is disabled but user is admin/owner, allowing notification",
                            chat_id
                        );
                        true
                    }
                    _ => {
                        info!("Skipping notification to disabled chat {}", chat_id);
                        false
                    }
                }
            };

            if !should_notify {
                continue;
            }

            // Get this subscription's last pushed illust ID
            let last_illust_id = subscription
                .latest_data
                .as_ref()
                .and_then(|data| data.get("latest_illust_id"))
                .and_then(|v| v.as_u64());

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

            if new_illusts.is_empty() {
                continue;
            }

            info!(
                "Found {} new illusts for subscription {} (chat {})",
                new_illusts.len(),
                subscription.id,
                chat_id
            );

            // Apply subscription tag filters
            let mut filtered_illusts =
                self.apply_tag_filters_ref(&new_illusts, &subscription.filter_tags);

            // Apply chat-level excluded tags
            filtered_illusts =
                self.apply_chat_excluded_tags_ref(filtered_illusts, &chat.excluded_tags);

            // Update subscription's latest_data with newest illust ID (before filtering)
            // We track the newest illust regardless of filter, to avoid re-processing
            if let Some(newest) = new_illusts.first() {
                let updated_data = json!({
                    "latest_illust_id": newest.id,
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

            if filtered_illusts.is_empty() {
                continue;
            }

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
                    "🎨 {}{}\nby {}\n\n👀 {} \\| ❤️ {} \\| 🔗 [来源](https://pixiv\\.net/artworks/{}){}", 
                    markdown::escape(&illust.title),
                    page_info,
                    markdown::escape(&illust.user.name),
                    illust.total_view,
                    illust.total_bookmarks,
                    illust.id,
                    tags
                );

                // 获取所有图片URL (支持单图和多图)
                let image_urls = illust.get_all_image_urls();

                if let Err(e) = self
                    .notifier
                    .notify_with_images(chat_id, &image_urls, Some(&caption), has_spoiler)
                    .await
                {
                    error!("Failed to notify chat {}: {}", chat_id, e);
                }

                // Small delay between messages
                sleep(Duration::from_millis(2000)).await;
            }
        }

        // Update task's next poll time
        let random_interval_sec =
            rand::rng().random_range(self.min_task_interval_sec..=self.max_task_interval_sec);
        let next_poll = Local::now() + chrono::Duration::seconds(random_interval_sec as i64);
        self.repo
            .update_task_after_poll(task.id, next_poll, task.created_by)
            .await?;

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
                .update_task_after_poll(
                    task.id,
                    Local::now() + chrono::Duration::seconds(86400),
                    task.updated_by,
                )
                .await?;
            return Ok(());
        }

        info!("Found {} ranking illusts for mode {}", illusts.len(), mode);

        // Get all subscriptions
        let subscriptions = self.repo.list_subscriptions_by_task(task.id).await?;

        if subscriptions.is_empty() {
            info!("No subscriptions for ranking task {}", task.id);
            self.repo
                .update_task_after_poll(
                    task.id,
                    Local::now() + chrono::Duration::seconds(86400),
                    task.updated_by,
                )
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
                    Ok(Some(user)) if user.role.is_admin() => {
                        info!(
                            "Chat {} is disabled but user is admin/owner, allowing notification",
                            chat_id
                        );
                        true
                    }
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

            // Collect new illust IDs
            let new_ids: Vec<u64> = new_illusts.iter().map(|i| i.id).collect();

            // Update subscription's latest_data with pushed IDs
            let mut all_pushed_ids = pushed_ids.clone();
            all_pushed_ids.extend(new_ids);

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

            // Apply chat-level excluded tags filter
            let filtered_illusts: Vec<&crate::pixiv_client::Illust> =
                self.apply_chat_excluded_tags(new_illusts.clone(), &chat.excluded_tags);

            if filtered_illusts.is_empty() {
                info!("No illusts to send to chat {} after filtering", chat_id);
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

            // Prepare image URLs and captions
            let mut image_urls: Vec<String> = Vec::new();
            let mut captions: Vec<String> = Vec::new();

            for (index, illust) in filtered_illusts.iter().enumerate() {
                // Get image URL
                let image_url =
                    if let Some(original_url) = &illust.meta_single_page.original_image_url {
                        original_url.clone()
                    } else {
                        illust.image_urls.large.clone()
                    };
                image_urls.push(image_url);

                // Build caption for this image
                let tags = self.format_tags(illust);

                let base_caption = format!(
                    "{}\\.  {}\nby {}\n\n❤️ {} \\| 🔗 [来源](https://pixiv\\.net/artworks/{}){}",
                    index + 1,
                    markdown::escape(&illust.title),
                    markdown::escape(&illust.user.name),
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
            if let Err(e) = self
                .notifier
                .notify_with_individual_captions(chat_id, &image_urls, &captions, has_spoiler)
                .await
            {
                error!(
                    "Failed to send ranking media group to chat {}: {}",
                    chat_id, e
                );
            }
        }

        // Update task's next poll time
        self.repo
            .update_task_after_poll(
                task.id,
                Local::now() + chrono::Duration::seconds(86400),
                task.updated_by,
            )
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
                    .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                    .collect()
            })
            .unwrap_or_default();

        let exclude_tags: Vec<String> = filters
            .get("exclude")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                    .collect()
            })
            .unwrap_or_default();

        illusts
            .iter()
            .filter(|illust| {
                let illust_tags: Vec<String> = illust
                    .tags
                    .iter()
                    .map(|tag| tag.name.to_lowercase())
                    .collect();

                // Check exclude tags first (must not contain any - exact match)
                if !exclude_tags.is_empty() {
                    for exclude_tag in &exclude_tags {
                        if illust_tags.iter().any(|t| t == exclude_tag) {
                            return false;
                        }
                    }
                }

                // Check include tags (must contain at least one if specified - exact match)
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
                    .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                    .collect()
            })
            .unwrap_or_default();

        let exclude_tags: Vec<String> = filters
            .get("exclude")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                    .collect()
            })
            .unwrap_or_default();

        illusts
            .iter()
            .filter(|illust| {
                let illust_tags: Vec<String> = illust
                    .tags
                    .iter()
                    .map(|tag| tag.name.to_lowercase())
                    .collect();

                // Check exclude tags first (must not contain any - exact match)
                if !exclude_tags.is_empty() {
                    for exclude_tag in &exclude_tags {
                        if illust_tags.iter().any(|t| t == exclude_tag) {
                            return false;
                        }
                    }
                }

                // Check include tags (must contain at least one if specified - exact match)
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

    /// Apply chat-level excluded tags filter (exact match, case-insensitive)
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
                tag_array.iter().map(|s| s.to_lowercase()).collect()
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
                    .map(|tag| tag.name.to_lowercase())
                    .collect();

                // Must not contain any excluded tag (exact match)
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

    /// Check if illust contains sensitive tags (exact match, case-insensitive)
    fn has_sensitive_tags(&self, illust: &crate::pixiv_client::Illust) -> bool {
        let illust_tags: Vec<String> = illust
            .tags
            .iter()
            .map(|tag| tag.name.to_lowercase())
            .collect();

        for sensitive_tag in &self.sensitive_tags {
            let sensitive_lower = sensitive_tag.to_lowercase();
            if illust_tags.iter().any(|t| t == &sensitive_lower) {
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
