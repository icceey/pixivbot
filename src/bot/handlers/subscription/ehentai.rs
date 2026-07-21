use crate::bot::notifier::ThrottledBot;
use crate::bot::BotHandler;
use crate::db::repo::eh_download_queue::{
    EhQueueSnapshot, EhQueueStatusItem, BACKGROUND_STATUS_PENDING, BACKGROUND_STATUS_RUNNING,
    SOURCE_DIRECT, STATUS_CANCELED, STATUS_DONE, STATUS_DOWNLOADED, STATUS_DOWNLOADING,
    STATUS_FAILED, STATUS_PENDING, STATUS_PUBLISHING, STATUS_UPLOADED, STATUS_UPLOADING,
};
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
                     • rating>=N — 最低评分 (2-5, 触发48h扫描)\n\
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
        if should_reject_telegraph_request(telegraph_on, self.has_telegraph) {
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
            let _ = bot.send_message(chat_id, "用法: /eunsub <搜索词>").await;
            return Ok(());
        }

        // Try to parse as internal key first (contains |)
        let task_value = if remaining.contains('|') {
            // Already a task value format
            if EhTaskKey::parse(remaining).is_some() {
                remaining.to_string()
            } else {
                let _ = bot.send_message(chat_id, "❌ 无效的订阅标识").await;
                return Ok(());
            }
        } else {
            // List subscriptions and find one whose query matches
            let subs = self.repo.list_subscriptions_by_chat(target_chat_id).await;
            match subs {
                Ok(subs) => {
                    let matching: Vec<_> = subs
                        .into_iter()
                        .filter(|(_, task)| task.r#type == crate::db::types::TaskType::Ehentai)
                        .filter_map(|(sub, task)| {
                            eh_task_value_for_query(&task.value, remaining)
                                .map(|value| (sub, value.to_string()))
                        })
                        .collect();

                    match matching.len() {
                        0 => {
                            let _ = bot.send_message(chat_id, "❌ 未找到对应的订阅").await;
                            return Ok(());
                        }
                        1 => matching[0].1.clone(),
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
                }
                Err(e) => {
                    let _ = bot
                        .send_message(chat_id, format!("❌ {}", markdown::escape(&e.to_string())))
                        .parse_mode(ParseMode::MarkdownV2)
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
                let msg = if e.to_string().contains("未订阅") {
                    "❌ 未找到对应的订阅".to_string()
                } else {
                    format!("❌ {}", markdown::escape(&e.to_string()))
                };
                let _ = bot
                    .send_message(chat_id, msg)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await;
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
        _user_id: Option<UserId>,
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
        if should_reject_telegraph_request(telegraph, self.has_telegraph) {
            let _ = bot
                .send_message(
                    chat_id,
                    "❌ Telegraph 未配置，无法启用 telegraph=on。请配置 ehentai.telegraph_access_token 后重试。",
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
        if let Err(e) = self
            .repo
            .enqueue_eh_download(
                chat_id.0,
                gid as i64,
                &token,
                &metadata.title,
                telegraph,
                SOURCE_DIRECT,
            )
            .await
        {
            error!("Failed to enqueue eh download: {:#}", e);
            let _ = bot.send_message(chat_id, "❌ 加入下载队列失败").await;
            return Ok(());
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
        _user_id: Option<UserId>,
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
        if should_reject_telegraph_request(true, self.has_telegraph) {
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
        if let Err(e) = self
            .repo
            .enqueue_eh_download(
                chat_id.0,
                gid as i64,
                &token,
                &metadata.title,
                true, // always telegraph
                SOURCE_DIRECT,
            )
            .await
        {
            error!("Failed to enqueue eh download: {:#}", e);
            let _ = bot.send_message(chat_id, "❌ 加入下载队列失败").await;
            return Ok(());
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
    let mut filter = EhFilter::new();

    for arg in args {
        if let Some(val) = arg.strip_prefix("rating>=") {
            let n: u8 = val.parse().map_err(|_| format!("无效的评分值: {}", val))?;
            if !(2..=5).contains(&n) {
                return Err(format!("评分范围: 2-5, 得到: {}", n));
            }
            filter.min_rating = Some(n);
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
            let n = val
                .parse::<u8>()
                .map_err(|_| format!("无效的评分值: {val}"))?;
            filter_args.push(format!("rating>={}", n.saturating_add(1)));
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

fn should_reject_telegraph_request(telegraph_requested: bool, has_telegraph: bool) -> bool {
    telegraph_requested && !has_telegraph
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

    fn queue_item(
        gid: i64,
        title: &str,
        status: &str,
        background_download_status: Option<&str>,
    ) -> EhQueueStatusItem {
        EhQueueStatusItem {
            gid,
            title: title.to_string(),
            status: status.to_string(),
            background_download_status: background_download_status.map(str::to_string),
        }
    }

    #[test]
    fn test_eh_queue_status_stage_labels() {
        assert_eq!(
            eh_queue_stage(STATUS_PENDING, Some(BACKGROUND_STATUS_RUNNING)),
            "后台下载中"
        );
        assert_eq!(
            eh_queue_stage(STATUS_PENDING, Some(BACKGROUND_STATUS_PENDING)),
            "后台排队"
        );
        assert_eq!(eh_queue_stage(STATUS_PENDING, None), "排队中");
        assert_eq!(
            eh_queue_stage(STATUS_DOWNLOADING, Some(BACKGROUND_STATUS_RUNNING)),
            "下载中"
        );
        assert_eq!(eh_queue_stage(STATUS_DOWNLOADED, None), "等待上传或发送");
        assert_eq!(eh_queue_stage(STATUS_UPLOADING, None), "上传中");
        assert_eq!(eh_queue_stage(STATUS_UPLOADED, None), "等待发送");
        assert_eq!(eh_queue_stage(STATUS_PUBLISHING, None), "发送中");
        assert_eq!(eh_queue_stage(STATUS_DONE, None), "已完成");
        assert_eq!(eh_queue_stage(STATUS_FAILED, None), "失败");
        assert_eq!(eh_queue_stage(STATUS_CANCELED, None), "已取消");
        assert_eq!(eh_queue_stage("unknown", None), "未知状态");
    }

    #[test]
    fn test_eh_queue_status_summarizes_all_active_stages() {
        let snapshot = EhQueueSnapshot {
            active: vec![
                queue_item(
                    1,
                    "后台下载",
                    STATUS_PENDING,
                    Some(BACKGROUND_STATUS_RUNNING),
                ),
                queue_item(
                    2,
                    "后台排队",
                    STATUS_PENDING,
                    Some(BACKGROUND_STATUS_PENDING),
                ),
                queue_item(3, "排队", STATUS_PENDING, None),
                queue_item(4, "下载", STATUS_DOWNLOADING, None),
                queue_item(5, "下载完成", STATUS_DOWNLOADED, None),
                queue_item(6, "上传", STATUS_UPLOADING, None),
                queue_item(7, "上传完成", STATUS_UPLOADED, None),
                queue_item(8, "发送", STATUS_PUBLISHING, None),
            ],
            recent_terminal: None,
        };

        assert_eq!(
            format_eh_queue_status(&snapshot),
            "📥 *EH 下载队列*\n\n活动任务：`8`\n阶段：后台下载中 `1` · 后台排队 `1` · 排队中 `1` · 下载中 `1` · 等待上传或发送 `1` · 上传中 `1` · 等待发送 `1` · 发送中 `1`\n\n*任务*\n• GID `1` · 后台下载 · 后台下载中\n• GID `2` · 后台排队 · 后台排队\n• GID `3` · 排队 · 排队中\n• GID `4` · 下载 · 下载中\n• GID `5` · 下载完成 · 等待上传或发送\n• GID `6` · 上传 · 上传中\n• GID `7` · 上传完成 · 等待发送\n• GID `8` · 发送 · 发送中"
        );
    }

    #[test]
    fn test_eh_queue_status_formats_empty_with_recent_terminal() {
        let snapshot = EhQueueSnapshot {
            active: Vec::new(),
            recent_terminal: Some(queue_item(900, "completed title", STATUS_DONE, None)),
        };

        assert_eq!(
            format_eh_queue_status(&snapshot),
            "📥 *EH 下载队列*\n\n当前聊天没有活动中的 EH 下载任务\n\n*最近记录*\n• GID `900` · completed title · 已完成"
        );
    }

    #[test]
    fn test_eh_queue_status_limits_entries_truncates_and_escapes() {
        let long_title = "界".repeat(81);
        let mut active = vec![queue_item(1, &long_title, STATUS_PENDING, None)];
        active.push(queue_item(2, "A_*[危险].!", STATUS_PENDING, None));
        active.extend(
            (3..=21).map(|gid| queue_item(gid, &format!("任务{gid}"), STATUS_PENDING, None)),
        );
        let snapshot = EhQueueSnapshot {
            active,
            recent_terminal: Some(queue_item(999, "最近失败", STATUS_FAILED, None)),
        };

        let formatted = format_eh_queue_status(&snapshot);

        assert!(formatted.contains("活动任务：`21`"));
        assert!(formatted.contains("阶段：排队中 `21`"));
        assert_eq!(formatted.matches('界').count(), 80);
        assert!(formatted.contains("A\\_\\*\\[危险\\]\\.\\!"));
        assert!(!formatted.contains("A_*[危险].!"));
        assert!(formatted.contains("GID `20`"));
        assert!(!formatted.contains("GID `21`"));
        assert!(formatted.contains("另有 `1` 项未显示"));
        assert!(formatted.contains("• GID `999` · 最近失败 · 失败"));
    }

    #[test]
    fn test_eh_queue_status_fits_telegram_limit_for_wide_titles() {
        let wide_title = "😀".repeat(EH_QUEUE_MAX_TITLE_CHARS);
        let snapshot = EhQueueSnapshot {
            active: (0..EH_QUEUE_MAX_VISIBLE_ACTIVE_ITEMS)
                .map(|offset| {
                    queue_item(i64::MAX - offset as i64, &wide_title, STATUS_PENDING, None)
                })
                .collect(),
            recent_terminal: Some(queue_item(i64::MIN, &wide_title, STATUS_FAILED, None)),
        };

        let output = format_eh_queue_status(&snapshot);
        let utf16_units = output.encode_utf16().count();
        assert!(
            utf16_units <= TELEGRAM_MAX_MESSAGE_UTF16_UNITS,
            "formatted output contains {utf16_units} UTF-16 units"
        );

        let active_section = output
            .split_once("\n\n*最近记录*")
            .map(|(active, _)| active)
            .expect("recent terminal should be present");
        let visible_active_count = active_section
            .lines()
            .filter(|line| line.starts_with("• GID "))
            .count();
        let hidden_active_count = active_section
            .lines()
            .find_map(|line| {
                line.strip_prefix("另有 `")
                    .and_then(|line| line.split_once("` 项未显示"))
                    .and_then(|(count, _)| count.parse::<usize>().ok())
            })
            .unwrap_or(0);

        assert_eq!(visible_active_count + hidden_active_count, 20);
        assert!(hidden_active_count > 0);
        assert!(output.contains(&format!(
            "*最近记录*\n• GID `{}` · {wide_title} · 失败",
            i64::MIN
        )));
    }

    #[test]
    fn test_parse_eh_filter_basic() {
        let filter = parse_eh_filter(&["rating>=4".to_string()]).unwrap();
        assert_eq!(filter.min_rating, Some(4));
        assert!(!filter.telegraph);
    }

    #[test]
    fn test_parse_eh_filter_pages() {
        let filter = parse_eh_filter(&[
            "rating>=3".to_string(),
            "pages>=20".to_string(),
            "pages<=500".to_string(),
        ])
        .unwrap();
        assert_eq!(filter.min_rating, Some(3));
        assert_eq!(filter.min_pages, Some(20));
        assert_eq!(filter.max_pages, Some(500));
        assert!(!filter.telegraph);
    }

    #[test]
    fn test_parse_eh_filter_invalid_rating() {
        let result = parse_eh_filter(&["rating>=1".to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_eh_filter_rating_out_of_range() {
        let result = parse_eh_filter(&["rating>=6".to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_eh_category_bitmask_rejects_unknown_category() {
        let result = parse_eh_category_bitmask(Some("mnga"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("未知的 E-Hentai 分类"));
    }

    #[test]
    fn test_parse_eh_category_bitmask_accepts_known_categories_and_all() {
        assert_eq!(
            parse_eh_category_bitmask(Some("manga,artistcg")).unwrap(),
            6
        );
        assert_eq!(parse_eh_category_bitmask(Some("all")).unwrap(), 0);
        assert_eq!(parse_eh_category_bitmask(None).unwrap(), 0);
    }

    #[test]
    fn test_parse_esub_remaining_rejects_invalid_strict_rating() {
        let result = parse_esub_remaining("foo rating>abc");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("无效的评分值: abc"));
    }

    #[test]
    fn test_parse_esub_remaining_rejects_invalid_strict_pages() {
        let result = parse_esub_remaining("foo pages>abc");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("无效的页数: abc"));

        let result = parse_esub_remaining("foo pages<abc");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("无效的页数: abc"));
    }

    #[test]
    fn test_parse_esub_remaining_preserves_valid_strict_filters() {
        let parsed =
            parse_esub_remaining("foo rating>3 pages>20 pages<100 telegraph=on cat=manga").unwrap();
        assert_eq!(parsed.query, "foo");
        assert_eq!(parsed.filter_args, ["rating>=4", "pages>=21", "pages<=99"]);
        assert_eq!(parsed.cat_str.as_deref(), Some("manga"));
        assert!(parsed.telegraph_on);
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
            min_rating: Some(4),
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
    fn test_parse_gallery_ref_url() {
        let (gid, token) = parse_gallery_ref("https://e-hentai.org/g/12345/abcdef0123/").unwrap();
        assert_eq!(gid, 12345);
        assert_eq!(token, "abcdef0123");
    }

    #[test]
    fn test_parse_gallery_ref_exhentai_url() {
        let (gid, token) = parse_gallery_ref("https://exhentai.org/g/99999/deadbeef00/").unwrap();
        assert_eq!(gid, 99999);
        assert_eq!(token, "deadbeef00");
    }

    #[test]
    fn test_parse_gallery_ref_gid_only() {
        // GID only is not supported (need token)
        assert!(parse_gallery_ref("12345").is_none());
    }

    #[test]
    fn test_parse_gallery_ref_invalid() {
        assert!(parse_gallery_ref("not a url").is_none());
        assert!(parse_gallery_ref("https://example.com/other/123").is_none());
    }

    #[test]
    fn test_parse_gallery_ref_rejects_short_token() {
        // Token length < 8 should be rejected
        assert!(parse_gallery_ref("https://e-hentai.org/g/12345/abc/").is_none());
    }

    #[test]
    fn test_parse_gallery_ref_rejects_token_with_spaces_from_trailing_option() {
        assert!(
            parse_gallery_ref("https://e-hentai.org/g/12345/abcdef0123 telegraph=on").is_none()
        );
    }

    #[test]
    fn test_split_edl_remaining_detects_trailing_telegraph_on() {
        let (gallery, telegraph) = split_edl_remaining_and_telegraph(
            "https://e-hentai.org/g/12345/abcdef0123/ telegraph=on",
        );
        assert_eq!(gallery, "https://e-hentai.org/g/12345/abcdef0123/");
        assert!(telegraph);
    }

    #[test]
    fn test_split_edl_remaining_keeps_telegraph_off_disabled() {
        let (gallery, telegraph) = split_edl_remaining_and_telegraph(
            "https://e-hentai.org/g/12345/abcdef0123/ telegraph=off",
        );
        assert_eq!(gallery, "https://e-hentai.org/g/12345/abcdef0123/");
        assert!(!telegraph);
    }

    #[test]
    fn test_should_reject_telegraph_request_only_when_requested_without_client() {
        assert!(should_reject_telegraph_request(true, false));
        assert!(!should_reject_telegraph_request(true, true));
        assert!(!should_reject_telegraph_request(false, false));
    }

    #[test]
    fn test_esub_success_label_is_markdown_safe() {
        let label = markdown::escape("E-Hentai");
        assert_eq!(label, "E\\-Hentai");
    }
}
