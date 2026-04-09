use crate::bot::notifier::ThrottledBot;
use crate::bot::BotHandler;
use crate::db::types::{TagFilter, TaskType};
use crate::pixiv::model::RankingMode;
use crate::utils::args;
use teloxide::prelude::*;
use teloxide::types::{ChatAction, ChatId, ParseMode, UserId};
use teloxide::utils::markdown;
use tracing::{error, warn};

impl BotHandler {
    /// 订阅 Pixiv 排行榜
    pub async fn handle_sub_ranking(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        if let Err(e) = bot.send_chat_action(chat_id, ChatAction::Typing).await {
            warn!("Failed to set chat action for chat {}: {:#}", chat_id, e);
        }

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
            let available_modes = RankingMode::all_modes().join(", ");
            bot.send_message(
                chat_id,
                format!(
                    "❌ 用法: `/subrank [channel=<id>] <mode> [+tag1 -tag2]`\n可用模式: {}",
                    markdown::escape(&available_modes)
                ),
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
            return Ok(());
        }

        let mode = match RankingMode::from_str(parts[0]) {
            Some(mode) => mode,
            None => {
                let available_modes = RankingMode::all_modes().join(", ");
                bot.send_message(
                    chat_id,
                    format!("❌ 无效的排行榜模式。可用模式: {}", available_modes),
                )
                .await?;
                return Ok(());
            }
        };

        let filter_tags = TagFilter::parse_from_args(&parts[1..]);

        match self
            .create_subscription(
                target_chat_id.0,
                TaskType::Ranking,
                mode.as_str(),
                None,
                filter_tags.clone(),
            )
            .await
        {
            Ok(_) => {
                let mut message = format!("✅ 成功订阅 {}", mode.display_name());
                if !filter_tags.is_empty() {
                    message.push_str(&format!("\n\n🏷 {}", filter_tags.format_for_display()));
                }
                if is_channel {
                    message.push_str(&format!("\n📢 频道: `{}`", target_chat_id.0));
                }
                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!("Failed to subscribe to ranking {}: {:#}", mode.as_str(), e);
                bot.send_message(chat_id, "❌ 创建订阅失败").await?;
            }
        }

        Ok(())
    }

    /// 取消订阅排行榜
    pub async fn handle_unsub_ranking(
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

        let mode_str = parsed.remaining.trim();

        if mode_str.is_empty() {
            bot.send_message(chat_id, "❌ 用法: `/unsubrank [channel=<id>] <mode>`")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        let mode = match RankingMode::from_str(mode_str) {
            Some(mode) => mode,
            None => {
                let available_modes = RankingMode::all_modes().join(", ");
                bot.send_message(
                    chat_id,
                    format!("❌ 无效的排行榜模式。可用模式: {}", available_modes),
                )
                .await?;
                return Ok(());
            }
        };

        match self
            .delete_subscription(target_chat_id.0, TaskType::Ranking, mode.as_str())
            .await
        {
            Ok(_) => {
                let mut message = format!("✅ 成功取消订阅 {}", mode.display_name());
                if is_channel {
                    message.push_str(&format!("\n📢 频道: `{}`", target_chat_id.0));
                }
                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!(
                    "Failed to unsubscribe from ranking {}: {:#}",
                    mode.as_str(),
                    e
                );
                bot.send_message(chat_id, "❌ 取消订阅失败").await?;
            }
        }

        Ok(())
    }
}
