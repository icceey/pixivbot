#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Database error: {0}")]
    Database(#[from] sea_orm::DbErr),
    
    #[error("Configuration error: {0}")]
    Config(#[from] config::ConfigError),
    
    #[error("Telegram Bot error: {0}")]
    Telegram(String),
    
    #[error("Pixiv API error: {0}")]
    PixivError(#[from] pixivrs::error::PixivError),
    
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Job Scheduler error: {0}")]
    JobScheduler(#[from] tokio_cron_scheduler::JobSchedulerError),

    #[error("Unknown error: {0}")]
    Unknown(String),
}

pub type AppResult<T> = Result<T, AppError>;
