use crate::bot::notifier::ThrottledBot;
use crate::bot::BotHandler;
use crate::db::types::Tags;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{
    CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, MessageId, ParseMode, UserId,
};
use teloxide::utils::markdown;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

pub const SETTINGS_CALLBACK_PREFIX: &str = "settings:";
const SETTINGS_BLUR_TOGGLE: &str = "settings:blur:toggle";
const SETTINGS_EDIT_SENSITIVE: &str = "settings:edit:sensitive";
const SETTINGS_EDIT_EXCLUDE: &str = "settings:edit:exclude";

#[derive(Clone, Debug, Default)]
pub enum SettingsState {
    #[default]
    Idle,
    WaitingForSensitiveTags,
    WaitingForExcludedTags,
}

#[derive(Debug)]
pub struct InMemStorage<K, V> {
    map: Mutex<HashMap<K, V>>,
}

impl<K, V> InMemStorage<K, V>
where
    K: Eq + Hash,
{
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            map: Mutex::new(HashMap::new()),
        })
    }

    pub async fn get(&self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        self.map.lock().await.get(key).cloned()
    }

    pub async fn insert(&self, key: K, value: V) {
        self.map.lock().await.insert(key, value);
    }
}

pub type SettingsStorage = Arc<InMemStorage<(ChatId, UserId), SettingsState>>;

impl BotHandler {
    // ------------------------------------------------------------------------
    // Chat Settings Commands
    // ------------------------------------------------------------------------

    /// 显示聊天设置
    pub async fn handle_settings(&self, bot: ThrottledBot, chat_id: ChatId) -> ResponseResult<()> {
        self.send_settings_panel(bot, chat_id, None).await
    }

    pub async fn handle_settings_callback(
        &self,
        bot: ThrottledBot,
        q: CallbackQuery,
        callback_data: String,
        storage: SettingsStorage,
    ) -> ResponseResult<()> {
        let user_id = q.from.id;
        let is_admin = match self.repo.get_user(user_id.0 as i64).await {
            Ok(Some(user)) => user.role.is_admin(),
            Ok(None) => false,
            Err(e) => {
                error!("Failed to get user {}: {:#}", user_id, e);
                false
            }
        };

        if !is_admin {
            if let Err(e) = bot
                .answer_callback_query(q.id)
                .text("Only admins can change settings")
                .show_alert(true)
                .await
            {
                warn!("Failed to answer callback query: {:#}", e);
            }
            return Ok(());
        }

        if let Err(e) = bot.answer_callback_query(q.id.clone()).await {
            warn!("Failed to answer callback query: {:#}", e);
        }

        let Some(message) = q.message.as_ref() else {
            warn!("Settings callback missing message");
            return Ok(());
        };

        let chat_id = message.chat().id;
        let message_id = message.id();

        match callback_data.as_str() {
            SETTINGS_BLUR_TOGGLE => {
                let current = match self.repo.get_chat(chat_id.0).await {
                    Ok(Some(chat)) => chat.blur_sensitive_tags,
                    Ok(None) => {
                        bot.send_message(chat_id, "❌ 未找到聊天").await?;
                        return Ok(());
                    }
                    Err(e) => {
                        error!("Failed to get chat settings: {:#}", e);
                        bot.send_message(chat_id, "❌ 获取设置失败").await?;
                        return Ok(());
                    }
                };

                let new_value = !current;
                match self
                    .repo
                    .set_blur_sensitive_tags(chat_id.0, new_value)
                    .await
                {
                    Ok(_) => {
                        info!("Chat {} set blur_sensitive_tags to {}", chat_id, new_value);
                        self.send_settings_panel(bot, chat_id, Some(message_id))
                            .await?;
                    }
                    Err(e) => {
                        error!("Failed to set blur_sensitive_tags: {:#}", e);
                        bot.send_message(chat_id, "❌ 更新设置失败").await?;
                    }
                }
            }
            SETTINGS_EDIT_SENSITIVE => {
                storage
                    .insert((chat_id, user_id), SettingsState::WaitingForSensitiveTags)
                    .await;
                let mention =
                    markdown::user_mention(user_id, &markdown::escape(&q.from.full_name()));
                let clear_hint = markdown::code_inline("clear");
                let cancel_hint = markdown::code_inline("/cancel");
                let message = format!(
                    "{} 请回复敏感标签，用逗号分隔，或发送 {} 清除。发送 {} 取消。",
                    mention, clear_hint, cancel_hint
                );
                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            SETTINGS_EDIT_EXCLUDE => {
                storage
                    .insert((chat_id, user_id), SettingsState::WaitingForExcludedTags)
                    .await;
                let mention =
                    markdown::user_mention(user_id, &markdown::escape(&q.from.full_name()));
                let clear_hint = markdown::code_inline("clear");
                let cancel_hint = markdown::code_inline("/cancel");
                let message = format!(
                    "{} 请回复排除标签，用逗号分隔，或发送 {} 清除。发送 {} 取消。",
                    mention, clear_hint, cancel_hint
                );
                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            _ => {}
        }

        Ok(())
    }

    pub async fn handle_settings_input(
        &self,
        bot: ThrottledBot,
        msg: Message,
        state: SettingsState,
        storage: SettingsStorage,
    ) -> ResponseResult<()> {
        let Some(user) = msg.from.as_ref() else {
            return Ok(());
        };
        let chat_id = msg.chat.id;
        let user_id = user.id;
        let text = match msg.text() {
            Some(text) => text.trim(),
            None => return Ok(()),
        };

        if text.eq_ignore_ascii_case("/cancel") {
            storage
                .insert((chat_id, user_id), SettingsState::Idle)
                .await;
            bot.send_message(chat_id, "✅ 已取消").await?;
            return Ok(());
        }

        let normalized = text.to_lowercase();
        let (tags, is_clear) = if normalized == "clear" {
            (Tags::default(), true)
        } else {
            let tag_list: Vec<String> = text
                .split([',', '，'])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if tag_list.is_empty() {
                bot.send_message(chat_id, "❌ 未提供有效的标签").await?;
                storage
                    .insert((chat_id, user_id), SettingsState::Idle)
                    .await;
                return Ok(());
            }
            (Tags::from(tag_list), false)
        };

        let result = match state {
            SettingsState::WaitingForSensitiveTags => {
                self.repo.set_sensitive_tags(chat_id.0, tags).await
            }
            SettingsState::WaitingForExcludedTags => {
                self.repo.set_excluded_tags(chat_id.0, tags).await
            }
            SettingsState::Idle => return Ok(()),
        };

        match result {
            Ok(_) => {
                if is_clear {
                    info!("Chat {} cleared settings tags", chat_id);
                } else {
                    info!("Chat {} updated settings tags", chat_id);
                }
                bot.send_message(chat_id, "✅ 设置已更新").await?;
            }
            Err(e) => {
                error!("Failed to update settings tags: {:#}", e);
                bot.send_message(chat_id, "❌ 更新设置失败").await?;
            }
        }

        storage
            .insert((chat_id, user_id), SettingsState::Idle)
            .await;
        Ok(())
    }

    async fn send_settings_panel(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        message_id: Option<MessageId>,
    ) -> ResponseResult<()> {
        match self.repo.get_chat(chat_id.0).await {
            Ok(Some(chat)) => {
                let (message, keyboard) = settings_panel(&chat);
                if let Some(message_id) = message_id {
                    if let Err(e) = bot
                        .edit_message_text(chat_id, message_id, message)
                        .parse_mode(ParseMode::MarkdownV2)
                        .reply_markup(keyboard)
                        .await
                    {
                        warn!("Failed to edit settings panel: {:#}", e);
                        bot.send_message(chat_id, "❌ 更新设置失败").await?;
                    }
                } else {
                    bot.send_message(chat_id, message)
                        .parse_mode(ParseMode::MarkdownV2)
                        .reply_markup(keyboard)
                        .await?;
                }
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

fn settings_panel(chat: &crate::db::entities::chats::Model) -> (String, InlineKeyboardMarkup) {
    let blur_status = if chat.blur_sensitive_tags {
        "已启用"
    } else {
        "已禁用"
    };
    let sensitive_status = if chat.sensitive_tags.is_empty() {
        "无"
    } else {
        "有标签"
    };
    let excluded_status = if chat.excluded_tags.is_empty() {
        "无"
    } else {
        "有标签"
    };

    let message = format!(
        "⚙️ *聊天设置*\n\n🔒 敏感内容模糊: {}\n🏷 敏感标签: {}\n🚫 排除标签: {}",
        blur_status, sensitive_status, excluded_status
    );

    let blur_button = if chat.blur_sensitive_tags {
        "🔓 关闭"
    } else {
        "🔒 开启"
    };

    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            blur_button,
            SETTINGS_BLUR_TOGGLE,
        )],
        vec![
            InlineKeyboardButton::callback("✏️ 编辑敏感标签", SETTINGS_EDIT_SENSITIVE),
            InlineKeyboardButton::callback("✏️ 编辑排除标签", SETTINGS_EDIT_EXCLUDE),
        ],
    ]);

    (message, keyboard)
}
