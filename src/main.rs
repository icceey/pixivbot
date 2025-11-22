use std::sync::Arc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod config;
mod error;
mod db;
mod pixiv;
mod bot;
mod scheduler;

use config::Config;
use db::establish_connection;
use bot::BotHandler;
use scheduler::Scheduler;
use error::AppError;

#[tokio::main]
async fn main() -> Result<(), AppError> {
    // Load configuration
    let config = Arc::new(Config::load()?);
    
    // Initialize logging
    init_logging(&config)?;
    
    tracing::info!("Starting PixivBot v0.1.0");
    
    // Establish database connection
    let db = establish_connection(&config.database).await?;
    
    // Create and run bot
    let bot_handler = BotHandler::new(config.clone(), db.clone());
    
    // Create scheduler
    let scheduler = Scheduler::new(config.clone(), db.clone());
    
    // Clone db for cleanup task
    let cleanup_db = db.clone();
    
    // Spawn bot and scheduler tasks
    let bot_handle = tokio::spawn(async move {
        bot_handler.run().await;
    });
    
    let scheduler_handle = tokio::spawn(async move {
        if let Err(e) = scheduler.run().await {
            tracing::error!("Scheduler error: {:?}", e);
        }
    });
    
    // Spawn cleanup task (runs once a day)
    let cleanup_scheduler = Scheduler::new(config.clone(), cleanup_db.clone());
    let cleanup_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(24 * 3600));
        
        loop {
            interval.tick().await;
            if let Err(e) = cleanup_scheduler.cleanup_cache().await {
                tracing::error!("Cache cleanup error: {:?}", e);
            }
        }
    });
    
    tracing::info!("All services started");
    
    // Wait for tasks to complete (they shouldn't unless there's an error)
    let _ = tokio::try_join!(bot_handle, scheduler_handle, cleanup_handle);
    
    Ok(())
}

fn init_logging(config: &Config) -> Result<(), AppError> {
    // Create log directory if it doesn't exist
    std::fs::create_dir_all(&config.logging.dir)?;
    
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config.logging.dir.join("pixivbot.log"))?;
    
    // Initialize subscriber with both console and file outputs
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_thread_ids(true)
                .with_file(true)
                .with_line_number(true)
                .with_writer(std::io::stdout)
        )
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(log_file)
        )
        .init();
    
    Ok(())
}
