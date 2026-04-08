use crate::bot::notifier::ThrottledBot;
use crate::bot::BotHandler;
use crate::utils::args;
use crate::utils::channel::{self, BotChannelExt};
use teloxide::types::{ChatId, UserId};
use tracing::{error, warn};

impl BotHandler {
    /// Resolve the target chat ID for a subscription operation.
    pub(super) async fn resolve_subscription_target(
        &self,
        bot: &ThrottledBot,
        current_chat_id: ChatId,
        user_id: Option<UserId>,
        parsed_args: &args::ParsedArgs,
    ) -> Result<(ChatId, bool), String> {
        let channel_param = parsed_args.get_any(&["channel", "ch"]);

        match channel_param {
            Some(channel_str) if !channel_str.is_empty() => {
                let channel_identifier: channel::ChannelIdentifier =
                    channel_str.parse().map_err(|e| {
                        warn!(
                            "Failed to parse channel identifier '{}': {}",
                            channel_str, e
                        );
                        e
                    })?;

                let user_id = user_id.ok_or_else(|| {
                    warn!("User ID not available for channel subscription");
                    "无法获取用户信息".to_string()
                })?;

                let channel_id = bot
                    .validate_channel_permissions(&channel_identifier, user_id)
                    .await?;

                if let Err(e) = self
                    .repo
                    .upsert_chat(
                        channel_id.0,
                        "channel".to_string(),
                        None,
                        true,
                        crate::db::types::Tags::from(self.default_sensitive_tags.clone()),
                    )
                    .await
                {
                    error!(
                        "Failed to create chat record for channel {} during subscription: {:#}",
                        channel_id, e
                    );
                    return Err(format!(
                        "创建频道记录失败 (Failed to create chat record for channel {})",
                        channel_id
                    ));
                }

                Ok((channel_id, true))
            }
            _ => Ok((current_chat_id, false)),
        }
    }
}
