use crate::bot::notifier::ThrottledBot;
use crate::bot::state::{SettingsPhase, SettingsState, SettingsStorage};
use crate::bot::BotHandler;
use crate::db::entities::chats;
use crate::db::types::{TagEditMode, Tags};
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, MessageId, ParseMode};
use teloxide::utils::markdown;
use tokio::time::Instant;
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

                match bot
                    .edit_message_text(chat_id, message_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .reply_markup(keyboard)
                    .await
                {
                    Ok(_)
                    | Err(teloxide::RequestError::Api(teloxide::ApiError::MessageNotModified)) => {}
                    Err(error) => return Err(error),
                }
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

    // Each source budget leaves room for Markdown escaping (up to 2x),
    // the other list and panel labels within Telegram's 4096-character limit.
    let sensitive_tags = markdown::escape(&tag_reference(&chat.sensitive_tags, 700));
    let excluded_tags = markdown::escape(&tag_reference(&chat.excluded_tags, 700));

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
        // Currently allows commands without @; pressing will turn on @ requirement
        "📢开启@要求"
    } else {
        // Currently requires @; pressing will turn off the @ requirement (allow without @)
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

/// Bound display only, counting UTF-16 units conservatively for Telegram.
/// Prompts use plain text; the Markdown panel escapes this text afterwards.
fn tag_reference(tags: &Tags, budget: usize) -> String {
    if tags.is_empty() {
        return "暂无标签".to_string();
    }
    let text = tags.join(", ");
    if text.encode_utf16().count() <= budget {
        return text;
    }
    let suffix = "…（仅展示部分；实际操作使用完整列表）";
    let limit = budget.saturating_sub(suffix.encode_utf16().count());
    let mut units = 0;
    let mut preview: String = text
        .chars()
        .take_while(|ch| {
            units += ch.len_utf16();
            units <= limit
        })
        .collect();
    preview.push_str(suffix);
    preview
}

async fn current_tag_reference(
    handler: &BotHandler,
    chat_id: ChatId,
    is_sensitive: bool,
) -> anyhow::Result<String> {
    use anyhow::Context;
    let chat = handler
        .repo
        .get_chat(chat_id.0)
        .await?
        .context("Chat not found")?;
    let tags = if is_sensitive {
        chat.sensitive_tags
    } else {
        chat.excluded_tags
    };
    Ok(tag_reference(&tags, 3000))
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
/// - `settings:edit:sensitive` - Choose a sensitive-tag edit mode
/// - `settings:edit:exclude` - Choose an excluded-tag edit mode
pub async fn handle_settings_callback(
    bot: ThrottledBot,
    q: CallbackQuery,
    callback_data: String,
    handler: BotHandler,
    storage: SettingsStorage,
    update_id: u32,
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

    let action = callback_data
        .strip_prefix(SETTINGS_CALLBACK_PREFIX)
        .unwrap_or("");
    // Reserve before permission I/O. Cancellation/reopening can invalidate this
    // generation even when Telegram is slow; never hold the global lock over I/O.
    let opening_session = if !q.from.is_bot && matches!(action, "edit:sensitive" | "edit:exclude") {
        let session_id = rand::random();
        storage.write().await.insert(
            (chat_id, user_id),
            SettingsState {
                session_id,
                is_sensitive: action == "edit:sensitive",
                settings_message_id: message_id,
                selection_message_id: None,
                phase: SettingsPhase::Preparing,
                created_at: Instant::now(),
            },
        );
        Some(session_id)
    } else {
        None
    };

    if q.from.is_bot
        || !crate::bot::permissions::can_manage_chat(
            &bot,
            &handler.repo,
            q.message.as_ref().unwrap().chat(),
            user_id,
        )
        .await
    {
        if let Some(session_id) = opening_session {
            remove_session(&storage, (chat_id, user_id), session_id).await;
        }
        bot.answer_callback_query(q.id)
            .text("只有管理员可以修改设置")
            .show_alert(true)
            .await?;
        return Ok(());
    }

    match action {
        "blur:toggle" | "mention:toggle" => {
            let chat = match handler.repo.get_chat(chat_id.0).await {
                Ok(chat) => chat,
                Err(e) => {
                    error!("Failed to fetch chat settings: {:#}", e);
                    None
                }
            };
            let Some(chat) = chat else {
                bot.answer_callback_query(q.id)
                    .text("获取聊天信息失败")
                    .show_alert(true)
                    .await?;
                return Ok(());
            };
            let result = if action == "blur:toggle" {
                handler
                    .repo
                    .set_blur_sensitive_tags(chat_id.0, !chat.blur_sensitive_tags)
                    .await
            } else {
                handler
                    .repo
                    .set_allow_without_mention(chat_id.0, !chat.allow_without_mention)
                    .await
            };
            match result {
                Ok(_) => {
                    info!("Chat {} {} by user {}", chat_id, action, user_id);
                    handler
                        .refresh_settings_panel(bot.clone(), chat_id, message_id)
                        .await?;
                    bot.answer_callback_query(q.id).await?;
                }
                Err(e) => {
                    error!("Failed to toggle chat setting: {:#}", e);
                    bot.answer_callback_query(q.id)
                        .text("更新设置失败")
                        .show_alert(true)
                        .await?;
                }
            }
        }
        "edit:sensitive" | "edit:exclude" => {
            let session_id = opening_session.unwrap();
            let is_sensitive = action == "edit:sensitive";
            let tag_type = if is_sensitive {
                "敏感标签"
            } else {
                "排除标签"
            };
            let reference = match current_tag_reference(&handler, chat_id, is_sensitive).await {
                Ok(reference) => reference,
                Err(e) => {
                    error!("Failed to read current tags: {:#}", e);
                    remove_session(&storage, (chat_id, user_id), session_id).await;
                    bot.answer_callback_query(q.id)
                        .text("获取当前标签失败，请重新打开设置。")
                        .show_alert(true)
                        .await?;
                    return Ok(());
                }
            };
            let button = |label, operation| {
                InlineKeyboardButton::callback(
                    label,
                    format!("settings:mode:{session_id}:{operation}"),
                )
            };
            let keyboard = InlineKeyboardMarkup::new(vec![
                vec![button("追加", "append"), button("删减", "remove")],
                vec![button("覆盖", "replace"), button("清空", "clear")],
            ]);
            let result = bot.send_message(chat_id, format!(
                "{}，请在3分钟内选择{}的操作模式：追加 / 删减 / 覆盖 / 清空。发送 /cancel 取消。\n\n当前标签：{}",
                q.from.first_name, tag_type, reference,
            )).reply_markup(keyboard).await;
            match result {
                Ok(prompt) => {
                    if let Some(state) = storage
                        .write()
                        .await
                        .get_mut(&(chat_id, user_id))
                        .filter(|s| s.session_id == session_id)
                    {
                        state.selection_message_id = Some(prompt.id);
                        state.phase = SettingsPhase::ChoosingMode;
                        state.created_at = Instant::now();
                    }
                }
                Err(e) => {
                    remove_session(&storage, (chat_id, user_id), session_id).await;
                    return Err(e.into());
                }
            }
            bot.answer_callback_query(q.id).await?;
        }
        action
            if action.starts_with("mode:")
                || action.starts_with("cancel:")
                || action.starts_with("confirm:") =>
        {
            let parts: Vec<_> = action.split(':').collect();
            let token = parts.get(1).and_then(|value| value.parse::<u64>().ok());
            let operation = parts.get(2).copied().unwrap_or("");
            let command = parts[0];
            let mut states = storage.write().await;
            let key = (chat_id, user_id);
            let Some(state) = states.get_mut(&key).filter(|state| {
                token == Some(state.session_id)
                    && parts.len() == 3
                    && state.selection_message_id == Some(message_id)
                    && matches!(
                        (command, operation, &state.phase),
                        (
                            "mode",
                            "append" | "remove" | "replace" | "clear",
                            SettingsPhase::ChoosingMode
                        ) | ("cancel", "input", SettingsPhase::WaitingForTags { .. })
                            | ("cancel", "confirm", SettingsPhase::ConfirmingClear)
                            | ("confirm", "clear", SettingsPhase::ConfirmingClear)
                    )
            }) else {
                drop(states);
                bot.answer_callback_query(q.id)
                    .text("此按钮不属于你的当前会话，或模式已选择；请重新打开设置。")
                    .show_alert(true)
                    .await?;
                return Ok(());
            };
            if state.is_expired() {
                states.remove(&key);
                drop(states);
                bot.answer_callback_query(q.id)
                    .text("操作已超时，请重新打开设置。")
                    .show_alert(true)
                    .await?;
                return Ok(());
            }
            if command == "cancel" || command == "confirm" {
                let state = states.remove(&key).unwrap();
                drop(states);
                let text = if command == "cancel" {
                    "✅ 操作已取消"
                } else {
                    match handler
                        .repo
                        .edit_chat_tags(
                            chat_id.0,
                            state.is_sensitive,
                            TagEditMode::Replace,
                            Tags::default(),
                        )
                        .await
                    {
                        Ok(_) => "✅ 标签已清空",
                        Err(e) => {
                            error!("Failed to clear tags: {:#}", e);
                            "❌ 更新设置失败，请重新打开设置后重试"
                        }
                    }
                };
                bot.edit_message_text(chat_id, message_id, text)
                    .reply_markup(InlineKeyboardMarkup::default())
                    .await?;
                if command == "confirm" {
                    handler
                        .refresh_settings_panel(bot.clone(), chat_id, state.settings_message_id)
                        .await?;
                }
                bot.answer_callback_query(q.id).await?;
                return Ok(());
            }
            let session_id = state.session_id;
            let is_sensitive = state.is_sensitive;
            let tag_type = if state.is_sensitive {
                "敏感标签"
            } else {
                "排除标签"
            };
            state.phase = SettingsPhase::Preparing;
            drop(states);
            let reference = match current_tag_reference(&handler, chat_id, is_sensitive).await {
                Ok(reference) => reference,
                Err(e) => {
                    error!("Failed to refresh current tags: {:#}", e);
                    remove_session(&storage, key, session_id).await;
                    bot.answer_callback_query(q.id)
                        .text("获取当前标签失败，请重新打开设置。")
                        .show_alert(true)
                        .await?;
                    return Ok(());
                }
            };
            let (mode, description) = match operation {
                "append" => (
                    TagEditMode::Append,
                    "追加：保留现有标签，并添加不重复的标签",
                ),
                "remove" => (
                    TagEditMode::Remove,
                    "删减：仅删除指定标签，不存在的标签将忽略",
                ),
                _ => (TagEditMode::Replace, "覆盖：用输入完整替换现有列表"),
            };
            let clearing = operation == "clear";
            let (prompt, keyboard) = if clearing {
                (
                    format!(
                        "确认清空{tag_type}？请在3分钟内确认，或取消。\n\n当前标签：{reference}"
                    ),
                    InlineKeyboardMarkup::new(vec![vec![
                        InlineKeyboardButton::callback(
                            "确认清空",
                            format!("settings:confirm:{session_id}:clear"),
                        ),
                        InlineKeyboardButton::callback(
                            "取消",
                            format!("settings:cancel:{session_id}:confirm"),
                        ),
                    ]]),
                )
            } else {
                (format!("{}，{}（{}）。请在3分钟内发送标签，用英文或中文逗号分隔。空输入不会清空列表。\n\n当前标签：{}",
                    q.from.first_name, description, tag_type, reference),
                 InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback("取消", format!("settings:cancel:{session_id}:input"))]]))
            };
            let result = bot
                .edit_message_text(chat_id, message_id, prompt)
                .reply_markup(keyboard)
                .await;
            match result {
                Ok(_) => {
                    if let Some(state) = storage
                        .write()
                        .await
                        .get_mut(&key)
                        .filter(|s| s.session_id == session_id)
                    {
                        state.phase = if clearing {
                            SettingsPhase::ConfirmingClear
                        } else {
                            SettingsPhase::WaitingForTags {
                                mode,
                                after_update_id: update_id,
                            }
                        };
                        state.created_at = Instant::now();
                    }
                }
                Err(e) => {
                    remove_session(&storage, key, session_id).await;
                    return Err(e.into());
                }
            }
            bot.answer_callback_query(q.id).await?;
        }
        _ => {
            warn!("Unknown settings callback action: {}", action);
            bot.answer_callback_query(q.id).await?;
        }
    }

    Ok(())
}

async fn remove_session(storage: &SettingsStorage, key: (ChatId, UserId), session_id: u64) {
    let mut states = storage.write().await;
    if states
        .get(&key)
        .is_some_and(|state| state.session_id == session_id)
    {
        states.remove(&key);
    }
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
    update_id: u32,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    // Commands that did not match this bot's command routes are never tag data.
    if msg
        .text()
        .is_some_and(|text| text.trim_start().starts_with('/'))
    {
        return Ok(false);
    }
    let chat_id = msg.chat.id;
    let Some(user) = crate::bot::permissions::message_actor(&msg) else {
        return Ok(false);
    };
    let user_id = user.id;
    let key = (chat_id, user_id);
    if !storage
        .read()
        .await
        .get(&key)
        .is_some_and(|state| matches!(state.phase, SettingsPhase::WaitingForTags { .. }))
    {
        return Ok(false);
    }
    // Recheck privileges on submission, including revocation during the dialogue.
    let authorized =
        crate::bot::permissions::can_manage_chat(&bot, &handler.repo, &msg.chat, user_id).await;
    let mut states = storage.write().await;
    let Some(state) = states.get(&key) else {
        return Ok(false);
    };
    let SettingsPhase::WaitingForTags {
        mode,
        after_update_id,
    } = state.phase
    else {
        return Ok(false);
    };
    // A message sent before mode selection (or before a replacement session)
    // must not become input just because its handler completed later.
    if update_id <= after_update_id {
        return Ok(false);
    }
    if state.is_expired() || !authorized {
        let error = if state.is_expired() {
            "操作已超时，请重新打开设置。"
        } else {
            "只有管理员可以修改设置"
        };
        states.remove(&key);
        drop(states);
        bot.send_message(chat_id, error).await?;
        return Ok(true);
    }
    let text = msg.text().unwrap_or("");
    let tags = parse_tags_input(text);
    if tags.is_empty() {
        drop(states);
        bot.send_message(
            chat_id,
            "❌ 未提供有效的标签；可在剩余时间内重新输入。清空请取消后选择清空按钮。",
        )
        .await?;
        return Ok(true);
    }
    // Claim this input once. Later callbacks cannot change its operation mode;
    // completion must not delete a newer session opened during the DB request.
    let state = states.remove(&key).unwrap();
    drop(states);
    let input = Tags::from(tags);
    match handler
        .repo
        .edit_chat_tags(chat_id.0, state.is_sensitive, mode, input)
        .await
    {
        Ok(tags) => {
            let tag_type = if state.is_sensitive {
                "敏感标签"
            } else {
                "排除标签"
            };
            let list = tag_reference(&tags, 3000);
            bot.send_message(chat_id, format!("✅ {}已更新: {}", tag_type, list))
                .await?;
            handler
                .refresh_settings_panel(bot, chat_id, state.settings_message_id)
                .await?;
        }
        Err(e) => {
            error!("Failed to edit chat tags: {:#}", e);
            bot.send_message(chat_id, "❌ 更新设置失败，请重新打开设置后重试")
                .await?;
        }
    }
    Ok(true)
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
    let Some(user) = crate::bot::permissions::message_actor(&msg) else {
        return Ok(false);
    };
    let user_id = user.id;

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
