use crate::bot::notifier::ThrottledBot;
use crate::bot::BotHandler;
use crate::db::types::{TagFilter, TaskType};
use crate::pixiv::model::RankingMode;
use crate::utils::args;
use crate::utils::channel::{self, BotChannelExt};
use anyhow::{Context, Result};
use teloxide::prelude::*;
use teloxide::types::{
    ChatAction, ChatId, InlineKeyboardButton, InlineKeyboardMarkup, ParseMode, UserId,
};
use teloxide::utils::markdown;
use tracing::{error, info, warn};

/// Maximum number of subscriptions per page
pub const PAGE_SIZE: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListPaginationAction {
    Noop,
    Page {
        page: usize,
        target_chat_id: Option<ChatId>,
        is_channel: bool,
    },
}

// ============================================================================
// Helper Types
// ============================================================================

/// 批量操作结果收集器
struct BatchResult {
    success: Vec<String>,
    failed: Vec<String>,
}

impl BatchResult {
    fn new() -> Self {
        Self {
            success: Vec::new(),
            failed: Vec::new(),
        }
    }

    fn add_success(&mut self, item: String) {
        self.success.push(item);
    }

    fn add_failure(&mut self, item: String) {
        self.failed.push(item);
    }

    /// 构建成功/失败列表的响应消息
    fn build_response(&self, success_prefix: &str, failure_prefix: &str) -> String {
        self.build_response_with_suffix(success_prefix, failure_prefix, None)
    }

    /// 构建成功/失败列表的响应消息，在成功列表后添加可选后缀
    fn build_response_with_suffix(
        &self,
        success_prefix: &str,
        failure_prefix: &str,
        success_suffix: Option<&str>,
    ) -> String {
        let mut response = String::new();

        if !self.success.is_empty() {
            response.push_str(success_prefix);
            response.push('\n');
            for item in &self.success {
                response.push_str(&format!("  • {}\n", item));
            }
            // Add suffix after success list if provided
            if let Some(suffix) = success_suffix {
                response.push_str(suffix);
            }
        }

        if !self.failed.is_empty() {
            if !response.is_empty() {
                response.push('\n');
            }
            response.push_str(failure_prefix);
            response.push('\n');
            for item in &self.failed {
                response.push_str(&format!("  • {}\n", item));
            }
        }

        response
    }
}

// ============================================================================
// Subscription Commands
// ============================================================================

impl BotHandler {
    // ------------------------------------------------------------------------
    // Channel Target Resolution Helper
    // ------------------------------------------------------------------------

    /// Resolve the target chat ID for a subscription operation.
    ///
    /// If a channel parameter is specified, validates permissions and returns the channel ID.
    /// Otherwise, returns the current chat ID.
    ///
    /// Returns:
    /// - Ok((target_chat_id, is_channel)) if successful
    /// - Err with error message if channel validation fails
    async fn resolve_subscription_target(
        &self,
        bot: &ThrottledBot,
        current_chat_id: ChatId,
        user_id: Option<UserId>,
        parsed_args: &args::ParsedArgs,
    ) -> Result<(ChatId, bool), String> {
        // Check for channel parameter (channel= or ch=)
        let channel_param = parsed_args.get_any(&["channel", "ch"]);

        match channel_param {
            Some(channel_str) if !channel_str.is_empty() => {
                // Parse channel identifier (can be numeric ID or @username)
                let channel_identifier: channel::ChannelIdentifier =
                    channel_str.parse().map_err(|e| {
                        warn!(
                            "Failed to parse channel identifier '{}': {}",
                            channel_str, e
                        );
                        e
                    })?;

                // Validate user_id is available
                let user_id = user_id.ok_or_else(|| {
                    warn!("User ID not available for channel subscription");
                    "无法获取用户信息".to_string()
                })?;

                // Validate channel permissions and get resolved numeric ID
                let channel_id = bot
                    .validate_channel_permissions(&channel_identifier, user_id)
                    .await?;

                // Ensure chat exists in database for the channel
                if let Err(e) = self
                    .repo
                    .upsert_chat(
                        channel_id.0,
                        "channel".to_string(),
                        None,
                        true, // Enable by default
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

    // ------------------------------------------------------------------------
    // Subscribe to Author
    // ------------------------------------------------------------------------

    /// 订阅 Pixiv 作者
    ///
    /// 用法: `/sub [channel=<id>] <id,...> [+tag1 -tag2]`
    pub async fn handle_sub_author(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        // Set bot status to typing
        if let Err(e) = bot.send_chat_action(chat_id, ChatAction::Typing).await {
            warn!("Failed to set chat action for chat {}: {:#}", chat_id, e);
        }

        // Parse key-value parameters from the beginning
        let parsed = args::parse_args(&args_str);

        // Resolve target chat (channel or current)
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

        // Parse comma-separated IDs
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

        // Parse filter tags using helper
        let filter_tags = TagFilter::parse_from_args(&parts[1..]);

        let mut result = BatchResult::new();

        for author_id_str in author_ids {
            // Validate ID format
            let author_id = match author_id_str.parse::<u64>() {
                Ok(id) => id,
                Err(_) => {
                    result.add_failure(format!("`{}` \\(无效 ID\\)", author_id_str));
                    continue;
                }
            };

            // Verify author exists and get author name
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

            // Create or get task and subscription
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

        // Build filter tags suffix if any
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

        // Build response message with filter suffix
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

    // ------------------------------------------------------------------------
    // Subscribe to Ranking
    // ------------------------------------------------------------------------

    /// 订阅 Pixiv 排行榜
    ///
    /// 用法: `/subrank [channel=<id>] <mode> [+tag1 -tag2]`
    pub async fn handle_sub_ranking(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        // Set bot status to typing
        if let Err(e) = bot.send_chat_action(chat_id, ChatAction::Typing).await {
            warn!("Failed to set chat action for chat {}: {:#}", chat_id, e);
        }

        // Parse key-value parameters from the beginning
        let parsed = args::parse_args(&args_str);

        // Resolve target chat (channel or current)
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
            let available_modes = RankingMode::all_modes().join(", ");
            bot.send_message(
                chat_id,
                format!(
                    "❌ 用法: `/subrank [channel=<id>] <mode> [+tag1 -tag2]`\n可用模式: {}",
                    markdown::escape(&available_modes)
                ),
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
            return Ok(());
        }

        // Parse ranking mode
        let mode = match RankingMode::from_str(parts[0]) {
            Some(mode) => mode,
            None => {
                let available_modes = RankingMode::all_modes().join(", ");
                bot.send_message(
                    chat_id,
                    format!("❌ 无效的排行榜模式。可用模式: {}", available_modes),
                )
                .await?;
                return Ok(());
            }
        };

        // Parse filter tags using helper
        let filter_tags = TagFilter::parse_from_args(&parts[1..]);

        // Create subscription
        match self
            .create_subscription(
                target_chat_id.0,
                TaskType::Ranking,
                mode.as_str(),
                None,
                filter_tags.clone(),
            )
            .await
        {
            Ok(_) => {
                let mut message = format!("✅ 成功订阅 {}", mode.display_name());
                if !filter_tags.is_empty() {
                    message.push_str(&format!("\n\n🏷 {}", filter_tags.format_for_display()));
                }
                if is_channel {
                    message.push_str(&format!("\n📢 频道: `{}`", target_chat_id.0));
                }
                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!("Failed to subscribe to ranking {}: {:#}", mode.as_str(), e);
                bot.send_message(chat_id, "❌ 创建订阅失败").await?;
            }
        }

        Ok(())
    }

    // ------------------------------------------------------------------------
    // Unsubscribe from Author
    // ------------------------------------------------------------------------

    /// 取消订阅作者
    ///
    /// 用法: `/unsub [channel=<id>] <author_id,...>`
    pub async fn handle_unsub_author(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        // Parse key-value parameters from the beginning
        let parsed = args::parse_args(&args_str);

        // Resolve target chat (channel or current)
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
                    // Display author name if available, otherwise just show ID
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
        if is_channel && !result.success.is_empty() {
            response.push_str(&format!("\n📢 频道: `{}`", target_chat_id.0));
        }
        bot.send_message(chat_id, response)
            .parse_mode(ParseMode::MarkdownV2)
            .await?;

        Ok(())
    }

    // ------------------------------------------------------------------------
    // Unsubscribe from Ranking
    // ------------------------------------------------------------------------

    /// 取消订阅排行榜
    ///
    /// 用法: `/unsubrank [channel=<id>] <mode>`
    pub async fn handle_unsub_ranking(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        // Parse key-value parameters from the beginning
        let parsed = args::parse_args(&args_str);

        // Resolve target chat (channel or current)
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

        let mode_str = parsed.remaining.trim();

        if mode_str.is_empty() {
            bot.send_message(chat_id, "❌ 用法: `/unsubrank [channel=<id>] <mode>`")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        // Parse ranking mode
        let mode = match RankingMode::from_str(mode_str) {
            Some(mode) => mode,
            None => {
                let available_modes = RankingMode::all_modes().join(", ");
                bot.send_message(
                    chat_id,
                    format!("❌ 无效的排行榜模式。可用模式: {}", available_modes),
                )
                .await?;
                return Ok(());
            }
        };

        match self
            .delete_subscription(target_chat_id.0, TaskType::Ranking, mode.as_str())
            .await
        {
            Ok(_) => {
                let mut message = format!("✅ 成功取消订阅 {}", mode.display_name());
                if is_channel {
                    message.push_str(&format!("\n📢 频道: `{}`", target_chat_id.0));
                }
                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!(
                    "Failed to unsubscribe from ranking {}: {:#}",
                    mode.as_str(),
                    e
                );
                bot.send_message(chat_id, "❌ 取消订阅失败").await?;
            }
        }

        Ok(())
    }

    // ------------------------------------------------------------------------
    // Unsubscribe by Reply (unsub this)
    // ------------------------------------------------------------------------

    /// 通过回复消息取消订阅
    ///
    /// 用法: 回复 bot 发送的订阅推送消息，发送 `/unsubthis`
    pub async fn handle_unsub_this(
        &self,
        bot: ThrottledBot,
        msg: Message,
        chat_id: ChatId,
    ) -> ResponseResult<()> {
        // Check if this is a reply to a message
        let reply_to = match msg.reply_to_message() {
            Some(reply) => reply,
            None => {
                bot.send_message(chat_id, "❌ 请回复一条订阅推送消息来取消对应的订阅")
                    .await?;
                return Ok(());
            }
        };

        let reply_message_id = reply_to.id.0;

        // Look up the message in our database
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

        // Delete the subscription
        let subscription_id = subscription.id;
        let task_id = task.id;
        let task_type = task.r#type;
        let task_value = task.value.clone();

        if let Err(e) = self.repo.delete_subscription(subscription_id).await {
            error!("Failed to delete subscription {}: {:#}", subscription_id, e);
            bot.send_message(chat_id, "❌ 取消订阅失败").await?;
            return Ok(());
        }

        // Cleanup orphaned task
        self.cleanup_orphaned_task(task_id, task_type, &task_value)
            .await;

        // Build success message based on task type
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
        };

        bot.send_message(chat_id, format!("✅ 成功取消订阅 {}", display_name))
            .parse_mode(ParseMode::MarkdownV2)
            .await?;

        Ok(())
    }

    // ------------------------------------------------------------------------
    // List Subscriptions
    // ------------------------------------------------------------------------

    /// 列出当前聊天的所有订阅 (从命令调用，默认第一页)
    ///
    /// 用法: `/list [channel=<id>]`
    pub async fn handle_list(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        // Parse key-value parameters from the beginning
        let parsed = args::parse_args(&args_str);

        // Resolve target chat (channel or current)
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
    ///
    /// - `reply_chat_id`: 发送响应消息的聊天ID
    /// - `target_chat_id`: 查询订阅的目标聊天ID (可以是频道)
    /// - `page`: 页码 (从 0 开始)
    /// - `message_id`: 如果提供，则编辑该消息；否则发送新消息
    /// - `is_channel`: 是否是查询频道的订阅
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

                // Separate authors and rankings, then combine (rankings first)
                let (authors, rankings): (Vec<_>, Vec<_>) = subscriptions
                    .into_iter()
                    .partition(|(_, task)| task.r#type == TaskType::Author);

                let all_subscriptions: Vec<_> =
                    rankings.into_iter().chain(authors.into_iter()).collect();

                let total = all_subscriptions.len();
                let total_pages = total.div_ceil(PAGE_SIZE);

                // Clamp page to valid range
                let page = page.min(total_pages.saturating_sub(1));

                // Get subscriptions for current page
                let start = page * PAGE_SIZE;
                let end = (start + PAGE_SIZE).min(total);
                let page_subscriptions = &all_subscriptions[start..end];

                // Build message header with channel info if applicable
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

                    // 构建显示名称：对于 author 类型显示作者名字，对于 ranking 类型显示排行榜类型和模式
                    // 使用代码块格式使得ID可以复制
                    let display_info = if task.r#type == TaskType::Author {
                        if let Some(ref name) = task.author_name {
                            format!("{} \\| ID: `{}`", markdown::escape(name), task.value)
                        } else {
                            format!("ID: `{}`", task.value)
                        }
                    } else if task.r#type == TaskType::Ranking {
                        // 对于排行榜，显示友好的排行榜名称和模式
                        match RankingMode::from_str(&task.value) {
                            Some(mode) => {
                                format!(
                                    "排行榜 \\({}\\) \\| MODE: `{}`",
                                    mode.display_name(),
                                    mode.as_str()
                                )
                            }
                            None => {
                                // 如果无法解析，显示原始值
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

                    // Show filter tags for all subscription types (author and ranking)
                    let filter_info = if !sub.filter_tags.is_empty() {
                        format!("\n  🏷 {}", sub.filter_tags.format_for_display())
                    } else {
                        String::new()
                    };

                    message.push_str(&format!("{} {}{}\n", type_emoji, display_info, filter_info));
                }

                // Add tip with channel parameter if applicable
                if is_channel {
                    message.push_str(&format!(
                        "\n💡 使用 `/unsub ch={} <id>` 或 `/unsubrank ch={} <mode>` 取消订阅",
                        target_chat_id.0, target_chat_id.0
                    ));
                } else {
                    message.push_str("\n💡 使用 `/unsub <id>` 或 `/unsubrank <mode>` 取消订阅");
                }

                // Build pagination keyboard if needed
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

                // Send or edit message
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

    // ------------------------------------------------------------------------
    // Subscription Helper Methods
    // ------------------------------------------------------------------------

    /// Create or update a subscription for a chat
    pub(crate) async fn create_subscription(
        &self,
        chat_id: i64,
        task_type: TaskType,
        task_value: &str,
        author_name: Option<&str>,
        filter_tags: TagFilter,
    ) -> Result<()> {
        // Get or create the task
        let task = self
            .repo
            .get_or_create_task(
                task_type,
                task_value.to_string(),
                author_name.map(|s| s.to_string()),
            )
            .await
            .context("Failed to create task")?;

        // Create subscription
        self.repo
            .upsert_subscription(chat_id, task.id, filter_tags)
            .await
            .context("Failed to upsert subscription")?;

        Ok(())
    }

    /// Delete a subscription and cleanup orphaned tasks
    /// Returns the author_name if available (for display purposes)
    pub(crate) async fn delete_subscription(
        &self,
        chat_id: i64,
        task_type: TaskType,
        task_value: &str,
    ) -> Result<Option<String>> {
        // Find the task
        let task = self
            .repo
            .get_task_by_type_value(task_type, task_value)
            .await
            .context("Failed to query task")?
            .ok_or_else(|| anyhow::anyhow!("未找到"))?;

        // Store author_name before cleanup
        let author_name = task.author_name.clone();

        // Delete subscription
        self.repo
            .delete_subscription_by_chat_task(chat_id, task.id)
            .await
            .context("未订阅")?;

        // Cleanup orphaned task if no more subscriptions
        self.cleanup_orphaned_task(task.id, task_type, task_value)
            .await;

        Ok(author_name)
    }

    /// Cleanup task if it has no more subscriptions
    async fn cleanup_orphaned_task(&self, task_id: i32, task_type: TaskType, task_value: &str) {
        match self.repo.count_subscriptions_for_task(task_id).await {
            Ok(0) => {
                if let Err(e) = self.repo.delete_task(task_id).await {
                    error!("Failed to delete task {}: {:#}", task_id, e);
                } else {
                    info!(
                        "Deleted task {} ({} {}) - no more subscriptions",
                        task_id, task_type, task_value
                    );
                }
            }
            Err(e) => {
                error!(
                    "Failed to count subscriptions for task {}: {:#}",
                    task_id, e
                );
            }
            _ => {}
        }
    }
}

// ============================================================================
// Pagination Helper Functions
// ============================================================================

/// Callback data prefix for list pagination
pub const LIST_CALLBACK_PREFIX: &str = "list:";

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

/// Build inline keyboard for pagination
fn build_pagination_keyboard(
    current_page: usize,
    total_pages: usize,
    target_chat_id: ChatId,
    is_channel: bool,
) -> InlineKeyboardMarkup {
    let mut buttons = Vec::new();

    // Previous button
    if current_page > 0 {
        buttons.push(InlineKeyboardButton::callback(
            "⬅️ 上一页",
            build_list_callback_data(current_page - 1, target_chat_id, is_channel),
        ));
    }

    // Page indicator (not clickable, using a callback that does nothing)
    buttons.push(InlineKeyboardButton::callback(
        format!("{}/{}", current_page + 1, total_pages),
        format!("{}noop", LIST_CALLBACK_PREFIX),
    ));

    // Next button
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
