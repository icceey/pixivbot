use serde::Deserialize;

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
    #[serde(default = "default_bot_mode")]
    pub bot_mode: String,
}

fn default_bot_mode() -> String {
    "private".to_string()
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
    /// Minimum random interval in seconds between task executions (default: 30 min)
    #[serde(default = "default_min_task_interval_sec")]
    pub min_task_interval_sec: u64,
    /// Maximum random interval in seconds between task executions (default: 40 min)
    #[serde(default = "default_max_task_interval_sec")]
    pub max_task_interval_sec: u64,
}

fn default_tick_interval_sec() -> u64 {
    30
}

fn default_min_task_interval_sec() -> u64 {
    30 * 60 // 30 minutes
}

fn default_max_task_interval_sec() -> u64 {
    40 * 60 // 40 minutes
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
    pub fn load() -> Result<Self, config::ConfigError> {
        let builder = config::Config::builder()
            .add_source(config::File::with_name("config.toml").required(false))
            .add_source(config::Environment::with_prefix("PIX").separator("__"));

        builder.build()?.try_deserialize()
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
