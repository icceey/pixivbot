use teloxide::prelude::*;
use teloxide::types::ParseMode;
use crate::db::repo::Repo;
use crate::db::entities::role::UserRole;
use crate::pixiv::client::PixivClient;
use crate::bot::Command;
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
            Command::Sub(args) => self.handle_sub(bot, chat_id, user_id, args).await,
            Command::Unsub(args) => self.handle_unsub(bot, chat_id, args).await,
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
📚 **PixivBot Help**

*Available Commands:*

📌 `/sub author <pixiv_id> [+tag1 -tag2]`
   Subscribe to a Pixiv author
   - `<pixiv_id>`: Pixiv user ID (numbers only)
   - `+tag`: Include only works with this tag
   - `-tag`: Exclude works with this tag
   - Example: `/sub author 123456 +原神 -R-18`

📊 `/sub ranking <mode>`
   Subscribe to Pixiv ranking
   - Modes: `daily`, `weekly`, `monthly`
   - R18 variants: `daily_r18`, `weekly_r18`
   - Gender-specific: `daily_male`, `daily_female`
   - Example: `/sub ranking daily`

📋 `/list`
   List all your active subscriptions

🗑 `/unsub <subscription_id>`
   Unsubscribe from a subscription
   - Get ID from `/list` command
   - Example: `/unsub 5`

❓ `/help`
   Show this help message

---
Made with ❤️ using Rust
"#;

        bot.send_message(chat_id, help_text)
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
        Ok(())
    }

    async fn handle_sub(
        &self,
        bot: Bot,
        chat_id: ChatId,
        user_id: i64,
        args: String,
    ) -> ResponseResult<()> {
        let parts: Vec<&str> = args.split_whitespace().collect();
        
        if parts.is_empty() {
            bot.send_message(chat_id, "❌ Usage: `/sub author <id>` or `/sub ranking <mode>`")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        match parts[0] {
            "author" => self.handle_sub_author(bot, chat_id, user_id, &parts[1..]).await,
            "ranking" => self.handle_sub_ranking(bot, chat_id, user_id, &parts[1..]).await,
            _ => {
                bot.send_message(
                    chat_id,
                    "❌ Unknown subscription type. Use `author` or `ranking`"
                )
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
                Ok(())
            }
        }
    }

    async fn handle_sub_author(
        &self,
        bot: Bot,
        chat_id: ChatId,
        user_id: i64,
        parts: &[&str],
    ) -> ResponseResult<()> {
        if parts.is_empty() {
            bot.send_message(chat_id, "❌ Usage: `/sub author <pixiv_id> [+tag1 -tag2]`")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        let author_id = parts[0];
        
        // Validate it's a number
        if author_id.parse::<u64>().is_err() {
            bot.send_message(chat_id, "❌ Author ID must be a number")
                .await?;
            return Ok(());
        }

        // Parse filter tags
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

        // Create or get task
        match self.repo.get_or_create_task(
            "author".to_string(),
            author_id.to_string(),
            4 * 3600, // 4 hours interval
            user_id,
        ).await {
            Ok(task) => {
                // Create subscription
                match self.repo.upsert_subscription(
                    chat_id.0,
                    task.id,
                    filter_tags.clone(),
                ).await {
                    Ok(_) => {
                        let filter_msg = if let Some(tags) = filter_tags {
                            format!(
                                "\n🏷 Filters: Include: {:?}, Exclude: {:?}",
                                tags.get("include"),
                                tags.get("exclude")
                            )
                        } else {
                            String::new()
                        };
                        
                        bot.send_message(
                            chat_id,
                            format!("✅ Successfully subscribed to author `{}`{}", author_id, filter_msg)
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

    async fn handle_sub_ranking(
        &self,
        bot: Bot,
        chat_id: ChatId,
        user_id: i64,
        parts: &[&str],
    ) -> ResponseResult<()> {
        if parts.is_empty() {
            bot.send_message(
                chat_id,
                "❌ Usage: `/sub ranking <mode>`\nModes: daily, weekly, monthly, daily_r18, etc."
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
            return Ok(());
        }

        let mode = parts[0];
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
            24 * 3600, // 24 hours interval
            user_id,
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
                            format!("✅ Successfully subscribed to `{}` ranking", mode)
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

    async fn handle_unsub(
        &self,
        bot: Bot,
        chat_id: ChatId,
        args: String,
    ) -> ResponseResult<()> {
        let sub_id = match args.trim().parse::<i32>() {
            Ok(id) => id,
            Err(_) => {
                bot.send_message(chat_id, "❌ Invalid subscription ID. Use `/list` to see IDs.")
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
                return Ok(());
            }
        };

        match self.repo.delete_subscription(sub_id).await {
            Ok(_) => {
                // Check if task still has subscriptions
                // (This is automatically handled by the database cascade, but we could add cleanup)
                bot.send_message(chat_id, "✅ Successfully unsubscribed")
                    .await?;
            }
            Err(e) => {
                error!("Failed to delete subscription: {}", e);
                bot.send_message(chat_id, "❌ Failed to unsubscribe. Invalid ID?")
                    .await?;
            }
        }

        Ok(())
    }

    async fn handle_list(&self, bot: Bot, chat_id: ChatId) -> ResponseResult<()> {
        match self.repo.list_subscriptions_by_chat(chat_id.0).await {
            Ok(subscriptions) => {
                if subscriptions.is_empty() {
                    bot.send_message(chat_id, "📭 You have no active subscriptions.\n\nUse `/sub` to subscribe!")
                        .parse_mode(ParseMode::MarkdownV2)
                        .await?;
                    return Ok(());
                }

                let mut message = "📋 **Your Subscriptions:**\n\n".to_string();
                
                for (sub, task) in subscriptions {
                    let type_emoji = match task.r#type.as_str() {
                        "author" => "👤",
                        "ranking" => "📊",
                        _ => "❓",
                    };
                    
                    let filter_info = if let Some(tags) = &sub.filter_tags {
                        if let Ok(filter) = serde_json::from_value::<serde_json::Value>(tags.clone()) {
                            let include = filter.get("include")
                                .and_then(|v| v.as_array())
                                .map(|arr| arr.iter()
                                    .filter_map(|v| v.as_str())
                                    .map(|s| format!("+{}", s))
                                    .collect::<Vec<_>>()
                                    .join(" "))
                                .unwrap_or_default();
                            
                            let exclude = filter.get("exclude")
                                .and_then(|v| v.as_array())
                                .map(|arr| arr.iter()
                                    .filter_map(|v| v.as_str())
                                    .map(|s| format!("-{}", s))
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
                                format!(" 🏷 `{}`", filters.join(" "))
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
                        "{} **[{}]** {} `{}`{}\n",
                        type_emoji,
                        sub.id,
                        task.r#type,
                        task.value,
                        filter_info
                    ));
                }

                message.push_str("\n💡 Use `/unsub <id>` to unsubscribe");

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
}
