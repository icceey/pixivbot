use crate::bot::notifier::{BatchSendResult, Notifier};
use crate::db::repo::Repo;
use crate::db::types::{
    AuthorState, PendingIllust, RankingState, SubscriptionState, TagFilter, TaskType,
};
use crate::pixiv::client::PixivClient;
use crate::pixiv_client::Illust;
use crate::utils::{sensitive, tag};
use anyhow::{Context, Result};
use chrono::Local;
use rand::Rng;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::utils::markdown;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

/// Result of processing a single illust push
#[derive(Debug)]
enum PushResult {
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
struct AuthorContext<'a> {
    subscription: &'a crate::db::entities::subscriptions::Model,
    chat: crate::db::entities::chats::Model,
    subscription_state: Option<AuthorState>,
}

/// Context for processing a single ranking subscription
struct RankingContext<'a> {
    subscription: &'a crate::db::entities::subscriptions::Model,
    chat: crate::db::entities::chats::Model,
    subscription_state: Option<RankingState>,
}

pub struct SchedulerEngine {
    repo: Arc<Repo>,
    pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
    notifier: Notifier,
    tick_interval_sec: u64,
    min_task_interval_sec: u64,
    max_task_interval_sec: u64,
    max_retry_count: i32,
}

impl SchedulerEngine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: Arc<Repo>,
        pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
        notifier: Notifier,
        tick_interval_sec: u64,
        min_task_interval_sec: u64,
        max_task_interval_sec: u64,
        max_retry_count: i32,
    ) -> Self {
        Self {
            repo,
            pixiv_client,
            notifier,
            tick_interval_sec,
            min_task_interval_sec,
            max_task_interval_sec,
            max_retry_count,
        }
    }

    /// Main scheduler loop - runs indefinitely
    pub async fn run(&self) {
        info!("🚀 Scheduler engine started");

        let mut interval = tokio::time::interval(Duration::from_secs(self.tick_interval_sec));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            // Wait for tick interval before checking for tasks
            interval.tick().await;

            if let Err(e) = self.tick().await {
                error!("Scheduler tick error: {:#}", e);
            }
        }
    }

    /// Single tick - fetch and execute one pending task
    async fn tick(&self) -> Result<()> {
        // Get one pending task
        let tasks = self.repo.get_pending_tasks(1).await?;

        let task = match tasks.first() {
            Some(t) => t,
            None => return Ok(()),
        };

        info!(
            "⚙️  Executing task [{}] {} {}",
            task.id, task.r#type, task.value
        );

        // Execute based on task type
        let result = match task.r#type {
            TaskType::Author => self.execute_author_task(task).await,
            TaskType::Ranking => self.execute_ranking_task(task).await,
        };

        // Note: task's next_poll_at is updated inside execute_*_task methods
        // We only log errors here, no need to update task again
        if let Err(e) = result {
            error!("Task execution failed: {:#}", e);

            // On error, still update the poll time to avoid immediate retry
            let random_interval_sec =
                rand::rng().random_range(self.min_task_interval_sec..=self.max_task_interval_sec);
            let next_poll = Local::now() + chrono::Duration::seconds(random_interval_sec as i64);

            self.repo.update_task_after_poll(task.id, next_poll).await?;
        }

        Ok(())
    }

    /// Execute author subscription task (Orchestrator)
    /// Fetches data once, iterates subscriptions, delegates to dispatcher
    async fn execute_author_task(&self, task: &crate::db::entities::tasks::Model) -> Result<()> {
        let author_id: u64 = task.value.parse()?;

        // Get latest illusts from Pixiv API
        let pixiv = self.pixiv_client.read().await;
        let illusts = pixiv.get_user_illusts(author_id, 10).await?;
        drop(pixiv);

        if illusts.is_empty() {
            self.schedule_next_poll(task.id).await?;
            return Ok(());
        }

        // Get all subscriptions for this task
        let subscriptions = self.repo.list_subscriptions_by_task(task.id).await?;

        if subscriptions.is_empty() {
            info!("No subscriptions for author task {}", task.id);
            self.schedule_next_poll(task.id).await?;
            return Ok(());
        }

        // Process each subscription independently (one push per subscription per tick)
        for subscription in subscriptions {
            // Prepare context
            let chat = match self.get_chat_if_should_notify(subscription.chat_id).await {
                Ok(Some(chat)) => chat,
                Ok(None) => continue,
                Err(e) => {
                    error!("Failed to process chat {}: {:#}", subscription.chat_id, e);
                    continue;
                }
            };

            let subscription_state = match &subscription.latest_data {
                Some(SubscriptionState::Author(state)) => Some(state.clone()),
                _ => None,
            };

            let ctx = AuthorContext {
                subscription: &subscription,
                chat,
                subscription_state,
            };

            // Delegate to dispatcher, get new state if any
            match self
                .process_single_author_sub(&ctx, &illusts)
                .await
                .context(format!(
                    "Failed to process subscription {}",
                    subscription.id
                )) {
                Ok(Some(new_state)) => {
                    // Worker returned new state, persist it
                    if let Err(e) = self
                        .update_subscription_state(subscription.id, new_state)
                        .await
                    {
                        error!(
                            "Failed to update subscription {} state: {:#}",
                            subscription.id, e
                        );
                    }
                }
                Ok(None) => {
                    // No state change
                }
                Err(e) => {
                    error!("{:#}", e);
                }
            }

            // Small delay between subscriptions
            sleep(Duration::from_millis(2000)).await;
        }

        // Schedule next poll
        self.schedule_next_poll(task.id).await?;

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
            let chat = match self.get_chat_if_should_notify(subscription.chat_id).await {
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

        // Schedule next poll (24 hours for ranking)
        self.schedule_ranking_next_poll(task.id).await?;

        Ok(())
    }

    // ==================== Helper Methods ====================

    /// Schedule next poll with randomized interval
    async fn schedule_next_poll(&self, task_id: i32) -> Result<()> {
        let random_interval_sec =
            rand::rng().random_range(self.min_task_interval_sec..=self.max_task_interval_sec);
        let next_poll = Local::now() + chrono::Duration::seconds(random_interval_sec as i64);
        self.repo.update_task_after_poll(task_id, next_poll).await?;
        Ok(())
    }

    /// Get chat and check if should notify (enabled or admin)
    async fn get_chat_if_should_notify(
        &self,
        chat_id: i64,
    ) -> Result<Option<crate::db::entities::chats::Model>> {
        let chat = self
            .repo
            .get_chat(chat_id)
            .await
            .context("Failed to get chat")?;

        let Some(chat) = chat else {
            info!("Chat {} not found, skipping", chat_id);
            return Ok(None);
        };

        if chat.enabled {
            return Ok(Some(chat));
        }

        // Check if admin/owner
        match self.repo.get_user(chat_id).await {
            Ok(Some(user)) if user.role.is_admin() => Ok(Some(chat)),
            _ => {
                info!("Skipping notification to disabled chat {}", chat_id);
                Ok(None)
            }
        }
    }

    /// Update subscription state in database
    async fn update_subscription_state(
        &self,
        subscription_id: i32,
        state: AuthorState,
    ) -> Result<()> {
        self.repo
            .update_subscription_latest_data(
                subscription_id,
                Some(SubscriptionState::Author(state)),
            )
            .await?;
        Ok(())
    }

    // ==================== Dispatcher ====================

    /// Dispatcher: Decide between pending retry or new illust processing
    /// Returns Some(new_state) if state changed, None if no change
    async fn process_single_author_sub(
        &self,
        ctx: &AuthorContext<'_>,
        illusts: &[Illust],
    ) -> Result<Option<AuthorState>> {
        // Check if there's a pending illust to resume
        if let Some(ref state) = ctx.subscription_state {
            if let Some(ref pending) = state.pending_illust {
                // Handle pending first (retry incomplete push)
                return self.handle_existing_pending(ctx, illusts, pending).await;
            }
        }

        // No pending, process new illusts
        self.handle_new_illusts(ctx, illusts).await
    }

    // ==================== Workers ====================

    /// Worker: Handle retry of existing pending illust
    /// Returns Some(new_state) if state changed, None if no change
    async fn handle_existing_pending(
        &self,
        ctx: &AuthorContext<'_>,
        illusts: &[Illust],
        pending: &PendingIllust,
    ) -> Result<Option<AuthorState>> {
        let chat_id = ChatId(ctx.subscription.chat_id);
        let state = ctx.subscription_state.as_ref().unwrap();

        // Check retry limit
        if self.max_retry_count <= 0 {
            // Retry disabled, abandon immediately
            warn!(
                "Retry disabled (max_retry_count={}), abandoning pending illust {} for chat {}",
                self.max_retry_count, pending.illust_id, chat_id
            );
            return Ok(Some(AuthorState {
                latest_illust_id: state.latest_illust_id,
                pending_illust: None,
            }));
        }

        if pending.retry_count >= self.max_retry_count as u8 {
            // Max retries reached, abandon
            warn!(
                "Max retry count reached ({}/{}), abandoning pending illust {} for chat {}",
                pending.retry_count, self.max_retry_count, pending.illust_id, chat_id
            );
            return Ok(Some(AuthorState {
                latest_illust_id: state.latest_illust_id,
                pending_illust: None,
            }));
        }

        // Find the illust in API response
        let Some(illust) = illusts.iter().find(|i| i.id == pending.illust_id) else {
            // Pending illust not found (deleted or too old), abandon it
            warn!(
                "Pending illust {} not found in API response, abandoning",
                pending.illust_id
            );
            return Ok(Some(AuthorState {
                latest_illust_id: state.latest_illust_id,
                pending_illust: None,
            }));
        };

        info!(
            "Resuming pending illust {} ({}/{} pages sent, retry {}/{})",
            pending.illust_id,
            pending.sent_pages.len(),
            pending.total_pages,
            pending.retry_count,
            self.max_retry_count
        );

        // Calculate remaining pages
        let all_urls = illust.get_all_image_urls();
        let remaining_pages: Vec<usize> = (0..all_urls.len())
            .filter(|i| !pending.sent_pages.contains(i))
            .collect();

        if remaining_pages.is_empty() {
            // All pages already sent, mark as complete
            return Ok(Some(AuthorState {
                latest_illust_id: pending.illust_id,
                pending_illust: None,
            }));
        }

        // Send remaining pages
        let push_result = self
            .process_illust_push(ctx, illust, &pending.sent_pages)
            .await?;

        // Calculate new state based on result
        let new_state = match push_result {
            PushResult::Success { illust_id } => {
                info!(
                    "✅ Completed pending illust {} for chat {}",
                    illust_id, chat_id
                );
                AuthorState {
                    latest_illust_id: illust_id,
                    pending_illust: None,
                }
            }
            PushResult::Partial {
                illust_id,
                sent_pages,
                total_pages,
            } => {
                warn!(
                    "⚠️  Partially sent illust {} ({}/{} pages)",
                    illust_id,
                    sent_pages.len(),
                    total_pages
                );
                AuthorState {
                    latest_illust_id: state.latest_illust_id,
                    pending_illust: Some(PendingIllust {
                        illust_id,
                        sent_pages,
                        total_pages,
                        retry_count: pending.retry_count + 1,
                    }),
                }
            }
            PushResult::Failure { illust_id } => {
                let new_retry_count = pending.retry_count + 1;
                // Check if we should give up after this failure
                if self.max_retry_count > 0 && new_retry_count >= self.max_retry_count as u8 {
                    error!(
                        "❌ Failed to send pending illust {} to chat {}, max retries reached ({}/{}), abandoning",
                        illust_id, chat_id, new_retry_count, self.max_retry_count
                    );
                    AuthorState {
                        latest_illust_id: state.latest_illust_id,
                        pending_illust: None,
                    }
                } else {
                    error!(
                        "❌ Failed to send pending illust {} to chat {}, will retry (attempt {}/{})",
                        illust_id, chat_id, new_retry_count, self.max_retry_count
                    );
                    // Increment retry count and keep pending state
                    AuthorState {
                        latest_illust_id: state.latest_illust_id,
                        pending_illust: Some(PendingIllust {
                            illust_id: pending.illust_id,
                            sent_pages: pending.sent_pages.clone(),
                            total_pages: pending.total_pages,
                            retry_count: new_retry_count,
                        }),
                    }
                }
            }
        };

        Ok(Some(new_state))
    }

    /// Worker: Select and push the oldest new illust
    /// Returns Some(new_state) if state changed, None if no change
    async fn handle_new_illusts(
        &self,
        ctx: &AuthorContext<'_>,
        illusts: &[Illust],
    ) -> Result<Option<AuthorState>> {
        let chat_id = ChatId(ctx.subscription.chat_id);
        let last_illust_id = ctx.subscription_state.as_ref().map(|s| s.latest_illust_id);

        // Find new illusts for this subscription
        let new_illusts: Vec<_> = if let Some(last_id) = last_illust_id {
            illusts.iter().take_while(|i| i.id > last_id).collect()
        } else {
            // First run: only send the latest one
            illusts.iter().take(1).collect()
        };

        if new_illusts.is_empty() {
            return Ok(None);
        }

        info!(
            "Found {} new illusts for subscription {} (chat {}): {:?}",
            new_illusts.len(),
            ctx.subscription.id,
            chat_id,
            new_illusts.iter().map(|i| i.id).collect::<Vec<_>>()
        );

        let newest_illust_id = new_illusts.first().map(|i| i.id);

        // Apply tag filters
        let chat_filter = TagFilter::from_excluded_tags(&ctx.chat.excluded_tags);
        let combined_filter = ctx.subscription.filter_tags.merged(&chat_filter);
        let filtered_illusts: Vec<&Illust> = combined_filter.filter(new_illusts.iter().copied());

        // If all filtered out, update cursor and return
        if filtered_illusts.is_empty() {
            return Ok(newest_illust_id.map(|newest_id| AuthorState {
                latest_illust_id: newest_id,
                pending_illust: None,
            }));
        }

        // *** KEY CHANGE: Only process the OLDEST new illust (last in the filtered list) ***
        let illust = filtered_illusts
            .last()
            .expect("filtered_illusts is not empty");

        // Push this single illust
        let push_result = self.process_illust_push(ctx, illust, &[]).await?;

        // Calculate new state based on result
        let new_state = match push_result {
            PushResult::Success { illust_id } => {
                info!(
                    "✅ Successfully sent illust {} to chat {}",
                    illust_id, chat_id
                );
                AuthorState {
                    latest_illust_id: illust_id,
                    pending_illust: None,
                }
            }
            PushResult::Partial {
                illust_id,
                sent_pages,
                total_pages,
            } => {
                warn!(
                    "⚠️  Partially sent illust {} ({}/{} pages)",
                    illust_id,
                    sent_pages.len(),
                    total_pages
                );
                AuthorState {
                    latest_illust_id: last_illust_id.unwrap_or(0),
                    pending_illust: Some(PendingIllust {
                        illust_id,
                        sent_pages,
                        total_pages,
                        retry_count: 0,
                    }),
                }
            }
            PushResult::Failure { illust_id } => {
                error!(
                    "❌ Failed to send illust {} to chat {}, will retry next poll",
                    illust_id, chat_id
                );
                // Don't update state, retry next tick
                return Ok(None);
            }
        };

        Ok(Some(new_state))
    }

    /// Generic push executor: Send specific illust pages (excluding already sent pages)
    async fn process_illust_push(
        &self,
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
            // Continuing from previous attempt
            format!(
                "🎨 {} \\(continued, {}/{} remaining\\)\nby *{}*\n\n🔗 [来源](https://pixiv\\.net/artworks/{})",
                markdown::escape(&illust.title),
                urls_to_send.len(),
                total_pages,
                markdown::escape(&illust.user.name),
                illust.id
            )
        };

        // Check spoiler setting
        let sensitive_tags = sensitive::get_chat_sensitive_tags(&ctx.chat);
        let has_spoiler = ctx.chat.blur_sensitive_tags
            && sensitive::contains_sensitive_tags(illust, sensitive_tags);

        // Send images
        let send_result = self
            .notifier
            .notify_with_images(chat_id, &urls_to_send, Some(&caption), has_spoiler)
            .await;

        // Map send result to PushResult
        let result = self.map_send_result_to_push_result(
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
        &self,
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

    // ==================== Ranking-Specific Methods ====================

    /// Schedule next poll for ranking task (24 hours)
    async fn schedule_ranking_next_poll(&self, task_id: i32) -> Result<()> {
        self.repo
            .update_task_after_poll(task_id, Local::now() + chrono::Duration::seconds(86400))
            .await?;
        Ok(())
    }

    /// Update ranking subscription state in database
    async fn update_ranking_state(&self, subscription_id: i32, state: RankingState) -> Result<()> {
        self.repo
            .update_subscription_latest_data(
                subscription_id,
                Some(SubscriptionState::Ranking(state)),
            )
            .await?;
        Ok(())
    }

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
        let chat_filter = TagFilter::from_excluded_tags(&ctx.chat.excluded_tags);
        let combined_filter = ctx.subscription.filter_tags.merged(&chat_filter);
        let filtered_illusts: Vec<&Illust> = combined_filter.filter(new_illusts.iter().copied());

        // Collect all new IDs for tracking
        let all_new_ids: Vec<u64> = new_illusts.iter().map(|i| i.id).collect();

        // If all filtered out, mark as processed and return
        if filtered_illusts.is_empty() {
            info!("No illusts to send to chat {} after filtering", chat_id);
            self.mark_ranking_illusts_as_pushed(ctx, pushed_ids, all_new_ids)
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
            warn!(
                "⚠️  Partially sent ranking to chat {} ({}/{} illusts)",
                chat_id,
                send_result.succeeded_indices.len(),
                filtered_illusts.len()
            );
        }

        Ok(())
    }

    /// Helper: Mark illusts as pushed (when filtered out but should be marked as processed)
    async fn mark_ranking_illusts_as_pushed(
        &self,
        ctx: &RankingContext<'_>,
        mut pushed_ids: Vec<u64>,
        new_ids: Vec<u64>,
    ) -> Result<()> {
        pushed_ids.extend(new_ids);
        self.trim_and_update_pushed_ids(ctx.subscription.id, pushed_ids)
            .await
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

        let new_state = RankingState {
            pushed_ids,
            pending_illust: None,
        };

        self.update_ranking_state(subscription_id, new_state).await
    }
}
