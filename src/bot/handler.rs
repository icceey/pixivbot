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
                bot.send_message(chat_id, "⚠️ Database error occurred").await?;
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
📚 *PixivBot Help*

*Available Commands:*

📌 `/sub <id,...> [+tag1 \-tag2]`
   Subscribe to Pixiv author\(s\)
   \- `<id,...>`: Comma\-separated Pixiv user IDs
   \- `\+tag`: Include only works with this tag
   \- `\-tag`: Exclude works with this tag
   \- Example: `/sub 123456,789012 \+原神 \-R\-18`

📊 `/subrank <mode>`
   Subscribe to Pixiv ranking
   \- Modes: `daily`, `weekly`, `monthly`
   \- R18 variants: `daily_r18`, `weekly_r18`
   \- Gender\-specific: `daily_male`, `daily_female`
   \- Example: `/subrank daily`

📋 `/list`
   List all your active subscriptions

🗑 `/unsub <author_id,...>`
   Unsubscribe from author subscription\(s\)
   \- Use comma\-separated author IDs \(Pixiv user ID\)
   \- Example: `/unsub 123456,789012`

🗑 `/unsubrank <mode>`
   Unsubscribe from ranking subscription
   \- Example: `/unsubrank daily`

⚙️ `/settings`
   Show current chat settings

🔒 `/blursensitive <on|off>`
   Enable or disable sensitive tag blur
   \- Example: `/blursensitive on`

🚫 `/excludetags <tag1,tag2,...>`
   Set globally excluded tags for this chat
   \- Example: `/excludetags R\-18,gore`

🗑 `/clearexcludedtags`
   Clear all excluded tags

❓ `/help`
   Show this help message

\-\-\-
Made with ❤️ using Rust
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
            bot.send_message(chat_id, "❌ Usage: `/sub <id,...> [+tag1 -tag2]`")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        // First part is comma-separated IDs
        let ids_str = parts[0];
        let author_ids: Vec<&str> = ids_str.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
        
        if author_ids.is_empty() {
            bot.send_message(chat_id, "❌ Please provide at least one author ID")
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
                    failed_list.push(format!("`{}` \\(invalid ID\\)", author_id_str));
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
                        failed_list.push(format!("`{}` \\(not found\\)", author_id));
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
                            failed_list.push(format!("`{}` \\(subscription error\\)", author_id));
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to create task for {}: {}", author_id, e);
                    failed_list.push(format!("`{}` \\(task error\\)", author_id));
                }
            }
        }

        // Build response message
        let mut response = String::new();
        
        if !success_list.is_empty() {
            response.push_str("✅ Successfully subscribed to:\n");
            for author in &success_list {
                response.push_str(&format!("  • {}\n", author));
            }
            
            if let Some(ref tags) = filter_tags {
                response.push_str(&format!(
                    "\n🏷 Filters: Include: {:?}, Exclude: {:?}",
                    tags.get("include"),
                    tags.get("exclude")
                ));
            }
        }
        
        if !failed_list.is_empty() {
            if !response.is_empty() {
                response.push_str("\n\n");
            }
            response.push_str("❌ Failed to subscribe to:\n");
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
                "❌ Usage: `/subrank <mode>`\nModes: daily, weekly, monthly, daily\\_r18, etc\\."
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
            return Ok(());
        }
        let valid_modes = vec![
            "daily", "weekly", "monthly",
            "daily_r18", "weekly_r18",
            "daily_male", "daily_female",
            "daily_male_r18", "daily_female_r18",
        ];

        if !valid_modes.contains(&mode) {
            bot.send_message(
                chat_id,
                format!("❌ Invalid ranking mode. Valid modes: {}", valid_modes.join(", "))
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
                            format!("✅ Successfully subscribed to `{}` ranking", mode.replace('_', "\\_"))
                        )
                        .parse_mode(ParseMode::MarkdownV2)
                        .await?;
                    }
                    Err(e) => {
                        error!("Failed to create subscription: {}", e);
                        bot.send_message(chat_id, "❌ Failed to create subscription")
                            .await?;
                    }
                }
            }
            Err(e) => {
                error!("Failed to create task: {}", e);
                bot.send_message(chat_id, "❌ Failed to create subscription task")
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
            bot.send_message(chat_id, "❌ Usage: `/unsub <author_id,...>`")
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
                            failed_list.push(format!("`{}` (not subscribed)", author_id));
                        }
                    }
                }
                Ok(None) => {
                    failed_list.push(format!("`{}` (not found)", author_id));
                }
                Err(e) => {
                    error!("Failed to get task for {}: {}", author_id, e);
                    failed_list.push(format!("`{}` (error)", author_id));
                }
            }
        }

        // Build response message
        let mut response = String::new();
        
        if !success_list.is_empty() {
            response.push_str("✅ Successfully unsubscribed from:\n");
            for author in &success_list {
                response.push_str(&format!("  • {}\n", author));
            }
        }
        
        if !failed_list.is_empty() {
            if !response.is_empty() {
                response.push_str("\n");
            }
            response.push_str("❌ Failed to unsubscribe from:\n");
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
            bot.send_message(chat_id, "❌ Usage: `/unsubrank <mode>`")
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
                        
                        bot.send_message(chat_id, format!("✅ Successfully unsubscribed from `{}` ranking", mode.replace('_', "\\_")))
                            .parse_mode(ParseMode::MarkdownV2)
                            .await?;
                    }
                    Err(e) => {
                        error!("Failed to delete subscription: {}", e);
                        bot.send_message(chat_id, "❌ Failed to unsubscribe\\. You may not be subscribed to this ranking\\.")
                            .parse_mode(ParseMode::MarkdownV2)
                            .await?;
                    }
                }
            }
            Ok(None) => {
                bot.send_message(chat_id, format!("❌ Ranking `{}` not found in your subscriptions", mode.replace('_', "\\_")))
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!("Failed to get task: {}", e);
                bot.send_message(chat_id, "❌ Database error occurred")
                    .await?;
            }
        }

        Ok(())
    }

    async fn handle_list(&self, bot: Bot, chat_id: ChatId) -> ResponseResult<()> {
        match self.repo.list_subscriptions_by_chat(chat_id.0).await {
            Ok(subscriptions) => {
                if subscriptions.is_empty() {
                    bot.send_message(chat_id, "📭 You have no active subscriptions\\.\n\nUse `/sub` to subscribe\\!")
                        .parse_mode(ParseMode::MarkdownV2)
                        .await?;
                    return Ok(());
                }

                let mut message = "📋 *Your Subscriptions:*\n\n".to_string();
                
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

                message.push_str("\n💡 Use `/unsub <id>` or `/unsubrank <mode>` to unsubscribe");

                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!("Failed to list subscriptions: {}", e);
                bot.send_message(chat_id, "❌ Failed to retrieve subscriptions")
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
                        "❌ Usage: `/setadmin <user_id>`"
                    } else {
                        "❌ Usage: `/unsetadmin <user_id>`"
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
                        "✅ Successfully set user `{}` role to **{}**",
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
                    "❌ Failed to set user role. User may not exist yet."
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
                            "❌ Usage: `/enablechat [chat_id]`"
                        } else {
                            "❌ Usage: `/disablechat [chat_id]`"
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
                        format!("✅ Chat `{}` enabled successfully", target_chat_id)
                    } else {
                        format!("✅ Chat `{}` disabled successfully", target_chat_id)
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
                    "❌ Failed to update chat status"
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
                    "❌ Usage: `/blursensitive <on|off>`"
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
                        "✅ Sensitive content blur **enabled**"
                    } else {
                        "✅ Sensitive content blur **disabled**"
                    }
                )
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
                
                info!("Chat {} set blur_sensitive_tags to {}", chat_id, blur);
            }
            Err(e) => {
                error!("Failed to set blur_sensitive_tags: {}", e);
                bot.send_message(chat_id, "❌ Failed to update settings")
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
                "❌ Usage: `/excludetags <tag1,tag2,...>`"
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
                "❌ No valid tags provided"
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
                
                let message = format!("✅ Excluded tags updated: {}", tag_list.join(", "));
                
                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
                
                info!("Chat {} set excluded_tags", chat_id);
            }
            Err(e) => {
                error!("Failed to set excluded_tags: {}", e);
                bot.send_message(chat_id, "❌ Failed to update settings")
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
                bot.send_message(chat_id, "✅ Excluded tags cleared")
                    .await?;
                
                info!("Chat {} cleared excluded_tags", chat_id);
            }
            Err(e) => {
                error!("Failed to clear excluded_tags: {}", e);
                bot.send_message(chat_id, "❌ Failed to update settings")
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
                    "**Enabled**"
                } else {
                    "**Disabled**"
                };
                
                let excluded_tags = if let Some(tags) = chat.excluded_tags {
                    if let Ok(tag_array) = serde_json::from_value::<Vec<String>>(tags) {
                        if tag_array.is_empty() {
                            "None".to_string()
                        } else {
                            tag_array.iter()
                                .map(|s| format!("`{}`", markdown::escape(s)))
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                    } else {
                        "None".to_string()
                    }
                } else {
                    "None".to_string()
                };
                
                let message = format!(
                    "⚙️ *Chat Settings*\n\n🔒 Sensitive content blur: {}\n🚫 Excluded tags: {}",
                    blur_status,
                    excluded_tags
                );
                
                bot.send_message(chat_id, message)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Ok(None) => {
                bot.send_message(chat_id, "❌ Chat not found")
                    .await?;
            }
            Err(e) => {
                error!("Failed to get chat settings: {}", e);
                bot.send_message(chat_id, "❌ Failed to retrieve settings")
                    .await?;
            }
        }

        Ok(())
    }
}
