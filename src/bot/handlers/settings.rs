use crate::bot::notifier::ThrottledBot;
use crate::bot::BotHandler;
use crate::db::types::Tags;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use teloxide::utils::markdown;
use tracing::{error, info};

impl BotHandler {
    // ------------------------------------------------------------------------
    // Chat Settings Commands
    // ------------------------------------------------------------------------

    /// 启用或禁用敏感内容模糊
    ///
    /// 用法: `/blursensitive <on|off>`
    pub async fn handle_blur_sensitive(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        args: String,
    ) -> ResponseResult<()> {
        let arg = args.trim().to_lowercase();

        let blur = match arg.as_str() {
            "on" | "true" | "1" | "yes" => true,
            "off" | "false" | "0" | "no" => false,
            _ => {
                bot.send_message(chat_id, "❌ 用法: `/blursensitive <on|off>`")
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
                return Ok(());
            }
        };

        match self.repo.set_blur_sensitive_tags(chat_id.0, blur).await {
            Ok(_) => {
                bot.send_message(
                    chat_id,
                    if blur {
                        "✅ 敏感内容模糊已**启用**"
                    } else {
                        "✅ 敏感内容模糊已**禁用**"
                    },
                )
                .parse_mode(ParseMode::MarkdownV2)
                .await?;

                info!("Chat {} set blur_sensitive_tags to {}", chat_id, blur);
            }
            Err(e) => {
                error!("Failed to set blur_sensitive_tags: {:#}", e);
                bot.send_message(chat_id, "❌ 更新设置失败").await?;
            }
        }

        Ok(())
    }

    /// 设置敏感标签
    ///
    /// 用法: `/sensitivetags <tag1,tag2,...>`
    pub async fn handle_sensitive_tags(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        args: String,
    ) -> ResponseResult<()> {
        let arg = args.trim();

        if arg.is_empty() {
            bot.send_message(chat_id, "❌ 用法: `/sensitivetags <tag1,tag2,...>`")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        let tags: Vec<String> = arg
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        if tags.is_empty() {
            bot.send_message(chat_id, "❌ 未提供有效的标签").await?;
            return Ok(());
        }

        let sensitive_tags = Tags::from(tags.clone());

        match self
            .repo
            .set_sensitive_tags(chat_id.0, sensitive_tags)
            .await
        {
            Ok(_) => {
                let tag_list: Vec<String> = tags
                    .iter()
                    .map(|s| format!("`{}`", markdown::escape(s)))
                    .collect();

                let message = format!("✅ 敏感标签已更新: {}", tag_list.join(", "));

                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;

                info!("Chat {} set sensitive_tags", chat_id);
            }
            Err(e) => {
                error!("Failed to set sensitive_tags: {:#}", e);
                bot.send_message(chat_id, "❌ 更新设置失败").await?;
            }
        }

        Ok(())
    }

    /// 清除所有敏感标签
    pub async fn handle_clear_sensitive_tags(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
    ) -> ResponseResult<()> {
        match self
            .repo
            .set_sensitive_tags(chat_id.0, Tags::default())
            .await
        {
            Ok(_) => {
                bot.send_message(chat_id, "✅ 敏感标签已清除").await?;

                info!("Chat {} cleared sensitive_tags", chat_id);
            }
            Err(e) => {
                error!("Failed to clear sensitive_tags: {:#}", e);
                bot.send_message(chat_id, "❌ 更新设置失败").await?;
            }
        }

        Ok(())
    }

    /// 设置排除的标签
    ///
    /// 用法: `/excludetags <tag1,tag2,...>`
    pub async fn handle_exclude_tags(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        args: String,
    ) -> ResponseResult<()> {
        let arg = args.trim();

        if arg.is_empty() {
            bot.send_message(chat_id, "❌ 用法: `/excludetags <tag1,tag2,...>`")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        let tags: Vec<String> = arg
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        if tags.is_empty() {
            bot.send_message(chat_id, "❌ 未提供有效的标签").await?;
            return Ok(());
        }

        let excluded_tags = Tags::from(tags.clone());

        match self.repo.set_excluded_tags(chat_id.0, excluded_tags).await {
            Ok(_) => {
                let tag_list: Vec<String> = tags
                    .iter()
                    .map(|s| format!("`{}`", markdown::escape(s)))
                    .collect();

                let message = format!("✅ 排除标签已更新: {}", tag_list.join(", "));

                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;

                info!("Chat {} set excluded_tags", chat_id);
            }
            Err(e) => {
                error!("Failed to set excluded_tags: {:#}", e);
                bot.send_message(chat_id, "❌ 更新设置失败").await?;
            }
        }

        Ok(())
    }

    /// 清除所有排除的标签
    pub async fn handle_clear_excluded_tags(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
    ) -> ResponseResult<()> {
        match self
            .repo
            .set_excluded_tags(chat_id.0, Tags::default())
            .await
        {
            Ok(_) => {
                bot.send_message(chat_id, "✅ 排除标签已清除").await?;

                info!("Chat {} cleared excluded_tags", chat_id);
            }
            Err(e) => {
                error!("Failed to clear excluded_tags: {:#}", e);
                bot.send_message(chat_id, "❌ 更新设置失败").await?;
            }
        }

        Ok(())
    }

    /// 显示聊天设置
    pub async fn handle_settings(&self, bot: ThrottledBot, chat_id: ChatId) -> ResponseResult<()> {
        match self.repo.get_chat(chat_id.0).await {
            Ok(Some(chat)) => {
                let blur_status = if chat.blur_sensitive_tags {
                    "**已启用**"
                } else {
                    "**已禁用**"
                };

                let sensitive_tags = if chat.sensitive_tags.is_empty() {
                    "无".to_string()
                } else {
                    chat.sensitive_tags
                        .iter()
                        .map(|s| format!("`{}`", markdown::escape(s)))
                        .collect::<Vec<_>>()
                        .join(", ")
                };

                let excluded_tags = if chat.excluded_tags.is_empty() {
                    "无".to_string()
                } else {
                    chat.excluded_tags
                        .iter()
                        .map(|s| format!("`{}`", markdown::escape(s)))
                        .collect::<Vec<_>>()
                        .join(", ")
                };

                let message = format!(
                    "⚙️ *聊天设置*\n\n🔒 敏感内容模糊: {}\n🏷 敏感标签: {}\n🚫 排除标签: {}",
                    blur_status, sensitive_tags, excluded_tags
                );

                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Ok(None) => {
                bot.send_message(chat_id, "❌ 未找到聊天").await?;
            }
            Err(e) => {
                error!("Failed to get chat settings: {:#}", e);
                bot.send_message(chat_id, "❌ 获取设置失败").await?;
            }
        }

        Ok(())
    }
}
