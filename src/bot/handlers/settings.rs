use crate::bot::notifier::ThrottledBot;
use crate::bot::state::{SettingsState, SettingsStorage};
use crate::bot::BotHandler;
use crate::db::entities::chats;
use crate::db::types::Tags;
use std::time::Instant;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, MessageId, ParseMode};
use teloxide::utils::markdown;
use tracing::{error, info, warn};

/// Callback data prefix for settings buttons
pub const SETTINGS_CALLBACK_PREFIX: &str = "settings:";

impl BotHandler {
    // ------------------------------------------------------------------------
    // Chat Settings - Interactive UI
    // ------------------------------------------------------------------------

    /// Display the settings panel with inline keyboard buttons
    ///
    /// When /settings is invoked:
    /// - Fetch current chat settings from DB
    /// - Display a message summarizing blur status, sensitive tags, and excluded tags
    /// - Attach inline keyboard with toggle and edit buttons
    ///
    /// **Security Note**: Viewing settings is intentionally unrestricted to allow
    /// all users to see current chat configuration. Modifications (via callback
    /// handlers) require admin permissions.
    pub async fn handle_settings(&self, bot: ThrottledBot, chat_id: ChatId) -> ResponseResult<()> {
        match self.repo.get_chat(chat_id.0).await {
            Ok(Some(chat)) => {
                let (message, keyboard) = build_settings_panel(&chat);

                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .reply_markup(keyboard)
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

    /// Update the settings panel message (edit existing message)
    pub async fn refresh_settings_panel(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        message_id: MessageId,
    ) -> ResponseResult<()> {
        match self.repo.get_chat(chat_id.0).await {
            Ok(Some(chat)) => {
                let (message, keyboard) = build_settings_panel(&chat);

                bot.edit_message_text(chat_id, message_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .reply_markup(keyboard)
                    .await?;
            }
            Ok(None) => {
                warn!("Chat {} not found when refreshing settings panel", chat_id);
            }
            Err(e) => {
                error!("Failed to get chat settings for refresh: {:#}", e);
            }
        }

        Ok(())
    }
}

/// Build the settings panel message and inline keyboard
fn build_settings_panel(chat: &chats::Model) -> (String, InlineKeyboardMarkup) {
    // Build status text
    let blur_status = if chat.blur_sensitive_tags {
        "*已启用*"
    } else {
        "*已禁用*"
    };

    let mention_status = if chat.allow_without_mention {
        "*无需@响应*"
    } else {
        "*需要@响应*"
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

    // 私聊时不显示群组命令响应设置（该设置只对群组有意义）
    let is_private = chat.r#type == "private";

    let message = if is_private {
        format!(
            "⚙️ *聊天设置*\n\n\
             🔒 敏感内容模糊: {}\n\
             🏷 敏感标签: {}\n\
             🚫 排除标签: {}",
            blur_status, sensitive_tags, excluded_tags
        )
    } else {
        format!(
            "⚙️ *聊天设置*\n\n\
             🔒 敏感内容模糊: {}\n\
             📢 群组命令响应: {}\n\
             🏷 敏感标签: {}\n\
             🚫 排除标签: {}",
            blur_status, mention_status, sensitive_tags, excluded_tags
        )
    };

    // Build inline keyboard
    // Row 1: Toggle blur button
    let blur_button_text = if chat.blur_sensitive_tags {
        "🔓关闭模糊"
    } else {
        "🔒开启模糊"
    };
    let blur_button = InlineKeyboardButton::callback(
        blur_button_text,
        format!("{}blur:toggle", SETTINGS_CALLBACK_PREFIX),
    );

    // Row 2: Toggle mention requirement button (only meaningful for groups)
    let mention_button_text = if chat.allow_without_mention {
        // Currently allows commands without @; pressing will enable @ requirement
        "📢启用@要求"
    } else {
        // Currently requires @; pressing will disable the @ requirement (allow without @)
        "📢关闭@要求"
    };
    let mention_button = InlineKeyboardButton::callback(
        mention_button_text,
        format!("{}mention:toggle", SETTINGS_CALLBACK_PREFIX),
    );

    // Row 3: Edit tags buttons
    let sensitive_tags_button = InlineKeyboardButton::callback(
        "✏️敏感标签",
        format!("{}edit:sensitive", SETTINGS_CALLBACK_PREFIX),
    );
    let excluded_tags_button = InlineKeyboardButton::callback(
        "✏️排除标签",
        format!("{}edit:exclude", SETTINGS_CALLBACK_PREFIX),
    );

    // 私聊时不显示 mention 按钮（该设置只对群组有意义）
    let keyboard = if is_private {
        InlineKeyboardMarkup::new(vec![
            vec![blur_button],
            vec![sensitive_tags_button, excluded_tags_button],
        ])
    } else {
        InlineKeyboardMarkup::new(vec![
            vec![blur_button],
            vec![mention_button],
            vec![sensitive_tags_button, excluded_tags_button],
        ])
    };

    (message, keyboard)
}

/// Parse tags from user input (comma-separated, supports both , and ，)
pub fn parse_tags_input(input: &str) -> Vec<String> {
    input
        .split([',', '，'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Process settings callback query
///
/// This function handles callback queries from the settings panel buttons.
/// It's called from the dispatcher and handles:
/// - `settings:blur:toggle` - Toggle blur setting
/// - `settings:edit:sensitive` - Prompt for sensitive tags input
/// - `settings:edit:exclude` - Prompt for excluded tags input
pub async fn handle_settings_callback(
    bot: ThrottledBot,
    q: CallbackQuery,
    callback_data: String,
    handler: BotHandler,
    storage: SettingsStorage,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Get chat and message info
    let (chat_id, message_id) = match &q.message {
        Some(msg) => (msg.chat().id, msg.id()),
        None => {
            warn!("No message in settings callback query");
            bot.answer_callback_query(q.id).await?;
            return Ok(());
        }
    };

    let user_id = q.from.id;

    // Check if user is admin (security check)
    // Note: The middleware ensures users exist via ensure_user_and_chat, so Ok(None)
    // indicates a data inconsistency issue that should be logged.
    let user_role = match handler.repo.get_user(user_id.0 as i64).await {
        Ok(Some(user)) => user.role,
        Ok(None) => {
            // This shouldn't happen as middleware ensures users exist
            warn!(
                "User {} not found in database during settings callback - data inconsistency",
                user_id
            );
            bot.answer_callback_query(q.id)
                .text("发生错误，请稍后重试")
                .show_alert(true)
                .await?;
            return Ok(());
        }
        Err(e) => {
            error!("Failed to get user for callback: {:#}", e);
            bot.answer_callback_query(q.id)
                .text("发生错误，请稍后重试")
                .show_alert(true)
                .await?;
            return Ok(());
        }
    };

    if !user_role.is_admin() {
        bot.answer_callback_query(q.id)
            .text("只有管理员可以修改设置")
            .show_alert(true)
            .await?;
        return Ok(());
    }

    // Parse callback data (format: settings:action:value)
    let action = callback_data
        .strip_prefix(SETTINGS_CALLBACK_PREFIX)
        .unwrap_or("");

    match action {
        "blur:toggle" => {
            // Toggle blur setting
            match handler.repo.get_chat(chat_id.0).await {
                Ok(Some(chat)) => {
                    let new_blur = !chat.blur_sensitive_tags;
                    match handler
                        .repo
                        .set_blur_sensitive_tags(chat_id.0, new_blur)
                        .await
                    {
                        Ok(_) => {
                            info!(
                                "Chat {} blur_sensitive_tags toggled to {} by user {}",
                                chat_id, new_blur, user_id
                            );

                            // Refresh the settings panel
                            handler
                                .refresh_settings_panel(bot.clone(), chat_id, message_id)
                                .await?;

                            bot.answer_callback_query(q.id).await?;
                        }
                        Err(e) => {
                            error!("Failed to toggle blur setting: {:#}", e);
                            bot.answer_callback_query(q.id)
                                .text("更新设置失败")
                                .show_alert(true)
                                .await?;
                        }
                    }
                }
                Ok(None) => {
                    warn!(
                        "Chat {} not found when toggling blur_sensitive_tags by user {}",
                        chat_id, user_id
                    );
                    bot.answer_callback_query(q.id)
                        .text("获取聊天信息失败")
                        .show_alert(true)
                        .await?;
                }
                Err(e) => {
                    error!(
                        "Failed to fetch chat {} for blur toggle by user {}: {:#}",
                        chat_id, user_id, e
                    );
                    bot.answer_callback_query(q.id)
                        .text("获取聊天信息失败")
                        .show_alert(true)
                        .await?;
                }
            }
        }
        "mention:toggle" => {
            // Toggle allow_without_mention setting
            match handler.repo.get_chat(chat_id.0).await {
                Ok(Some(chat)) => {
                    let new_allow = !chat.allow_without_mention;
                    match handler
                        .repo
                        .set_allow_without_mention(chat_id.0, new_allow)
                        .await
                    {
                        Ok(_) => {
                            info!(
                                "Chat {} allow_without_mention toggled to {} by user {}",
                                chat_id, new_allow, user_id
                            );

                            // Refresh the settings panel
                            handler
                                .refresh_settings_panel(bot.clone(), chat_id, message_id)
                                .await?;

                            bot.answer_callback_query(q.id).await?;
                        }
                        Err(e) => {
                            error!("Failed to toggle mention setting: {:#}", e);
                            bot.answer_callback_query(q.id)
                                .text("更新设置失败")
                                .show_alert(true)
                                .await?;
                        }
                    }
                }
                Ok(None) => {
                    warn!(
                        "Chat {} not found when toggling allow_without_mention by user {}",
                        chat_id, user_id
                    );
                    bot.answer_callback_query(q.id)
                        .text("获取聊天信息失败")
                        .show_alert(true)
                        .await?;
                }
                Err(e) => {
                    error!(
                        "Failed to fetch chat {} for mention toggle by user {}: {:#}",
                        chat_id, user_id, e
                    );
                    bot.answer_callback_query(q.id)
                        .text("获取聊天信息失败")
                        .show_alert(true)
                        .await?;
                }
            }
        }
        "edit:sensitive" | "edit:exclude" => {
            // Store dialogue state for this user
            let is_sensitive = action == "edit:sensitive";
            let state = if is_sensitive {
                SettingsState::WaitingForSensitiveTags {
                    settings_message_id: message_id,
                    created_at: Instant::now(),
                }
            } else {
                SettingsState::WaitingForExcludedTags {
                    settings_message_id: message_id,
                    created_at: Instant::now(),
                }
            };

            // Store the state
            {
                let mut storage_guard = storage.write().await;
                storage_guard.insert((chat_id, user_id), state);
            }

            let tag_type = if is_sensitive {
                "敏感标签"
            } else {
                "排除标签"
            };

            let username = q
                .from
                .username
                .as_ref()
                .map(|u| format!("@{}", u))
                .unwrap_or_else(|| q.from.first_name.clone());

            let prompt = format!(
                "{} 请在5分钟内发送新的{}（用逗号分隔），或发送 `clear` 清除所有标签。您发送的内容将直接覆盖原有配置\n\n发送 /cancel 取消操作。",
                markdown::escape(&username),
                tag_type
            );

            bot.send_message(chat_id, prompt)
                .parse_mode(ParseMode::MarkdownV2)
                .await?;

            bot.answer_callback_query(q.id).await?;

            info!(
                "User {} in chat {} started editing {} (message_id: {})",
                user_id, chat_id, tag_type, message_id
            );
        }
        _ => {
            warn!("Unknown settings callback action: {}", action);
            bot.answer_callback_query(q.id).await?;
        }
    }

    Ok(())
}

/// Process settings text input (for tag editing)
///
/// This function handles text messages when a user is in a Waiting... state.
/// It's called from the dispatcher for users who have an active settings dialogue.
///
/// Returns true if the message was handled (user was in a waiting state),
/// false if the user has no active settings dialogue.
pub async fn handle_settings_input(
    bot: ThrottledBot,
    msg: Message,
    handler: BotHandler,
    storage: SettingsStorage,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let chat_id = msg.chat.id;
    let user_id = match msg.from.as_ref() {
        Some(user) => user.id,
        None => return Ok(false),
    };

    // Check if user has an active dialogue state
    let state = {
        let storage_guard = storage.read().await;
        storage_guard.get(&(chat_id, user_id)).cloned()
    };

    let (is_sensitive, settings_message_id) = match &state {
        Some(s @ SettingsState::WaitingForSensitiveTags { .. }) => (true, s.settings_message_id()),
        Some(s @ SettingsState::WaitingForExcludedTags { .. }) => (false, s.settings_message_id()),
        None => return Ok(false), // No active state, not handled
    };

    let text = msg.text().unwrap_or("");

    // Check for clear keyword
    if text.eq_ignore_ascii_case("clear") {
        let result = if is_sensitive {
            handler
                .repo
                .set_sensitive_tags(chat_id.0, Tags::default())
                .await
        } else {
            handler
                .repo
                .set_excluded_tags(chat_id.0, Tags::default())
                .await
        };

        match result {
            Ok(_) => {
                let tag_type = if is_sensitive {
                    "敏感标签"
                } else {
                    "排除标签"
                };
                bot.send_message(chat_id, format!("✅ {}已清除", tag_type))
                    .await?;

                info!("Chat {} cleared {} by user {}", chat_id, tag_type, user_id);
            }
            Err(e) => {
                error!("Failed to clear tags: {:#}", e);
                bot.send_message(chat_id, "❌ 更新设置失败").await?;
            }
        }
    } else {
        // Parse tags
        let tags = parse_tags_input(text);

        if tags.is_empty() {
            bot.send_message(chat_id, "❌ 未提供有效的标签").await?;
            // Clear state and return as handled
            {
                let mut storage_guard = storage.write().await;
                storage_guard.remove(&(chat_id, user_id));
            }
            return Ok(true);
        }

        let tags_obj = Tags::from(tags.clone());

        let result = if is_sensitive {
            handler.repo.set_sensitive_tags(chat_id.0, tags_obj).await
        } else {
            handler.repo.set_excluded_tags(chat_id.0, tags_obj).await
        };

        match result {
            Ok(_) => {
                let tag_type = if is_sensitive {
                    "敏感标签"
                } else {
                    "排除标签"
                };

                let tag_list: Vec<String> = tags
                    .iter()
                    .map(|s| format!("`{}`", markdown::escape(s)))
                    .collect();

                let message = format!("✅ {}已更新: {}", tag_type, tag_list.join(", "));

                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;

                info!("Chat {} updated {} by user {}", chat_id, tag_type, user_id);
            }
            Err(e) => {
                error!("Failed to update tags: {:#}", e);
                bot.send_message(chat_id, "❌ 更新设置失败").await?;
            }
        }
    }

    // Clear state after processing
    {
        let mut storage_guard = storage.write().await;
        storage_guard.remove(&(chat_id, user_id));
    }

    // Refresh the settings panel
    handler
        .refresh_settings_panel(bot, chat_id, settings_message_id)
        .await?;

    Ok(true) // Message was handled
}

/// Handle /cancel command - clear any pending settings dialogue state
///
/// Returns true if the user had an active state that was cleared,
/// false if no state was active.
pub async fn handle_settings_cancel(
    bot: ThrottledBot,
    msg: Message,
    storage: SettingsStorage,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let chat_id = msg.chat.id;
    let user_id = match msg.from.as_ref() {
        Some(user) => user.id,
        None => return Ok(false),
    };

    // Check if user has an active dialogue state
    let had_state = {
        let mut storage_guard = storage.write().await;
        storage_guard.remove(&(chat_id, user_id)).is_some()
    };

    if had_state {
        bot.send_message(chat_id, "✅ 操作已取消").await?;
        info!(
            "User {} in chat {} cancelled settings operation",
            user_id, chat_id
        );
    }

    Ok(had_state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_tags_input_normal_comma() {
        let result = parse_tags_input("tag1, tag2, tag3");
        assert_eq!(result, vec!["tag1", "tag2", "tag3"]);
    }

    #[test]
    fn test_parse_tags_input_chinese_comma() {
        let result = parse_tags_input("tag1，tag2，tag3");
        assert_eq!(result, vec!["tag1", "tag2", "tag3"]);
    }

    #[test]
    fn test_parse_tags_input_mixed_commas() {
        let result = parse_tags_input("tag1, tag2，tag3, tag4");
        assert_eq!(result, vec!["tag1", "tag2", "tag3", "tag4"]);
    }

    #[test]
    fn test_parse_tags_input_empty() {
        let result = parse_tags_input("");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_tags_input_whitespace_only() {
        let result = parse_tags_input("   ,   ,   ");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_tags_input_whitespace_handling() {
        let result = parse_tags_input("  tag1  ,  tag2  ");
        assert_eq!(result, vec!["tag1", "tag2"]);
    }

    #[test]
    fn test_parse_tags_input_single_tag() {
        let result = parse_tags_input("single_tag");
        assert_eq!(result, vec!["single_tag"]);
    }

    #[test]
    fn test_parse_tags_input_duplicate_tags() {
        // Note: This function does NOT deduplicate - just parses
        let result = parse_tags_input("tag1, tag1, tag2");
        assert_eq!(result, vec!["tag1", "tag1", "tag2"]);
    }

    #[test]
    fn test_parse_tags_input_unicode_tags() {
        let result = parse_tags_input("日本語, R-18, 原神");
        assert_eq!(result, vec!["日本語", "R-18", "原神"]);
    }

    #[test]
    fn test_parse_tags_input_special_chars() {
        let result = parse_tags_input("tag-with-dash, tag_with_underscore, tag.with.dot");
        assert_eq!(
            result,
            vec!["tag-with-dash", "tag_with_underscore", "tag.with.dot"]
        );
    }
}
