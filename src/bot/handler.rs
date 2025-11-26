use teloxide::prelude::*;
use teloxide::types::ParseMode;
use crate::db::repo::Repo;
use crate::db::entities::role::UserRole;
use crate::pixiv::client::PixivClient;
use crate::bot::Command;
use crate::utils::markdown;
use std::sync::Arc;
use tracing::{info, error};
use serde_json::json;

#[derive(Clone)]
pub struct BotHandler {
    bot: Bot,
    repo: Arc<Repo>,
    pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
    owner_id: Option<i64>,
    is_public_mode: bool,
}

impl BotHandler {
    pub fn new(
        bot: Bot,
        repo: Arc<Repo>,
        pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
        owner_id: Option<i64>,
        is_public_mode: bool,
    ) -> Self {
        Self {
            bot,
            repo,
            pixiv_client,
            owner_id,
            is_public_mode,
        }
    }

    pub async fn handle_command(
        &self,
        bot: Bot,
        msg: Message,
        cmd: Command,
    ) -> ResponseResult<()> {
        let chat_id = msg.chat.id;
        let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
        
        info!("Received command from user {} in chat {}: {:?}", user_id, chat_id, cmd);

        // Ensure user and chat exist in database
        let (user_role, chat_enabled) = match self.ensure_user_and_chat(&msg).await {
            Ok(data) => data,
            Err(e) => {
                let error_msg = format!("Failed to ensure user/chat: {}", e);
                error!("{}", error_msg);
                bot.send_message(chat_id, "⚠️ 数据库错误").await?;
                return Ok(());
            }
        };

        // Check if chat is enabled
        // Special case: private chat with admin/owner, consider it enabled ()
        let is_private_chat_with_admin = chat_id.is_user() && user_role.is_admin();
        
        if !chat_enabled && !is_private_chat_with_admin {
            info!("Ignoring command from disabled chat {} (user: {}, role: {:?})", chat_id, user_id, user_role);
            return Ok(());
        }

        match cmd {
            Command::Help => self.handle_help(bot, chat_id).await,
            Command::Sub(args) => self.handle_sub_author(bot, chat_id, user_id, args).await,
            Command::SubRank(args) => self.handle_sub_ranking_cmd(bot, chat_id, user_id, args).await,
            Command::Unsub(args) => self.handle_unsub_author(bot, chat_id, args).await,
            Command::UnsubRank(args) => self.handle_unsub_ranking(bot, chat_id, args).await,
            Command::List => self.handle_list(bot, chat_id).await,
            Command::SetAdmin(args) => {
                // Only owner can use this command
                if !user_role.is_owner() {
                    info!("User {} attempted to use SetAdmin without permission", user_id);
                    return Ok(()); // Silently ignore
                }
                self.handle_set_admin(bot, chat_id, args, true).await
            }
            Command::UnsetAdmin(args) => {
                // Only owner can use this command
                if !user_role.is_owner() {
                    info!("User {} attempted to use UnsetAdmin without permission", user_id);
                    return Ok(()); // Silently ignore
                }
                self.handle_set_admin(bot, chat_id, args, false).await
            }
            Command::EnableChat(args) => {
                // Only admin or owner can use this command
                if !user_role.is_admin() {
                    info!("User {} attempted to use EnableChat without permission", user_id);
                    return Ok(()); // Silently ignore
                }
                self.handle_enable_chat(bot, chat_id, args, true).await
            }
            Command::DisableChat(args) => {
                // Only admin or owner can use this command
                if !user_role.is_admin() {
                    info!("User {} attempted to use DisableChat without permission", user_id);
                    return Ok(()); // Silently ignore
                }
                self.handle_enable_chat(bot, chat_id, args, false).await
            }
            Command::BlurSensitive(args) => self.handle_blur_sensitive(bot, chat_id, args).await,
            Command::ExcludeTags(args) => self.handle_exclude_tags(bot, chat_id, args).await,
            Command::ClearExcludedTags => self.handle_clear_excluded_tags(bot, chat_id).await,
            Command::Settings => self.handle_settings(bot, chat_id).await,
        }
    }

    async fn ensure_user_and_chat(&self, msg: &Message) -> Result<(UserRole, bool), String> {
        let chat_id = msg.chat.id.0;
        let chat_type = match msg.chat.is_group() || msg.chat.is_supergroup() {
            true => "group",
            false => "private",
        };
        let chat_title = msg.chat.title().map(|s| s.to_string());

        // Upsert chat - new chats get enabled status based on bot mode
        let chat = self.repo.upsert_chat(chat_id, chat_type.to_string(), chat_title, self.is_public_mode)
            .await
            .map_err(|e| e.to_string())?;

        if let Some(user) = msg.from.as_ref() {
            let user_id = user.id.0 as i64;
            let username = user.username.clone();
            
            // Check if user already exists
            let user_model = match self.repo.get_user(user_id).await.map_err(|e| e.to_string())? {
                Some(existing_user) => existing_user,
                None => {
                    // New user - determine role
                    let role = if self.owner_id == Some(user_id) {
                        UserRole::Owner
                    } else {
                        UserRole::User
                    };
                    
                    info!("Creating new user {} with role {:?}", user_id, role);
                    
                    self.repo.upsert_user(user_id, username, role)
                        .await
                        .map_err(|e| e.to_string())?
                }
            };
            
            return Ok((user_model.role, chat.enabled));
        }

        // If no user info, return default user with chat enabled status
        Ok((UserRole::User, chat.enabled))
    }

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

📊 `/subrank <mode>`
   订阅 Pixiv 排行榜
   \- 模式: `day`, `week`, `month`, `day_male`, `day_female`, `week_original`, `week_rookie`, `day_manga`
   \- R18 模式: `day_r18`, `week_r18`, `week_r18g`, `day_male_r18`, `day_female_r18`
   \- 示例: `/subrank day`

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

    async fn handle_sub_author(
        &self,
        bot: Bot,
        chat_id: ChatId,
        user_id: i64,
        args: String,
    ) -> ResponseResult<()> {
        let parts: Vec<&str> = args.split_whitespace().collect();
        
        if parts.is_empty() {
            bot.send_message(chat_id, "❌ 用法: `/sub <id,...> [+tag1 -tag2]`")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        // First part is comma-separated IDs
        let ids_str = parts[0];
        let author_ids: Vec<&str> = ids_str.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
        
        if author_ids.is_empty() {
            bot.send_message(chat_id, "❌ 请提供至少一个作者 ID")
                .await?;
            return Ok(());
        }

        // Parse filter tags (shared by all authors in this batch)
        let mut include_tags = Vec::new();
        let mut exclude_tags = Vec::new();
        
        for tag in &parts[1..] {
            if let Some(stripped) = tag.strip_prefix('+') {
                include_tags.push(stripped.to_string());
            } else if let Some(stripped) = tag.strip_prefix('-') {
                exclude_tags.push(stripped.to_string());
            } else {
                include_tags.push(tag.to_string());
            }
        }

        let filter_tags = if !include_tags.is_empty() || !exclude_tags.is_empty() {
            Some(json!({
                "include": include_tags,
                "exclude": exclude_tags,
            }))
        } else {
            None
        };

        let mut success_list: Vec<String> = Vec::new();
        let mut failed_list: Vec<String> = Vec::new();

        for author_id_str in author_ids {
            // Validate it's a number
            let author_id = match author_id_str.parse::<u64>() {
                Ok(id) => id,
                Err(_) => {
                    failed_list.push(format!("`{}` \\(无效 ID\\)", author_id_str));
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
                        failed_list.push(format!("`{}` \\(未找到\\)", author_id));
                        continue;
                    }
                }
            };

            // Create or get task
            match self.repo.get_or_create_task(
                "author".to_string(),
                author_id_str.to_string(),
                user_id,
                Some(author_name.clone()),
            ).await {
                Ok(task) => {
                    // Create subscription
                    match self.repo.upsert_subscription(
                        chat_id.0,
                        task.id,
                        filter_tags.clone(),
                    ).await {
                        Ok(_) => {
                            success_list.push(format!("*{}* \\(ID: `{}`\\)", markdown::escape(&author_name), author_id));
                        }
                        Err(e) => {
                            error!("Failed to create subscription for {}: {}", author_id, e);
                            failed_list.push(format!("`{}` \\(订阅失败\\)", author_id));
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to create task for {}: {}", author_id, e);
                    failed_list.push(format!("`{}` \\(任务创建失败\\)", author_id));
                }
            }
        }

        // Build response message
        let mut response = String::new();
        
        if !success_list.is_empty() {
            response.push_str("✅ 成功订阅:\n");
            for author in &success_list {
                response.push_str(&format!("  • {}\n", author));
            }
            
            if let Some(ref tags) = filter_tags {
                response.push_str(&format!(
                    "\n🏷 过滤器: 包含: {:?}, 排除: {:?}",
                    tags.get("include"),
                    tags.get("exclude")
                ));
            }
        }
        
        if !failed_list.is_empty() {
            if !response.is_empty() {
                response.push_str("\n\n");
            }
            response.push_str("❌ 订阅失败:\n");
            for author in &failed_list {
                response.push_str(&format!("  • {}\n", author));
            }
        }

        bot.send_message(chat_id, response)
            .parse_mode(ParseMode::MarkdownV2)
            .await?;

        Ok(())
    }

    async fn handle_sub_ranking_cmd(
        &self,
        bot: Bot,
        chat_id: ChatId,
        user_id: i64,
        args: String,
    ) -> ResponseResult<()> {
        let mode = args.trim();
        
        if mode.is_empty() {
            bot.send_message(
                chat_id,
                "❌ 用法: `/subrank <mode>`\n模式: day, week, month, day\\_r18 等"
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
            return Ok(());
        }
        let valid_modes = vec![
            "day", "week", "month",
            "day_male", "day_female",
            "week_original", "week_rookie",
            "day_manga",
            "day_r18", "week_r18", "week_r18g",
            "day_male_r18", "day_female_r18",
        ];

        if !valid_modes.contains(&mode) {
            bot.send_message(
                chat_id,
                format!("❌ 无效的排行榜模式。有效模式: {}", valid_modes.join(", "))
            )
            .await?;
            return Ok(());
        }

        // Create or get task
        match self.repo.get_or_create_task(
            "ranking".to_string(),
            mode.to_string(),
            user_id,
            None, // No author_name for ranking tasks
        ).await {
            Ok(task) => {
                // Create subscription
                match self.repo.upsert_subscription(
                    chat_id.0,
                    task.id,
                    None,
                ).await {
                    Ok(_) => {
                        bot.send_message(
                            chat_id,
                            format!("✅ 成功订阅 `{}` 排行榜", mode.replace('_', "\\_"))
                        )
                        .parse_mode(ParseMode::MarkdownV2)
                        .await?;
                    }
                    Err(e) => {
                        error!("Failed to create subscription: {}", e);
                        bot.send_message(chat_id, "❌ 创建订阅失败")
                            .await?;
                    }
                }
            }
            Err(e) => {
                error!("Failed to create task: {}", e);
                bot.send_message(chat_id, "❌ 创建订阅任务失败")
                    .await?;
            }
        }

        Ok(())
    }

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

        let author_ids: Vec<&str> = ids_str.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
        
        let mut success_list: Vec<String> = Vec::new();
        let mut failed_list: Vec<String> = Vec::new();

        for author_id in author_ids {
            // Find task by author ID
            match self.repo.get_task_by_type_value("author", author_id).await {
                Ok(Some(task)) => {
                    // Delete subscription for this chat and task
                    match self.repo.delete_subscription_by_chat_task(chat_id.0, task.id).await {
                        Ok(_) => {
                            // Check if task still has other subscriptions
                            match self.repo.count_subscriptions_for_task(task.id).await {
                                Ok(count) => {
                                    if count == 0 {
                                        // No more subscriptions, delete the task
                                        if let Err(e) = self.repo.delete_task(task.id).await {
                                            error!("Failed to delete task {}: {}", task.id, e);
                                        } else {
                                            info!("Deleted task {} (author {}) - no more subscriptions", task.id, author_id);
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to count subscriptions for task {}: {}", task.id, e);
                                }
                            }
                            success_list.push(format!("`{}`", author_id));
                        }
                        Err(e) => {
                            error!("Failed to delete subscription for {}: {}", author_id, e);
                            failed_list.push(format!("`{}` (未订阅)", author_id));
                        }
                    }
                }
                Ok(None) => {
                    failed_list.push(format!("`{}` (未找到)", author_id));
                }
                Err(e) => {
                    error!("Failed to get task for {}: {}", author_id, e);
                    failed_list.push(format!("`{}` (错误)", author_id));
                }
            }
        }

        // Build response message
        let mut response = String::new();
        
        if !success_list.is_empty() {
            response.push_str("✅ 成功取消订阅:\n");
            for author in &success_list {
                response.push_str(&format!("  • {}\n", author));
            }
        }
        
        if !failed_list.is_empty() {
            if !response.is_empty() {
                response.push_str("\n");
            }
            response.push_str("❌ 取消订阅失败:\n");
            for author in &failed_list {
                response.push_str(&format!("  • {}\n", author));
            }
        }

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
        let mode = args.trim();
        
        if mode.is_empty() {
            bot.send_message(chat_id, "❌ 用法: `/unsubrank <mode>`")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        // Find task by ranking mode
        match self.repo.get_task_by_type_value("ranking", mode).await {
            Ok(Some(task)) => {
                // Delete subscription for this chat and task
                match self.repo.delete_subscription_by_chat_task(chat_id.0, task.id).await {
                    Ok(_) => {
                        // Check if task still has other subscriptions
                        match self.repo.count_subscriptions_for_task(task.id).await {
                            Ok(count) => {
                                if count == 0 {
                                    // No more subscriptions, delete the task
                                    if let Err(e) = self.repo.delete_task(task.id).await {
                                        error!("Failed to delete task {}: {}", task.id, e);
                                    } else {
                                        info!("Deleted task {} (ranking {}) - no more subscriptions", task.id, mode);
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Failed to count subscriptions for task {}: {}", task.id, e);
                            }
                        }
                        
                        bot.send_message(chat_id, format!("✅ 成功取消订阅 `{}` 排行榜", mode.replace('_', "\\_")))
                            .parse_mode(ParseMode::MarkdownV2)
                            .await?;
                    }
                    Err(e) => {
                        error!("Failed to delete subscription: {}", e);
                        bot.send_message(chat_id, "❌ 取消订阅失败。您可能未订阅此排行榜。")
                            .parse_mode(ParseMode::MarkdownV2)
                            .await?;
                    }
                }
            }
            Ok(None) => {
                bot.send_message(chat_id, format!("❌ 未在您的订阅中找到 `{}` 排行榜", mode.replace('_', "\\_")))
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!("Failed to get task: {}", e);
                bot.send_message(chat_id, "❌ 数据库错误")
                    .await?;
            }
        }

        Ok(())
    }

    async fn handle_list(&self, bot: Bot, chat_id: ChatId) -> ResponseResult<()> {
        match self.repo.list_subscriptions_by_chat(chat_id.0).await {
            Ok(subscriptions) => {
                if subscriptions.is_empty() {
                    bot.send_message(chat_id, "📭 您没有生效的订阅。\n\n使用 `/sub` 开始订阅！")
                        .parse_mode(ParseMode::MarkdownV2)
                        .await?;
                    return Ok(());
                }

                let mut message = "📋 *您的订阅:*\n\n".to_string();
                
                for (sub, task) in subscriptions {
                    let type_emoji = match task.r#type.as_str() {
                        "author" => "🎨",
                        "ranking" => "📊",
                        _ => "❓",
                    };
                    
                    // 构建显示名称：对于 author 类型显示作者名字，否则显示 value
                    // 使用代码块格式使得ID可以复制
                    let display_info = if task.r#type == "author" {
                        if let Some(ref name) = task.author_name {
                            format!("{} \\| ID: `{}`", markdown::escape(name), task.value)
                        } else {
                            format!("ID: `{}`", task.value)
                        }
                    } else {
                        task.value.replace('_', "\\_")
                    };
                    
                    let filter_info = if task.r#type == "author" {
                        // Show filter tags for author subscriptions
                        if let Some(tags) = &sub.filter_tags {
                            if let Ok(filter) = serde_json::from_value::<serde_json::Value>(tags.clone()) {
                                let include = filter.get("include")
                                    .and_then(|v| v.as_array())
                                    .map(|arr| arr.iter()
                                        .filter_map(|v| v.as_str())
                                        .map(|s| format!("\\+{}", s.replace('-', "\\-")))
                                        .collect::<Vec<_>>()
                                        .join(" "))
                                    .unwrap_or_default();
                                
                                let exclude = filter.get("exclude")
                                    .and_then(|v| v.as_array())
                                    .map(|arr| arr.iter()
                                        .filter_map(|v| v.as_str())
                                        .map(|s| format!("\\-{}", s.replace('-', "\\-")))
                                        .collect::<Vec<_>>()
                                        .join(" "))
                                    .unwrap_or_default();
                                
                                let mut filters = Vec::new();
                                if !include.is_empty() {
                                    filters.push(include);
                                }
                                if !exclude.is_empty() {
                                    filters.push(exclude);
                                }
                                
                                if !filters.is_empty() {
                                    format!("\n  🏷 Tags: {}", filters.join(" "))
                                } else {
                                    String::new()
                                }
                            } else {
                                String::new()
                            }
                        } else {
                            String::new()
                        }
                    } else {
                        String::new()
                    };

                    message.push_str(&format!(
                        "{} {}{}\n",
                        type_emoji,
                        display_info,
                        filter_info
                    ));
                }

                message.push_str("\n💡 使用 `/unsub <id>` 或 `/unsubrank <mode>` 取消订阅");

                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!("Failed to list subscriptions: {}", e);
                bot.send_message(chat_id, "❌ 获取订阅列表失败")
                    .await?;
            }
        }

        Ok(())
    }

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
                    }
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
                    format!(
                        "✅ 成功将用户 `{}` 的角色设置为 **{}**",
                        user.id,
                        role
                    )
                )
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
                
                info!("Owner set user {} role to {:?}", target_user_id, role);
            }
            Err(e) => {
                error!("Failed to set user role: {}", e);
                bot.send_message(
                    chat_id,
                    "❌ 设置用户角色失败。用户可能不存在。"
                )
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
                        }
                    )
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
                    return Ok(());
                }
            }
        };

        match self.repo.set_chat_enabled(target_chat_id, enabled).await {
            Ok(_) => {
                bot.send_message(
                    current_chat_id,
                    if enabled {
                        format!("✅ 聊天 `{}` 已成功启用", target_chat_id)
                    } else {
                        format!("✅ 聊天 `{}` 已成功禁用", target_chat_id)
                    }
                )
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
                
                info!("Admin {} chat {}", if enabled { "enabled" } else { "disabled" }, target_chat_id);
            }
            Err(e) => {
                error!("Failed to set chat enabled status: {}", e);
                bot.send_message(
                    current_chat_id,
                    "❌ 更新聊天状态失败"
                )
                .await?;
            }
        }

        Ok(())
    }

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
                bot.send_message(
                    chat_id,
                    "❌ 用法: `/blursensitive <on|off>`"
                )
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
                    }
                )
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
                
                info!("Chat {} set blur_sensitive_tags to {}", chat_id, blur);
            }
            Err(e) => {
                error!("Failed to set blur_sensitive_tags: {}", e);
                bot.send_message(chat_id, "❌ 更新设置失败")
                    .await?;
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
            bot.send_message(
                chat_id,
                "❌ 用法: `/excludetags <tag1,tag2,...>`"
            )
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
            bot.send_message(
                chat_id,
                "❌ 未提供有效的标签"
            )
            .await?;
            return Ok(());
        }
        
        let excluded_tags = Some(json!(tags));

        match self.repo.set_excluded_tags(chat_id.0, excluded_tags.clone()).await {
            Ok(_) => {
                let tag_list: Vec<String> = tags.iter()
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
                bot.send_message(chat_id, "❌ 更新设置失败")
                    .await?;
            }
        }

        Ok(())
    }

    async fn handle_clear_excluded_tags(
        &self,
        bot: Bot,
        chat_id: ChatId,
    ) -> ResponseResult<()> {
        match self.repo.set_excluded_tags(chat_id.0, None).await {
            Ok(_) => {
                bot.send_message(chat_id, "✅ 排除标签已清除")
                    .await?;
                
                info!("Chat {} cleared excluded_tags", chat_id);
            }
            Err(e) => {
                error!("Failed to clear excluded_tags: {}", e);
                bot.send_message(chat_id, "❌ 更新设置失败")
                    .await?;
            }
        }

        Ok(())
    }

    async fn handle_settings(
        &self,
        bot: Bot,
        chat_id: ChatId,
    ) -> ResponseResult<()> {
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
                            tag_array.iter()
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
                    blur_status,
                    excluded_tags
                );
                
                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Ok(None) => {
                bot.send_message(chat_id, "❌ 未找到聊天")
                    .await?;
            }
            Err(e) => {
                error!("Failed to get chat settings: {}", e);
                bot.send_message(chat_id, "❌ 获取设置失败")
                    .await?;
            }
        }

        Ok(())
    }
}
