#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Database error: {0}")]
    Database(#[from] sea_orm::DbErr),
    
    #[error("Configuration error: {0}")]
    Config(#[from] config::ConfigError),
    
    #[error("Telegram Bot error: {0}")]
    Telegram(String),
    
    #[error("Pixiv API error: {0}")]
    PixivError(String),
    
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Job Scheduler error: {0}")]
    JobScheduler(#[from] tokio_cron_scheduler::JobSchedulerError),

    #[error("Unknown error: {0}")]
    Unknown(String),
}

impl From<crate::pixiv_client::Error> for AppError {
    fn from(err: crate::pixiv_client::Error) -> Self {
        AppError::PixivError(err.to_string())
    }
}

impl From<reqwest::Error> for AppError {
    fn from(err: reqwest::Error) -> Self {
        AppError::Unknown(format!("Reqwest error: {}", err))
    }
}

pub type AppResult<T> = Result<T, AppError>;
