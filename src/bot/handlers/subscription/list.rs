use super::{ListPaginationAction, PAGE_SIZE};
use crate::bot::notifier::ThrottledBot;
use crate::bot::BotHandler;
use crate::db::types::TaskType;
use crate::pixiv::model::RankingMode;
use crate::utils::args;
use teloxide::prelude::*;
use teloxide::types::{ChatId, InlineKeyboardButton, InlineKeyboardMarkup, ParseMode, UserId};
use teloxide::utils::markdown;
use tracing::error;

/// Callback data prefix for list pagination
pub const LIST_CALLBACK_PREFIX: &str = "list:";

impl BotHandler {
    /// 列出当前聊天的所有订阅 (从命令调用，默认第一页)
    pub async fn handle_list(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        let parsed = args::parse_args(&args_str);

        let (target_chat_id, is_channel) = match self
            .resolve_subscription_target(&bot, chat_id, user_id, &parsed)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                bot.send_message(chat_id, format!("❌ {}", e)).await?;
                return Ok(());
            }
        };

        self.send_subscription_list(bot, chat_id, target_chat_id, 0, None, is_channel)
            .await
    }

    /// 发送订阅列表（支持分页）
    pub async fn send_subscription_list(
        &self,
        bot: ThrottledBot,
        reply_chat_id: ChatId,
        target_chat_id: ChatId,
        page: usize,
        message_id: Option<teloxide::types::MessageId>,
        is_channel: bool,
    ) -> ResponseResult<()> {
        match self.repo.list_subscriptions_by_chat(target_chat_id.0).await {
            Ok(subscriptions) => {
                if subscriptions.is_empty() {
                    let msg = if is_channel {
                        format!(
                            "📭 频道 `{}` 没有生效的订阅。\n\n使用 `/sub ch={}` 开始订阅！",
                            target_chat_id.0, target_chat_id.0
                        )
                    } else {
                        "📭 您没有生效的订阅。\n\n使用 `/sub` 开始订阅！".to_string()
                    };
                    if let Some(mid) = message_id {
                        bot.edit_message_text(reply_chat_id, mid, msg)
                            .parse_mode(ParseMode::MarkdownV2)
                            .await?;
                    } else {
                        bot.send_message(reply_chat_id, msg)
                            .parse_mode(ParseMode::MarkdownV2)
                            .await?;
                    }
                    return Ok(());
                }

                let (authors, rankings): (Vec<_>, Vec<_>) = subscriptions
                    .into_iter()
                    .partition(|(_, task)| task.r#type == TaskType::Author);

                let all_subscriptions: Vec<_> =
                    rankings.into_iter().chain(authors.into_iter()).collect();

                let total = all_subscriptions.len();
                let total_pages = total.div_ceil(PAGE_SIZE);
                let page = page.min(total_pages.saturating_sub(1));

                let start = page * PAGE_SIZE;
                let end = (start + PAGE_SIZE).min(total);
                let page_subscriptions = &all_subscriptions[start..end];

                let header = if is_channel {
                    if total_pages > 1 {
                        format!(
                            "📋 *频道* `{}` *的订阅* \\(第 {}/{} 页，共 {} 条\\):\n\n",
                            target_chat_id.0,
                            page + 1,
                            total_pages,
                            total
                        )
                    } else {
                        format!(
                            "📋 *频道* `{}` *的订阅* \\(共 {} 条\\):\n\n",
                            target_chat_id.0, total
                        )
                    }
                } else if total_pages > 1 {
                    format!(
                        "📋 *您的订阅* \\(第 {}/{} 页，共 {} 条\\):\n\n",
                        page + 1,
                        total_pages,
                        total
                    )
                } else {
                    format!("📋 *您的订阅* \\(共 {} 条\\):\n\n", total)
                };
                let mut message = header;

                for (sub, task) in page_subscriptions {
                    let type_emoji = match task.r#type {
                        TaskType::Author => "🎨",
                        TaskType::Ranking => "📊",
                    };

                    let display_info = if task.r#type == TaskType::Author {
                        if let Some(ref name) = task.author_name {
                            format!("{} \\| ID: `{}`", markdown::escape(name), task.value)
                        } else {
                            format!("ID: `{}`", task.value)
                        }
                    } else if task.r#type == TaskType::Ranking {
                        match RankingMode::from_str(&task.value) {
                            Some(mode) => {
                                format!(
                                    "排行榜 \\({}\\) \\| MODE: `{}`",
                                    mode.display_name(),
                                    mode.as_str()
                                )
                            }
                            None => {
                                format!(
                                    "排行榜 \\({}\\) \\| MODE: `{}`",
                                    task.value.replace('_', "\\_"),
                                    task.value
                                )
                            }
                        }
                    } else {
                        task.value.replace('_', "\\_")
                    };

                    let filter_info = if !sub.filter_tags.is_empty() {
                        format!("\n  🏷 {}", sub.filter_tags.format_for_display())
                    } else {
                        String::new()
                    };

                    message.push_str(&format!("{} {}{}\n", type_emoji, display_info, filter_info));
                }

                if is_channel {
                    message.push_str(&format!(
                        "\n💡 使用 `/unsub ch={} <id>` 或 `/unsubrank ch={} <mode>` 取消订阅",
                        target_chat_id.0, target_chat_id.0
                    ));
                } else {
                    message.push_str("\n💡 使用 `/unsub <id>` 或 `/unsubrank <mode>` 取消订阅");
                }

                let keyboard = if total_pages > 1 {
                    Some(build_pagination_keyboard(
                        page,
                        total_pages,
                        target_chat_id,
                        is_channel,
                    ))
                } else {
                    None
                };

                if let Some(mid) = message_id {
                    let mut req = bot.edit_message_text(reply_chat_id, mid, &message);
                    req = req.parse_mode(ParseMode::MarkdownV2);
                    if let Some(kb) = keyboard {
                        req = req.reply_markup(kb);
                    }
                    req.await?;
                } else {
                    let mut req = bot.send_message(reply_chat_id, &message);
                    req = req.parse_mode(ParseMode::MarkdownV2);
                    if let Some(kb) = keyboard {
                        req = req.reply_markup(kb);
                    }
                    req.await?;
                }
            }
            Err(e) => {
                error!("Failed to list subscriptions: {:#}", e);
                let msg = "❌ 获取订阅列表失败";
                if let Some(mid) = message_id {
                    bot.edit_message_text(reply_chat_id, mid, msg).await?;
                } else {
                    bot.send_message(reply_chat_id, msg).await?;
                }
            }
        }

        Ok(())
    }
}

fn build_list_callback_data(page: usize, target_chat_id: ChatId, is_channel: bool) -> String {
    format!(
        "{}{page}:{}:{}",
        LIST_CALLBACK_PREFIX,
        target_chat_id.0,
        if is_channel { 1 } else { 0 }
    )
}

pub fn parse_list_callback_data(callback_data: &str) -> Option<ListPaginationAction> {
    let payload = callback_data.strip_prefix(LIST_CALLBACK_PREFIX)?;

    if payload == "noop" {
        return Some(ListPaginationAction::Noop);
    }

    let parts: Vec<_> = payload.split(':').collect();
    let page = parts.first()?.parse().ok()?;

    match parts.as_slice() {
        [_page] => Some(ListPaginationAction::Page {
            page,
            target_chat_id: None,
            is_channel: false,
        }),
        [_page, target_chat_id, is_channel] => Some(ListPaginationAction::Page {
            page,
            target_chat_id: Some(ChatId(target_chat_id.parse().ok()?)),
            is_channel: match *is_channel {
                "0" => false,
                "1" => true,
                _ => return None,
            },
        }),
        _ => None,
    }
}

fn build_pagination_keyboard(
    current_page: usize,
    total_pages: usize,
    target_chat_id: ChatId,
    is_channel: bool,
) -> InlineKeyboardMarkup {
    let mut buttons = Vec::new();

    if current_page > 0 {
        buttons.push(InlineKeyboardButton::callback(
            "⬅️ 上一页",
            build_list_callback_data(current_page - 1, target_chat_id, is_channel),
        ));
    }

    buttons.push(InlineKeyboardButton::callback(
        format!("{}/{}", current_page + 1, total_pages),
        format!("{}noop", LIST_CALLBACK_PREFIX),
    ));

    if current_page + 1 < total_pages {
        buttons.push(InlineKeyboardButton::callback(
            "下一页 ➡️",
            build_list_callback_data(current_page + 1, target_chat_id, is_channel),
        ));
    }

    InlineKeyboardMarkup::new(vec![buttons])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_list_callback_data_legacy_format() {
        assert_eq!(
            parse_list_callback_data("list:3"),
            Some(ListPaginationAction::Page {
                page: 3,
                target_chat_id: None,
                is_channel: false,
            })
        );
    }

    #[test]
    fn test_parse_list_callback_data_channel_format() {
        assert_eq!(
            parse_list_callback_data("list:2:-1001234567890:1"),
            Some(ListPaginationAction::Page {
                page: 2,
                target_chat_id: Some(ChatId(-1001234567890)),
                is_channel: true,
            })
        );
    }

    #[test]
    fn test_parse_list_callback_data_noop() {
        assert_eq!(
            parse_list_callback_data("list:noop"),
            Some(ListPaginationAction::Noop)
        );
    }

    #[test]
    fn test_build_list_callback_data_encodes_context() {
        assert_eq!(
            build_list_callback_data(4, ChatId(-1001234567890), true),
            "list:4:-1001234567890:1"
        );
    }
}
