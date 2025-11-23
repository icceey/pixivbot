mod config;
mod error;
mod db;
mod pixiv;
mod bot;
mod scheduler;

use crate::config::Config;
use crate::error::AppResult;
use tracing::info;
use tracing_subscriber::{prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> AppResult<()> {
    // Load configuration
    let config = Config::load()?;

    // Initialize variables
    let log_level = config.log_level();
    let log_dir = &config.logging.dir;

    // Create log directory if it doesn't exist
    std::fs::create_dir_all(log_dir)?;

    // Setup file appender (daily rotation)
    let file_appender = tracing_appender::rolling::daily(log_dir, "pixivbot.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    // Setup stdout layer
    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_line_number(true)
        .with_target(false)
        .pretty();

    // Setup file layer
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(non_blocking);

    // Filter layer based on config
    let filter_layer = EnvFilter::from_default_env()
        .add_directive(log_level.into());

    // Combine layers
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(stdout_layer)
        .with(file_layer)
        .init();

    info!("Starting PixivBot...");
    info!("Configuration loaded from config.toml (or env)");
    info!("Logging initialized at level: {}", log_level);
    info!("Logs are written to: {}", log_dir);

    // Connect to database
    let _db = db::establish_connection(&config.database.url).await?;

    
    // Initialize Pixiv Client
    // let mut pixiv_client = pixiv::client::PixivClient::new(config.pixiv.clone());
    // pixiv_client.login().await?; 
    
    // Start Bot (Commented out for now as it's a blocking call or needs to be spawned)
    // bot::run(config.telegram).await?;

    Ok(())
}
