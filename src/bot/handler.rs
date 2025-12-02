use crate::bot::link_handler::{is_bot_mentioned, parse_pixiv_links, PixivLink};
use crate::bot::notifier::Notifier;
use crate::bot::Command;
use crate::db::entities::role::UserRole;
use crate::db::repo::Repo;
use crate::pixiv::client::PixivClient;
use crate::pixiv::downloader::Downloader;
use crate::pixiv::model::RankingMode;
use crate::utils::markdown;
use serde_json::{json, Value};
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{Me, ParseMode};
use tracing::{error, info};

// ============================================================================
// Helper Types and Functions
// ============================================================================

/// 解析后的过滤标签
#[derive(Debug, Clone, Default)]
struct FilterTags {
    include: Vec<String>,
    exclude: Vec<String>,
}

impl FilterTags {
    /// 从命令参数中解析过滤标签
    /// 格式: +tag1 -tag2 tag3 (无前缀视为 include)
    fn parse_from_args(args: &[&str]) -> Self {
        let mut include = Vec::new();
        let mut exclude = Vec::new();

        for tag in args {
            if let Some(stripped) = tag.strip_prefix('+') {
                include.push(stripped.to_string());
            } else if let Some(stripped) = tag.strip_prefix('-') {
                exclude.push(stripped.to_string());
            } else {
                include.push(tag.to_string());
            }
        }

        Self { include, exclude }
    }

    /// 检查是否为空（没有任何过滤条件）
    fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    /// 转换为 JSON Value (用于数据库存储)
    fn to_json(&self) -> Option<Value> {
        if self.is_empty() {
            None
        } else {
            Some(json!({
                "include": self.include,
                "exclude": self.exclude,
            }))
        }
    }
}

/// 从 filter_tags JSON 中提取并格式化过滤器信息（用于 MarkdownV2）
fn format_filter_tags(tags: &Value) -> String {
    let include: Vec<&str> = tags
        .get("include")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let exclude: Vec<&str> = tags
        .get("exclude")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let mut parts = Vec::new();
    if !include.is_empty() {
        parts.push(format!(
            "\\+{}",
            include
                .iter()
                .map(|s| markdown::escape(s))
                .collect::<Vec<_>>()
                .join(" \\+")
        ));
    }
    if !exclude.is_empty() {
        parts.push(format!(
            "\\-{}",
            exclude
                .iter()
                .map(|s| markdown::escape(s))
                .collect::<Vec<_>>()
                .join(" \\-")
        ));
    }
    parts.join(" ")
}

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
// BotHandler - Core Handler Structure
// ============================================================================

#[derive(Clone)]
pub struct BotHandler {
    #[allow(dead_code)]
    bot: Bot,
    repo: Arc<Repo>,
    pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
    notifier: Notifier,
    sensitive_tags: Vec<String>,
    owner_id: Option<i64>,
    is_public_mode: bool,
}

impl BotHandler {
    // ------------------------------------------------------------------------
    // Constructor
    // ------------------------------------------------------------------------

    pub fn new(
        bot: Bot,
        repo: Arc<Repo>,
        pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
        downloader: Arc<Downloader>,
        sensitive_tags: Vec<String>,
        owner_id: Option<i64>,
        is_public_mode: bool,
    ) -> Self {
        let notifier = Notifier::new(bot.clone(), downloader);
        Self {
            bot,
            repo,
            pixiv_client,
            notifier,
            sensitive_tags,
            owner_id,
            is_public_mode,
        }
    }

    // ------------------------------------------------------------------------
    // Command Entry Point
    // ------------------------------------------------------------------------

    pub async fn handle_command(&self, bot: Bot, msg: Message, cmd: Command) -> ResponseResult<()> {
        let chat_id = msg.chat.id;
        let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);

        info!(
            "Received command from user {} in chat {}: {:?}",
            user_id, chat_id, cmd
        );

        // Ensure user and chat exist in database
        let (user_role, chat_enabled) = match self.ensure_user_and_chat(&msg).await {
            Ok(data) => data,
            Err(e) => {
                error!("Failed to ensure user/chat: {}", e);
                bot.send_message(chat_id, "⚠️ 数据库错误").await?;
                return Ok(());
            }
        };

        // Check if chat is enabled (private chat with admin/owner is always considered enabled)
        if !self.is_chat_accessible(chat_id, chat_enabled, &user_role) {
            info!(
                "Ignoring command from disabled chat {} (user: {}, role: {:?})",
                chat_id, user_id, user_role
            );
            return Ok(());
        }

        // Route command to appropriate handler
        self.dispatch_command(bot, chat_id, cmd, &user_role).await
    }

    /// Check if the chat is accessible for command processing
    fn is_chat_accessible(
        &self,
        chat_id: ChatId,
        chat_enabled: bool,
        user_role: &UserRole,
    ) -> bool {
        if chat_enabled {
            return true;
        }
        // Special case: private chat with admin/owner is always accessible
        chat_id.is_user() && user_role.is_admin()
    }

    /// Dispatch command to the appropriate handler
    async fn dispatch_command(
        &self,
        bot: Bot,
        chat_id: ChatId,
        cmd: Command,
        user_role: &UserRole,
    ) -> ResponseResult<()> {
        match cmd {
            // User commands (available to all users)
            Command::Help => self.handle_help(bot, chat_id).await,
            Command::Sub(args) => self.handle_sub_author(bot, chat_id, args).await,
            Command::SubRank(args) => self.handle_sub_ranking(bot, chat_id, args).await,
            Command::Unsub(args) => self.handle_unsub_author(bot, chat_id, args).await,
            Command::UnsubRank(args) => self.handle_unsub_ranking(bot, chat_id, args).await,
            Command::List => self.handle_list(bot, chat_id).await,
            Command::BlurSensitive(args) => self.handle_blur_sensitive(bot, chat_id, args).await,
            Command::ExcludeTags(args) => self.handle_exclude_tags(bot, chat_id, args).await,
            Command::ClearExcludedTags => self.handle_clear_excluded_tags(bot, chat_id).await,
            Command::Settings => self.handle_settings(bot, chat_id).await,

            // Admin commands (require admin or owner role)
            Command::EnableChat(args) if user_role.is_admin() => {
                self.handle_enable_chat(bot, chat_id, args, true).await
            }
            Command::DisableChat(args) if user_role.is_admin() => {
                self.handle_enable_chat(bot, chat_id, args, false).await
            }
            Command::Info if user_role.is_admin() && chat_id.is_user() => {
                self.handle_info(bot, chat_id).await
            }

            // Owner commands (require owner role)
            Command::SetAdmin(args) if user_role.is_owner() => {
                self.handle_set_admin(bot, chat_id, args, true).await
            }
            Command::UnsetAdmin(args) if user_role.is_owner() => {
                self.handle_set_admin(bot, chat_id, args, false).await
            }

            // Silently ignore unauthorized commands
            _ => Ok(()),
        }
    }

    // ------------------------------------------------------------------------
    // User/Chat Management
    // ------------------------------------------------------------------------

    async fn ensure_user_and_chat(&self, msg: &Message) -> Result<(UserRole, bool), String> {
        let chat_id = msg.chat.id.0;
        let chat_type = match msg.chat.is_group() || msg.chat.is_supergroup() {
            true => "group",
            false => "private",
        };
        let chat_title = msg.chat.title().map(|s| s.to_string());

        // Upsert chat - new chats get enabled status based on bot mode
        let chat = self
            .repo
            .upsert_chat(
                chat_id,
                chat_type.to_string(),
                chat_title,
                self.is_public_mode,
            )
            .await
            .map_err(|e| e.to_string())?;

        if let Some(user) = msg.from.as_ref() {
            let user_id = user.id.0 as i64;
            let username = user.username.clone();

            // Check if user already exists
            let user_model = match self
                .repo
                .get_user(user_id)
                .await
                .map_err(|e| e.to_string())?
            {
                Some(existing_user) => existing_user,
                None => {
                    // New user - determine role
                    let role = if self.owner_id == Some(user_id) {
                        UserRole::Owner
                    } else {
                        UserRole::User
                    };

                    info!("Creating new user {} with role {:?}", user_id, role);

                    self.repo
                        .upsert_user(user_id, username, role)
                        .await
                        .map_err(|e| e.to_string())?
                }
            };

            return Ok((user_model.role, chat.enabled));
        }

        // If no user info, return default user with chat enabled status
        Ok((UserRole::User, chat.enabled))
    }

    // ------------------------------------------------------------------------
    // Help Command
    // ------------------------------------------------------------------------

    async fn handle_help(&self, bot: Bot, chat_id: ChatId) -> ResponseResult<()> {
        let help_text = r#"
📚 *PixivBot 帮助*

*可用命令:*

📌 `/sub <id,...> [+tag1 \-tag2]`
   订阅 Pixiv 作者
   \- `<id,...>`: 以逗号分隔的 Pixiv 用户 ID
   \- `\+tag`: 仅包含带有此标签的作品
   \- `\-tag`: 排除带有此标签的作品
   \- 示例: `/sub 123456,789012 \+原神 \-R\-18`

📊 `/subrank <mode> [+tag1 \-tag2]`
   订阅 Pixiv 排行榜
   \- 模式: `day`, `week`, `month`, `day_male`, `day_female`, `week_original`, `week_rookie`, `day_manga`
   \- R18 模式: `day_r18`, `week_r18`, `week_r18g`, `day_male_r18`, `day_female_r18`
   \- `\+tag`: 仅包含带有此标签的作品
   \- `\-tag`: 排除带有此标签的作品
   \- 示例: `/subrank day \+原神`

🗑 `/unsub <author_id,...>`
   取消订阅作者
   \- 使用逗号分隔的作者 ID \(Pixiv 用户 ID\)
   \- 示例: `/unsub 123456,789012`

🗑 `/unsubrank <mode>`
   取消订阅排行榜
   \- 示例: `/unsubrank day`

🔒 `/blursensitive <on|off>`
   启用或禁用敏感内容模糊
   \- 示例: `/blursensitive on`

🚫 `/excludetags <tag1,tag2,...>`
   设置此聊天的全局排除标签
   \- 示例: `/excludetags R\-18,gore`

🗑 `/clearexcludedtags`
   清除所有排除的标签
"#;

        bot.send_message(chat_id, help_text)
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
        Ok(())
    }

    // ------------------------------------------------------------------------
    // Subscription Commands
    // ------------------------------------------------------------------------

    async fn handle_sub_author(
        &self,
        bot: Bot,
        chat_id: ChatId,
        args: String,
    ) -> ResponseResult<()> {
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
        let filter_tags = FilterTags::parse_from_args(&parts[1..]);
        let filter_tags_json = filter_tags.to_json();

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
                        error!("Failed to get user detail for {}: {}", author_id, e);
                        result.add_failure(format!("`{}` \\(未找到\\)", author_id));
                        continue;
                    }
                }
            };

            // Create or get task and subscription
            match self
                .create_subscription(
                    chat_id.0,
                    "author",
                    author_id_str,
                    Some(&author_name),
                    filter_tags_json.clone(),
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
                    error!("Failed to subscribe to author {}: {}", author_id, e);
                    result.add_failure(format!("`{}` \\(订阅失败\\)", author_id));
                }
            }
        }

        // Build filter tags suffix if any
        let filter_suffix = filter_tags_json.as_ref().and_then(|tags| {
            let filter_str = format_filter_tags(tags);
            if filter_str.is_empty() {
                None
            } else {
                Some(format!("\n🏷 {}", filter_str))
            }
        });

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

    async fn handle_sub_ranking(
        &self,
        bot: Bot,
        chat_id: ChatId,
        args: String,
    ) -> ResponseResult<()> {
        let parts: Vec<&str> = args.split_whitespace().collect();

        if parts.is_empty() {
            let available_modes = RankingMode::all_modes().join(", ");
            bot.send_message(
                chat_id,
                format!(
                    "❌ 用法: `/subrank <mode> [+tag1 -tag2]`\n可用模式: {}",
                    available_modes
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
        let filter_tags = FilterTags::parse_from_args(&parts[1..]);
        let filter_tags_json = filter_tags.to_json();

        // Create subscription
        match self
            .create_subscription(
                chat_id.0,
                "ranking",
                mode.as_str(),
                None,
                filter_tags_json.clone(),
            )
            .await
        {
            Ok(_) => {
                let mut message = format!("✅ 成功订阅 {}", mode.display_name());
                if let Some(ref tags) = filter_tags_json {
                    let filter_str = format_filter_tags(tags);
                    if !filter_str.is_empty() {
                        message.push_str(&format!("\n\n🏷 {}", filter_str));
                    }
                }
                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!("Failed to subscribe to ranking {}: {}", mode.as_str(), e);
                bot.send_message(chat_id, "❌ 创建订阅失败").await?;
            }
        }

        Ok(())
    }

    // ------------------------------------------------------------------------
    // Unsubscribe Commands
    // ------------------------------------------------------------------------

    async fn handle_unsub_author(
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
                .delete_subscription(chat_id.0, "author", author_id)
                .await
            {
                Ok(_) => result.add_success(format!("`{}`", author_id)),
                Err(e) => {
                    error!("Failed to unsubscribe from author {}: {}", author_id, e);
                    result.add_failure(format!("`{}` \\({}\\)", author_id, e));
                }
            }
        }

        let response = result.build_response("✅ 成功取消订阅:", "❌ 取消订阅失败:");
        bot.send_message(chat_id, response)
            .parse_mode(ParseMode::MarkdownV2)
            .await?;

        Ok(())
    }

    async fn handle_unsub_ranking(
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
            .delete_subscription(chat_id.0, "ranking", mode.as_str())
            .await
        {
            Ok(_) => {
                bot.send_message(chat_id, format!("✅ 成功取消订阅 {}", mode.display_name()))
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!(
                    "Failed to unsubscribe from ranking {}: {}",
                    mode.as_str(),
                    e
                );
                bot.send_message(chat_id, format!("❌ 取消订阅失败: {}", e))
                    .await?;
            }
        }

        Ok(())
    }

    // ------------------------------------------------------------------------
    // List Subscriptions
    // ------------------------------------------------------------------------

    async fn handle_list(&self, bot: Bot, chat_id: ChatId) -> ResponseResult<()> {
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
                    .partition(|(_, task)| task.r#type == "author");

                let mut message = "📋 *您的订阅:*\n\n".to_string();

                // First show authors
                for (sub, task) in authors.iter().chain(rankings.iter()) {
                    let type_emoji = match task.r#type.as_str() {
                        "author" => "🎨",
                        "ranking" => "📊",
                        _ => "❓",
                    };

                    // 构建显示名称：对于 author 类型显示作者名字，对于 ranking 类型显示排行榜类型和模式
                    // 使用代码块格式使得ID可以复制
                    let display_info = if task.r#type == "author" {
                        if let Some(ref name) = task.author_name {
                            format!("{} \\| ID: `{}`", markdown::escape(name), task.value)
                        } else {
                            format!("ID: `{}`", task.value)
                        }
                    } else if task.r#type == "ranking" {
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
                    let filter_info = if let Some(tags) = &sub.filter_tags {
                        let filter_str = format_filter_tags(tags);
                        if !filter_str.is_empty() {
                            format!("\n  🏷 {}", filter_str)
                        } else {
                            String::new()
                        }
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
                error!("Failed to list subscriptions: {}", e);
                bot.send_message(chat_id, "❌ 获取订阅列表失败").await?;
            }
        }

        Ok(())
    }

    // ------------------------------------------------------------------------
    // Admin Commands
    // ------------------------------------------------------------------------

    async fn handle_set_admin(
        &self,
        bot: Bot,
        chat_id: ChatId,
        args: String,
        is_admin: bool,
    ) -> ResponseResult<()> {
        let target_user_id = match args.trim().parse::<i64>() {
            Ok(id) => id,
            Err(_) => {
                bot.send_message(
                    chat_id,
                    if is_admin {
                        "❌ 用法: `/setadmin <user_id>`"
                    } else {
                        "❌ 用法: `/unsetadmin <user_id>`"
                    },
                )
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
                return Ok(());
            }
        };

        let role = if is_admin {
            UserRole::Admin
        } else {
            UserRole::User
        };

        match self.repo.set_user_role(target_user_id, role).await {
            Ok(user) => {
                bot.send_message(
                    chat_id,
                    format!("✅ 成功将用户 `{}` 的角色设置为 **{}**", user.id, role),
                )
                .parse_mode(ParseMode::MarkdownV2)
                .await?;

                info!("Owner set user {} role to {:?}", target_user_id, role);
            }
            Err(e) => {
                error!("Failed to set user role: {}", e);
                bot.send_message(chat_id, "❌ 设置用户角色失败。用户可能不存在。")
                    .await?;
            }
        }

        Ok(())
    }

    async fn handle_enable_chat(
        &self,
        bot: Bot,
        current_chat_id: ChatId,
        args: String,
        enabled: bool,
    ) -> ResponseResult<()> {
        // Parse target chat_id from args, or use current chat_id
        let target_chat_id = if args.trim().is_empty() {
            current_chat_id.0
        } else {
            match args.trim().parse::<i64>() {
                Ok(id) => id,
                Err(_) => {
                    bot.send_message(
                        current_chat_id,
                        if enabled {
                            "❌ 用法: `/enablechat [chat_id]`"
                        } else {
                            "❌ 用法: `/disablechat [chat_id]`"
                        },
                    )
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
                    return Ok(());
                }
            }
        };

        match self.repo.set_chat_enabled(target_chat_id, enabled).await {
            Ok(_) => {
                // 判断是否是当前聊天
                let is_current_chat = target_chat_id == current_chat_id.0;

                let message = if enabled {
                    if is_current_chat {
                        "✅ 当前聊天已成功启用".to_string()
                    } else {
                        format!("✅ 聊天 `{}` 已成功启用", target_chat_id)
                    }
                } else if is_current_chat {
                    "✅ 当前聊天已成功禁用".to_string()
                } else {
                    format!("✅ 聊天 `{}` 已成功禁用", target_chat_id)
                };

                bot.send_message(current_chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;

                info!(
                    "Admin {} chat {}",
                    if enabled { "enabled" } else { "disabled" },
                    target_chat_id
                );
            }
            Err(e) => {
                error!("Failed to set chat enabled status: {}", e);
                bot.send_message(current_chat_id, "❌ 更新聊天状态失败")
                    .await?;
            }
        }

        Ok(())
    }

    // ------------------------------------------------------------------------
    // Chat Settings Commands
    // ------------------------------------------------------------------------

    async fn handle_blur_sensitive(
        &self,
        bot: Bot,
        chat_id: ChatId,
        args: String,
    ) -> ResponseResult<()> {
        let arg = args.trim().to_lowercase();

        let blur = match arg.as_str() {
            "on" | "true" | "1" | "yes" => true,
            "off" | "false" | "0" | "no" => false,
            _ => {
                bot.send_message(chat_id, "❌ 用法: `/blursensitive <on|off>`")
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
                return Ok(());
            }
        };

        match self.repo.set_blur_sensitive_tags(chat_id.0, blur).await {
            Ok(_) => {
                bot.send_message(
                    chat_id,
                    if blur {
                        "✅ 敏感内容模糊已**启用**"
                    } else {
                        "✅ 敏感内容模糊已**禁用**"
                    },
                )
                .parse_mode(ParseMode::MarkdownV2)
                .await?;

                info!("Chat {} set blur_sensitive_tags to {}", chat_id, blur);
            }
            Err(e) => {
                error!("Failed to set blur_sensitive_tags: {}", e);
                bot.send_message(chat_id, "❌ 更新设置失败").await?;
            }
        }

        Ok(())
    }

    async fn handle_exclude_tags(
        &self,
        bot: Bot,
        chat_id: ChatId,
        args: String,
    ) -> ResponseResult<()> {
        let arg = args.trim();

        if arg.is_empty() {
            bot.send_message(chat_id, "❌ 用法: `/excludetags <tag1,tag2,...>`")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        let tags: Vec<String> = arg
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        if tags.is_empty() {
            bot.send_message(chat_id, "❌ 未提供有效的标签").await?;
            return Ok(());
        }

        let excluded_tags = Some(json!(tags));

        match self
            .repo
            .set_excluded_tags(chat_id.0, excluded_tags.clone())
            .await
        {
            Ok(_) => {
                let tag_list: Vec<String> = tags
                    .iter()
                    .map(|s| format!("`{}`", markdown::escape(s)))
                    .collect();

                let message = format!("✅ 排除标签已更新: {}", tag_list.join(", "));

                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;

                info!("Chat {} set excluded_tags", chat_id);
            }
            Err(e) => {
                error!("Failed to set excluded_tags: {}", e);
                bot.send_message(chat_id, "❌ 更新设置失败").await?;
            }
        }

        Ok(())
    }

    async fn handle_clear_excluded_tags(&self, bot: Bot, chat_id: ChatId) -> ResponseResult<()> {
        match self.repo.set_excluded_tags(chat_id.0, None).await {
            Ok(_) => {
                bot.send_message(chat_id, "✅ 排除标签已清除").await?;

                info!("Chat {} cleared excluded_tags", chat_id);
            }
            Err(e) => {
                error!("Failed to clear excluded_tags: {}", e);
                bot.send_message(chat_id, "❌ 更新设置失败").await?;
            }
        }

        Ok(())
    }

    async fn handle_settings(&self, bot: Bot, chat_id: ChatId) -> ResponseResult<()> {
        match self.repo.get_chat(chat_id.0).await {
            Ok(Some(chat)) => {
                let blur_status = if chat.blur_sensitive_tags {
                    "**已启用**"
                } else {
                    "**已禁用**"
                };

                let excluded_tags = if let Some(tags) = chat.excluded_tags {
                    if let Ok(tag_array) = serde_json::from_value::<Vec<String>>(tags) {
                        if tag_array.is_empty() {
                            "无".to_string()
                        } else {
                            tag_array
                                .iter()
                                .map(|s| format!("`{}`", markdown::escape(s)))
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                    } else {
                        "无".to_string()
                    }
                } else {
                    "无".to_string()
                };

                let message = format!(
                    "⚙️ *聊天设置*\n\n🔒 敏感内容模糊: {}\n🚫 排除标签: {}",
                    blur_status, excluded_tags
                );

                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Ok(None) => {
                bot.send_message(chat_id, "❌ 未找到聊天").await?;
            }
            Err(e) => {
                error!("Failed to get chat settings: {}", e);
                bot.send_message(chat_id, "❌ 获取设置失败").await?;
            }
        }

        Ok(())
    }

    // ------------------------------------------------------------------------
    // Bot Info (Admin only)
    // ------------------------------------------------------------------------

    async fn handle_info(&self, bot: Bot, chat_id: ChatId) -> ResponseResult<()> {
        // Gather statistics
        let admin_count = self.repo.count_admin_users().await.unwrap_or(0);
        let enabled_chat_count = self.repo.count_enabled_chats().await.unwrap_or(0);
        let subscription_count = self.repo.count_all_subscriptions().await.unwrap_or(0);
        let task_count = self.repo.count_all_tasks().await.unwrap_or(0);

        let message = format!(
            "📊 *PixivBot 状态信息*\n\n\
            👥 管理员人数: `{}`\n\
            💬 启用的聊天数: `{}`\n\
            📋 订阅数: `{}`\n\
            📝 任务数: `{}`",
            admin_count, enabled_chat_count, subscription_count, task_count
        );

        bot.send_message(chat_id, message)
            .parse_mode(ParseMode::MarkdownV2)
            .await?;

        Ok(())
    }

    // ------------------------------------------------------------------------
    // Message Handler (for Pixiv links)
    // ------------------------------------------------------------------------

    /// 处理普通消息（检查 Pixiv 链接）
    ///
    /// - 作品链接 (https://www.pixiv.net/artworks/xxx): 一次性推送作品
    /// - 作者链接 (https://www.pixiv.net/users/xxx): 订阅作者
    ///
    /// 群组中只在被 @ 时响应
    pub async fn handle_message(&self, bot: Bot, msg: Message, me: Me) -> ResponseResult<()> {
        // 获取消息文本
        let text = match msg.text() {
            Some(t) => t,
            None => return Ok(()), // 没有文本，忽略
        };

        // 检查是否包含 Pixiv 链接
        let links = parse_pixiv_links(text);
        if links.is_empty() {
            return Ok(()); // 没有链接，忽略
        }

        let chat_id = msg.chat.id;
        let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
        let is_group = msg.chat.is_group() || msg.chat.is_supergroup();

        // 群组中需要检查是否被 @
        if is_group {
            let bot_username = me.username();
            let entities = msg.entities().unwrap_or(&[]);

            if !is_bot_mentioned(text, entities, bot_username) {
                return Ok(()); // 群组中没被 @，忽略
            }
        }

        info!(
            "Processing Pixiv links from user {} in chat {}: {:?}",
            user_id, chat_id, links
        );

        // 确保用户和聊天存在于数据库中
        let (user_role, chat_enabled) = match self.ensure_user_and_chat(&msg).await {
            Ok(data) => data,
            Err(e) => {
                error!("Failed to ensure user/chat: {}", e);
                return Ok(());
            }
        };

        // 检查聊天是否启用
        let is_private_chat_with_admin = chat_id.is_user() && user_role.is_admin();
        if !chat_enabled && !is_private_chat_with_admin {
            info!(
                "Ignoring message from disabled chat {} (user: {}, role: {:?})",
                chat_id, user_id, user_role
            );
            return Ok(());
        }

        // 获取聊天设置（用于模糊敏感内容）
        let chat_settings = self.repo.get_chat(chat_id.0).await.ok().flatten();
        let blur_sensitive = chat_settings
            .as_ref()
            .map(|c| c.blur_sensitive_tags)
            .unwrap_or(false);

        // 处理每个链接
        for link in links {
            match link {
                PixivLink::Illust(illust_id) => {
                    self.handle_illust_link(bot.clone(), chat_id, illust_id, blur_sensitive)
                        .await?;
                }
                PixivLink::User(user_id) => {
                    self.handle_user_link(bot.clone(), chat_id, user_id).await?;
                }
            }
        }

        Ok(())
    }

    /// 处理作品链接 - 推送作品图片
    async fn handle_illust_link(
        &self,
        bot: Bot,
        chat_id: ChatId,
        illust_id: u64,
        blur_sensitive: bool,
    ) -> ResponseResult<()> {
        info!("Fetching illust {} for chat {}", illust_id, chat_id);

        // 获取作品详情
        let pixiv = self.pixiv_client.read().await;
        let illust = match pixiv.get_illust_detail(illust_id).await {
            Ok(illust) => illust,
            Err(e) => {
                error!("Failed to get illust {}: {}", illust_id, e);
                bot.send_message(chat_id, format!("❌ 获取作品 {} 失败: {}", illust_id, e))
                    .await?;
                return Ok(());
            }
        };
        drop(pixiv);

        // 构建消息
        let page_info = if illust.is_multi_page() {
            format!(" \\({} photos\\)", illust.page_count)
        } else {
            String::new()
        };

        let tags = self.format_tags(&illust);

        let caption = format!(
            "🎨 {}{}\nby *{}* \\(ID: `{}`\\)\n\n👀 {} \\| ❤️ {} \\| 🔗 [来源](https://pixiv\\.net/artworks/{}){}", 
            markdown::escape(&illust.title),
            page_info,
            markdown::escape(&illust.user.name),
            illust.user.id,
            illust.total_view,
            illust.total_bookmarks,
            illust.id,
            tags
        );

        // 检查是否有敏感标签
        let has_spoiler = blur_sensitive && self.has_sensitive_tags(&illust);

        // 获取所有图片 URL
        let image_urls = illust.get_all_image_urls();

        // 发送图片
        let _ = self
            .notifier
            .notify_with_images(chat_id, &image_urls, Some(&caption), has_spoiler)
            .await;

        Ok(())
    }

    /// 处理用户链接 - 订阅作者
    async fn handle_user_link(
        &self,
        bot: Bot,
        chat_id: ChatId,
        user_id: u64,
    ) -> ResponseResult<()> {
        info!("Subscribing to user {} for chat {}", user_id, chat_id);

        // 获取用户详情
        let pixiv = self.pixiv_client.read().await;
        let author = match pixiv.get_user_detail(user_id).await {
            Ok(user) => user,
            Err(e) => {
                error!("Failed to get user {}: {}", user_id, e);
                bot.send_message(chat_id, format!("❌ 获取用户 {} 失败: {}", user_id, e))
                    .await?;
                return Ok(());
            }
        };
        drop(pixiv);

        // 创建或获取任务
        match self
            .repo
            .get_or_create_task(
                "author".to_string(),
                user_id.to_string(),
                Some(author.name.clone()),
            )
            .await
        {
            Ok(task) => {
                // 创建订阅
                match self
                    .repo
                    .upsert_subscription(chat_id.0, task.id, None)
                    .await
                {
                    Ok(_) => {
                        let message = format!(
                            "✅ 成功订阅作者 *{}* \\(ID: `{}`\\)",
                            markdown::escape(&author.name),
                            user_id
                        );
                        bot.send_message(chat_id, message)
                            .parse_mode(ParseMode::MarkdownV2)
                            .await?;
                    }
                    Err(e) => {
                        error!("Failed to create subscription for {}: {}", user_id, e);
                        bot.send_message(chat_id, "❌ 创建订阅失败").await?;
                    }
                }
            }
            Err(e) => {
                error!("Failed to create task for {}: {}", user_id, e);
                bot.send_message(chat_id, "❌ 创建任务失败").await?;
            }
        }

        Ok(())
    }

    /// 检查作品是否包含敏感标签（使用标准化匹配）
    fn has_sensitive_tags(&self, illust: &crate::pixiv_client::Illust) -> bool {
        use crate::utils::html;

        let illust_tags: Vec<String> = illust
            .tags
            .iter()
            .map(|tag| html::normalize_tag(&tag.name))
            .collect();

        for sensitive_tag in &self.sensitive_tags {
            let sensitive_normalized = html::normalize_tag(sensitive_tag);
            if illust_tags.iter().any(|t| t == &sensitive_normalized) {
                return true;
            }
        }

        false
    }

    /// 格式化标签用于显示
    fn format_tags(&self, illust: &crate::pixiv_client::Illust) -> String {
        use crate::utils::html;

        let tag_names: Vec<&str> = illust.tags.iter().map(|t| t.name.as_str()).collect();
        let formatted = html::format_tags(&tag_names);

        if formatted.is_empty() {
            return String::new();
        }

        let escaped: Vec<String> = formatted
            .iter()
            .map(|t| format!("\\#{}", markdown::escape(t)))
            .collect();

        format!("\n\n{}", escaped.join("  "))
    }

    // ------------------------------------------------------------------------
    // Subscription Helper Methods
    // ------------------------------------------------------------------------

    /// Create or update a subscription for a chat
    async fn create_subscription(
        &self,
        chat_id: i64,
        task_type: &str,
        task_value: &str,
        author_name: Option<&str>,
        filter_tags: Option<Value>,
    ) -> Result<(), String> {
        // Get or create the task
        let task = self
            .repo
            .get_or_create_task(
                task_type.to_string(),
                task_value.to_string(),
                author_name.map(|s| s.to_string()),
            )
            .await
            .map_err(|e| format!("任务创建失败: {}", e))?;

        // Create subscription
        self.repo
            .upsert_subscription(chat_id, task.id, filter_tags)
            .await
            .map_err(|e| format!("订阅失败: {}", e))?;

        Ok(())
    }

    /// Delete a subscription and cleanup orphaned tasks
    async fn delete_subscription(
        &self,
        chat_id: i64,
        task_type: &str,
        task_value: &str,
    ) -> Result<(), String> {
        // Find the task
        let task = self
            .repo
            .get_task_by_type_value(task_type, task_value)
            .await
            .map_err(|e| format!("数据库错误: {}", e))?
            .ok_or_else(|| "未找到".to_string())?;

        // Delete subscription
        self.repo
            .delete_subscription_by_chat_task(chat_id, task.id)
            .await
            .map_err(|_| "未订阅".to_string())?;

        // Cleanup orphaned task if no more subscriptions
        self.cleanup_orphaned_task(task.id, task_type, task_value)
            .await;

        Ok(())
    }

    /// Cleanup task if it has no more subscriptions
    async fn cleanup_orphaned_task(&self, task_id: i32, task_type: &str, task_value: &str) {
        match self.repo.count_subscriptions_for_task(task_id).await {
            Ok(0) => {
                if let Err(e) = self.repo.delete_task(task_id).await {
                    error!("Failed to delete task {}: {}", task_id, e);
                } else {
                    info!(
                        "Deleted task {} ({} {}) - no more subscriptions",
                        task_id, task_type, task_value
                    );
                }
            }
            Err(e) => {
                error!("Failed to count subscriptions for task {}: {}", task_id, e);
            }
            _ => {}
        }
    }
}
