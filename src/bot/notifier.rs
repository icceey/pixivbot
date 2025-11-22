use teloxide::prelude::*;
use teloxide::types::{ChatId, InputFile};
use std::sync::Arc;

use crate::config::Config;
use crate::db::repo::{self, subscriptions, tasks};
use crate::error::{AppError, Result};
use crate::pixiv::{PixivClient, ImageCache};

pub struct Notifier {
    bot: Bot,
    client: PixivClient,
    image_cache: ImageCache,
    db: sea_orm::DatabaseConnection,
}

impl Notifier {
    pub fn new(
        config: Arc<Config>,
        db: sea_orm::DatabaseConnection,
    ) -> Self {
        let bot = Bot::new(&config.telegram.bot_token);
        let client = PixivClient::new(config.pixiv.clone());
        let image_cache = ImageCache::new(config.logging.dir.join("cache"));
        
        Self {
            bot,
            client,
            image_cache,
            db,
        }
    }
    
    pub async fn process_ready_tasks(&self) -> Result<()> {
        // Get next ready task
        let tasks = tasks::find_ready_tasks(&self.db).await?;
        
        for task in tasks {
            self.process_single_task(task).await?;
        }
        
        Ok(())
    }
    
    async fn process_single_task(&self, task: repo::tasks::Model) -> Result<()> {
        tracing::info!("Processing task {}: {} {}", task.id, task.r#type, task.value);
        
        match task.r#type.as_str() {
            "author" => {
                self.process_author_task(task).await?;
            },
            "ranking" => {
                self.process_ranking_task(task).await?;
            },
            _ => {
                tracing::warn!("Unknown task type: {}", task.r#type);
            }
        }
        
        Ok(())
    }
    
    async fn process_author_task(&self, task: repo::tasks::Model) -> Result<()> {
        let author_id = task.value.parse::<u64>()
            .map_err(|_| AppError::Custom("Invalid author ID in task".to_string()))?;
        
        let works = self.client.fetch_user_illusts(author_id).await?;
        
        if works.is_empty() {
            // No works found, just reschedule
            let next_poll = chrono::Utc::now() + chrono::Duration::seconds(task.interval_sec as i64);
            tasks::update_next_poll(&self.db, task.id, next_poll).await?;
            return Ok(());
        }
        
        // Get latest work ID
        let latest_work_id = works.iter().map(|w| w.id).max().unwrap_or(0);
        
        // Check if we have a stored latest ID
        let current_latest = task.latest_data
            .as_ref()
            .and_then(|v| v.get("latest_illust_id"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        
        if latest_work_id <= current_latest {
            // No new works, just reschedule
            let next_poll = chrono::Utc::now() + chrono::Duration::seconds(task.interval_sec as i64);
            tasks::update_next_poll(&self.db, task.id, next_poll).await?;
            return Ok(());
        }
        
        // Get new works (those with ID > current_latest)
        let new_works: Vec<_> = works.iter()
            .filter(|w| w.id > current_latest)
            .collect();
        
        // Get subscriptions for this task
        let subscriptions = subscriptions::find_by_task(&self.db, task.id).await?;
        
        for sub in subscriptions {
            self.notify_subscription(sub, &new_works).await?;
        }
        
        // Update latest data and reschedule
        let latest_data = serde_json::json!({
            "latest_illust_id": latest_work_id
        });
        
        tasks::update_latest_data(&self.db, task.id, latest_data, None).await?;
        
        let next_poll = chrono::Utc::now() + chrono::Duration::seconds(task.interval_sec as i64);
        tasks::update_next_poll(&self.db, task.id, next_poll).await?;
        
        Ok(())
    }
    
    async fn process_ranking_task(&self, task: repo::tasks::Model) -> Result<()> {
        // TODO: Implement ranking task processing
        // This would call a Pixiv ranking API endpoint
        
        // For now, just reschedule
        let next_poll = chrono::Utc::now() + chrono::Duration::seconds(task.interval_sec as i64);
        tasks::update_next_poll(&self.db, task.id, next_poll).await?;
        Ok(())
    }
    
    async fn notify_subscription(
        &self,
        sub: repo::subscriptions::Model,
        works: &[&crate::pixiv::model::Illust],
    ) -> Result<()> {
        let chat_id = ChatId(sub.chat_id as i64);
        
        for work in works {
            // Extract tags from work
            let work_tags: Vec<String> = work.tags.iter()
                .map(|t| t.name.clone())
                .collect();
            
            // Check if work matches filter
            let filter_tags = sub.filter_tags.as_ref();
            
            let include_tags_vec = filter_tags
                .and_then(|v| v.get("include"))
                .and_then(|v| v.as_array())
                .map(|v| v.to_owned())
                .unwrap_or_default();
                
            let exclude_tags_vec = filter_tags
                .and_then(|v| v.get("exclude"))
                .and_then(|v| v.as_array())
                .map(|v| v.to_owned())
                .unwrap_or_default();
            
            let include_tags = include_tags_vec.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>();
            
            let exclude_tags = exclude_tags_vec.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>();
            
            let include_tags_str: Vec<String> = include_tags.iter().map(|&s| s.to_string()).collect();
            let exclude_tags_str: Vec<String> = exclude_tags.iter().map(|&s| s.to_string()).collect();
            
            // If no include tags, pass all works
            let include_match = include_tags_str.is_empty() || 
                include_tags_str.iter().any(|tag| work_tags.contains(tag));
            
            // Exclude if any exclude tag matches
            let exclude_match = exclude_tags_str.iter().any(|tag| work_tags.contains(tag));
            
            // Include if matches include criteria and doesn't match exclude criteria
            if !include_match || exclude_match {
                continue; // Skip this work as it doesn't match filter
            }
            
            // Get image URL
            let image_url = &work.image_urls.original;
            
            // Check if image is already cached
            let image_data = if self.image_cache.exists(image_url) {
                self.image_cache.get(image_url)?
            } else {
                let data = self.client.download_image(image_url).await?;
                self.image_cache.put(image_url, data)?;
                self.image_cache.get(image_url)?
            };
            
            // Send image to chat
            let file = InputFile::memory(image_data)
                .file_name(format!("{}.jpg", work.id));
            
            let caption = format!(
                "{}\n作者: {}\n链接: https://www.pixiv.net/artworks/{}",
                work.title,
                work.author.name,
                work.id
            );
            
            self.bot.send_photo(chat_id, file).caption(caption).await?;
        }
        
        Ok(())
    }
}