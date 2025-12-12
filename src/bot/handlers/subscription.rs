use crate::bot::BotHandler;
use crate::db::types::{TagFilter, TaskType};
use crate::pixiv::model::RankingMode;
use anyhow::{Context, Result};
use teloxide::prelude::*;
use teloxide::types::{ChatAction, ParseMode};
use teloxide::utils::markdown;
use tracing::{error, info, warn};

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
    // Subscribe to Author
    // ------------------------------------------------------------------------

    /// 订阅 Pixiv 作者
    ///
    /// 用法: `/sub <id,...> [+tag1 -tag2]`
    pub async fn handle_sub_author(
        &self,
        bot: Bot,
        chat_id: ChatId,
        args: String,
    ) -> ResponseResult<()> {
        // Set bot status to typing
        if let Err(e) = bot.send_chat_action(chat_id, ChatAction::Typing).await {
            warn!("Failed to set chat action for chat {}: {:#}", chat_id, e);
        }

        let parts: Vec<&str> = args.split_whitespace().collect();

        if parts.is_empty() {
            bot.send_message(chat_id, "❌ 用法: `/sub <id,...> [+tag1 -tag2]`")
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
                    chat_id.0,
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
        let filter_suffix = if filter_tags.is_empty() {
            None
        } else {
            Some(format!("\n🏷 {}", filter_tags.format_for_display()))
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
    /// 用法: `/subrank <mode> [+tag1 -tag2]`
    pub async fn handle_sub_ranking(
        &self,
        bot: Bot,
        chat_id: ChatId,
        args: String,
    ) -> ResponseResult<()> {
        // Set bot status to typing
        if let Err(e) = bot.send_chat_action(chat_id, ChatAction::Typing).await {
            warn!("Failed to set chat action for chat {}: {:#}", chat_id, e);
        }

        let parts: Vec<&str> = args.split_whitespace().collect();

        if parts.is_empty() {
            let available_modes = RankingMode::all_modes().join(", ");
            bot.send_message(
                chat_id,
                format!(
                    "❌ 用法: `/subrank <mode> [+tag1 -tag2]`\n可用模式: {}",
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
                chat_id.0,
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
    /// 用法: `/unsub <author_id,...>`
    pub async fn handle_unsub_author(
        &self,
        bot: Bot,
        chat_id: ChatId,
        args: String,
    ) -> ResponseResult<()> {
        let ids_str = args.trim();

        if ids_str.is_empty() {
            bot.send_message(chat_id, "❌ 用法: `/unsub <author_id,...>`")
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
                .delete_subscription(chat_id.0, TaskType::Author, author_id)
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

        let response = result.build_response("✅ 成功取消订阅:", "❌ 取消订阅失败:");
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
    /// 用法: `/unsubrank <mode>`
    pub async fn handle_unsub_ranking(
        &self,
        bot: Bot,
        chat_id: ChatId,
        args: String,
    ) -> ResponseResult<()> {
        let mode_str = args.trim();

        if mode_str.is_empty() {
            bot.send_message(chat_id, "❌ 用法: `/unsubrank <mode>`")
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
            .delete_subscription(chat_id.0, TaskType::Ranking, mode.as_str())
            .await
        {
            Ok(_) => {
                bot.send_message(chat_id, format!("✅ 成功取消订阅 {}", mode.display_name()))
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
    // List Subscriptions
    // ------------------------------------------------------------------------

    /// 列出当前聊天的所有订阅
    pub async fn handle_list(&self, bot: Bot, chat_id: ChatId) -> ResponseResult<()> {
        match self.repo.list_subscriptions_by_chat(chat_id.0).await {
            Ok(subscriptions) => {
                if subscriptions.is_empty() {
                    bot.send_message(chat_id, "📭 您没有生效的订阅。\n\n使用 `/sub` 开始订阅！")
                        .parse_mode(ParseMode::MarkdownV2)
                        .await?;
                    return Ok(());
                }

                // Separate authors and rankings
                let (authors, rankings): (Vec<_>, Vec<_>) = subscriptions
                    .into_iter()
                    .partition(|(_, task)| task.r#type == TaskType::Author);

                let mut message = "📋 *您的订阅:*\n\n".to_string();

                // First show authors
                for (sub, task) in authors.iter().chain(rankings.iter()) {
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

                message.push_str("\n💡 使用 `/unsub <id>` 或 `/unsubrank <mode>` 取消订阅");

                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!("Failed to list subscriptions: {:#}", e);
                bot.send_message(chat_id, "❌ 获取订阅列表失败").await?;
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
