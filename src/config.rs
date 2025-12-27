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
    pub api_url: Option<String>,
    /// Whether to require @mention to respond in group chats (default: true)
    /// When true, the bot only responds to messages in groups when @mentioned or replied to
    /// When false, the bot responds to all messages in groups without requiring @mention
    #[serde(default = "default_require_mention_in_group")]
    pub require_mention_in_group: bool,
}

fn default_require_mention_in_group() -> bool {
    true
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
    /// Ranking task execution time in HH:MM format (default: "19:00")
    #[serde(default = "default_ranking_execution_time")]
    pub ranking_execution_time: String,
    /// Author name update time in HH:MM format (default: "21:00")
    /// Updates author names daily to sync with Pixiv profile changes
    #[serde(default = "default_author_name_update_time")]
    pub author_name_update_time: String,
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

fn default_ranking_execution_time() -> String {
    "19:00".to_string()
}

fn default_author_name_update_time() -> String {
    "21:00".to_string()
}

/// 图片尺寸选项
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ImageSize {
    /// 原图 (最高质量)
    Original,
    /// 大图 (推荐，平衡质量和大小)
    #[default]
    Large,
    /// 中图
    Medium,
    /// 正方形缩略图
    SquareMedium,
}

impl ImageSize {
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            ImageSize::Original => "original",
            ImageSize::Large => "large",
            ImageSize::Medium => "medium",
            ImageSize::SquareMedium => "square_medium",
        }
    }

    /// Convert to pixiv_client::ImageSize
    pub fn to_pixiv_image_size(self) -> pixiv_client::ImageSize {
        match self {
            ImageSize::Original => pixiv_client::ImageSize::Original,
            ImageSize::Large => pixiv_client::ImageSize::Large,
            ImageSize::Medium => pixiv_client::ImageSize::Medium,
            ImageSize::SquareMedium => pixiv_client::ImageSize::SquareMedium,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ContentConfig {
    #[serde(default)]
    pub sensitive_tags: Vec<String>,
    /// 推送图片时使用的默认尺寸 (original, large, medium, square_medium)
    /// 默认: large (平衡质量和大小)
    /// 注意: 下载功能永远使用原图
    #[serde(default)]
    pub image_size: ImageSize,
    /// 下载时发送原图的阈值 (1-10)
    /// 图片总数不超过此值时逐张发送原图，超过时打包为 ZIP
    /// 默认: 1
    #[serde(default = "default_download_original_threshold")]
    pub download_original_threshold: u8,
}

fn default_download_original_threshold() -> u8 {
    1
}

impl Default for ContentConfig {
    fn default() -> Self {
        Self {
            sensitive_tags: vec!["R-18".to_string(), "R-18G".to_string(), "NSFW".to_string()],
            image_size: ImageSize::default(),
            download_original_threshold: default_download_original_threshold(),
        }
    }
}

impl ContentConfig {
    /// 获取经过验证的下载原图阈值 (限制在 1-10 范围内)
    pub fn download_threshold(&self) -> u8 {
        self.download_original_threshold.clamp(1, 10)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_download_threshold_default() {
        let config = ContentConfig::default();
        assert_eq!(config.download_threshold(), 1);
    }

    #[test]
    fn test_download_threshold_clamped() {
        // Test lower bound clamping (0 -> 1)
        let config = ContentConfig {
            download_original_threshold: 0,
            ..Default::default()
        };
        assert_eq!(config.download_threshold(), 1);

        // Test upper bound clamping (15 -> 10)
        let config = ContentConfig {
            download_original_threshold: 15,
            ..Default::default()
        };
        assert_eq!(config.download_threshold(), 10);

        // Test within range (5 -> 5)
        let config = ContentConfig {
            download_original_threshold: 5,
            ..Default::default()
        };
        assert_eq!(config.download_threshold(), 5);

        // Test exact boundaries
        let config = ContentConfig {
            download_original_threshold: 1,
            ..Default::default()
        };
        assert_eq!(config.download_threshold(), 1);

        let config = ContentConfig {
            download_original_threshold: 10,
            ..Default::default()
        };
        assert_eq!(config.download_threshold(), 10);
    }
}
