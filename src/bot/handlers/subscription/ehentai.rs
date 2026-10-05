use crate::bot::notifier::ThrottledBot;
use crate::bot::BotHandler;
use crate::db::repo::eh_download_queue::{
    EhQueueSnapshot, EhQueueStatusItem, BACKGROUND_STATUS_PENDING, BACKGROUND_STATUS_RUNNING,
    SOURCE_DIRECT, STATUS_CANCELED, STATUS_DONE, STATUS_DOWNLOADED, STATUS_DOWNLOADING,
    STATUS_FAILED, STATUS_PENDING, STATUS_PUBLISHING, STATUS_UPLOADED, STATUS_UPLOADING,
};
use crate::db::repo::eh_gallery_jobs::EhGalleryVariant;
use crate::db::types::{EhFilter, EhTaskKey, TagFilter, TaskType};
use crate::utils::args;
use eh_client::EhCategory;
use teloxide::prelude::*;
use teloxide::types::{ChatId, ParseMode, UserId};
use teloxide::utils::markdown;
use tracing::{error, warn};

const EH_QUEUE_MAX_VISIBLE_ACTIVE_ITEMS: usize = 20;
const EH_QUEUE_MAX_TITLE_CHARS: usize = 80;
const TELEGRAM_MAX_MESSAGE_UTF16_UNITS: usize = 4096;
const EH_QUEUE_ACTIVE_STAGE_ORDER: [&str; 8] = [
    "后台下载中",
    "后台排队",
    "排队中",
    "下载中",
    "等待上传或发送",
    "上传中",
    "等待发送",
    "发送中",
];

impl BotHandler {
    pub async fn handle_esub(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        _user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        if self.eh_client.is_none() {
            let _ = bot.send_message(chat_id, "E-Hentai 功能未启用").await;
            return Ok(());
        }

        let parsed = args::parse_args(&args_str);

        // Resolve target chat (ch= param)
        let (target_chat, _is_channel) = match self
            .resolve_subscription_target(&bot, chat_id, _user_id, &parsed)
            .await
        {
            Ok((chat_id, is_ch)) => (chat_id, is_ch),
            Err(e) => {
                let _ = bot
                    .send_message(chat_id, format!("❌ {}", markdown::escape(&e)))
                    .parse_mode(ParseMode::MarkdownV2)
                    .await;
                return Ok(());
            }
        };
        let target_chat_id = target_chat.0;

        let remaining = parsed.remaining.trim();
        if remaining.is_empty() {
            let _ = bot
                .send_message(
                    chat_id,
                    "用法: /esub <搜索词> [过滤条件]\n\n\
                     过滤条件:\n\
                     • rating>=N / rating>N — 最低评分 (支持小数，2-5；默认扫描窗口48h)\n\
                     • pages>=N — 最低页数\n\
                     • pages<=N — 最高页数\n\
                     • cat=<类别> — 分类筛选 (逗号分隔)\n\
                     • telegraph=on — 启用 Telegraph 上传",
                )
                .await;
            return Ok(());
        }

        let parsed_esub = match parse_esub_remaining(remaining) {
            Ok(parsed) => parsed,
            Err(e) => {
                let _ = bot
                    .send_message(chat_id, format!("❌ {}", markdown::escape(&e)))
                    .parse_mode(ParseMode::MarkdownV2)
                    .await;
                return Ok(());
            }
        };
        let query = parsed_esub.query;
        let filter_args = parsed_esub.filter_args;
        let cat_str = parsed_esub.cat_str;
        let telegraph_on = parsed_esub.telegraph_on;

        // Parse filter
        let mut eh_filter = match parse_eh_filter(&filter_args) {
            Ok(f) => f,
            Err(e) => {
                let _ = bot
                    .send_message(chat_id, format!("❌ {}", markdown::escape(&e)))
                    .parse_mode(ParseMode::MarkdownV2)
                    .await;
                return Ok(());
            }
        };
        eh_filter.telegraph = telegraph_on;

        // Reject telegraph=on when Telegraph is not configured
        if telegraph_on && !self.has_telegraph {
            let _ = bot
                .send_message(
                    chat_id,
                    "❌ Telegraph 未配置，无法启用 telegraph=on。请配置 ehentai.telegraph_access_token 后重试。",
                )
                .await;
            return Ok(());
        }

        // Parse category bitmask
        let cats = match parse_eh_category_bitmask(cat_str.as_deref()) {
            Ok(cats) => cats,
            Err(e) => {
                let _ = bot
                    .send_message(chat_id, format!("❌ {}", markdown::escape(&e)))
                    .parse_mode(ParseMode::MarkdownV2)
                    .await;
                return Ok(());
            }
        };

        // Build task key
        let task_key = EhTaskKey::new(&query, cats, &eh_filter);
        let task_value = task_key.to_task_value();

        // Create subscription
        if let Err(e) = self
            .create_eh_subscription(
                target_chat_id,
                TaskType::Ehentai,
                &task_value,
                None,
                TagFilter::default(),
                eh_filter.clone(),
            )
            .await
        {
            error!("Failed to create eh subscription: {:#}", e);
            let _ = bot
                .send_message(chat_id, "❌ 创建订阅失败，请稍后重试")
                .await;
            return Ok(());
        }

        // Build success message
        let mut msg = format!(
            "✅ 已订阅 {}: {}\n",
            markdown::escape("E-Hentai"),
            markdown::escape(&query)
        );
        if cats > 0 {
            msg.push_str(&format!(
                "分类: {}\n",
                markdown::escape(cat_str.as_deref().unwrap_or_default())
            ));
        }
        let filter_display = eh_filter.format_for_display();
        if !filter_display.is_empty() {
            msg.push_str(&format!("过滤: {}", markdown::escape(&filter_display)));
        }
        if target_chat_id != chat_id.0 {
            msg.push_str(&format!("\n目标: `{}`", target_chat_id));
        }

        let _ = bot
            .send_message(chat_id, msg)
            .parse_mode(ParseMode::MarkdownV2)
            .await;

        Ok(())
    }

    pub async fn handle_eunsub(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        _user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        let parsed = args::parse_args(&args_str);

        let (target_chat, _is_channel) = match self
            .resolve_subscription_target(&bot, chat_id, _user_id, &parsed)
            .await
        {
            Ok((chat_id, is_ch)) => (chat_id, is_ch),
            Err(e) => {
                let _ = bot
                    .send_message(chat_id, format!("❌ {}", markdown::escape(&e)))
                    .parse_mode(ParseMode::MarkdownV2)
                    .await;
                return Ok(());
            }
        };
        let target_chat_id = target_chat.0;

        let remaining = parsed.remaining.trim();
        if remaining.is_empty() {
            let _ = bot
                .send_message(
                    chat_id,
                    "用法: /eunsub [ch=<频道ID>] <ID或搜索词>\n\nID 请复制 /list 中代码格式的内容，无需输入反引号。合法 eh:/ehq: ID 只做精确匹配，未找到不会按搜索词重试；搜索词本身看起来像 ID 时，请使用它在 /list 中的完整 ID。",
                )
                .await;
            return Ok(());
        }

        let subscriptions = match self.repo.list_subscriptions_by_chat(target_chat_id).await {
            Ok(subscriptions) => subscriptions,
            Err(e) => {
                error!(
                    "Failed to list EH subscriptions for chat {}: {:#}",
                    target_chat_id, e
                );
                let _ = bot
                    .send_message(chat_id, "❌ 查询订阅失败，请稍后重试")
                    .await;
                return Ok(());
            }
        };

        let task_value = if EhTaskKey::parse(remaining).is_some() {
            match subscriptions
                .iter()
                .find(|(_, task)| task.r#type == TaskType::Ehentai && task.value == remaining)
            {
                Some((_, task)) => task.value.clone(),
                None => {
                    let _ = bot.send_message(chat_id, "❌ 未找到对应的订阅").await;
                    return Ok(());
                }
            }
        } else if remaining.contains('|') {
            let _ = bot.send_message(chat_id, "❌ 无效的订阅标识").await;
            return Ok(());
        } else {
            let matching: Vec<_> = subscriptions
                .iter()
                .filter(|(_, task)| task.r#type == TaskType::Ehentai)
                .filter_map(|(_, task)| {
                    eh_task_value_for_query(&task.value, remaining).map(str::to_string)
                })
                .collect();

            match matching.len() {
                0 => {
                    let _ = bot.send_message(chat_id, "❌ 未找到对应的订阅").await;
                    return Ok(());
                }
                1 => matching[0].clone(),
                _ => {
                    let _ = bot
                        .send_message(
                            chat_id,
                            "❌ 找到多个匹配的订阅，请使用 /list 查看完整标识后用 /eunsub <标识>",
                        )
                        .await;
                    return Ok(());
                }
            }
        };

        match self
            .delete_subscription(target_chat_id, TaskType::Ehentai, &task_value)
            .await
        {
            Ok(_) => {
                let _ = bot.send_message(chat_id, "✅ 已取消 E-Hentai 订阅").await;
            }
            Err(e) => {
                error!(
                    "Failed to unsubscribe EH task {} for chat {}: {:#}",
                    task_value, target_chat_id, e
                );
                let error_message = e.root_cause().to_string();
                let user_message = if matches!(
                    error_message.as_str(),
                    "未找到"
                        | "未订阅"
                        | "EH subscription disappeared before cancellation"
                        | "EH subscription disappeared during cancellation"
                ) {
                    "❌ 未找到对应的订阅"
                } else {
                    "❌ 取消订阅失败，请稍后重试"
                };
                let _ = bot.send_message(chat_id, user_message).await;
            }
        }

        Ok(())
    }

    pub async fn handle_estatus(&self, bot: ThrottledBot, chat_id: ChatId) -> ResponseResult<()> {
        if self.eh_client.is_none() {
            let _ = bot.send_message(chat_id, "E-Hentai 功能未启用").await;
            return Ok(());
        }

        let snapshot = match self.repo.get_eh_queue_snapshot(chat_id.0).await {
            Ok(snapshot) => snapshot,
            Err(e) => {
                error!(
                    "Failed to get EH queue status for chat {}: {:#}",
                    chat_id, e
                );
                let _ = bot
                    .send_message(chat_id, "❌ 获取 EH 下载队列状态失败，请稍后重试")
                    .await;
                return Ok(());
            }
        };

        bot.send_message(chat_id, format_eh_queue_status(&snapshot))
            .parse_mode(ParseMode::MarkdownV2)
            .await?;

        Ok(())
    }

    pub async fn handle_edl(
        &self,
        bot: ThrottledBot,
        msg: teloxide::types::Message,
        chat_id: ChatId,
        args_str: String,
    ) -> ResponseResult<()> {
        let eh_client = match &self.eh_client {
            Some(c) => c.clone(),
            None => {
                let _ = bot.send_message(chat_id, "E-Hentai 功能未启用").await;
                return Ok(());
            }
        };

        let parsed = args::parse_args(&args_str);
        let (remaining, trailing_telegraph) = split_edl_remaining_and_telegraph(&parsed.remaining);
        let remaining = remaining.trim();

        // If no args, check if replying to a message containing a gallery URL
        let input = if remaining.is_empty() {
            // Try to extract from replied message
            if let Some(reply) = msg.reply_to_message() {
                let reply_text = reply.text().unwrap_or("");
                extract_gallery_url_from_text(reply_text)
            } else {
                None
            }
        } else {
            Some(remaining.to_string())
        };

        let input = match input {
            Some(s) => s,
            None => {
                let _ = bot
                    .send_message(
                        chat_id,
                        "用法: /edl <画廊URL> [telegraph=on]\n\n\
                         支持:\n\
                         • 画廊 URL: https://e-hentai.org/g/12345/token/\n\
                         • 回复包含画廊链接的消息使用 /edl",
                    )
                    .await;
                return Ok(());
            }
        };

        // Check telegraph param — check both leading parsed params and trailing in remaining text.
        // parse_args() only extracts leading key=value, so trailing telegraph=on after a URL
        // needs to be detected from the remaining text.
        let telegraph = parsed
            .get("telegraph")
            .map(is_telegraph_enabled_value)
            .unwrap_or(trailing_telegraph);

        // Reject telegraph=on when Telegraph is not configured
        if telegraph && !self.has_telegraph {
            let _ = bot
                .send_message(
                    chat_id,
                    "❌ Telegraph 未配置，无法启用 telegraph=on。请配置 ehentai.telegraph_access_token 后重试。",
                )
                .await;
            return Ok(());
        }

        if !self.eh_config.send_archive && !telegraph {
            let _ = bot
                .send_message(
                    chat_id,
                    "❌ 本次请求没有可用的投递方式。请启用 ZIP 投递或使用 telegraph=on。",
                )
                .await;
            return Ok(());
        }

        // Parse gallery URL
        let (gid, token) = match parse_gallery_ref(&input) {
            Some(g) => g,
            None => {
                let _ = bot
                    .send_message(chat_id, "❌ 无法解析画廊标识。请提供画廊 URL。")
                    .await;
                return Ok(());
            }
        };

        // Send "processing" message
        let status_msg = bot
            .send_message(chat_id, "⏳ 正在获取画廊信息...")
            .await
            .ok();

        // Fetch metadata
        let metadata = match eh_client.get_metadata(&[(gid, &token)]).await {
            Ok(m) if !m.is_empty() => m.into_iter().next().unwrap(),
            Ok(_) => {
                let _ = bot.send_message(chat_id, "❌ 未找到画廊").await;
                return Ok(());
            }
            Err(e) => {
                warn!("Failed to fetch eh metadata: {:#}", e);
                let _ = bot.send_message(chat_id, "❌ 获取画廊信息失败").await;
                return Ok(());
            }
        };

        // Enqueue download
        let variant = EhGalleryVariant::for_request(
            eh_client.is_logged_in(),
            SOURCE_DIRECT,
            self.eh_config.as_ref(),
        );
        let fingerprint = metadata.source_fingerprint();
        match self
            .repo
            .enqueue_eh_download(
                chat_id.0,
                gid as i64,
                &token,
                &metadata.title,
                telegraph,
                SOURCE_DIRECT,
                &variant,
                Some(&fingerprint),
                self.eh_config.send_archive,
            )
            .await
        {
            Ok(Some(_)) => {}
            Ok(None) => {
                error!("Direct EH enqueue from /edl returned no delivery");
                let _ = bot.send_message(chat_id, "❌ 加入下载队列失败").await;
                return Ok(());
            }
            Err(e) => {
                error!("Failed to enqueue eh download: {:#}", e);
                let _ = bot.send_message(chat_id, "❌ 加入下载队列失败").await;
                return Ok(());
            }
        }

        // Delete status message
        if let Some(msg) = status_msg {
            let _ = bot.delete_message(chat_id, msg.id).await;
        }

        let _ = bot
            .send_message(
                chat_id,
                format!(
                    "✅ 已加入下载队列: {}\n_gid: {}_",
                    markdown::escape(&metadata.title),
                    gid
                ),
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await;

        Ok(())
    }

    /// /telegraph command: download gallery and upload to Telegraph, send link.
    /// Like /edl but always uploads to Telegraph (uses free 1280x resolution).
    pub async fn handle_telegraph(
        &self,
        bot: ThrottledBot,
        msg: teloxide::types::Message,
        chat_id: ChatId,
        args_str: String,
    ) -> ResponseResult<()> {
        let eh_client = match &self.eh_client {
            Some(c) => c.clone(),
            None => {
                let _ = bot.send_message(chat_id, "E-Hentai 功能未启用").await;
                return Ok(());
            }
        };

        // Reject Telegraph request when no token is configured
        if !self.has_telegraph {
            let _ = bot
                .send_message(
                    chat_id,
                    "❌ Telegraph 未配置。请配置 ehentai.telegraph_access_token 后重试。",
                )
                .await;
            return Ok(());
        }

        let parsed = args::parse_args(&args_str);
        let remaining = parsed.remaining.trim();

        // If no args, check if replying to a message containing a gallery URL
        let input = if remaining.is_empty() {
            if let Some(reply) = msg.reply_to_message() {
                let reply_text = reply.text().unwrap_or("");
                extract_gallery_url_from_text(reply_text)
            } else {
                None
            }
        } else {
            Some(remaining.to_string())
        };

        let input = match input {
            Some(s) => s,
            None => {
                let _ = bot
                    .send_message(
                        chat_id,
                        "用法: /telegraph <画廊URL>\n\n\
                         下载画廊并上传 Telegraph，发送阅读链接。\n\
                         也可回复包含画廊链接的消息使用 /telegraph",
                    )
                    .await;
                return Ok(());
            }
        };

        // Parse gallery URL
        let (gid, token) = match parse_gallery_ref(&input) {
            Some(g) => g,
            None => {
                let _ = bot
                    .send_message(chat_id, "❌ 无法解析画廊标识。请提供画廊 URL。")
                    .await;
                return Ok(());
            }
        };

        // Send "processing" message
        let status_msg = bot
            .send_message(chat_id, "⏳ 正在获取画廊信息...")
            .await
            .ok();

        // Fetch metadata
        let metadata = match eh_client.get_metadata(&[(gid, &token)]).await {
            Ok(m) if !m.is_empty() => m.into_iter().next().unwrap(),
            Ok(_) => {
                let _ = bot.send_message(chat_id, "❌ 未找到画廊").await;
                return Ok(());
            }
            Err(e) => {
                warn!("Failed to fetch eh metadata: {:#}", e);
                let _ = bot.send_message(chat_id, "❌ 获取画廊信息失败").await;
                return Ok(());
            }
        };

        // Enqueue download with telegraph=true (processor handles upload)
        let variant = EhGalleryVariant::for_request(
            eh_client.is_logged_in(),
            SOURCE_DIRECT,
            self.eh_config.as_ref(),
        );
        let fingerprint = metadata.source_fingerprint();
        match self
            .repo
            .enqueue_eh_download(
                chat_id.0,
                gid as i64,
                &token,
                &metadata.title,
                true, // always telegraph
                SOURCE_DIRECT,
                &variant,
                Some(&fingerprint),
                self.eh_config.send_archive,
            )
            .await
        {
            Ok(Some(_)) => {}
            Ok(None) => {
                error!("Direct EH enqueue from /telegraph returned no delivery");
                let _ = bot.send_message(chat_id, "❌ 加入下载队列失败").await;
                return Ok(());
            }
            Err(e) => {
                error!("Failed to enqueue eh download: {:#}", e);
                let _ = bot.send_message(chat_id, "❌ 加入下载队列失败").await;
                return Ok(());
            }
        }

        // Delete status message
        if let Some(msg) = status_msg {
            let _ = bot.delete_message(chat_id, msg.id).await;
        }

        let _ = bot
            .send_message(
                chat_id,
                format!(
                    "✅ 已加入 Telegraph 下载队列: {}\n_gid: {}_",
                    markdown::escape(&metadata.title),
                    gid
                ),
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await;

        Ok(())
    }
}

/// Parse filter args into EhFilter.
fn parse_eh_filter(args: &[String]) -> Result<EhFilter, String> {
    let mut filter = EhFilter::default();

    for arg in args {
        if let Some(val) = arg.strip_prefix("rating>=") {
            let n: f64 = val.parse().map_err(|_| format!("无效的评分值: {}", val))?;
            if !n.is_finite() || !(2.0..=5.0).contains(&n) {
                return Err(format!("评分范围: 2-5, 得到: {}", n));
            }
            filter.min_rating = Some(n);
            filter.min_rating_strict = false;
        } else if let Some(val) = arg.strip_prefix("rating>") {
            let n: f64 = val.parse().map_err(|_| format!("无效的评分值: {}", val))?;
            if !n.is_finite() || !(2.0..=5.0).contains(&n) {
                return Err(format!("评分范围: 2-5, 得到: {}", n));
            }
            filter.min_rating = Some(n);
            filter.min_rating_strict = true;
        } else if let Some(val) = arg.strip_prefix("pages>=") {
            let n: u32 = val.parse().map_err(|_| format!("无效的页数: {}", val))?;
            filter.min_pages = Some(n);
        } else if let Some(val) = arg.strip_prefix("pages<=") {
            let n: u32 = val.parse().map_err(|_| format!("无效的页数: {}", val))?;
            filter.max_pages = Some(n);
        }
    }

    Ok(filter)
}

fn parse_eh_category_bitmask(cat_str: Option<&str>) -> Result<u32, String> {
    let Some(cat_str) = cat_str else {
        return Ok(0);
    };
    let mut bitmask = 0u32;
    for raw in cat_str.split(',') {
        let cat = raw.trim();
        if cat.is_empty() || cat.eq_ignore_ascii_case("all") {
            continue;
        }
        let parsed =
            EhCategory::parse_str(cat).ok_or_else(|| format!("未知的 E-Hentai 分类: {}", cat))?;
        bitmask |= parsed as u32;
    }
    Ok(bitmask)
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedEhSubscriptionArgs {
    query: String,
    filter_args: Vec<String>,
    cat_str: Option<String>,
    telegraph_on: bool,
}

fn parse_esub_remaining(remaining: &str) -> Result<ParsedEhSubscriptionArgs, String> {
    let mut query_parts = Vec::new();
    let mut filter_args = Vec::new();
    let mut cat_str: Option<String> = None;
    let mut telegraph_on = false;

    for part in remaining.split_whitespace() {
        if let Some(val) = part.strip_prefix("rating>=") {
            filter_args.push(format!("rating>={val}"));
        } else if let Some(val) = part.strip_prefix("rating>") {
            filter_args.push(format!("rating>{val}"));
        } else if let Some(val) = part.strip_prefix("pages>=") {
            filter_args.push(format!("pages>={val}"));
        } else if let Some(val) = part.strip_prefix("pages>") {
            let n = val
                .parse::<u32>()
                .map_err(|_| format!("无效的页数: {val}"))?;
            filter_args.push(format!("pages>={}", n.saturating_add(1)));
        } else if let Some(val) = part.strip_prefix("pages<=") {
            filter_args.push(format!("pages<={val}"));
        } else if let Some(val) = part.strip_prefix("pages<") {
            let n = val
                .parse::<u32>()
                .map_err(|_| format!("无效的页数: {val}"))?;
            filter_args.push(format!("pages<={}", n.saturating_sub(1)));
        } else if let Some(val) = part.strip_prefix("cat=") {
            cat_str = Some(val.to_string());
        } else if part == "telegraph=on" {
            telegraph_on = true;
        } else {
            query_parts.push(part);
        }
    }

    if query_parts.is_empty() {
        return Err("请提供搜索词".to_string());
    }

    Ok(ParsedEhSubscriptionArgs {
        query: query_parts.join(" "),
        filter_args,
        cat_str,
        telegraph_on,
    })
}

fn eh_task_value_for_query<'a>(task_value: &'a str, query: &str) -> Option<&'a str> {
    let key = EhTaskKey::parse(task_value)?;
    (key.query == query).then_some(task_value)
}

/// Parse a gallery URL or GID into (gid, token).
fn parse_gallery_ref(s: &str) -> Option<(u64, String)> {
    let s = s.trim();

    // Try URL format: https://e-hentai.org/g/{gid}/{token}/
    if s.contains("/g/") {
        let after_g = s.split("/g/").nth(1)?;
        let parts: Vec<&str> = after_g.split('/').take(2).collect();
        if parts.len() == 2 {
            let gid: u64 = parts[0].parse().ok()?;
            let token = parts[1].to_string();
            if token.len() >= 8
                && token
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return Some((gid, token));
            }
        }
        return None;
    }

    // Try GID only — need to make an API call to get token, but we can't here.
    // For GID-only, we'd need to use the gtoken API method. For now, require URL.
    None
}

fn is_telegraph_enabled_value(value: &str) -> bool {
    value.eq_ignore_ascii_case("on") || value.eq_ignore_ascii_case("true") || value == "1"
}

fn split_edl_remaining_and_telegraph(remaining: &str) -> (String, bool) {
    let mut telegraph = false;
    let gallery_parts = remaining
        .split_whitespace()
        .filter(|part| {
            if let Some(value) = part.strip_prefix("telegraph=") {
                telegraph = is_telegraph_enabled_value(value);
                false
            } else {
                true
            }
        })
        .collect::<Vec<_>>();

    (gallery_parts.join(" "), telegraph)
}

/// Extract the first e-hentai/exhentai gallery URL from a text message.
fn extract_gallery_url_from_text(text: &str) -> Option<String> {
    for word in text.split_whitespace() {
        if (word.contains("e-hentai.org/g/") || word.contains("exhentai.org/g/"))
            && parse_gallery_ref(word).is_some()
        {
            return Some(
                word.trim_matches(|c| {
                    !char::is_alphanumeric(c) && c != '/' && c != ':' && c != '-' && c != '.'
                })
                .to_string(),
            );
        }
    }
    None
}

fn eh_queue_stage(status: &str, background_status: Option<&str>) -> &'static str {
    match status {
        STATUS_PENDING => match background_status {
            Some(BACKGROUND_STATUS_RUNNING) => "后台下载中",
            Some(BACKGROUND_STATUS_PENDING) => "后台排队",
            _ => "排队中",
        },
        STATUS_DOWNLOADING => "下载中",
        STATUS_DOWNLOADED => "等待上传或发送",
        STATUS_UPLOADING => "上传中",
        STATUS_UPLOADED => "等待发送",
        STATUS_PUBLISHING => "发送中",
        STATUS_DONE => "已完成",
        STATUS_FAILED => "失败",
        STATUS_CANCELED => "已取消",
        _ => "未知状态",
    }
}

fn format_eh_queue_status_item(item: &EhQueueStatusItem) -> String {
    let title = item
        .title
        .chars()
        .take(EH_QUEUE_MAX_TITLE_CHARS)
        .collect::<String>();
    let stage = eh_queue_stage(&item.status, item.background_download_status.as_deref());

    format!(
        "GID `{}` · {} · {stage}",
        item.gid,
        markdown::escape(&title)
    )
}

fn format_eh_queue_status(snapshot: &EhQueueSnapshot) -> String {
    for visible_active_count in
        (0..=snapshot.active.len().min(EH_QUEUE_MAX_VISIBLE_ACTIVE_ITEMS)).rev()
    {
        let message =
            format_eh_queue_status_with_visible_active_count(snapshot, visible_active_count);
        if message.encode_utf16().count() <= TELEGRAM_MAX_MESSAGE_UTF16_UNITS {
            return message;
        }
    }

    unreachable!("a queue status without active details always fits Telegram's message limit")
}

fn format_eh_queue_status_with_visible_active_count(
    snapshot: &EhQueueSnapshot,
    visible_active_count: usize,
) -> String {
    let mut message = "📥 *EH 下载队列*".to_string();

    if snapshot.active.is_empty() {
        message.push_str("\n\n当前聊天没有活动中的 EH 下载任务");
    } else {
        message.push_str(&format!("\n\n活动任务：`{}`", snapshot.active.len()));

        let stage_summary = EH_QUEUE_ACTIVE_STAGE_ORDER
            .iter()
            .filter_map(|stage| {
                let count = snapshot
                    .active
                    .iter()
                    .filter(|item| {
                        eh_queue_stage(&item.status, item.background_download_status.as_deref())
                            == *stage
                    })
                    .count();
                (count > 0).then(|| format!("{stage} `{count}`"))
            })
            .collect::<Vec<_>>()
            .join(" · ");
        message.push_str(&format!("\n阶段：{stage_summary}\n\n*任务*"));

        for item in snapshot.active.iter().take(visible_active_count) {
            message.push_str("\n• ");
            message.push_str(&format_eh_queue_status_item(item));
        }

        let hidden_count = snapshot.active.len() - visible_active_count;
        if hidden_count > 0 {
            message.push_str(&format!("\n另有 `{hidden_count}` 项未显示"));
        }
    }

    if let Some(item) = &snapshot.recent_terminal {
        message.push_str("\n\n*最近记录*\n• ");
        message.push_str(&format_eh_queue_status_item(item));
    }

    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use eh_client::EhGallery;

    #[test]
    fn subscription_options_parse_through_to_filters() {
        let parsed =
            parse_esub_remaining("foo rating>4.5 pages>20 pages<100 telegraph=on cat=manga")
                .unwrap();
        assert_eq!(parsed.query, "foo");
        assert_eq!(parsed.cat_str.as_deref(), Some("manga"));
        assert!(parsed.telegraph_on);
        let filter = parse_eh_filter(&parsed.filter_args).unwrap();
        assert_eq!((filter.min_pages, filter.max_pages), (Some(21), Some(99)));

        let gallery_with_rating = |rating| EhGallery {
            gid: 1,
            token: "token".to_string(),
            title: "title".to_string(),
            title_jpn: None,
            category: "Manga".to_string(),
            thumb: "thumb".to_string(),
            uploader: "uploader".to_string(),
            posted: 1,
            filecount: 50,
            filesize: 1,
            expunged: false,
            rating,
            tags: Vec::new(),
        };
        assert!(!filter.matches(&gallery_with_rating(4.5)));
        assert!(filter.matches(&gallery_with_rating(4.75)));

        let inclusive = parse_esub_remaining("foo rating>4 rating>=4.5").unwrap();
        let inclusive = parse_eh_filter(&inclusive.filter_args).unwrap();
        assert!(inclusive.matches(&gallery_with_rating(4.5)));

        let non_finite = parse_esub_remaining("foo rating>NaN").unwrap();
        assert!(parse_eh_filter(&non_finite.filter_args).is_err());
    }

    #[test]
    fn test_eh_task_value_for_query_preserves_legacy_value() {
        let legacy = "eh:~foo%7Cbar|f=r4";
        assert_eq!(eh_task_value_for_query(legacy, "~foo%7Cbar"), Some(legacy));
        assert_eq!(eh_task_value_for_query(legacy, "~foo|bar"), None);
    }

    #[test]
    fn test_eh_task_value_for_query_matches_encoded_value() {
        let filter = EhFilter {
            min_rating: Some(4.0),
            ..Default::default()
        };
        let key = EhTaskKey::new("foo|bar", 0, &filter);
        let value = key.to_task_value();
        assert_eq!(value, "ehq:foo%7Cbar|f=r4");
        assert_eq!(
            eh_task_value_for_query(&value, "foo|bar"),
            Some(value.as_str())
        );
    }

    #[test]
    fn gallery_reference_requires_a_complete_supported_url() {
        for (input, expected) in [
            (
                "https://e-hentai.org/g/12345/abcdef0123/",
                Some((12345, "abcdef0123")),
            ),
            (
                "https://exhentai.org/g/99999/deadbeef00/",
                Some((99999, "deadbeef00")),
            ),
            ("12345", None),
            ("not a url", None),
            ("https://example.com/other/123", None),
            ("https://e-hentai.org/g/12345/abc/", None),
            ("https://e-hentai.org/g/12345/abcdef0123 telegraph=on", None),
        ] {
            assert_eq!(
                parse_gallery_ref(input),
                expected.map(|(gid, token)| (gid, token.to_string())),
                "{input}"
            );
        }
    }
}
