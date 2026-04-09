use crate::bot::BotHandler;
use crate::db::types::{TagFilter, TaskType};
use anyhow::{Context, Result};
use tracing::{error, info};

impl BotHandler {
    pub(crate) async fn create_subscription(
        &self,
        chat_id: i64,
        task_type: TaskType,
        task_value: &str,
        author_name: Option<&str>,
        filter_tags: TagFilter,
    ) -> Result<()> {
        let task = self
            .repo
            .get_or_create_task(
                task_type,
                task_value.to_string(),
                author_name.map(|s| s.to_string()),
            )
            .await
            .context("Failed to create task")?;

        self.repo
            .upsert_subscription(chat_id, task.id, filter_tags)
            .await
            .context("Failed to upsert subscription")?;

        Ok(())
    }

    pub(crate) async fn delete_subscription(
        &self,
        chat_id: i64,
        task_type: TaskType,
        task_value: &str,
    ) -> Result<Option<String>> {
        let task = self
            .repo
            .get_task_by_type_value(task_type, task_value)
            .await
            .context("Failed to query task")?
            .ok_or_else(|| anyhow::anyhow!("未找到"))?;

        let author_name = task.author_name.clone();

        self.repo
            .delete_subscription_by_chat_task(chat_id, task.id)
            .await
            .context("未订阅")?;

        self.cleanup_orphaned_task(task.id, task_type, task_value)
            .await;

        Ok(author_name)
    }

    pub(super) async fn cleanup_orphaned_task(
        &self,
        task_id: i32,
        task_type: TaskType,
        task_value: &str,
    ) {
        match self.repo.count_subscriptions_for_task(task_id).await {
            Ok(0) => {
                if let Err(e) = self.repo.delete_task(task_id).await {
                    error!("Failed to delete task {}: {:#}", task_id, e);
                } else {
                    info!(
                        "Deleted task {} ({} {}) - no more subscriptions",
                        task_id, task_type, task_value
                    );
                }
            }
            Err(e) => {
                error!(
                    "Failed to count subscriptions for task {}: {:#}",
                    task_id, e
                );
            }
            _ => {}
        }
    }
}
