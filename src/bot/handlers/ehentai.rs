//! E-Hentai 订阅处理器

use crate::bot::BotHandler;
use crate::db::types::TaskType;
use crate::utils::args;
use regex::Regex;
use std::sync::LazyLock;
use teloxide::prelude::*;
use teloxide::types::{ChatAction, ChatId, ParseMode, UserId};
use teloxide::utils::markdown;
use tracing::{error, info, warn};

/// E-Hentai 画廊 URL 解析正则
static EH_GALLERY_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://(?:e-hentai|exhentai)\.org/g/(\d+)/([a-f0-9]+)/?").unwrap()
});

/// E-Hentai 画廊 ID 解析正则 (g=123 or gallery=123)
static EH_GALLERY_ID_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:g|gallery)=(\d+)$").unwrap());

/// 解析画廊 ID 和 Token
#[derive(Debug, Clone)]
pub struct GalleryId {
    pub gid: u64,
    pub token: Option<String>,
}

impl GalleryId {
    /// 从 URL 或 ID 参数解析
    pub fn parse(input: &str) -> Option<Self> {
        // Try URL pattern first
        if let Some(caps) = EH_GALLERY_REGEX.captures(input) {
            let gid = caps.get(1)?.as_str().parse().ok()?;
            let token = caps.get(2).map(|m| m.as_str().to_string());
            return Some(GalleryId { gid, token });
        }

        // Try g=123 or gallery=123 pattern
        if let Some(caps) = EH_GALLERY_ID_REGEX.captures(input) {
            let gid = caps.get(1)?.as_str().parse().ok()?;
            return Some(GalleryId { gid, token: None });
        }

        // Try pure numeric ID
        if let Ok(gid) = input.parse::<u64>() {
            return Some(GalleryId { gid, token: None });
        }

        None
    }

    /// 生成任务值 (用于数据库存储)
    pub fn to_task_value(&self) -> String {
        match &self.token {
            Some(token) => format!("{}/{}", self.gid, token),
            None => self.gid.to_string(),
        }
    }

    /// 从任务值解析
    #[allow(dead_code)]
    pub fn from_task_value(value: &str) -> Option<Self> {
        if let Some((gid_str, token)) = value.split_once('/') {
            let gid = gid_str.parse().ok()?;
            Some(GalleryId {
                gid,
                token: Some(token.to_string()),
            })
        } else {
            let gid = value.parse().ok()?;
            Some(GalleryId { gid, token: None })
        }
    }
}

/// 解析 E-Hentai 搜索参数
#[derive(Debug, Clone, Default)]
pub struct EhSearchParams {
    /// 搜索关键词
    pub query: String,
    /// 最低评分 (2-5)
    pub min_stars: Option<u8>,
    /// 分类过滤
    pub categories: Vec<String>,
}

impl EhSearchParams {
    /// 从参数解析
    pub fn parse(args: &args::ParsedArgs) -> Self {
        let mut params = EhSearchParams {
            query: args.remaining.trim().to_string(),
            min_stars: None,
            categories: Vec::new(),
        };

        // Parse stars parameter
        if let Some(stars_str) = args.get_any(&["stars", "s"]) {
            if let Ok(stars) = stars_str.parse::<u8>() {
                if (2..=5).contains(&stars) {
                    params.min_stars = Some(stars);
                }
            }
        }

        // Parse categories parameter
        if let Some(cats_str) = args.get_any(&["cats", "c", "categories"]) {
            params.categories = cats_str
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
        }

        params
    }

    /// 生成任务值 (用于数据库存储)
    pub fn to_task_value(&self) -> String {
        // Format: query|stars=N|cats=a,b,c
        let mut parts = vec![self.query.clone()];

        if let Some(stars) = self.min_stars {
            parts.push(format!("stars={}", stars));
        }

        if !self.categories.is_empty() {
            parts.push(format!("cats={}", self.categories.join(",")));
        }

        parts.join("|")
    }

    /// 从任务值解析
    pub fn from_task_value(value: &str) -> Self {
        let parts: Vec<&str> = value.split('|').collect();
        let mut params = EhSearchParams::default();

        if let Some(query) = parts.first() {
            params.query = query.to_string();
        }

        for part in parts.iter().skip(1) {
            if let Some(stars_str) = part.strip_prefix("stars=") {
                params.min_stars = stars_str.parse().ok();
            } else if let Some(cats_str) = part.strip_prefix("cats=") {
                params.categories = cats_str.split(',').map(|s| s.to_string()).collect();
            }
        }

        params
    }
}

impl BotHandler {
    /// 检查 E-Hentai 是否已配置
    #[allow(dead_code)]
    pub fn is_ehentai_enabled(&self) -> bool {
        // Check via repo or some runtime flag
        // For now, we assume if the commands are called, they should work
        true
    }

    // ------------------------------------------------------------------------
    // E-Hentai Subscribe
    // ------------------------------------------------------------------------

    /// 订阅 E-Hentai 画廊或搜索
    ///
    /// 用法:
    /// - `/ehsub <画廊URL>` - 订阅画廊更新
    /// - `/ehsub g=123` - 订阅画廊更新 (仅 ID)
    /// - `/ehsub [stars=N] [cats=...] <搜索词>` - 订阅搜索更新
    pub async fn handle_eh_sub(
        &self,
        bot: Bot,
        chat_id: ChatId,
        user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        // Set bot status to typing
        if let Err(e) = bot.send_chat_action(chat_id, ChatAction::Typing).await {
            warn!("Failed to set chat action for chat {}: {:#}", chat_id, e);
        }

        // Parse arguments
        let parsed = args::parse_args(&args_str);

        // Resolve target chat (channel or current)
        let (target_chat_id, is_channel) = match self
            .resolve_subscription_target(&bot, chat_id, user_id, &parsed)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                bot.send_message(chat_id, format!("❌ {}", e)).await?;
                return Ok(());
            }
        };

        // Check if it's a gallery subscription
        let remaining = parsed.remaining.trim();
        if remaining.is_empty() {
            bot.send_message(
                chat_id,
                "❌ 用法:\n\
                • `/ehsub <画廊URL>` \\- 订阅画廊更新\n\
                • `/ehsub g=123` \\- 订阅画廊更新 \\(仅 ID\\)\n\
                • `/ehsub [stars=N] [cats=\\.\\.\\.] <搜索词>` \\- 订阅搜索更新",
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
            return Ok(());
        }

        // Try parsing as gallery first
        if let Some(gallery_id) = GalleryId::parse(remaining) {
            return self
                .handle_eh_sub_gallery(bot, chat_id, target_chat_id, is_channel, gallery_id)
                .await;
        }

        // Otherwise, treat as search subscription
        let search_params = EhSearchParams::parse(&parsed);
        self.handle_eh_sub_search(bot, chat_id, target_chat_id, is_channel, search_params)
            .await
    }

    /// 订阅画廊更新
    async fn handle_eh_sub_gallery(
        &self,
        bot: Bot,
        reply_chat_id: ChatId,
        target_chat_id: ChatId,
        is_channel: bool,
        gallery_id: GalleryId,
    ) -> ResponseResult<()> {
        let task_value = gallery_id.to_task_value();

        info!(
            "Subscribing to E-Hentai gallery {} for chat {}",
            task_value, target_chat_id
        );

        // Create subscription
        match self
            .create_subscription(
                target_chat_id.0,
                TaskType::EhGallery,
                &task_value,
                None, // No author name for galleries
                Default::default(),
            )
            .await
        {
            Ok(_) => {
                let mut message = format!(
                    "✅ 成功订阅 E\\-Hentai 画廊 `{}`",
                    markdown::escape(&task_value)
                );
                if is_channel {
                    message.push_str(&format!("\n📢 频道: `{}`", target_chat_id.0));
                }
                bot.send_message(reply_chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!(
                    "Failed to subscribe to E-Hentai gallery {}: {:#}",
                    task_value, e
                );
                bot.send_message(reply_chat_id, "❌ 订阅失败").await?;
            }
        }

        Ok(())
    }

    /// 订阅搜索更新
    async fn handle_eh_sub_search(
        &self,
        bot: Bot,
        reply_chat_id: ChatId,
        target_chat_id: ChatId,
        is_channel: bool,
        params: EhSearchParams,
    ) -> ResponseResult<()> {
        if params.query.is_empty() {
            bot.send_message(reply_chat_id, "❌ 请提供搜索关键词")
                .await?;
            return Ok(());
        }

        let task_value = params.to_task_value();

        info!(
            "Subscribing to E-Hentai search '{}' for chat {}",
            task_value, target_chat_id
        );

        // Create subscription
        match self
            .create_subscription(
                target_chat_id.0,
                TaskType::EhSearch,
                &task_value,
                None,
                Default::default(),
            )
            .await
        {
            Ok(_) => {
                let mut message = format!(
                    "✅ 成功订阅 E\\-Hentai 搜索: `{}`",
                    markdown::escape(&params.query)
                );
                if let Some(stars) = params.min_stars {
                    message.push_str(&format!("\n⭐ 最低评分: {}", stars));
                }
                if !params.categories.is_empty() {
                    message.push_str(&format!(
                        "\n📂 分类: {}",
                        markdown::escape(&params.categories.join(", "))
                    ));
                }
                if is_channel {
                    message.push_str(&format!("\n📢 频道: `{}`", target_chat_id.0));
                }
                bot.send_message(reply_chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!(
                    "Failed to subscribe to E-Hentai search '{}': {:#}",
                    params.query, e
                );
                bot.send_message(reply_chat_id, "❌ 订阅失败").await?;
            }
        }

        Ok(())
    }

    // ------------------------------------------------------------------------
    // E-Hentai Unsubscribe
    // ------------------------------------------------------------------------

    /// 取消订阅 E-Hentai
    ///
    /// 用法: `/ehunsub <搜索词|画廊ID>`
    pub async fn handle_eh_unsub(
        &self,
        bot: Bot,
        chat_id: ChatId,
        user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        // Parse arguments
        let parsed = args::parse_args(&args_str);

        // Resolve target chat (channel or current)
        let (target_chat_id, is_channel) = match self
            .resolve_subscription_target(&bot, chat_id, user_id, &parsed)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                bot.send_message(chat_id, format!("❌ {}", e)).await?;
                return Ok(());
            }
        };

        let remaining = parsed.remaining.trim();
        if remaining.is_empty() {
            bot.send_message(chat_id, "❌ 用法: `/ehunsub <搜索词|画廊ID>`")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        // Try to find matching subscription
        // First try as gallery ID
        if let Some(gallery_id) = GalleryId::parse(remaining) {
            let task_value = gallery_id.to_task_value();
            if self
                .delete_subscription(target_chat_id.0, TaskType::EhGallery, &task_value)
                .await
                .is_ok()
            {
                let mut message = format!(
                    "✅ 成功取消订阅 E\\-Hentai 画廊 `{}`",
                    markdown::escape(&task_value)
                );
                if is_channel {
                    message.push_str(&format!("\n📢 频道: `{}`", target_chat_id.0));
                }
                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
                return Ok(());
            }
        }

        // Try as search query
        // We need to search for matching task values that start with the query
        // For simplicity, we'll try exact match first
        if self
            .delete_subscription(target_chat_id.0, TaskType::EhSearch, remaining)
            .await
            .is_ok()
        {
            let mut message = format!(
                "✅ 成功取消订阅 E\\-Hentai 搜索 `{}`",
                markdown::escape(remaining)
            );
            if is_channel {
                message.push_str(&format!("\n📢 频道: `{}`", target_chat_id.0));
            }
            bot.send_message(chat_id, message)
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        // Also try matching search with parameters
        let search_params = EhSearchParams::parse(&parsed);
        let task_value = search_params.to_task_value();
        if self
            .delete_subscription(target_chat_id.0, TaskType::EhSearch, &task_value)
            .await
            .is_ok()
        {
            let mut message = format!(
                "✅ 成功取消订阅 E\\-Hentai 搜索 `{}`",
                markdown::escape(&search_params.query)
            );
            if is_channel {
                message.push_str(&format!("\n📢 频道: `{}`", target_chat_id.0));
            }
            bot.send_message(chat_id, message)
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        bot.send_message(chat_id, "❌ 未找到匹配的 E-Hentai 订阅")
            .await?;
        Ok(())
    }

    // ------------------------------------------------------------------------
    // E-Hentai List
    // ------------------------------------------------------------------------

    /// 列出 E-Hentai 订阅
    ///
    /// 用法: `/ehlist [ch=<频道ID>]`
    pub async fn handle_eh_list(
        &self,
        bot: Bot,
        chat_id: ChatId,
        user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        // Parse arguments
        let parsed = args::parse_args(&args_str);

        // Resolve target chat (channel or current)
        let (target_chat_id, is_channel) = match self
            .resolve_subscription_target(&bot, chat_id, user_id, &parsed)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                bot.send_message(chat_id, format!("❌ {}", e)).await?;
                return Ok(());
            }
        };

        // Get all subscriptions and filter for E-Hentai ones
        match self.repo.list_subscriptions_by_chat(target_chat_id.0).await {
            Ok(subscriptions) => {
                let eh_subs: Vec<_> = subscriptions
                    .into_iter()
                    .filter(|(_, task)| task.r#type.is_ehentai())
                    .collect();

                if eh_subs.is_empty() {
                    let msg = if is_channel {
                        format!(
                            "📭 频道 `{}` 没有 E\\-Hentai 订阅。\n\n使用 `/ehsub ch={}` 开始订阅！",
                            target_chat_id.0, target_chat_id.0
                        )
                    } else {
                        "📭 您没有 E\\-Hentai 订阅。\n\n使用 `/ehsub` 开始订阅！".to_string()
                    };
                    bot.send_message(chat_id, msg)
                        .parse_mode(ParseMode::MarkdownV2)
                        .await?;
                    return Ok(());
                }

                let total = eh_subs.len();
                let header = if is_channel {
                    format!(
                        "📋 *频道* `{}` *的 E\\-Hentai 订阅* \\(共 {} 条\\):\n\n",
                        target_chat_id.0, total
                    )
                } else {
                    format!("📋 *您的 E\\-Hentai 订阅* \\(共 {} 条\\):\n\n", total)
                };
                let mut message = header;

                for (_sub, task) in &eh_subs {
                    let type_emoji = match task.r#type {
                        TaskType::EhGallery => "🖼",
                        TaskType::EhSearch => "🔍",
                        _ => "📦",
                    };

                    let display_info = match task.r#type {
                        TaskType::EhGallery => {
                            format!("画廊 `{}`", markdown::escape(&task.value))
                        }
                        TaskType::EhSearch => {
                            let params = EhSearchParams::from_task_value(&task.value);
                            let mut info = format!("搜索: `{}`", markdown::escape(&params.query));
                            if let Some(stars) = params.min_stars {
                                info.push_str(&format!(" ⭐{}", stars));
                            }
                            if !params.categories.is_empty() {
                                info.push_str(&format!(
                                    " 📂{}",
                                    markdown::escape(&params.categories.join(","))
                                ));
                            }
                            info
                        }
                        _ => markdown::escape(&task.value),
                    };

                    message.push_str(&format!("{} {}\n", type_emoji, display_info));
                }

                if is_channel {
                    message.push_str(&format!(
                        "\n💡 使用 `/ehunsub ch={} <ID或搜索词>` 取消订阅",
                        target_chat_id.0
                    ));
                } else {
                    message.push_str("\n💡 使用 `/ehunsub <ID或搜索词>` 取消订阅");
                }

                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!("Failed to list E-Hentai subscriptions: {:#}", e);
                bot.send_message(chat_id, "❌ 获取订阅列表失败").await?;
            }
        }

        Ok(())
    }
}
