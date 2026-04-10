use crate::bot::notifier::ThrottledBot;
use crate::bot::BotHandler;
use crate::db::types::{BooruFilter, TagFilter, TaskType};
use crate::utils::args;
use booru_client::BooruRating;
use teloxide::prelude::*;
use teloxide::types::{ChatId, ParseMode, UserId};
use teloxide::utils::markdown;
use tracing::{error, warn};

impl BotHandler {
    pub async fn handle_bsub(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        let parsed = args::parse_args(&args_str);

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

        let parts: Vec<&str> = parsed.remaining.split_whitespace().collect();

        if parts.is_empty() {
            bot.send_message(
                chat_id,
                "❌ 用法: `/bsub [ch=<频道ID>] <站点名:标签> [score>N] [fav>N] [rating=s,q,e]`\n\n\
                 示例:\n\
                 `/bsub konachan:landscape`\n\
                 `/bsub konachan: score>50`\n\
                 `/bsub konachan:blue_sky rating=s`",
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
            return Ok(());
        }

        let site_tags_str = parts[0];
        let (site_name, tags) = match site_tags_str.split_once(':') {
            Some((site, tags)) => (site, tags),
            None => {
                bot.send_message(chat_id, "❌ 格式: `站点名:标签`，例如 `konachan:landscape`")
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
                return Ok(());
            }
        };

        let site_config = self
            .booru_config
            .sites
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(site_name));

        if site_config.is_none() {
            let available: Vec<&str> = self
                .booru_config
                .sites
                .iter()
                .map(|s| s.name.as_str())
                .collect();
            let msg = if available.is_empty() {
                "❌ 未配置任何 Booru 站点".to_string()
            } else {
                format!(
                    "❌ 未找到站点 `{}`\n可用站点: {}",
                    markdown::escape(site_name),
                    available
                        .iter()
                        .map(|s| format!("`{}`", markdown::escape(s)))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            bot.send_message(chat_id, msg)
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        let filter_args = &parts[1..];
        let (booru_filter, tag_filter) = parse_booru_filter_args(filter_args);

        let task_value = format!("{}:{}", site_name.to_lowercase(), tags);
        let display_name = if tags.is_empty() {
            format!("{} (all)", site_name)
        } else {
            format!("{}:{}", site_name, tags)
        };

        match self
            .create_booru_subscription(
                target_chat_id.0,
                TaskType::BooruTag,
                &task_value,
                Some(&display_name),
                tag_filter.clone(),
                booru_filter.clone(),
            )
            .await
        {
            Ok(_) => {
                let mut msg = format!(
                    "✅ 已订阅 Booru 标签: *{}*",
                    markdown::escape(&display_name)
                );

                if !booru_filter.is_empty() {
                    msg.push_str(&format!(
                        "\n🔧 {}",
                        markdown::escape(&booru_filter.format_for_display())
                    ));
                }
                if !tag_filter.is_empty() {
                    msg.push_str(&format!("\n🏷 {}", tag_filter.format_for_display()));
                }
                if is_channel {
                    msg.push_str(&format!("\n📢 频道: `{}`", target_chat_id.0));
                }

                bot.send_message(chat_id, msg)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!(
                    "Failed to subscribe to booru tag {}:{} for chat {}: {:#}",
                    site_name, tags, target_chat_id.0, e
                );
                bot.send_message(chat_id, "❌ 订阅失败").await?;
            }
        }

        Ok(())
    }

    pub async fn handle_bunsub(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        let parsed = args::parse_args(&args_str);

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

        let parts: Vec<&str> = parsed.remaining.split_whitespace().collect();

        if parts.is_empty() {
            bot.send_message(chat_id, "❌ 用法: `/bunsub [ch=<频道ID>] <站点名:标签>`")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        let site_tags_str = parts[0];
        let (site_name, tags) = match site_tags_str.split_once(':') {
            Some((site, tags)) => (site, tags),
            None => {
                bot.send_message(chat_id, "❌ 格式: `站点名:标签`")
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
                return Ok(());
            }
        };

        let task_value = format!("{}:{}", site_name.to_lowercase(), tags);

        match self
            .delete_subscription(target_chat_id.0, TaskType::BooruTag, &task_value)
            .await
        {
            Ok(display) => {
                let name = display.unwrap_or_else(|| task_value.clone());
                let mut msg = format!("✅ 已取消订阅: *{}*", markdown::escape(&name));
                if is_channel {
                    msg.push_str(&format!("\n📢 频道: `{}`", target_chat_id.0));
                }
                bot.send_message(chat_id, msg)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                warn!(
                    "Failed to unsubscribe booru tag {} for chat {}: {:#}",
                    task_value, target_chat_id.0, e
                );
                bot.send_message(chat_id, "❌ 未找到该订阅").await?;
            }
        }

        Ok(())
    }
}

fn parse_booru_filter_args(args: &[&str]) -> (BooruFilter, TagFilter) {
    let mut score_min = None;
    let mut fav_count_min = None;
    let mut allowed_ratings = Vec::new();
    let mut tag_parts = Vec::new();

    for &arg in args {
        if let Some(val) = arg.strip_prefix("score>") {
            if let Ok(n) = val.parse::<i32>() {
                score_min = Some(n);
                continue;
            }
        }
        if let Some(val) = arg.strip_prefix("fav>") {
            if let Ok(n) = val.parse::<i32>() {
                fav_count_min = Some(n);
                continue;
            }
        }
        if let Some(val) = arg.strip_prefix("rating=") {
            for r in val.split(',') {
                match r.trim() {
                    "s" | "safe" => allowed_ratings.push(BooruRating::Safe),
                    "g" | "general" => allowed_ratings.push(BooruRating::General),
                    "q" | "questionable" => allowed_ratings.push(BooruRating::Questionable),
                    "e" | "explicit" => allowed_ratings.push(BooruRating::Explicit),
                    _ => {}
                }
            }
            continue;
        }
        tag_parts.push(arg);
    }

    let booru_filter = BooruFilter::new(score_min, fav_count_min, allowed_ratings);
    let tag_filter = TagFilter::parse_from_args(&tag_parts);

    (booru_filter, tag_filter)
}
