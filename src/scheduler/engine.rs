use crate::db::repo::Repo;
use crate::pixiv::client::PixivClient;
use crate::pixiv::downloader::Downloader;
use crate::bot::notifier::Notifier;
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{info, warn, error};
use chrono::Utc;
use teloxide::Bot;
use teloxide::prelude::*;
use serde_json::json;
use rand::Rng;

pub struct SchedulerEngine {
    repo: Arc<Repo>,
    pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
    notifier: Notifier,
    min_interval_sec: u64,
    max_interval_sec: u64,
}

impl SchedulerEngine {
    pub fn new(
        repo: Arc<Repo>,
        pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
        bot: Bot,
        downloader: Arc<Downloader>,
        min_interval_sec: u64,
        max_interval_sec: u64,
    ) -> Self {
        Self {
            repo,
            pixiv_client,
            notifier: Notifier::new(bot, downloader),
            min_interval_sec,
            max_interval_sec,
        }
    }

    /// Main scheduler loop - runs indefinitely
    pub async fn run(&self) {
        info!("🚀 Scheduler engine started");
        
        loop {
            // Tick every second
            sleep(Duration::from_secs(1)).await;
            
            match self.tick().await {
                Ok(executed) => {
                    if executed {
                        // Add random delay between tasks to avoid rate limiting
                        let delay_sec = rand::rng()
                            .random_range(self.min_interval_sec..=self.max_interval_sec);
                        sleep(Duration::from_secs(delay_sec)).await;
                    }
                }
                Err(e) => {
                    error!("Scheduler tick error: {}", e);
                    sleep(Duration::from_secs(5)).await; // Back off on error
                }
            }
        }
    }

    /// Single tick - fetch and execute one pending task
    /// Returns true if a task was executed
    async fn tick(&self) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        // Get one pending task
        let tasks = self.repo.get_pending_tasks(1).await?;
        
        if tasks.is_empty() {
            return Ok(false);
        }
        
        let task = &tasks[0];
        info!("⚙️  Executing task [{}] {} {}", task.id, task.r#type, task.value);
        
        // Execute based on task type
        let result = match task.r#type.as_str() {
            "author" => self.execute_author_task(task).await,
            "ranking" => self.execute_ranking_task(task).await,
            _ => {
                warn!("Unknown task type: {}", task.r#type);
                Ok(())
            }
        };
        
        // Calculate next poll time
        let next_poll = Utc::now() + chrono::Duration::seconds(task.interval_sec as i64);
        
        // Update task status
        let latest_data = if result.is_ok() {
            // Keep existing latest_data or set empty
            task.latest_data.clone()
        } else {
            task.latest_data.clone()
        };
        
        // Use task creator as updater (maintains foreign key integrity)
        self.repo.update_task_after_poll(
            task.id,
            next_poll,
            latest_data,
            task.created_by,
        ).await?;
        
        if let Err(e) = result {
            error!("Task execution failed: {}", e);
        }
        
        Ok(true)
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
            return Ok(());
        }
        
        // Get latest illust ID from task data
        let last_illust_id = task.latest_data.as_ref()
            .and_then(|data| data.get("latest_illust_id"))
            .and_then(|v| v.as_u64());
        
        // Find new illusts
        let new_illusts: Vec<_> = if let Some(last_id) = last_illust_id {
            illusts.into_iter()
                .take_while(|illust| illust.id != last_id)
                .collect()
        } else {
            // First run - only send the latest one
            illusts.into_iter().take(1).collect()
        };
        
        if new_illusts.is_empty() {
            info!("No new illusts for author {}", author_id);
            return Ok(());
        }
        
        info!("Found {} new illusts for author {}", new_illusts.len(), author_id);
        
        // Update latest_data with newest illust ID
        if let Some(newest) = new_illusts.first() {
            let updated_data = json!({
                "latest_illust_id": newest.id,
                "last_check": Utc::now().to_rfc3339(),
            });
            
            self.repo.update_task_after_poll(
                task.id,
                Utc::now() + chrono::Duration::seconds(task.interval_sec as i64),
                Some(updated_data),
                task.created_by,
            ).await?;
        }
        
        // Get all subscriptions for this task
        let subscriptions = self.repo.list_subscriptions_by_task(task.id).await?;
        
        // Notify each subscriber
        for subscription in subscriptions {
            let chat_id = ChatId(subscription.chat_id);
            
            // Check if chat is enabled or if it's an admin/owner private chat
            let should_notify = match self.repo.get_chat(subscription.chat_id).await {
                Ok(Some(chat)) if chat.enabled => true,
                Ok(Some(_chat)) => {
                    // Chat is disabled, check if it's admin/owner private chat
                    match self.repo.get_user(subscription.chat_id).await {
                        Ok(Some(user)) if user.role.is_admin() => {
                            info!("Chat {} is disabled but user is admin/owner, allowing notification", chat_id);
                            true
                        }
                        _ => {
                            info!("Skipping notification to disabled chat {}", chat_id);
                            false
                        }
                    }
                }
                Ok(None) => {
                    info!("Chat {} not found, skipping", chat_id);
                    false
                }
                Err(e) => {
                    error!("Failed to get chat {}: {}", chat_id, e);
                    false
                }
            };
            
            if !should_notify {
                continue;
            }
            
            // Apply tag filters
            let filtered_illusts = self.apply_tag_filters(&new_illusts, &subscription.filter_tags);
            
            if filtered_illusts.is_empty() {
                continue;
            }
            
            for illust in filtered_illusts {
                let caption = format!(
                    "🎨 {}\nby {}\n👁 {} | ❤️ {}\n🔗 pixiv.net/artworks/{}",
                    illust.title,
                    illust.user.name,
                    illust.total_view,
                    illust.total_bookmarks,
                    illust.id
                );
                
                // Get image URL - prefer original from meta_single_page, fallback to large
                let image_url = if let Some(original_url) = &illust.meta_single_page.original_image_url {
                    original_url.as_str()
                } else {
                    // Fallback to large image
                    illust.image_urls.large.as_str()
                };
                
                if let Err(e) = self.notifier.notify_with_image(chat_id, image_url, Some(&caption)).await {
                    error!("Failed to notify chat {}: {}", chat_id, e);
                }
                
                // Small delay between messages
                sleep(Duration::from_millis(500)).await;
            }
        }
        
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
            return Ok(());
        }
        
        info!("Found {} ranking illusts for mode {}", illusts.len(), mode);
        
        // Get last ranking date
        let last_date = task.latest_data.as_ref()
            .and_then(|data| data.get("date"))
            .and_then(|v| v.as_str());
        
        let today = Utc::now().format("%Y-%m-%d").to_string();
        
        // Only notify if it's a new day or first run
        if last_date == Some(today.as_str()) {
            info!("Ranking for {} already sent today", mode);
            return Ok(());
        }
        
        // Update latest_data
        let updated_data = json!({
            "date": today,
            "last_check": Utc::now().to_rfc3339(),
        });
        
        self.repo.update_task_after_poll(
            task.id,
            Utc::now() + chrono::Duration::seconds(task.interval_sec as i64),
            Some(updated_data),
            0,
        ).await?;
        
        // Get all subscriptions
        let subscriptions = self.repo.list_subscriptions_by_task(task.id).await?;
        
        // Notify each subscriber
        for subscription in subscriptions {
            let chat_id = ChatId(subscription.chat_id);
            
            // Check if chat is enabled or if it's an admin/owner private chat
            let should_notify = match self.repo.get_chat(subscription.chat_id).await {
                Ok(Some(chat)) if chat.enabled => true,
                Ok(Some(_chat)) => {
                    // Chat is disabled, check if it's admin/owner private chat
                    match self.repo.get_user(subscription.chat_id).await {
                        Ok(Some(user)) if user.role.is_admin() => {
                            info!("Chat {} is disabled but user is admin/owner, allowing notification", chat_id);
                            true
                        }
                        _ => {
                            info!("Skipping notification to disabled chat {}", chat_id);
                            false
                        }
                    }
                }
                Ok(None) => {
                    info!("Chat {} not found, skipping", chat_id);
                    false
                }
                Err(e) => {
                    error!("Failed to get chat {}: {}", chat_id, e);
                    false
                }
            };
            
            if !should_notify {
                continue;
            }
            
            let message = format!(
                "📊 **{} Ranking** - Top 10\n\n",
                mode.replace('_', " ").to_uppercase()
            );
            
            if let Err(e) = self.notifier.notify(chat_id, &message).await {
                error!("Failed to notify chat {}: {}", chat_id, e);
                continue;
            }
            
            // Send top illusts (limit to 10)
            for (index, illust) in illusts.iter().take(10).enumerate() {
                let caption = format!(
                    "{}. {}\nby {}\n❤️ {} bookmarks\n🔗 pixiv.net/artworks/{}",
                    index + 1,
                    illust.title,
                    illust.user.name,
                    illust.total_bookmarks,
                    illust.id
                );
                
                // Get image URL
                let image_url = if let Some(original_url) = &illust.meta_single_page.original_image_url {
                    original_url.as_str()
                } else {
                    illust.image_urls.large.as_str()
                };
                
                if let Err(e) = self.notifier.notify_with_image(chat_id, image_url, Some(&caption)).await {
                    error!("Failed to notify chat {}: {}", chat_id, e);
                }
                
                sleep(Duration::from_millis(500)).await;
            }
        }
        
        Ok(())
    }

    /// Apply tag filters to illusts
    fn apply_tag_filters<'a>(
        &self,
        illusts: &'a [crate::pixiv_client::Illust],
        filter_tags: &Option<serde_json::Value>,
    ) -> Vec<&'a crate::pixiv_client::Illust> {
        let Some(filters) = filter_tags else {
            return illusts.iter().collect();
        };
        
        let include_tags: Vec<String> = filters.get("include")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                .collect())
            .unwrap_or_default();
        
        let exclude_tags: Vec<String> = filters.get("exclude")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                .collect())
            .unwrap_or_default();
        
        illusts.iter().filter(|illust| {
            let illust_tags: Vec<String> = illust.tags.iter()
                .map(|tag| tag.name.to_lowercase())
                .collect();
            
            // Check exclude tags first (must not contain any)
            if !exclude_tags.is_empty() {
                for exclude_tag in &exclude_tags {
                    if illust_tags.iter().any(|t| t.contains(exclude_tag)) {
                        return false;
                    }
                }
            }
            
            // Check include tags (must contain at least one if specified)
            if !include_tags.is_empty() {
                for include_tag in &include_tags {
                    if illust_tags.iter().any(|t| t.contains(include_tag)) {
                        return true;
                    }
                }
                return false;
            }
            
            true
        }).collect()
    }
}
