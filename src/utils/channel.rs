//! Channel permission checking utilities.
//!
//! Provides functions to verify that:
//! 1. The bot has permission to post in a channel
//! 2. The user is an administrator of the channel

use teloxide::prelude::*;
use teloxide::types::{ChatId, ChatMemberKind, ChatMemberStatus, Recipient};
use tracing::info;

/// A channel identifier that can be either a numeric ID or a username.
#[derive(Debug, Clone)]
pub enum ChannelIdentifier {
    /// Numeric chat ID (e.g., -1001234567890)
    Id(ChatId),
    /// Username starting with @ (e.g., @channelname)
    Username(String),
}

impl ChannelIdentifier {
    /// Convert to a Recipient for use with Telegram API calls.
    pub fn to_recipient(&self) -> Recipient {
        match self {
            ChannelIdentifier::Id(id) => Recipient::Id(*id),
            ChannelIdentifier::Username(username) => Recipient::ChannelUsername(username.clone()),
        }
    }
}

/// Check if the bot has permission to post messages in a channel.
///
/// Returns true if the bot is a member and has the right to post messages.
pub async fn check_bot_can_post(bot: &Bot, channel: &ChannelIdentifier) -> Result<bool, String> {
    let me = bot
        .get_me()
        .await
        .map_err(|e| format!("获取机器人信息失败: {}", e))?;
    let bot_user_id = me.id;

    let recipient = channel.to_recipient();
    match bot.get_chat_member(recipient.clone(), bot_user_id).await {
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
                "Bot {} in channel {:?}: status={:?}, can_post={}",
                bot_user_id,
                channel,
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
    channel: &ChannelIdentifier,
    user_id: UserId,
) -> Result<bool, String> {
    let recipient = channel.to_recipient();
    match bot.get_chat_member(recipient, user_id).await {
        Ok(member) => {
            let is_admin = matches!(
                member.status(),
                ChatMemberStatus::Administrator | ChatMemberStatus::Owner
            );

            info!(
                "User {} in channel {:?}: status={:?}, is_admin={}",
                user_id,
                channel,
                member.status(),
                is_admin
            );

            Ok(is_admin)
        }
        Err(e) => Err(format!("无法获取用户在频道中的状态: {}", e)),
    }
}

/// Resolve a channel identifier to a numeric ChatId.
///
/// For usernames, this fetches the chat info from Telegram to get the numeric ID.
pub async fn resolve_channel_id(bot: &Bot, channel: &ChannelIdentifier) -> Result<ChatId, String> {
    match channel {
        ChannelIdentifier::Id(id) => Ok(*id),
        ChannelIdentifier::Username(username) => {
            // Fetch chat info to get the numeric ID
            let recipient = Recipient::ChannelUsername(username.clone());
            match bot.get_chat(recipient).await {
                Ok(chat) => Ok(chat.id),
                Err(e) => Err(format!("无法获取频道信息: {}", e)),
            }
        }
    }
}

/// Validate channel permissions for a subscription operation.
///
/// Checks both that:
/// 1. The bot can post to the channel
/// 2. The user is an admin of the channel
///
/// Returns Ok(ChatId) with the resolved numeric channel ID if both conditions are met,
/// or an error message otherwise.
pub async fn validate_channel_permissions(
    bot: &Bot,
    channel: &ChannelIdentifier,
    user_id: UserId,
) -> Result<ChatId, String> {
    // Check bot permissions first
    let bot_can_post = check_bot_can_post(bot, channel).await?;
    if !bot_can_post {
        return Err("机器人在该频道没有发送消息的权限".to_string());
    }

    // Check user is admin
    let user_is_admin = check_user_is_channel_admin(bot, channel, user_id).await?;
    if !user_is_admin {
        return Err("您不是该频道的管理员".to_string());
    }

    // Resolve to numeric ID for database storage
    resolve_channel_id(bot, channel).await
}

/// Parse a channel identifier from string.
///
/// Supports:
/// - Numeric channel IDs (e.g., "-1001234567890")
/// - Channel usernames starting with @ (e.g., "@channelname")
///
/// Returns a ChannelIdentifier.
pub fn parse_channel_id(input: &str) -> Result<ChannelIdentifier, String> {
    let input = input.trim();

    if input.is_empty() {
        return Err("频道 ID 不能为空".to_string());
    }

    // Try parsing as a numeric ID first
    if let Ok(id) = input.parse::<i64>() {
        return Ok(ChannelIdentifier::Id(ChatId(id)));
    }

    // If starts with @, it's a username
    if let Some(username) = input.strip_prefix('@') {
        // Validate username format: @ followed by alphanumeric and underscores, min 5 chars after @
        if username.len() >= 5
            && username
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Ok(ChannelIdentifier::Username(input.to_string()));
        } else {
            return Err("无效的频道用户名格式 (用户名需至少5个字符)".to_string());
        }
    }

    Err(format!("无效的频道 ID: {}", input))
}
