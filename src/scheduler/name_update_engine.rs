use crate::db::repo::Repo;
use crate::db::types::TaskType;
use crate::pixiv::client::PixivClient;
use anyhow::{Context, Result};
use chrono::{Local, NaiveTime, TimeZone, Timelike};
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

/// Engine responsible for daily author name updates
pub struct NameUpdateEngine {
    repo: Arc<Repo>,
    pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
    execution_time: String,
}

impl NameUpdateEngine {
    pub fn new(
        repo: Arc<Repo>,
        pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
        execution_time: String,
    ) -> Self {
        Self {
            repo,
            pixiv_client,
            execution_time,
        }
    }

    /// Main scheduler loop - runs indefinitely at specified time daily
    pub async fn run(&self) {
        info!(
            "🚀 Name update engine started (execution time: {})",
            self.execution_time
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
                "⏰ Next author name update at: {} (in {} seconds)",
                next_execution.format("%Y-%m-%d %H:%M:%S"),
                duration_until_execution.as_secs()
            );

            // Wait until execution time
            sleep(duration_until_execution).await;

            // Execute author name updates
            if let Err(e) = self.update_all_author_names().await {
                error!("Author name update error: {:#}", e);
            }

            // Sleep a bit to avoid executing twice in the same minute
            sleep(Duration::from_secs(60)).await;
        }
    }

    /// Calculate next execution time based on current time
    fn calculate_next_execution_time(&self) -> Result<chrono::DateTime<Local>> {
        let (h, m) = self.parse_execution_time()?;

        let target_time = NaiveTime::from_hms_opt(h, m, 0).context("Invalid time configuration")?;

        let now = Local::now();
        let target_date = if now.time() < target_time {
            now.date_naive()
        } else {
            now.date_naive() + chrono::Duration::days(1)
        };

        let target_naive = target_date.and_time(target_time);
        Local::from_local_datetime(&Local, &target_naive)
            .single()
            .context("Ambiguous or invalid local time (e.g. skipped by DST)")
    }

    /// Parse execution time string (HH:MM format) into (hour, minute)
    fn parse_execution_time(&self) -> Result<(u32, u32)> {
        let time = NaiveTime::parse_from_str(&self.execution_time, "%H:%M")
            .context("Invalid execution time format (expected HH:MM)")?;

        Ok((time.hour(), time.minute()))
    }

    /// Update all author names by fetching latest from Pixiv API
    async fn update_all_author_names(&self) -> Result<()> {
        info!("🔄 Starting author name update...");

        // Get all author tasks
        let tasks = self.repo.get_all_tasks_by_type(TaskType::Author).await?;

        if tasks.is_empty() {
            info!("No author tasks to update");
            return Ok(());
        }

        info!("Found {} author tasks to update", tasks.len());

        let mut updated_count = 0;
        let mut failed_count = 0;

        for task in tasks {
            let author_id: u64 = match task.value.parse() {
                Ok(id) => id,
                Err(e) => {
                    warn!(
                        "Invalid author ID '{}' in task {}: {:#}",
                        task.value, task.id, e
                    );
                    failed_count += 1;
                    continue;
                }
            };

            // Fetch latest author info from Pixiv
            let pixiv = self.pixiv_client.read().await;
            match pixiv.get_user_detail(author_id).await {
                Ok(user) => {
                    let new_name = user.name.clone();
                    let old_name = task.author_name.clone();

                    // Only update if name changed or was empty
                    if old_name.as_ref() != Some(&new_name) {
                        drop(pixiv); // Release read lock before write operation
                        if let Err(e) = self
                            .repo
                            .update_task_author_name(task.id, Some(new_name.clone()))
                            .await
                        {
                            error!("Failed to update author name for task {}: {:#}", task.id, e);
                            failed_count += 1;
                        } else {
                            info!(
                                "Updated author name: {} -> {} (ID: {})",
                                old_name.as_deref().unwrap_or("<none>"),
                                new_name,
                                author_id
                            );
                            updated_count += 1;
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to fetch author info for {} (task {}): {:#}",
                        author_id, task.id, e
                    );
                    failed_count += 1;
                }
            }

            // Small delay between API calls to avoid rate limiting
            sleep(Duration::from_millis(500)).await;
        }

        info!(
            "✅ Author name update completed: {} updated, {} failed",
            updated_count, failed_count
        );

        Ok(())
    }
}
