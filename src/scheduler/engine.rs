use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::config::Config;
use crate::bot::Notifier;
use rand::Rng;
use crate::error::Result;

pub struct Scheduler {
    config: Arc<Config>,
    db: sea_orm::DatabaseConnection,
    notifier: Notifier,
}

impl Scheduler {
    pub fn new(
        config: Arc<Config>,
        db: sea_orm::DatabaseConnection,
    ) -> Self {
        let notifier = Notifier::new(config.clone(), db.clone());
        
        Self {
            config,
            db,
            notifier,
        }
    }
    
    pub async fn run(&self) -> Result<()> {
        tracing::info!("Starting task scheduler");
        
        loop {
            // Process all ready tasks (should be just one at a time)
            match self.notifier.process_ready_tasks().await {
                Ok(_) => {
                    // Add a random sleep between 1.5 and 3.0 seconds after each task
                    let min_ms = self.config.scheduler.min_interval_ms;
                    let max_ms = self.config.scheduler.max_interval_ms;
                    
                    let random_ms = rand::thread_rng().gen_range(min_ms..=max_ms);
                    let sleep_duration = Duration::from_millis(random_ms);
                    
                    tracing::debug!("Sleeping for {:?} between tasks", sleep_duration);
                    sleep(sleep_duration).await;
                },
                Err(e) => {
                    tracing::error!("Error processing tasks: {:?}", e);
                    // Sleep for a bit before retrying
                    sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }
    
    pub async fn cleanup_cache(&self) -> Result<()> {
        // Clean up cache files older than 7 days
        let cache_dir = self.config.logging.dir.join("cache");
        let cache = crate::pixiv::ImageCache::new(cache_dir);
        cache.cleanup_old_files(7 * 24 * 3600).await?;
        Ok(())
    }
}