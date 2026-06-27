use crate::bot::notifier::ThrottledBot;
use crate::bot::BotHandler;
use crate::db::repo::eh_download_queue::SOURCE_DIRECT;
use crate::db::types::{EhFilter, EhTaskKey, TagFilter, TaskType};
use crate::utils::args;
use eh_client::EhCategory;
use teloxide::prelude::*;
use teloxide::types::{ChatId, ParseMode, UserId};
use teloxide::utils::markdown;
use tracing::{error, warn};

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
                     • cat=<类别> — 分类筛选 (逗号分隔)",
                )
                .await;
            return Ok(());
        }

        // Parse query and filter args
        let parts: Vec<&str> = remaining.split_whitespace().collect();
        let mut query_parts = Vec::new();
        let mut filter_args = Vec::new();
        let mut cat_str: Option<String> = None;

        for part in &parts {
            if let Some(val) = part.strip_prefix("rating>=") {
                filter_args.push(format!("rating>={}", val));
            } else if let Some(val) = part.strip_prefix("rating>") {
                // Strict > stored as +1
                if let Ok(n) = val.parse::<u8>() {
                    filter_args.push(format!("rating>={}", n.saturating_add(1)));
                }
            } else if let Some(val) = part.strip_prefix("pages>=") {
                filter_args.push(format!("pages>={}", val));
            } else if let Some(val) = part.strip_prefix("pages>") {
                if let Ok(n) = val.parse::<u32>() {
                    filter_args.push(format!("pages>={}", n.saturating_add(1)));
                }
            } else if let Some(val) = part.strip_prefix("pages<=") {
                filter_args.push(format!("pages<={}", val));
            } else if let Some(val) = part.strip_prefix("pages<") {
                if let Ok(n) = val.parse::<u32>() {
                    filter_args.push(format!("pages<={}", n.saturating_sub(1)));
                }
            } else if let Some(val) = part.strip_prefix("cat=") {
                cat_str = Some(val.to_string());
            } else {
                query_parts.push(*part);
            }
        }

        if query_parts.is_empty() {
            let _ = bot.send_message(chat_id, "❌ 请提供搜索词").await;
            return Ok(());
        }

        let query = query_parts.join(" ");

        // Parse filter
        let eh_filter = match parse_eh_filter(&filter_args) {
            Ok(f) => f,
            Err(e) => {
                let _ = bot
                    .send_message(chat_id, format!("❌ {}", markdown::escape(&e)))
                    .parse_mode(ParseMode::MarkdownV2)
                    .await;
                return Ok(());
            }
        };

        // Parse category bitmask
        let cats = cat_str
            .as_deref()
            .map(EhCategory::bitmask_from_str)
            .unwrap_or(0);

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
        let mut msg = format!("✅ 已订阅 E-Hentai: {}\n", markdown::escape(&query));
        if cats > 0 {
            msg.push_str(&format!("分类: {}\n", markdown::escape(&cat_str.unwrap())));
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
                            EhTaskKey::parse(&task.value).map(|key| (sub, key))
                        })
                        .filter(|(_, key)| key.query == remaining)
                        .collect();

                    match matching.len() {
                        0 => {
                            let _ = bot.send_message(chat_id, "❌ 未找到对应的订阅").await;
                            return Ok(());
                        }
                        1 => matching[0].1.to_task_value(),
                        _ => {
                            let _ = bot
                                .send_message(
                                    chat_id,
                                    "❌ 找到多个匹配的订阅，请使用 /elist 查看完整标识后用 /eunsub <标识>",
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
        let remaining = parsed.remaining.trim();

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

        // Check telegraph param
        let telegraph = parsed
            .get("telegraph")
            .map(|v| v.eq_ignore_ascii_case("on") || v == "true" || v == "1")
            .unwrap_or(false);

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
            if token.len() >= 8 {
                return Some((gid, token));
            }
        }
        return None;
    }

    // Try GID only — need to make an API call to get token, but we can't here.
    // For GID-only, we'd need to use the gtoken API method. For now, require URL.
    None
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
