use crate::bot::notifier::Notifier;
use crate::db::repo::Repo;
use crate::db::types::{AuthorState, PendingIllust, SubscriptionState, TagFilter, TaskType};
use crate::pixiv::client::PixivClient;
use crate::scheduler::helpers::{
    get_chat_if_should_notify, process_illust_push, AuthorContext, PushResult,
};
use anyhow::{Context, Result};
use chrono::Local;
use pixiv_client::Illust;
use rand::Rng;
use std::sync::Arc;
use teloxide::prelude::*;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

pub struct AuthorEngine {
    repo: Arc<Repo>,
    pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
    notifier: Notifier,
    tick_interval_sec: u64,
    min_task_interval_sec: u64,
    max_task_interval_sec: u64,
    max_retry_count: i32,
    image_size: pixiv_client::ImageSize,
}

impl AuthorEngine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: Arc<Repo>,
        pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
        notifier: Notifier,
        tick_interval_sec: u64,
        min_task_interval_sec: u64,
        max_task_interval_sec: u64,
        max_retry_count: i32,
        image_size: pixiv_client::ImageSize,
    ) -> Self {
        Self {
            repo,
            pixiv_client,
            notifier,
            tick_interval_sec,
            min_task_interval_sec,
            max_task_interval_sec,
            max_retry_count,
            image_size,
        }
    }

    /// Main scheduler loop - runs indefinitely
    pub async fn run(&self) {
        info!("🚀 Author engine started");

        let mut interval = tokio::time::interval(Duration::from_secs(self.tick_interval_sec));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            // Wait for tick interval before checking for tasks
            interval.tick().await;

            if let Err(e) = self.tick().await {
                error!("Author engine tick error: {:#}", e);
            }
        }
    }

    /// Single tick - fetch and execute one pending author task
    async fn tick(&self) -> Result<()> {
        // Get one pending author task
        let tasks = self
            .repo
            .get_pending_tasks_by_type(TaskType::Author, 1)
            .await?;

        let task = match tasks.first() {
            Some(t) => t,
            None => return Ok(()),
        };

        info!(
            "⚙️  Executing author task [{}] {} {}",
            task.id, task.r#type, task.value
        );

        // Execute task
        let result = self.execute_author_task(task).await;

        // Note: task's next_poll_at is updated inside execute_author_task
        // We only log errors here, no need to update task again
        if let Err(e) = result {
            error!("Author task execution failed: {:#}", e);

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
            let chat = match get_chat_if_should_notify(&self.repo, subscription.chat_id).await {
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

    // ==================== Helper Methods ====================

    /// Schedule next poll with randomized interval
    async fn schedule_next_poll(&self, task_id: i32) -> Result<()> {
        let random_interval_sec =
            rand::rng().random_range(self.min_task_interval_sec..=self.max_task_interval_sec);
        let next_poll = Local::now() + chrono::Duration::seconds(random_interval_sec as i64);
        self.repo.update_task_after_poll(task_id, next_poll).await?;
        Ok(())
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
        let state = ctx
            .subscription_state
            .as_ref()
            .context("Missing subscription state for pending illust")?;

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

        // Compare retry_count (u8) with max_retry_count (i32) safely
        if (pending.retry_count as i32) >= self.max_retry_count {
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
        let total_pages = illust.get_all_image_urls_with_size(self.image_size).len();
        let remaining_pages: Vec<usize> = (0..total_pages)
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
        let push_result = process_illust_push(
            &self.notifier,
            ctx,
            illust,
            &pending.sent_pages,
            self.image_size,
        )
        .await?;

        // Calculate new state based on result
        let new_state = match push_result {
            PushResult::Success {
                illust_id,
                first_message_id,
            } => {
                info!(
                    "✅ Completed pending illust {} for chat {}",
                    illust_id, chat_id
                );
                // Save message record for reply-based unsubscribe
                if let Some(msg_id) = first_message_id {
                    if let Err(e) = self
                        .repo
                        .save_message(chat_id.0, msg_id, ctx.subscription.id, Some(illust_id))
                        .await
                    {
                        warn!("Failed to save message record: {:#}", e);
                    }
                }
                AuthorState {
                    latest_illust_id: illust_id,
                    pending_illust: None,
                }
            }
            PushResult::Partial {
                illust_id,
                sent_pages,
                total_pages,
                first_message_id,
            } => {
                warn!(
                    "⚠️  Partially sent illust {} ({}/{} pages)",
                    illust_id,
                    sent_pages.len(),
                    total_pages
                );
                // Save message record even for partial success
                if let Some(msg_id) = first_message_id {
                    if let Err(e) = self
                        .repo
                        .save_message(chat_id.0, msg_id, ctx.subscription.id, Some(illust_id))
                        .await
                    {
                        warn!("Failed to save message record: {:#}", e);
                    }
                }
                AuthorState {
                    latest_illust_id: state.latest_illust_id,
                    pending_illust: Some(PendingIllust {
                        illust_id,
                        sent_pages,
                        total_pages,
                        retry_count: pending.retry_count.saturating_add(1),
                    }),
                }
            }
            PushResult::Failure { illust_id } => {
                // Use saturating_add to prevent u8 overflow
                let new_retry_count = pending.retry_count.saturating_add(1);
                // Check if we should give up after this failure (compare u8 with i32 safely)
                if self.max_retry_count > 0 && (new_retry_count as i32) >= self.max_retry_count {
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
        let push_result =
            process_illust_push(&self.notifier, ctx, illust, &[], self.image_size).await?;

        // Calculate new state based on result
        let new_state = match push_result {
            PushResult::Success {
                illust_id,
                first_message_id,
            } => {
                info!(
                    "✅ Successfully sent illust {} to chat {}",
                    illust_id, chat_id
                );
                // Save message record for reply-based unsubscribe
                if let Some(msg_id) = first_message_id {
                    if let Err(e) = self
                        .repo
                        .save_message(chat_id.0, msg_id, ctx.subscription.id, Some(illust_id))
                        .await
                    {
                        warn!("Failed to save message record: {:#}", e);
                    }
                }
                AuthorState {
                    latest_illust_id: illust_id,
                    pending_illust: None,
                }
            }
            PushResult::Partial {
                illust_id,
                sent_pages,
                total_pages,
                first_message_id,
            } => {
                warn!(
                    "⚠️  Partially sent illust {} ({}/{} pages)",
                    illust_id,
                    sent_pages.len(),
                    total_pages
                );
                // Save message record even for partial success
                if let Some(msg_id) = first_message_id {
                    if let Err(e) = self
                        .repo
                        .save_message(chat_id.0, msg_id, ctx.subscription.id, Some(illust_id))
                        .await
                    {
                        warn!("Failed to save message record: {:#}", e);
                    }
                }
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
}
