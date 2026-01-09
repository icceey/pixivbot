//! Channel permission checking utilities.
//!
//! Provides an extension trait for `Bot` to verify that:
//! 1. The bot has permission to post in a channel
//! 2. The user is an administrator of the channel
//!
//! # Example
//!
//! ```ignore
//! use crate::utils::channel::{ChannelIdentifier, BotChannelExt};
//!
//! let channel: ChannelIdentifier = "@mychannel".parse()?;
//! if bot.can_post_to_channel(&channel).await? {
//!     // Bot has permission to post
//! }
//! ```

use std::str::FromStr;

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

impl FromStr for ChannelIdentifier {
    type Err = String;

    /// Parse a channel identifier from string.
    ///
    /// Supports:
    /// - Numeric channel IDs (e.g., "-1001234567890")
    /// - Channel usernames starting with @ (e.g., "@channelname")
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let input = s.trim();

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
}

/// Extension trait for `Bot` providing channel permission checking methods.
///
/// This trait adds fluent methods to the `Bot` type for checking channel permissions,
/// allowing calls like `bot.can_post_to_channel(&channel)` instead of
/// `check_bot_can_post(&bot, &channel)`.
#[allow(async_fn_in_trait)]
pub trait BotChannelExt {
    /// Check if the bot has permission to post messages in a channel.
    ///
    /// Returns `Ok(true)` if the bot is an administrator with posting rights or the owner.
    async fn can_post_to_channel(&self, channel: &ChannelIdentifier) -> Result<bool, String>;

    /// Check if a user is an administrator of a channel.
    ///
    /// Returns `Ok(true)` if the user is either the owner or an administrator.
    async fn is_user_channel_admin(
        &self,
        channel: &ChannelIdentifier,
        user_id: UserId,
    ) -> Result<bool, String>;

    /// Resolve a channel identifier to a numeric ChatId.
    ///
    /// For usernames, this fetches the chat info from Telegram to get the numeric ID.
    async fn resolve_channel_id(&self, channel: &ChannelIdentifier) -> Result<ChatId, String>;

    /// Validate channel permissions for a subscription operation.
    ///
    /// Checks both that:
    /// 1. The bot can post to the channel
    /// 2. The user is an admin of the channel
    ///
    /// Returns `Ok(ChatId)` with the resolved numeric channel ID if both conditions are met,
    /// or an error message otherwise.
    async fn validate_channel_permissions(
        &self,
        channel: &ChannelIdentifier,
        user_id: UserId,
    ) -> Result<ChatId, String>;
}

impl BotChannelExt for Bot {
    async fn can_post_to_channel(&self, channel: &ChannelIdentifier) -> Result<bool, String> {
        let me = match self.get_me().await {
            Ok(me) => me,
            Err(e) => {
                tracing::error!("Failed to get bot info: {:#}", e);
                return Err("获取机器人信息失败".to_string());
            }
        };
        let bot_user_id = me.id;

        let recipient = channel.to_recipient();
        match self.get_chat_member(recipient.clone(), bot_user_id).await {
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
            Err(e) => {
                tracing::error!(
                    "Failed to get bot member status in channel {:?}: {:#}",
                    channel,
                    e
                );
                Err("无法获取机器人在频道中的状态".to_string())
            }
        }
    }

    async fn is_user_channel_admin(
        &self,
        channel: &ChannelIdentifier,
        user_id: UserId,
    ) -> Result<bool, String> {
        let recipient = channel.to_recipient();
        match self.get_chat_member(recipient, user_id).await {
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
            Err(e) => {
                tracing::error!(
                    "Failed to get user {} member status in channel {:?}: {:#}",
                    user_id,
                    channel,
                    e
                );
                Err("无法获取用户在频道中的状态".to_string())
            }
        }
    }

    async fn resolve_channel_id(&self, channel: &ChannelIdentifier) -> Result<ChatId, String> {
        match channel {
            ChannelIdentifier::Id(id) => Ok(*id),
            ChannelIdentifier::Username(username) => {
                // Fetch chat info to get the numeric ID
                let recipient = Recipient::ChannelUsername(username.clone());
                match self.get_chat(recipient).await {
                    Ok(chat) => Ok(chat.id),
                    Err(e) => {
                        tracing::error!("Failed to get channel info for {:?}: {:#}", channel, e);
                        Err("无法获取频道信息".to_string())
                    }
                }
            }
        }
    }

    async fn validate_channel_permissions(
        &self,
        channel: &ChannelIdentifier,
        user_id: UserId,
    ) -> Result<ChatId, String> {
        // Check bot permissions first
        let bot_can_post = self.can_post_to_channel(channel).await?;
        if !bot_can_post {
            return Err("机器人在该频道没有发送消息的权限".to_string());
        }

        // Check user is admin
        let user_is_admin = self.is_user_channel_admin(channel, user_id).await?;
        if !user_is_admin {
            return Err("您不是该频道的管理员".to_string());
        }

        // Resolve to numeric ID for database storage
        self.resolve_channel_id(channel).await
    }
}
