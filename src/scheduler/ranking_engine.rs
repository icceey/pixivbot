use crate::bot::notifier::Notifier;
use crate::db::repo::Repo;
use crate::db::types::{SubscriptionState, TaskType};
use crate::pixiv::client::PixivClient;
use crate::scheduler::helpers::{
    get_chat_if_should_notify, process_single_ranking_sub, RankingContext,
};
use anyhow::{Context, Result};
use chrono::{Local, TimeZone};
use std::sync::Arc;
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
            let next_execution = self.calculate_next_execution_time();
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
    fn calculate_next_execution_time(&self) -> chrono::DateTime<Local> {
        let now = Local::now();
        let today = now.date_naive();

        // Try today's execution time
        let today_execution = today
            .and_hms_opt(self.execution_hour, self.execution_minute, 0)
            .unwrap();
        let today_execution_time = Local
            .from_local_datetime(&today_execution)
            .single()
            .unwrap();

        if now < today_execution_time {
            // Today's execution time hasn't passed yet
            today_execution_time
        } else {
            // Today's execution time has passed, schedule for tomorrow
            let tomorrow = today + chrono::Duration::days(1);
            let tomorrow_execution = tomorrow
                .and_hms_opt(self.execution_hour, self.execution_minute, 0)
                .unwrap();
            Local
                .from_local_datetime(&tomorrow_execution)
                .single()
                .unwrap()
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
            if let Err(e) =
                process_single_ranking_sub(&self.repo, &self.notifier, &ctx, &illusts, mode)
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
        let next_poll = self.calculate_next_execution_time();
        self.repo.update_task_after_poll(task_id, next_poll).await?;
        Ok(())
    }
}
