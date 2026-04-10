use super::BatchResult;
use crate::bot::notifier::ThrottledBot;
use crate::bot::BotHandler;
use crate::db::types::{TagFilter, TaskType};
use crate::pixiv::model::RankingMode;
use crate::utils::args;
use teloxide::prelude::*;
use teloxide::types::{ChatAction, ChatId, ParseMode, UserId};
use teloxide::utils::markdown;
use tracing::{error, warn};

impl BotHandler {
    /// 订阅 Pixiv 作者
    pub async fn handle_sub_author(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        if let Err(e) = bot.send_chat_action(chat_id, ChatAction::Typing).await {
            warn!("Failed to set chat action for chat {}: {:#}", chat_id, e);
        }

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

        let parts: Vec<&str> = parsed.remaining.split_whitespace().collect();

        if parts.is_empty() {
            bot.send_message(
                chat_id,
                "❌ 用法: `/sub [channel=<id>] <id,...> [+tag1 -tag2]`",
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
            return Ok(());
        }

        let author_ids: Vec<&str> = parts[0]
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        if author_ids.is_empty() {
            bot.send_message(chat_id, "❌ 请提供至少一个作者 ID")
                .await?;
            return Ok(());
        }

        let filter_tags = TagFilter::parse_from_args(&parts[1..]);

        let mut result = BatchResult::new();

        for author_id_str in author_ids {
            let author_id = match author_id_str.parse::<u64>() {
                Ok(id) => id,
                Err(_) => {
                    result.add_failure(format!("`{}` \\(无效 ID\\)", author_id_str));
                    continue;
                }
            };

            let author_name = {
                let pixiv = self.pixiv_client.read().await;
                match pixiv.get_user_detail(author_id).await {
                    Ok(user) => user.name,
                    Err(e) => {
                        error!("Failed to get user detail for {}: {:#}", author_id, e);
                        result.add_failure(format!("`{}` \\(未找到\\)", author_id));
                        continue;
                    }
                }
            };

            match self
                .create_subscription(
                    target_chat_id.0,
                    TaskType::Author,
                    author_id_str,
                    Some(&author_name),
                    filter_tags.clone(),
                )
                .await
            {
                Ok(_) => {
                    result.add_success(format!(
                        "*{}* \\(ID: `{}`\\)",
                        markdown::escape(&author_name),
                        author_id
                    ));
                }
                Err(e) => {
                    error!("Failed to subscribe to author {}: {:#}", author_id, e);
                    result.add_failure(format!("`{}` \\(订阅失败\\)", author_id));
                }
            }
        }

        let mut suffix_parts = Vec::new();
        if !filter_tags.is_empty() {
            suffix_parts.push(format!("🏷 {}", filter_tags.format_for_display()));
        }
        if is_channel {
            suffix_parts.push(format!("📢 频道: `{}`", target_chat_id.0));
        }
        let filter_suffix = if suffix_parts.is_empty() {
            None
        } else {
            Some(format!("\n{}", suffix_parts.join("\n")))
        };

        let response = result.build_response_with_suffix(
            "✅ 成功订阅:",
            "❌ 订阅失败:",
            filter_suffix.as_deref(),
        );

        bot.send_message(chat_id, response)
            .parse_mode(ParseMode::MarkdownV2)
            .await?;

        Ok(())
    }

    /// 取消订阅作者
    pub async fn handle_unsub_author(
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

        let ids_str = parsed.remaining.trim();

        if ids_str.is_empty() {
            bot.send_message(chat_id, "❌ 用法: `/unsub [channel=<id>] <author_id,...>`")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        let author_ids: Vec<&str> = ids_str
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        let mut result = BatchResult::new();

        for author_id in author_ids {
            match self
                .delete_subscription(target_chat_id.0, TaskType::Author, author_id)
                .await
            {
                Ok(author_name) => {
                    let display = if let Some(name) = author_name {
                        format!("*{}* \\(ID: `{}`\\)", markdown::escape(&name), author_id)
                    } else {
                        format!("`{}`", author_id)
                    };
                    result.add_success(display);
                }
                Err(e) => {
                    error!("Failed to unsubscribe from author {}: {:#}", author_id, e);
                    result.add_failure(format!("`{}` \\(未找到订阅\\)", author_id));
                }
            }
        }

        let mut response = result.build_response("✅ 成功取消订阅:", "❌ 取消订阅失败:");
        if is_channel && result.has_success() {
            response.push_str(&format!("\n📢 频道: `{}`", target_chat_id.0));
        }
        bot.send_message(chat_id, response)
            .parse_mode(ParseMode::MarkdownV2)
            .await?;

        Ok(())
    }

    /// 通过回复消息取消订阅
    pub async fn handle_unsub_this(
        &self,
        bot: ThrottledBot,
        msg: Message,
        chat_id: ChatId,
    ) -> ResponseResult<()> {
        let reply_to = match msg.reply_to_message() {
            Some(reply) => reply,
            None => {
                bot.send_message(chat_id, "❌ 请回复一条订阅推送消息来取消对应的订阅")
                    .await?;
                return Ok(());
            }
        };

        let reply_message_id = reply_to.id.0;

        let message_info = match self
            .repo
            .get_message_with_subscription(chat_id.0, reply_message_id)
            .await
        {
            Ok(Some((msg_record, Some((sub, task))))) => (msg_record, sub, task),
            Ok(Some((_, None))) => {
                warn!(
                    "Subscription not found for message {} in chat {}",
                    reply_message_id, chat_id
                );
                bot.send_message(chat_id, "❌ 该订阅已不存在").await?;
                return Ok(());
            }
            Ok(None) => {
                warn!(
                    "No message record found for message {} in chat {}",
                    reply_message_id, chat_id
                );
                bot.send_message(chat_id, "❌ 未找到该消息对应的订阅记录")
                    .await?;
                return Ok(());
            }
            Err(e) => {
                error!("Failed to get message: {:#}", e);
                bot.send_message(chat_id, "❌ 查询订阅记录失败").await?;
                return Ok(());
            }
        };

        let (_msg_record, subscription, task) = message_info;
        let task = match task {
            Some(t) => t,
            None => {
                warn!(
                    "Task not found for subscription {} in chat {}",
                    subscription.id, chat_id
                );
                bot.send_message(chat_id, "❌ 该订阅的任务已不存在").await?;
                return Ok(());
            }
        };

        let subscription_id = subscription.id;
        let task_id = task.id;
        let task_type = task.r#type;
        let task_value = task.value.clone();

        if let Err(e) = self.repo.delete_subscription(subscription_id).await {
            error!("Failed to delete subscription {}: {:#}", subscription_id, e);
            bot.send_message(chat_id, "❌ 取消订阅失败").await?;
            return Ok(());
        }

        self.cleanup_orphaned_task(task_id, task_type, &task_value)
            .await;

        let display_name = match task_type {
            TaskType::Author => {
                if let Some(ref name) = task.author_name {
                    format!(
                        "作者 *{}* \\(ID: `{}`\\)",
                        markdown::escape(name),
                        task_value
                    )
                } else {
                    format!("作者 `{}`", task_value)
                }
            }
            TaskType::Ranking => match RankingMode::from_str(&task_value) {
                Some(mode) => mode.display_name().to_string(),
                None => format!("排行榜 `{}`", markdown::escape(&task_value)),
            },
            TaskType::BooruTag => {
                format!("Booru标签 `{}`", markdown::escape(&task_value))
            }
            TaskType::BooruPool => {
                format!("Booru Pool `{}`", markdown::escape(&task_value))
            }
        };

        bot.send_message(chat_id, format!("✅ 成功取消订阅 {}", display_name))
            .parse_mode(ParseMode::MarkdownV2)
            .await?;

        Ok(())
    }
}
