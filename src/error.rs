use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("Configuration error: {0}")]
    Config(#[from] config::ConfigError),
    
    #[error("Database error: {0}")]
    Database(#[from] sea_orm::DbErr),
    
    #[error("Telegram bot error: {0}")]
    Telegram(#[from] teloxide::RequestError),
    
    #[error("Pixiv API error: {0}")]
    Pixiv(String),
    
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    
    #[error("JSON serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    
    #[error("Chrono parsing error: {0}")]
    Chrono(#[from] chrono::ParseError),
    
    #[error("Anyhow error: {0}")]
    Anyhow(#[from] anyhow::Error),
    
    #[error("Custom error: {0}")]
    Custom(String),
}

pub type Result<T> = std::result::Result<T, AppError>;