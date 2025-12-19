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
    /// E-Hentai 配置 (可选)
    #[serde(default)]
    #[allow(dead_code)]
    pub ehentai: Option<EhConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub owner_id: Option<i64>,
    #[serde(default)]
    pub bot_mode: BotMode,
    pub api_url: Option<String>,
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
}

impl Default for ContentConfig {
    fn default() -> Self {
        Self {
            sensitive_tags: vec!["R-18".to_string(), "R-18G".to_string(), "NSFW".to_string()],
            image_size: ImageSize::default(),
        }
    }
}

/// E-Hentai 源选择
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)]
pub enum EhSource {
    /// e-hentai.org (不需要登录)
    #[default]
    EHentai,
    /// exhentai.org (需要登录)
    ExHentai,
}

impl EhSource {
    /// 转换为 eh_client::EhSource
    #[allow(dead_code)]
    pub fn to_client_source(self) -> eh_client::EhSource {
        match self {
            EhSource::EHentai => eh_client::EhSource::EHentai,
            EhSource::ExHentai => eh_client::EhSource::ExHentai,
        }
    }
}

/// E-Hentai 订阅输出模式
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)]
pub enum EhOutputMode {
    /// 仅发送通知消息 (带画廊链接)
    #[default]
    NotifyOnly,
    /// 发送预览图片
    Preview,
    /// 发送完整画廊
    Full,
}

/// E-Hentai 配置
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct EhConfig {
    /// 使用的源 (ehentai 或 exhentai)
    #[serde(default)]
    pub source: EhSource,
    /// ipb_member_id cookie (登录后获取)
    pub member_id: Option<String>,
    /// ipb_pass_hash cookie (登录后获取)
    pub pass_hash: Option<String>,
    /// igneous cookie (exhentai 可能需要)
    pub igneous: Option<String>,
    /// 订阅输出模式
    #[serde(default)]
    pub output_mode: EhOutputMode,
    /// 预览图片数量 (用于 Preview 模式)
    #[serde(default = "default_preview_count")]
    pub preview_count: u32,
    /// 搜索最低评分过滤 (2-5)
    #[serde(default)]
    pub min_rating: Option<u8>,
}

fn default_preview_count() -> u32 {
    5
}

#[allow(dead_code)]
impl EhConfig {
    /// 获取凭据 (如果已配置)
    pub fn credentials(&self) -> Option<eh_client::EhCredentials> {
        match (&self.member_id, &self.pass_hash) {
            (Some(member_id), Some(pass_hash)) => Some(eh_client::EhCredentials {
                member_id: member_id.clone(),
                pass_hash: pass_hash.clone(),
                igneous: self.igneous.clone(),
            }),
            _ => None,
        }
    }

    /// 检查是否已配置登录
    pub fn is_authenticated(&self) -> bool {
        self.member_id.is_some() && self.pass_hash.is_some()
    }

    /// 验证配置
    pub fn validate(&self) -> Result<()> {
        // ExHentai requires authentication
        if self.source == EhSource::ExHentai && !self.is_authenticated() {
            anyhow::bail!("ExHentai requires member_id and pass_hash");
        }
        Ok(())
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
