use crate::bot::notifier::ThrottledBot;
use crate::bot::state::{SettingsState, SettingsStorage};
use crate::bot::BotHandler;
use crate::db::entities::chats;
use crate::db::types::Tags;
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
        "⚙️ *聊天设置*\n\n\
         🔒 敏感内容模糊: {}\n\
         🏷 敏感标签: {}\n\
         🚫 排除标签: {}",
        blur_status, sensitive_tags, excluded_tags
    );

    // Build inline keyboard
    // Row 1: Toggle blur button
    let blur_button_text = if chat.blur_sensitive_tags {
        "🔓 关闭模糊"
    } else {
        "🔒 开启模糊"
    };
    let blur_button = InlineKeyboardButton::callback(
        blur_button_text,
        format!("{}blur:toggle", SETTINGS_CALLBACK_PREFIX),
    );

    // Row 2: Edit tags buttons
    let sensitive_tags_button = InlineKeyboardButton::callback(
        "✏️ 编辑敏感标签",
        format!("{}edit:sensitive", SETTINGS_CALLBACK_PREFIX),
    );
    let excluded_tags_button = InlineKeyboardButton::callback(
        "✏️ 编辑排除标签",
        format!("{}edit:exclude", SETTINGS_CALLBACK_PREFIX),
    );

    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![blur_button],
        vec![sensitive_tags_button, excluded_tags_button],
    ]);

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
    let user_role = match handler.repo.get_user(user_id.0 as i64).await {
        Ok(Some(user)) => user.role,
        Ok(None) => {
            bot.answer_callback_query(q.id)
                .text("只有管理员可以修改设置")
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
                _ => {
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
                }
            } else {
                SettingsState::WaitingForExcludedTags {
                    settings_message_id: message_id,
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
                "{} 请回复此消息输入新的{}（用逗号分隔），或输入 `clear` 清除所有标签。\n\n发送 /cancel 取消操作。",
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

    let (is_sensitive, settings_message_id) = match state {
        Some(SettingsState::WaitingForSensitiveTags {
            settings_message_id,
        }) => (true, settings_message_id),
        Some(SettingsState::WaitingForExcludedTags {
            settings_message_id,
        }) => (false, settings_message_id),
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

                info!(
                    "Chat {} cleared {} by user {:?}",
                    chat_id,
                    tag_type,
                    msg.from.as_ref().map(|u| u.id)
                );
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

                info!(
                    "Chat {} updated {} by user {:?}",
                    chat_id,
                    tag_type,
                    msg.from.as_ref().map(|u| u.id)
                );
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
