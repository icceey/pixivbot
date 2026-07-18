use anyhow::{Context, Result};
use booru_client::{BooruEngineType, BypassConfig};
use serde::Deserialize;

use eh_client::{EhCookies, ImageUploadConfig};

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
    #[serde(default)]
    pub booru: BooruConfig,
    #[serde(default)]
    pub ehentai: EhentaiConfig,
    #[serde(default)]
    pub image_upload: ImageUploadConfig,
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
    pub fn download_threshold(&self) -> u8 {
        self.download_original_threshold.clamp(1, 10)
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct BooruConfig {
    #[serde(default)]
    pub sites: Vec<BooruSiteConfig>,
    #[serde(default = "default_grace_window_hours")]
    pub grace_window_hours: u64,
    #[serde(default = "default_ranking_top_n")]
    pub ranking_top_n: u32,
    #[serde(default = "default_ranking_pushed_cap")]
    pub ranking_pushed_cap: usize,
    #[serde(default = "default_hot_posts_cap")]
    pub hot_posts_cap: usize,
}

impl Default for BooruConfig {
    fn default() -> Self {
        Self {
            sites: Vec::new(),
            grace_window_hours: default_grace_window_hours(),
            ranking_top_n: default_ranking_top_n(),
            ranking_pushed_cap: default_ranking_pushed_cap(),
            hot_posts_cap: default_hot_posts_cap(),
        }
    }
}

fn default_grace_window_hours() -> u64 {
    48
}

fn default_ranking_top_n() -> u32 {
    20
}

fn default_ranking_pushed_cap() -> usize {
    500
}

fn default_hot_posts_cap() -> usize {
    500
}

#[derive(Debug, Deserialize, Clone)]
pub struct BooruSiteConfig {
    pub name: String,
    pub engine_type: BooruEngineType,
    pub base_url: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_booru_min_interval_sec")]
    pub min_interval_sec: u64,
    #[serde(default = "default_booru_max_interval_sec")]
    pub max_interval_sec: u64,
    #[serde(default = "default_booru_page_limit")]
    pub page_limit: u32,
    #[serde(default)]
    pub bypass: Option<BooruBypassConfig>,
}

/// Optional per-site bot-protection bypass. Currently only FlareSolverr is
/// supported; the `mode` discriminator keeps the door open for future
/// strategies (cookie-injection, third-party captcha solver) without
/// breaking existing config files.
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum BooruBypassConfig {
    Flaresolverr {
        endpoint: String,
        #[serde(default = "default_flare_max_timeout_ms")]
        max_timeout_ms: u32,
        #[serde(default)]
        session: Option<String>,
    },
}

impl BooruBypassConfig {
    pub fn to_client_config(&self) -> BypassConfig {
        match self {
            BooruBypassConfig::Flaresolverr {
                endpoint,
                max_timeout_ms,
                session,
            } => {
                let mut cfg = BypassConfig::new(endpoint).with_max_timeout_ms(*max_timeout_ms);
                if let Some(s) = session {
                    cfg = cfg.with_session(s);
                }
                cfg
            }
        }
    }
}

fn default_flare_max_timeout_ms() -> u32 {
    60_000
}

fn default_booru_min_interval_sec() -> u64 {
    30 * 60
}

fn default_booru_max_interval_sec() -> u64 {
    60 * 60
}

fn default_booru_page_limit() -> u32 {
    20
}

// ── E-Hentai / ExHentai config ──────────────────────────────────────────

/// Configuration for e-hentai/exhentai subscription feature.
///
/// EH is enabled by default. Set `enabled = false` to explicitly disable.
/// Uncomment `[ehentai]` to customize site, credentials, and resolutions.
/// For e-hentai, no auth cookies are required (public galleries).
/// For exhentai, `ipb_member_id`, `ipb_pass_hash`, and `igneous` are all required.
#[derive(Debug, Deserialize, Clone)]
pub struct EhentaiConfig {
    /// Whether the E-Hentai / ExHentai feature is enabled (default: true).
    /// Set to `false` to explicitly disable EH regardless of site configuration.
    #[serde(default = "default_eh_enabled")]
    pub enabled: bool,
    /// "e-hentai" or "exhentai" (default: "e-hentai")
    #[serde(default = "default_eh_site")]
    pub site: String,
    #[serde(default)]
    pub ipb_member_id: Option<String>,
    #[serde(default)]
    pub ipb_pass_hash: Option<String>,
    #[serde(default)]
    pub igneous: Option<String>,
    /// Resolution for subscription downloads: "780x", "980x", "1280x" (free),
    /// "1600x"/"2400x" (donors), "original" (costs GP). Default: "1280x" (free max).
    #[serde(default = "default_eh_subscription_resolution")]
    pub subscription_resolution: String,
    /// Resolution for /edl direct downloads. Default: "1280x".
    #[serde(default = "default_eh_download_resolution")]
    pub download_resolution: String,
    /// Whether subscription updates send the archive ZIP (default: true).
    #[serde(default = "default_eh_send_archive")]
    pub send_archive: bool,
    /// Whether subscription updates upload to Telegraph (default: false).
    #[serde(default)]
    pub upload_telegraph: bool,
    #[serde(default = "default_eh_min_interval_sec")]
    pub min_interval_sec: u64,
    #[serde(default = "default_eh_max_interval_sec")]
    pub max_interval_sec: u64,
    /// Telegraph access token for creating Telegraph pages.
    /// Required for Telegraph page creation. Image hosting uses `[image_upload]`.
    #[serde(default)]
    pub telegraph_access_token: Option<String>,
    #[serde(default = "default_eh_max_push_per_tick")]
    pub max_push_per_tick: usize,
    #[serde(default = "default_eh_max_retry_count")]
    pub max_retry_count: u8,
    #[serde(default = "default_eh_scan_window_hours")]
    pub scan_window_hours: u64,
    #[serde(default = "default_eh_download_rate_limit_gb")]
    pub download_rate_limit_gb: u64,
    #[serde(default = "default_eh_download_rate_window_hours")]
    pub download_rate_window_hours: u64,
    /// Maximum EH gallery metadata size allowed for logged-in archive downloads, in MiB.
    /// The check runs before the archive request that can spend EH archive points.
    /// `0` disables this per-gallery archive gate.
    #[serde(default = "default_eh_max_archive_size_mb")]
    pub max_archive_size_mb: u64,
    /// Maximum GP cost allowed for a single archive download. `0` (default) means
    /// only Free / Unlocked archives are downloaded; any gallery that would cost
    /// GP is deferred. Set to a positive value to allow paid downloads up to that
    /// amount. Insufficient Funds / N/A / Unknown always defer regardless of this
    /// setting.
    #[serde(default = "default_eh_max_archive_gp_cost")]
    pub max_archive_gp_cost: u64,
    /// Maximum total GP that can be spent on archive downloads within the rolling
    /// window configured by `gp_rate_window_hours`. `0` (default) means no daily
    /// GP budget beyond the per-archive `max_archive_gp_cost` check.
    #[serde(default = "default_eh_gp_rate_limit")]
    pub gp_rate_limit: u64,
    /// Rolling window length (in hours) for the `gp_rate_limit` GP budget.
    /// Default: 24 (one day).
    #[serde(default = "default_eh_gp_rate_window_hours")]
    pub gp_rate_window_hours: u64,
    #[serde(default = "default_eh_download_poll_interval_sec")]
    pub download_poll_interval_sec: u64,
    #[serde(default = "default_eh_background_download_enabled")]
    pub background_download_enabled: bool,
    #[serde(default = "default_eh_background_download_concurrency")]
    pub background_download_concurrency: usize,
    #[serde(default = "default_eh_background_download_max_attempts")]
    pub background_download_max_attempts: u8,
    #[serde(default = "default_eh_background_download_stale_sec")]
    pub background_download_stale_sec: u64,
    #[serde(default = "default_eh_pushed_cap")]
    pub pushed_cap: usize,
}

impl Default for EhentaiConfig {
    fn default() -> Self {
        Self {
            enabled: default_eh_enabled(),
            site: default_eh_site(),
            ipb_member_id: None,
            ipb_pass_hash: None,
            igneous: None,
            subscription_resolution: default_eh_subscription_resolution(),
            download_resolution: default_eh_download_resolution(),
            send_archive: default_eh_send_archive(),
            upload_telegraph: false,
            min_interval_sec: default_eh_min_interval_sec(),
            max_interval_sec: default_eh_max_interval_sec(),
            telegraph_access_token: None,
            max_push_per_tick: default_eh_max_push_per_tick(),
            max_retry_count: default_eh_max_retry_count(),
            scan_window_hours: default_eh_scan_window_hours(),
            download_rate_limit_gb: default_eh_download_rate_limit_gb(),
            download_rate_window_hours: default_eh_download_rate_window_hours(),
            max_archive_size_mb: default_eh_max_archive_size_mb(),
            max_archive_gp_cost: default_eh_max_archive_gp_cost(),
            gp_rate_limit: default_eh_gp_rate_limit(),
            gp_rate_window_hours: default_eh_gp_rate_window_hours(),
            download_poll_interval_sec: default_eh_download_poll_interval_sec(),
            background_download_enabled: default_eh_background_download_enabled(),
            background_download_concurrency: default_eh_background_download_concurrency(),
            background_download_max_attempts: default_eh_background_download_max_attempts(),
            background_download_stale_sec: default_eh_background_download_stale_sec(),
            pushed_cap: default_eh_pushed_cap(),
        }
    }
}

impl EhentaiConfig {
    /// Build EhCookies from config values.
    pub fn to_cookies(&self) -> EhCookies {
        EhCookies {
            ipb_member_id: self.ipb_member_id.clone(),
            ipb_pass_hash: self.ipb_pass_hash.clone(),
            igneous: self.igneous.clone(),
            nw: true,
        }
    }

    /// Check if exhentai mode is enabled with all required cookies.
    pub fn is_exhentai_ready(&self) -> bool {
        self.site == "exhentai" && self.to_cookies().is_exhentai_capable()
    }

    /// Check if the feature is enabled (explicit flag + supported site).
    pub fn is_enabled(&self) -> bool {
        self.enabled && matches!(self.site.as_str(), "exhentai" | "e-hentai")
    }

    /// Download rate limit in bytes.
    pub fn download_rate_limit_bytes(&self) -> u64 {
        self.download_rate_limit_gb * 1024 * 1024 * 1024
    }

    /// Maximum EH gallery archive size in bytes, or `None` when the gate is disabled.
    ///
    /// `max_archive_size_mb = 0` disables the per-gallery archive size gate.
    pub fn max_archive_size_bytes(&self) -> Option<u64> {
        if self.max_archive_size_mb == 0 {
            None
        } else {
            Some(self.max_archive_size_mb.saturating_mul(1024 * 1024))
        }
    }

    /// Returns true if a download with the given GP cost is allowed by the
    /// per-archive `max_archive_gp_cost` setting.
    ///
    /// - Free / Unlocked: always allowed (no GP spent).
    /// - `Gp(n)` with `n > 0`: allowed iff `n <= max_archive_gp_cost`.
    ///   When `max_archive_gp_cost == 0` (default), any non-zero GP cost is
    ///   rejected, since 0 means "only free downloads".
    /// - `Gp(0)`: treated as a non-free variant (the page said "0 GP" rather
    ///   than "Free!"), so it is rejected when `max_archive_gp_cost == 0`.
    /// - Insufficient / Unavailable / Unknown: never allowed (conservative reject).
    pub fn allows_archive_gp_cost(&self, cost: &eh_client::parser::DownloadCost) -> bool {
        use eh_client::parser::DownloadCost;
        match cost {
            DownloadCost::Free | DownloadCost::Unlocked => true,
            DownloadCost::Gp(n) => {
                if self.max_archive_gp_cost == 0 {
                    false
                } else {
                    *n <= self.max_archive_gp_cost
                }
            }
            DownloadCost::Insufficient | DownloadCost::Unavailable | DownloadCost::Unknown => false,
        }
    }

    /// Rolling GP rate window in hours, clamped to a minimum of 1 to avoid
    /// divide-by-zero and meaningless zero-length windows.
    pub fn gp_rate_window_hours_clamped(&self) -> u64 {
        self.gp_rate_window_hours.max(1)
    }
}

fn default_eh_enabled() -> bool {
    true
}

fn default_eh_site() -> String {
    "e-hentai".to_string()
}

fn default_eh_subscription_resolution() -> String {
    "1280x".to_string()
}

fn default_eh_download_resolution() -> String {
    "1280x".to_string()
}

fn default_eh_send_archive() -> bool {
    true
}

fn default_eh_min_interval_sec() -> u64 {
    30 * 60
}

fn default_eh_max_interval_sec() -> u64 {
    60 * 60
}

fn default_eh_max_push_per_tick() -> usize {
    3
}

fn default_eh_max_retry_count() -> u8 {
    3
}

fn default_eh_scan_window_hours() -> u64 {
    48
}

fn default_eh_download_rate_limit_gb() -> u64 {
    7
}

fn default_eh_download_rate_window_hours() -> u64 {
    168
}

fn default_eh_max_archive_size_mb() -> u64 {
    300
}

fn default_eh_max_archive_gp_cost() -> u64 {
    0
}

fn default_eh_gp_rate_limit() -> u64 {
    0
}

fn default_eh_gp_rate_window_hours() -> u64 {
    24
}

fn default_eh_download_poll_interval_sec() -> u64 {
    60
}

fn default_eh_background_download_enabled() -> bool {
    true
}

fn default_eh_background_download_concurrency() -> usize {
    2
}

fn default_eh_background_download_max_attempts() -> u8 {
    6
}

fn default_eh_background_download_stale_sec() -> u64 {
    3600
}

fn default_eh_pushed_cap() -> usize {
    500
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

    #[test]
    fn test_eh_enabled_defaults_true() {
        let cfg = EhentaiConfig::default();
        assert!(cfg.enabled);
        assert!(cfg.is_enabled());
    }

    #[test]
    fn test_eh_enabled_false_disables_supported_site() {
        let cfg = EhentaiConfig {
            enabled: false,
            site: "e-hentai".to_string(),
            ..Default::default()
        };
        assert!(!cfg.is_enabled());
    }

    #[test]
    fn test_eh_max_archive_size_defaults_to_300_mib() {
        let cfg = EhentaiConfig::default();
        assert_eq!(cfg.max_archive_size_mb, 300);
        assert_eq!(cfg.max_archive_size_bytes(), Some(300 * 1024 * 1024));
    }

    #[test]
    fn test_eh_max_archive_size_zero_disables_limit() {
        let cfg = EhentaiConfig {
            max_archive_size_mb: 0,
            ..Default::default()
        };
        assert_eq!(cfg.max_archive_size_bytes(), None);
    }
}
