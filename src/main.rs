mod booru;
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
use teloxide::requests::RequesterExt;
use tracing::{error, info, warn};
use tracing_subscriber::fmt::time::ChronoLocal;
use tracing_subscriber::{prelude::*, EnvFilter};

fn should_build_eh_image_uploader(
    eh_enabled: bool,
    telegraph_enabled: bool,
    provider: eh_client::ImageUploadProvider,
) -> bool {
    eh_enabled
        && (telegraph_enabled
            || matches!(
                provider,
                eh_client::ImageUploadProvider::S3 | eh_client::ImageUploadProvider::IpfS3
            ))
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load configuration
    let config = Config::load()?;

    // Initialize variables
    let log_level = config.log_level();
    let log_dir = &config.logging.dir;

    // Create log directory if it doesn't exist
    std::fs::create_dir_all(log_dir)?;

    // Setup file appender (single file, no rotation)
    let file_appender = tracing_appender::rolling::never(log_dir, "pixivbot.log");
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
        .add_directive("sea_orm=warn".parse().unwrap())
        .add_directive("hyper_util=warn".parse().unwrap());

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
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36")
        .build()?;
    let downloader = std::sync::Arc::new(pixiv::downloader::Downloader::new(
        http_client,
        cache_manager,
    ));
    info!("✅ Downloader initialized");

    info!("PixivBot initialization complete");

    // Initialize Telegram Bot with automatic rate limiting
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

    // Wrap bot with Throttle adaptor for automatic rate limiting
    // This replaces manual sleep() calls throughout the codebase
    let bot = bot.throttle(teloxide::adaptors::throttle::Limits::default());
    info!("✅ Telegram bot initialized with automatic rate limiting");

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

    let booru_registry = booru::BooruSiteRegistry::from_configs(&config.booru.sites);

    let booru_engine_handle = if !booru_registry.is_empty() {
        let booru_engine = scheduler::BooruEngine::new(
            repo.clone(),
            notifier.clone(),
            scheduler_config.tick_interval_sec,
            scheduler_config.max_retry_count,
            booru_registry.clone(),
            std::sync::Arc::new(config.booru.clone()),
        );
        info!(
            "✅ Booru engine initialized with {} site(s)",
            booru_registry.len()
        );
        Some(tokio::spawn(async move {
            booru_engine.run().await;
        }))
    } else {
        info!("No booru sites configured, skipping booru engine");
        None
    };

    // Initialize E-Hentai client and engines
    let eh_client: Option<std::sync::Arc<eh_client::EhClient>> = if config.ehentai.is_enabled() {
        if config.ehentai.site == "exhentai" && !config.ehentai.is_exhentai_ready() {
            tracing::warn!(
                "ExHentai enabled but missing required cookies (ipb_member_id, ipb_pass_hash, \
                 igneous). EH feature disabled."
            );
            None
        } else {
            let site = &config.ehentai.site;
            let base_url = if site == "exhentai" {
                "https://exhentai.org"
            } else {
                "https://e-hentai.org"
            };
            let api_url = "https://api.e-hentai.org/api.php";
            let cookies = config.ehentai.to_cookies();

            match eh_client::EhClient::new(base_url, api_url, cookies) {
                Ok(client) => {
                    info!(
                        "✅ E-Hentai client initialized (site: {})",
                        config.ehentai.site
                    );
                    Some(std::sync::Arc::new(client))
                }
                Err(e) => {
                    error!("Failed to initialize EH client: {:#}", e);
                    None
                }
            }
        }
    } else {
        info!("E-Hentai not configured, skipping EH engines");
        None
    };

    let telegraph_client = if let Some(token) = config.ehentai.telegraph_access_token.as_ref() {
        Some(std::sync::Arc::new(eh_client::TelegraphClient::new(
            token.clone(),
        )))
    } else if config.ehentai.upload_telegraph {
        match eh_client::TelegraphClient::create_account("PixivBot", Some("PixivBot"), None).await {
            Ok(client) => {
                info!("✅ Telegraph account auto-created for this process");
                Some(std::sync::Arc::new(client))
            }
            Err(e) => {
                warn!(
                    "ehentai.upload_telegraph=true but Telegraph account auto-creation failed; \
                     Telegraph upload is disabled: {:#}",
                    e
                );
                None
            }
        }
    } else {
        None
    };
    let eh_telegraph_rewrite_config = config.image_upload.ipfs3_preview_rewrite_config();
    let eh_telegraph_rewrite_enabled =
        telegraph_client.is_some() && eh_telegraph_rewrite_config.is_some();

    // The provider Abort capability must exist before startup takes ownership
    // of orphaned or durable cleanup generations.
    let eh_image_uploader = if should_build_eh_image_uploader(
        eh_client.is_some(),
        telegraph_client.is_some(),
        config.image_upload.provider,
    ) {
        Some(config.image_upload.build_uploader().await?)
    } else {
        None
    };
    let eh_startup_abort_uploader = if matches!(
        config.image_upload.provider,
        eh_client::ImageUploadProvider::S3 | eh_client::ImageUploadProvider::IpfS3
    ) {
        eh_image_uploader.clone()
    } else {
        None
    };
    let eh_cache_dir = std::path::PathBuf::from(&config.scheduler.cache_dir);

    // Startup ownership order is strict: recover durable claims, preserve every
    // persisted job family during orphan cleanup, drain due cleanup, then make
    // configuration-specific state changes before spawning workers.
    if eh_client.is_some() {
        if let Err(e) = repo
            .reset_stale_eh_shared_work(
                config.ehentai.background_download_stale_sec,
                config.ehentai.background_download_stale_sec as i64,
            )
            .await
        {
            tracing::warn!("Failed to reset stale shared EH work: {:#}", e);
        }
        if let Err(e) = repo
            .reconcile_eh_shared_job_liveness(config.ehentai.send_archive)
            .await
        {
            tracing::warn!("Failed to reconcile shared EH job liveness: {:#}", e);
        }
        if let Err(e) = repo
            .cleanup_eh_cache_orphans(
                &eh_cache_dir.join("eh_cache"),
                eh_startup_abort_uploader.as_deref(),
            )
            .await
        {
            tracing::warn!(
                "Failed to cleanup orphaned shared EH cache families: {:#}",
                e
            );
        }
        if let Err(e) = scheduler::drain_eh_job_cleanup_maintenance(
            repo.as_ref(),
            eh_startup_abort_uploader.as_deref(),
            config.ehentai.download_poll_interval_sec as i64,
            config.ehentai.send_archive,
        )
        .await
        {
            tracing::warn!("Failed to drain shared EH artifact cleanup: {:#}", e);
        }
        if !config.ehentai.background_download_enabled {
            if let Err(e) = repo
                .release_eh_job_background_downloads_to_main_queue()
                .await
            {
                tracing::warn!(
                    "Failed to release shared EH background downloads to main queue: {:#}",
                    e
                );
            }
        }
        if telegraph_client.is_none() {
            match repo.disable_eh_telegraph_for_unuploaded_jobs().await {
                Ok(count) if count > 0 => warn!(
                    "Disabled Telegraph upload requirement for {} shared EH jobs because no telegraph token is configured",
                    count
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!(
                    "Failed to disable unclaimed shared EH Telegraph jobs without token: {:#}",
                    e
                ),
            }
        }
    }

    let eh_engine_handle = if let Some(ref eh_client) = eh_client {
        let eh_engine = scheduler::EhEngine::new(
            repo.clone(),
            std::sync::Arc::clone(eh_client),
            std::sync::Arc::new(config.ehentai.clone()),
            telegraph_client.is_some(),
            scheduler_config.tick_interval_sec,
        );
        info!("✅ E-Hentai engine initialized");
        Some(tokio::spawn(async move {
            eh_engine.run().await;
        }))
    } else {
        None
    };

    let eh_download_worker_handle = if let Some(ref eh_client) = eh_client {
        let worker = scheduler::EhDownloadWorker::new(
            repo.clone(),
            std::sync::Arc::clone(eh_client),
            std::sync::Arc::new(config.ehentai.clone()),
            eh_cache_dir.clone(),
            eh_startup_abort_uploader.clone(),
        );
        info!("✅ E-Hentai download worker initialized");
        Some(tokio::spawn(async move { worker.run().await }))
    } else {
        None
    };

    let eh_background_download_worker_handle = if let Some(ref eh_client) = eh_client {
        if config.ehentai.background_download_enabled {
            let worker = scheduler::EhBackgroundDownloadWorker::new(
                repo.clone(),
                std::sync::Arc::clone(eh_client),
                std::sync::Arc::new(config.ehentai.clone()),
                eh_cache_dir.clone(),
            );
            info!("✅ E-Hentai background download worker initialized");
            Some(tokio::spawn(async move { worker.run().await }))
        } else {
            info!("E-Hentai background download worker disabled");
            None
        }
    } else {
        None
    };

    let eh_upload_worker_handle = if eh_client.is_some() {
        if let (Some(telegraph), Some(image_uploader)) =
            (telegraph_client.as_ref(), eh_image_uploader.as_ref())
        {
            let worker = scheduler::EhUploadWorker::new_with_abort_uploader(
                repo.clone(),
                notifier.clone(),
                std::sync::Arc::clone(telegraph),
                std::sync::Arc::clone(image_uploader),
                eh_startup_abort_uploader.clone(),
                eh_telegraph_rewrite_config.clone(),
                std::sync::Arc::new(config.ehentai.clone()),
            );
            info!("✅ E-Hentai upload worker initialized");
            Some(tokio::spawn(async move { worker.run().await }))
        } else {
            info!("E-Hentai upload worker disabled (no telegraph token)");
            None
        }
    } else {
        None
    };

    let eh_publish_worker_handle = if let Some(ref eh_client) = eh_client {
        let worker = scheduler::EhPublishWorker::new_with_abort_uploader(
            repo.clone(),
            notifier.clone(),
            std::sync::Arc::clone(eh_client),
            if eh_telegraph_rewrite_enabled {
                eh_telegraph_rewrite_config
                    .as_ref()
                    .map(|rewrite| rewrite.delay_sec)
            } else {
                None
            },
            eh_startup_abort_uploader.clone(),
            std::sync::Arc::new(config.ehentai.clone()),
        );
        info!("✅ E-Hentai publish worker initialized");
        Some(tokio::spawn(async move { worker.run().await }))
    } else {
        None
    };

    let eh_telegraph_rewrite_worker_handle = if eh_client.is_some() {
        if eh_telegraph_rewrite_enabled {
            if let Some(ref telegraph) = telegraph_client {
                let worker = scheduler::EhTelegraphRewriteWorker::new(
                    repo.clone(),
                    std::sync::Arc::clone(telegraph),
                    config.ehentai.send_archive,
                    std::sync::Arc::new(config.ehentai.clone()),
                );
                info!("✅ E-Hentai Telegraph rewrite worker initialized");
                Some(tokio::spawn(async move { worker.run().await }))
            } else {
                None
            }
        } else {
            info!("E-Hentai Telegraph rewrite worker disabled (no preview gateway rewrite config)");
            None
        }
    } else {
        None
    };

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
    let cache_dir_for_bot = config.scheduler.cache_dir.clone();
    let log_dir_for_bot = config.logging.dir.clone();
    let booru_registry_for_bot = booru_registry.clone();
    let eh_client_for_bot = eh_client.clone();
    let eh_config_for_bot = std::sync::Arc::new(config.ehentai.clone());
    let has_telegraph_for_bot = telegraph_client.is_some();
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
            cache_dir_for_bot,
            log_dir_for_bot,
            booru_registry_for_bot,
            eh_client_for_bot,
            eh_config_for_bot,
            has_telegraph_for_bot,
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
    if let Some(handle) = booru_engine_handle {
        handle.abort();
    }
    if let Some(handle) = eh_engine_handle {
        handle.abort();
    }
    if let Some(handle) = eh_download_worker_handle {
        handle.abort();
    }
    if let Some(handle) = eh_background_download_worker_handle {
        handle.abort();
    }
    if let Some(handle) = eh_upload_worker_handle {
        handle.abort();
    }
    if let Some(handle) = eh_publish_worker_handle {
        handle.abort();
    }
    if let Some(handle) = eh_telegraph_rewrite_worker_handle {
        handle.abort();
    }

    info!("✅ Shutdown complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use eh_client::ImageUploadProvider;

    use super::should_build_eh_image_uploader;

    #[test]
    fn eh_image_uploader_policy_covers_telegraph_and_abort_matrix() {
        let providers = [
            ImageUploadProvider::Pixi,
            ImageUploadProvider::S3,
            ImageUploadProvider::Catbox,
            ImageUploadProvider::IpfS3,
        ];

        for provider in providers {
            assert!(!should_build_eh_image_uploader(false, false, provider));
            assert!(!should_build_eh_image_uploader(false, true, provider));
            assert!(should_build_eh_image_uploader(true, true, provider));
        }

        assert!(!should_build_eh_image_uploader(
            true,
            false,
            ImageUploadProvider::Pixi,
        ));
        assert!(should_build_eh_image_uploader(
            true,
            false,
            ImageUploadProvider::S3,
        ));
        assert!(!should_build_eh_image_uploader(
            true,
            false,
            ImageUploadProvider::Catbox,
        ));
        assert!(should_build_eh_image_uploader(
            true,
            false,
            ImageUploadProvider::IpfS3,
        ));
    }
}
