mod bot;
mod cache;
mod config;
mod db;
mod pixiv;
mod scheduler;
mod utils;

use crate::config::Config;
use anyhow::Result;
use sea_orm_migration::MigratorTrait;
use tracing::{error, info};
use tracing_subscriber::fmt::time::ChronoLocal;
use tracing_subscriber::{prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
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

    // Use local time for log timestamps
    let local_timer = ChronoLocal::rfc_3339();

    // Setup stdout layer with local time
    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_line_number(true)
        .with_file(true)
        .with_target(false)
        .with_timer(local_timer.clone());

    // Setup file layer with local time
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_timer(local_timer)
        .with_writer(non_blocking);

    // Filter layer based on config
    let filter_layer = EnvFilter::from_default_env()
        .add_directive(log_level.into())
        .add_directive("sqlx=warn".parse().unwrap())
        .add_directive("sea_orm=warn".parse().unwrap());

    // Combine layers
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(stdout_layer)
        .with(file_layer)
        .init();

    info!("Starting PixivBot...");
    info!("Logs are written to: {}", log_dir);

    // Connect to database
    let db = db::establish_connection(&config.database.url).await?;
    info!("Database connection established");

    // Run migrations
    migration::Migrator::up(&db, None).await?;
    info!("✅ Database migrations completed");

    // Initialize repository
    let repo = std::sync::Arc::new(db::repo::Repo::new(db.clone()));

    // Test database connection
    repo.ping().await?;
    info!("✅ Database ping successful");

    // Initialize Pixiv Client
    let mut pixiv_client = pixiv::client::PixivClient::new(config.pixiv.clone())?;
    pixiv_client.login().await?;
    let pixiv_client = std::sync::Arc::new(tokio::sync::RwLock::new(pixiv_client));
    info!("✅ Pixiv client initialized");

    // Initialize cache manager (starts background cleanup task)
    let cache_dir = &config.scheduler.cache_dir;
    let cache_retention_days = config.scheduler.cache_retention_days;
    let cache_manager = cache::FileCacheManager::new(cache_dir, cache_retention_days);
    info!(
        "✅ Cache manager initialized (retention: {} days)",
        cache_retention_days
    );

    // Initialize Downloader (use reqwest client)
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let downloader = std::sync::Arc::new(pixiv::downloader::Downloader::new(
        http_client,
        cache_manager,
    ));
    info!("✅ Downloader initialized");

    info!("PixivBot initialization complete");

    // Initialize Telegram Bot
    let mut bot = teloxide::Bot::new(config.telegram.bot_token.clone());

    // Set custom API URL if configured
    if let Some(api_url) = &config.telegram.api_url {
        match url::Url::parse(api_url) {
            Ok(parsed_url) => {
                info!("Using custom Telegram API URL: {}", api_url);
                bot = bot.set_api_url(parsed_url);
            }
            Err(e) => {
                error!("Failed to parse custom API URL '{}': {:#}", api_url, e);
                return Err(anyhow::anyhow!("Invalid Telegram API URL in configuration"));
            }
        }
    }

    // Initialize Notifier
    let notifier = bot::notifier::Notifier::new(bot.clone(), downloader.clone());

    // Initialize author engine
    let scheduler_config = config.scheduler.clone();
    let image_size = config.content.image_size.to_pixiv_image_size();
    let author_engine = scheduler::AuthorEngine::new(
        repo.clone(),
        pixiv_client.clone(),
        notifier.clone(),
        scheduler_config.tick_interval_sec,
        scheduler_config.min_task_interval_sec,
        scheduler_config.max_task_interval_sec,
        scheduler_config.max_retry_count,
        image_size,
    );

    // Initialize ranking engine
    let ranking_engine = scheduler::RankingEngine::new(
        repo.clone(),
        pixiv_client.clone(),
        notifier.clone(),
        scheduler_config.ranking_execution_time.clone(),
        image_size,
    );

    // Initialize name update engine
    let name_update_engine = scheduler::NameUpdateEngine::new(
        repo.clone(),
        pixiv_client.clone(),
        scheduler_config.author_name_update_time.clone(),
    );

    info!("✅ Author, Ranking, and Name Update engines initialized");

    // Spawn all engines in background
    let author_engine_handle = tokio::spawn(async move {
        author_engine.run().await;
    });

    let ranking_engine_handle = tokio::spawn(async move {
        ranking_engine.run().await;
    });

    let name_update_engine_handle = tokio::spawn(async move {
        name_update_engine.run().await;
    });

    info!("🤖 Starting Telegram Bot...");

    // Setup Ctrl+C handler
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for Ctrl+C");
        info!("Received Ctrl+C, shutting down...");
        let _ = shutdown_tx.send(()).await;
    });

    // Start Bot in a separate task (non-blocking)
    let sensitive_tags_for_bot = config.content.sensitive_tags.clone();
    let image_size_for_bot = config.content.image_size.to_pixiv_image_size();
    let download_threshold_for_bot = config.content.download_threshold();
    let bot_handle = tokio::spawn(async move {
        if let Err(e) = bot::run(
            bot,
            config.telegram,
            repo.clone(),
            pixiv_client.clone(),
            notifier.clone(),
            sensitive_tags_for_bot,
            image_size_for_bot,
            download_threshold_for_bot,
        )
        .await
        {
            error!("Bot error: {:#}", e);
        }
    });

    // Wait for shutdown signal
    shutdown_rx.recv().await;
    info!("Shutting down gracefully...");

    // Abort tasks
    bot_handle.abort();
    author_engine_handle.abort();
    ranking_engine_handle.abort();
    name_update_engine_handle.abort();

    info!("✅ Shutdown complete");
    Ok(())
}
