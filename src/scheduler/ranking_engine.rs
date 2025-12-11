use crate::bot::notifier::Notifier;
use crate::db::repo::Repo;
use crate::db::types::{SubscriptionState, TaskType};
use crate::pixiv::client::PixivClient;
use crate::scheduler::helpers::{get_chat_if_should_notify, RankingContext};
use anyhow::{Context, Result};
use chrono::{Local, TimeZone};
use pixiv_client::Illust;
use std::sync::Arc;
use teloxide::prelude::*;
use tokio::time::{sleep, Duration};
use tracing::{error, info};

pub struct RankingEngine {
    repo: Arc<Repo>,
    pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
    notifier: Notifier,
    execution_hour: u32,
    execution_minute: u32,
}

impl RankingEngine {
    pub fn new(
        repo: Arc<Repo>,
        pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
        notifier: Notifier,
        execution_hour: u32,
        execution_minute: u32,
    ) -> Self {
        Self {
            repo,
            pixiv_client,
            notifier,
            execution_hour,
            execution_minute,
        }
    }

    /// Main scheduler loop - runs indefinitely at specified time daily
    pub async fn run(&self) {
        info!(
            "🚀 Ranking engine started (execution time: {:02}:{:02})",
            self.execution_hour, self.execution_minute
        );

        loop {
            // Calculate next execution time
            let next_execution = match self.calculate_next_execution_time() {
                Ok(time) => time,
                Err(e) => {
                    error!("Failed to calculate next execution time: {:#}", e);
                    // Wait for an hour and try again
                    sleep(Duration::from_secs(3600)).await;
                    continue;
                }
            };
            let now = Local::now();
            let duration_until_execution = (next_execution - now).to_std().unwrap_or_default();

            info!(
                "⏰ Next ranking execution at: {} (in {} seconds)",
                next_execution.format("%Y-%m-%d %H:%M:%S"),
                duration_until_execution.as_secs()
            );

            // Wait until execution time
            sleep(duration_until_execution).await;

            // Execute all ranking tasks
            if let Err(e) = self.execute_all_ranking_tasks().await {
                error!("Ranking engine execution error: {:#}", e);
            }

            // Sleep a bit to avoid executing twice in the same minute
            sleep(Duration::from_secs(60)).await;
        }
    }

    /// Calculate next execution time based on current time
    fn calculate_next_execution_time(&self) -> Result<chrono::DateTime<Local>> {
        let now = Local::now();
        let today = now.date_naive();

        // Try today's execution time
        let today_execution = today
            .and_hms_opt(self.execution_hour, self.execution_minute, 0)
            .context("Invalid execution time configuration")?;
        let today_execution_time = Local
            .from_local_datetime(&today_execution)
            .single()
            .context("Ambiguous local datetime for today's execution time")?;

        if now < today_execution_time {
            // Today's execution time hasn't passed yet
            Ok(today_execution_time)
        } else {
            // Today's execution time has passed, schedule for tomorrow
            let tomorrow = today + chrono::Duration::days(1);
            let tomorrow_execution = tomorrow
                .and_hms_opt(self.execution_hour, self.execution_minute, 0)
                .context("Invalid execution time configuration for tomorrow")?;
            Ok(Local
                .from_local_datetime(&tomorrow_execution)
                .single()
                .context("Ambiguous local datetime for tomorrow's execution time")?)
        }
    }

    /// Execute all pending ranking tasks
    async fn execute_all_ranking_tasks(&self) -> Result<()> {
        info!("⚙️  Executing all ranking tasks");

        // Get all ranking tasks (not just pending ones, execute all at the scheduled time)
        let tasks = self.repo.get_all_tasks_by_type(TaskType::Ranking).await?;

        if tasks.is_empty() {
            info!("No ranking tasks found");
            return Ok(());
        }

        info!("Found {} ranking tasks to execute", tasks.len());

        for task in tasks {
            info!(
                "⚙️  Executing ranking task [{}] {} {}",
                task.id, task.r#type, task.value
            );

            if let Err(e) = self.execute_ranking_task(&task).await {
                error!("Failed to execute ranking task [{}]: {:#}", task.id, e);
            }

            // Small delay between tasks
            sleep(Duration::from_secs(2)).await;
        }

        Ok(())
    }

    /// Execute ranking subscription task (Orchestrator)
    async fn execute_ranking_task(&self, task: &crate::db::entities::tasks::Model) -> Result<()> {
        let mode = &task.value;

        // Get ranking illusts from Pixiv API
        let pixiv = self.pixiv_client.read().await;
        let illusts = pixiv.get_ranking(mode, None, 10).await?;
        drop(pixiv);

        if illusts.is_empty() {
            info!("No ranking illusts found for mode {}", mode);
            self.schedule_ranking_next_poll(task.id).await?;
            return Ok(());
        }

        info!("Found {} ranking illusts for mode {}", illusts.len(), mode);

        // Get all subscriptions for this task
        let subscriptions = self.repo.list_subscriptions_by_task(task.id).await?;

        if subscriptions.is_empty() {
            info!("No subscriptions for ranking task {}", task.id);
            self.schedule_ranking_next_poll(task.id).await?;
            return Ok(());
        }

        // Process each subscription independently (one push per subscription per tick)
        for subscription in subscriptions {
            // Prepare context
            let chat = match get_chat_if_should_notify(&self.repo, subscription.chat_id).await {
                Ok(Some(chat)) => chat,
                Ok(None) => continue,
                Err(e) => {
                    error!("Failed to process chat {}: {:#}", subscription.chat_id, e);
                    continue;
                }
            };

            let subscription_state = match &subscription.latest_data {
                Some(SubscriptionState::Ranking(state)) => Some(state.clone()),
                _ => None,
            };

            let ctx = RankingContext {
                subscription: &subscription,
                chat,
                subscription_state,
            };

            // Delegate to dispatcher
            if let Err(e) = self
                .process_single_ranking_sub(&ctx, &illusts, mode)
                .await
                .context(format!(
                    "Failed to process subscription {}",
                    subscription.id
                ))
            {
                error!("{:#}", e);
            }

            // Small delay between subscriptions
            sleep(Duration::from_millis(2000)).await;
        }

        // Schedule next poll (next day at execution time)
        self.schedule_ranking_next_poll(task.id).await?;

        Ok(())
    }

    /// Schedule next poll for ranking task (next execution time)
    async fn schedule_ranking_next_poll(&self, task_id: i32) -> Result<()> {
        let next_poll = self.calculate_next_execution_time()?;
        self.repo.update_task_after_poll(task_id, next_poll).await?;
        Ok(())
    }

    // ==================== Ranking-Specific Methods ====================

    /// Dispatcher: Process single ranking subscription
    async fn process_single_ranking_sub(
        &self,
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
        let chat_filter = crate::db::types::TagFilter::from_excluded_tags(&ctx.chat.excluded_tags);
        let combined_filter = ctx.subscription.filter_tags.merged(&chat_filter);
        let filtered_illusts: Vec<&pixiv_client::Illust> =
            combined_filter.filter(new_illusts.iter().copied());

        // Collect all new IDs for tracking
        let all_new_ids: Vec<u64> = new_illusts.iter().map(|i| i.id).collect();

        // If all filtered out, mark as processed and return
        if filtered_illusts.is_empty() {
            info!("No illusts to send to chat {} after filtering", chat_id);
            self.mark_ranking_illusts_as_pushed(ctx.subscription.id, pushed_ids, all_new_ids)
                .await?;
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
            teloxide::utils::markdown::escape(&mode.replace('_', " ").to_uppercase()),
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
            let tags = crate::utils::tag::format_tags_escaped(illust);
            let base_caption = format!(
                "{}\nby *{}* \\(ID: `{}`\\)\n\n❤️ {} \\| 🔗 [来源](https://pixiv\\.net/artworks/{}){}",
                teloxide::utils::markdown::escape(&illust.title),
                teloxide::utils::markdown::escape(&illust.user.name),
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
        let sensitive_tags = crate::utils::sensitive::get_chat_sensitive_tags(&ctx.chat);
        let has_spoiler = ctx.chat.blur_sensitive_tags
            && filtered_illusts.iter().any(|illust| {
                crate::utils::sensitive::contains_sensitive_tags(illust, sensitive_tags)
            });

        // Send as media group with individual captions
        let send_result = self
            .notifier
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
        self.trim_and_update_pushed_ids(ctx.subscription.id, new_pushed_ids)
            .await?;

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

    /// Helper: Trim pushed_ids to last 100 and update state
    async fn trim_and_update_pushed_ids(
        &self,
        subscription_id: i32,
        mut pushed_ids: Vec<u64>,
    ) -> Result<()> {
        // Keep only the last 100 IDs to prevent unbounded growth
        if pushed_ids.len() > 100 {
            let skip_count = pushed_ids.len() - 100;
            pushed_ids = pushed_ids.into_iter().skip(skip_count).collect();
        }

        let new_state = crate::db::types::RankingState {
            pushed_ids,
            pending_illust: None,
        };

        self.update_ranking_state(subscription_id, new_state).await
    }

    /// Update ranking subscription state in database
    async fn update_ranking_state(
        &self,
        subscription_id: i32,
        state: crate::db::types::RankingState,
    ) -> Result<()> {
        self.repo
            .update_subscription_latest_data(
                subscription_id,
                Some(SubscriptionState::Ranking(state)),
            )
            .await?;
        Ok(())
    }

    /// Helper: Mark illusts as pushed (when filtered out but should be marked as processed)
    async fn mark_ranking_illusts_as_pushed(
        &self,
        subscription_id: i32,
        mut pushed_ids: Vec<u64>,
        new_ids: Vec<u64>,
    ) -> Result<()> {
        pushed_ids.extend(new_ids);
        self.trim_and_update_pushed_ids(subscription_id, pushed_ids)
            .await
    }
}
