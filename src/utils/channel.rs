//! Channel permission checking utilities.
//!
//! Provides functions to verify that:
//! 1. The bot has permission to post in a channel
//! 2. The user is an administrator of the channel

use teloxide::prelude::*;
use teloxide::types::{ChatId, ChatMemberKind, ChatMemberStatus};
use tracing::info;

/// Check if the bot has permission to post messages in a channel.
///
/// Returns true if the bot is a member and has the right to post messages.
pub async fn check_bot_can_post(bot: &Bot, channel_id: ChatId) -> Result<bool, String> {
    let me = bot
        .get_me()
        .await
        .map_err(|e| format!("获取机器人信息失败: {}", e))?;
    let bot_user_id = me.id;

    match bot.get_chat_member(channel_id, bot_user_id).await {
        Ok(member) => {
            let can_post = match &member.kind {
                ChatMemberKind::Administrator(admin) => {
                    // Check if admin can post messages
                    admin.can_post_messages
                }
                ChatMemberKind::Owner(_) => true,
                _ => false,
            };

            info!(
                "Bot {} in channel {}: status={:?}, can_post={}",
                bot_user_id,
                channel_id,
                member.status(),
                can_post
            );

            Ok(can_post)
        }
        Err(e) => Err(format!("无法获取机器人在频道中的状态: {}", e)),
    }
}

/// Check if a user is an administrator of a channel.
///
/// Returns true if the user is either the owner or an administrator.
pub async fn check_user_is_channel_admin(
    bot: &Bot,
    channel_id: ChatId,
    user_id: UserId,
) -> Result<bool, String> {
    match bot.get_chat_member(channel_id, user_id).await {
        Ok(member) => {
            let is_admin = matches!(
                member.status(),
                ChatMemberStatus::Administrator | ChatMemberStatus::Owner
            );

            info!(
                "User {} in channel {}: status={:?}, is_admin={}",
                user_id,
                channel_id,
                member.status(),
                is_admin
            );

            Ok(is_admin)
        }
        Err(e) => Err(format!("无法获取用户在频道中的状态: {}", e)),
    }
}

/// Validate channel permissions for a subscription operation.
///
/// Checks both that:
/// 1. The bot can post to the channel
/// 2. The user is an admin of the channel
///
/// Returns Ok(()) if both conditions are met, or an error message otherwise.
pub async fn validate_channel_permissions(
    bot: &Bot,
    channel_id: ChatId,
    user_id: UserId,
) -> Result<(), String> {
    // Check bot permissions first
    let bot_can_post = check_bot_can_post(bot, channel_id).await?;
    if !bot_can_post {
        return Err("机器人在该频道没有发送消息的权限".to_string());
    }

    // Check user is admin
    let user_is_admin = check_user_is_channel_admin(bot, channel_id, user_id).await?;
    if !user_is_admin {
        return Err("您不是该频道的管理员".to_string());
    }

    Ok(())
}

/// Parse a channel identifier from string.
///
/// Supports:
/// - Channel ID as number (e.g., "-1001234567890")
/// - Channel username (e.g., "@channelname" or "channelname")
///
/// Returns the ChatId for the channel.
pub fn parse_channel_id(input: &str) -> Result<ChatId, String> {
    let input = input.trim();

    if input.is_empty() {
        return Err("频道 ID 不能为空".to_string());
    }

    // Try parsing as a numeric ID
    if let Ok(id) = input.parse::<i64>() {
        return Ok(ChatId(id));
    }

    // If starts with @, it's a username - we'll need to resolve it
    // For now, we only support numeric IDs
    // Username resolution would require additional API calls
    if input.starts_with('@') {
        return Err("请使用频道 ID (数字格式) 而非用户名".to_string());
    }

    Err(format!("无效的频道 ID: {}", input))
}
