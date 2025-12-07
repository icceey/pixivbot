use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum BotMode {
    #[default]
    Private,
    Public,
}

impl BotMode {
    pub fn is_public(&self) -> bool {
        matches!(self, Self::Public)
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub telegram: TelegramConfig,
    pub pixiv: PixivConfig,
    pub database: DatabaseConfig,
    pub logging: LoggingConfig,
    pub scheduler: SchedulerConfig,
    #[serde(default)]
    pub content: ContentConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub owner_id: Option<i64>,
    #[serde(default)]
    pub bot_mode: BotMode,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PixivConfig {
    pub refresh_token: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DatabaseConfig {
    pub url: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LoggingConfig {
    pub level: String,
    pub dir: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            dir: "data/logs".to_string(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct SchedulerConfig {
    /// Tick interval in seconds (how often to check for pending tasks)
    #[serde(default = "default_tick_interval_sec")]
    pub tick_interval_sec: u64,
    /// Minimum random interval in seconds between task executions (default: 2 hours)
    #[serde(default = "default_min_task_interval_sec")]
    pub min_task_interval_sec: u64,
    /// Maximum random interval in seconds between task executions (default: 3 hours)
    #[serde(default = "default_max_task_interval_sec")]
    pub max_task_interval_sec: u64,
    /// Cache retention period in days (default: 7 days)
    #[serde(default = "default_cache_retention_days")]
    pub cache_retention_days: u64,
    /// Cache directory path (default: "data/cache")
    #[serde(default = "default_cache_dir")]
    pub cache_dir: String,
    /// Maximum retry count for failed pushes (default: 3, <=0 means no retry)
    #[serde(default = "default_max_retry_count")]
    pub max_retry_count: i32,
}

fn default_tick_interval_sec() -> u64 {
    30
}

fn default_min_task_interval_sec() -> u64 {
    2 * 60 * 60 // 120 minutes
}

fn default_max_task_interval_sec() -> u64 {
    3 * 60 * 60 // 180 minutes
}

fn default_cache_retention_days() -> u64 {
    7 // 7 days
}

fn default_cache_dir() -> String {
    "data/cache".to_string()
}

fn default_max_retry_count() -> i32 {
    3
}

#[derive(Debug, Deserialize, Clone)]
pub struct ContentConfig {
    #[serde(default)]
    pub sensitive_tags: Vec<String>,
}

impl Default for ContentConfig {
    fn default() -> Self {
        Self {
            sensitive_tags: vec!["R-18".to_string(), "R-18G".to_string(), "NSFW".to_string()],
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let builder = config::Config::builder()
            .add_source(config::File::with_name("config.toml").required(false))
            .add_source(config::Environment::with_prefix("PIX").separator("__"));

        builder
            .build()
            .context("Failed to build configuration")?
            .try_deserialize()
            .context("Failed to deserialize configuration")
    }

    pub fn log_level(&self) -> tracing::Level {
        match self.logging.level.to_lowercase().as_str() {
            "error" => tracing::Level::ERROR,
            "warn" => tracing::Level::WARN,
            "info" => tracing::Level::INFO,
            "debug" => tracing::Level::DEBUG,
            "trace" => tracing::Level::TRACE,
            _ => tracing::Level::INFO,
        }
    }
}
