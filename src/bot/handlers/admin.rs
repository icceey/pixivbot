use crate::bot::notifier::ThrottledBot;
use crate::bot::BotHandler;
use crate::db::types::UserRole;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tracing::{error, info};

impl BotHandler {
    // ------------------------------------------------------------------------
    // Admin Commands
    // ------------------------------------------------------------------------

    /// 设置用户为管理员或移除管理员角色
    ///
    /// # Arguments
    /// * `is_admin` - true: 设置为管理员, false: 设置为普通用户
    pub async fn handle_set_admin(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        args: String,
        is_admin: bool,
    ) -> ResponseResult<()> {
        let target_user_id = match args.trim().parse::<i64>() {
            Ok(id) => id,
            Err(_) => {
                bot.send_message(
                    chat_id,
                    if is_admin {
                        "❌ 用法: `/setadmin <user_id>`"
                    } else {
                        "❌ 用法: `/unsetadmin <user_id>`"
                    },
                )
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
                return Ok(());
            }
        };

        let role = if is_admin {
            UserRole::Admin
        } else {
            UserRole::User
        };

        match self.repo.set_user_role(target_user_id, role).await {
            Ok(user) => {
                bot.send_message(
                    chat_id,
                    format!("✅ 成功将用户 `{}` 的角色设置为 **{}**", user.id, role),
                )
                .parse_mode(ParseMode::MarkdownV2)
                .await?;

                info!("Owner set user {} role to {:?}", target_user_id, role);
            }
            Err(e) => {
                error!("Failed to set user role: {:#}", e);
                bot.send_message(chat_id, "❌ 设置用户角色失败。用户可能不存在。")
                    .await?;
            }
        }

        Ok(())
    }

    /// 启用或禁用聊天
    ///
    /// # Arguments
    /// * `current_chat_id` - 当前聊天ID（用于发送响应消息）
    /// * `args` - 目标聊天ID（可选，默认为当前聊天）
    /// * `enabled` - true: 启用, false: 禁用
    pub async fn handle_enable_chat(
        &self,
        bot: ThrottledBot,
        current_chat_id: ChatId,
        args: String,
        enabled: bool,
    ) -> ResponseResult<()> {
        // Parse target chat_id from args, or use current chat_id
        let target_chat_id = if args.trim().is_empty() {
            current_chat_id.0
        } else {
            match args.trim().parse::<i64>() {
                Ok(id) => id,
                Err(_) => {
                    bot.send_message(
                        current_chat_id,
                        if enabled {
                            "❌ 用法: `/enablechat [chat_id]`"
                        } else {
                            "❌ 用法: `/disablechat [chat_id]`"
                        },
                    )
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
                    return Ok(());
                }
            }
        };

        match self.repo.set_chat_enabled(target_chat_id, enabled).await {
            Ok(_) => {
                // 判断是否是当前聊天
                let is_current_chat = target_chat_id == current_chat_id.0;

                let message = if enabled {
                    if is_current_chat {
                        "✅ 当前聊天已成功启用".to_string()
                    } else {
                        format!("✅ 聊天 `{}` 已成功启用", target_chat_id)
                    }
                } else if is_current_chat {
                    "✅ 当前聊天已成功禁用".to_string()
                } else {
                    format!("✅ 聊天 `{}` 已成功禁用", target_chat_id)
                };

                bot.send_message(current_chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;

                info!(
                    "Admin {} chat {}",
                    if enabled { "enabled" } else { "disabled" },
                    target_chat_id
                );
            }
            Err(e) => {
                error!("Failed to set chat enabled status: {:#}", e);
                bot.send_message(current_chat_id, "❌ 更新聊天状态失败")
                    .await?;
            }
        }

        Ok(())
    }
}
