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
        msg: &Message,
        args: String,
        is_admin: bool,
    ) -> ResponseResult<()> {
        let chat_id = msg.chat.id;
        let Some(actor) = crate::bot::permissions::message_actor(msg) else {
            return Ok(());
        };
        // Only the persisted Owner can change global roles, regardless of group status.
        match self.repo.get_user(actor.id.0 as i64).await {
            Ok(Some(user)) if user.role.is_owner() => {}
            Ok(_) => return Ok(()),
            Err(e) => {
                error!("Failed to authorize role change: {:#}", e);
                return Ok(());
            }
        }
        let target_user_id = match resolve_admin_target(&bot, &self.repo, msg, &args).await {
            Ok(id) => id,
            Err(message) => {
                bot.send_message(chat_id, message).await?;
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
                bot.send_message(
                    chat_id,
                    "❌ 设置用户角色失败。目标须已有数据库记录，且不能修改 Owner。",
                )
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
        msg: &Message,
        args: String,
        enabled: bool,
    ) -> ResponseResult<()> {
        let current_chat_id = msg.chat.id;
        let Some(actor) = crate::bot::permissions::message_actor(msg) else {
            return Ok(());
        };
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

        let global_admin = match self.repo.get_user(actor.id.0 as i64).await {
            Ok(Some(user)) => user.role.is_admin(),
            Ok(None) => false,
            Err(e) => {
                error!("Failed to authorize chat control: {:#}", e);
                return Ok(());
            }
        };
        if !global_admin
            && (target_chat_id != current_chat_id.0
                || !crate::bot::permissions::can_manage_chat(&bot, &self.repo, &msg.chat, actor.id)
                    .await)
        {
            return Ok(());
        }

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

/// Resolve only explicit IDs, actual reply senders, or freshly verified cached names.
async fn resolve_admin_target(
    bot: &ThrottledBot,
    repo: &crate::db::repo::Repo,
    msg: &Message,
    args: &str,
) -> Result<i64, &'static str> {
    let arg = args.trim();
    if arg.is_empty() {
        return msg
            .reply_to_message()
            .filter(|reply| reply.chat.id == msg.chat.id)
            .and_then(crate::bot::permissions::message_actor)
            .map(|user| user.id.0 as i64)
            .ok_or("❌ 请提供 user_id、@username，或回复真实用户的消息；不支持匿名或频道发送者。");
    }
    if let Ok(id) = arg.parse::<i64>() {
        return (id > 0).then_some(id).ok_or("❌ user_id 必须为正整数。");
    }
    let unresolved = "❌ 无法可靠解析 @username。目标需已与机器人交互且 Telegram 能验证其当前用户名；请改用数字 ID 或回复该用户消息。";
    let username = arg
        .strip_prefix('@')
        .filter(|name| {
            !name.is_empty() && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        })
        .ok_or(
            "❌ 用法：/setadmin 或 /unsetadmin <user_id|@username>，也可不带参数回复用户消息。",
        )?;
    let candidates = repo.get_username_candidates(username).await.map_err(|e| {
        error!("Failed to look up username: {:#}", e);
        unresolved
    })?;
    let mut verified = None;
    for candidate in candidates {
        if candidate.id <= 0 {
            continue;
        }
        let matches = if msg.chat.is_group() || msg.chat.is_supergroup() {
            let member = bot
                .get_chat_member(msg.chat.id, UserId(candidate.id as u64))
                .await
                .map_err(|e| {
                    error!("Failed to verify target member: {:#}", e);
                    unresolved
                })?;
            member.user.id.0 == candidate.id as u64
                && !member.user.is_bot
                && member
                    .user
                    .username
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(username))
        } else if msg.chat.is_private() {
            // Bot API cannot get an arbitrary private user by @username.
            // Resolve a known numeric ID, then verify its current private-chat username.
            let chat = bot.get_chat(ChatId(candidate.id)).await.map_err(|e| {
                error!("Failed to verify target private chat: {:#}", e);
                unresolved
            })?;
            chat.id.0 == candidate.id
                && chat.is_private()
                && chat
                    .username()
                    .is_some_and(|name| name.eq_ignore_ascii_case(username))
        } else {
            false
        };
        if matches {
            if verified.is_some() {
                return Err(unresolved);
            }
            verified = Some(candidate.id);
        }
    }
    verified.ok_or(unresolved)
}

#[cfg(test)]
#[path = "admin_tests.rs"]
mod tests;
