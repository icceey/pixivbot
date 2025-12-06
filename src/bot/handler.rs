use crate::bot::link_handler::{is_bot_mentioned, parse_pixiv_links, PixivLink};
use crate::bot::notifier::Notifier;
use crate::bot::Command;
use crate::db::repo::Repo;
use crate::db::types::{TagFilter, Tags, TaskType, UserRole};
use crate::pixiv::client::PixivClient;
use crate::utils::tag;
use anyhow::{Context, Result};
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{Me, ParseMode};
use teloxide::utils::markdown;
use tracing::{error, info};

// ============================================================================
// BotHandler - Core Handler Structure
// ============================================================================

#[derive(Clone)]
pub struct BotHandler {
    pub(crate) repo: Arc<Repo>,
    pub(crate) pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
    pub(crate) notifier: Notifier,
    pub(crate) default_sensitive_tags: Vec<String>,
    pub(crate) owner_id: Option<i64>,
    pub(crate) is_public_mode: bool,
}

impl BotHandler {
    // ------------------------------------------------------------------------
    // Constructor
    // ------------------------------------------------------------------------

    pub fn new(
        repo: Arc<Repo>,
        pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
        notifier: Notifier,
        default_sensitive_tags: Vec<String>,
        owner_id: Option<i64>,
        is_public_mode: bool,
    ) -> Self {
        Self {
            repo,
            pixiv_client,
            notifier,
            default_sensitive_tags,
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
                error!("Failed to ensure user/chat: {:#}", e);
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
            // Help and Info commands (defined in handlers/info.rs)
            Command::Help => self.handle_help(bot, chat_id).await,
            Command::Info if user_role.is_admin() && chat_id.is_user() => {
                self.handle_info(bot, chat_id).await
            }

            // Subscription commands (defined in handlers/subscription.rs)
            Command::Sub(args) => self.handle_sub_author(bot, chat_id, args).await,
            Command::SubRank(args) => self.handle_sub_ranking(bot, chat_id, args).await,
            Command::Unsub(args) => self.handle_unsub_author(bot, chat_id, args).await,
            Command::UnsubRank(args) => self.handle_unsub_ranking(bot, chat_id, args).await,
            Command::List => self.handle_list(bot, chat_id).await,

            // Chat settings commands (defined in handlers/settings.rs)
            Command::BlurSensitive(args) => self.handle_blur_sensitive(bot, chat_id, args).await,
            Command::SensitiveTags(args) => self.handle_sensitive_tags(bot, chat_id, args).await,
            Command::ClearSensitiveTags => self.handle_clear_sensitive_tags(bot, chat_id).await,
            Command::ExcludeTags(args) => self.handle_exclude_tags(bot, chat_id, args).await,
            Command::ClearExcludedTags => self.handle_clear_excluded_tags(bot, chat_id).await,
            Command::Settings => self.handle_settings(bot, chat_id).await,

            // Admin commands (require admin or owner role, defined in handlers/admin.rs)
            Command::EnableChat(args) if user_role.is_admin() => {
                self.handle_enable_chat(bot, chat_id, args, true).await
            }
            Command::DisableChat(args) if user_role.is_admin() => {
                self.handle_enable_chat(bot, chat_id, args, false).await
            }

            // Owner commands (require owner role, defined in handlers/admin.rs)
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

    async fn ensure_user_and_chat(&self, msg: &Message) -> Result<(UserRole, bool)> {
        let chat_id = msg.chat.id.0;
        let chat_type = match msg.chat.is_group() || msg.chat.is_supergroup() {
            true => "group",
            false => "private",
        };
        let chat_title = msg.chat.title().map(|s| s.to_string());

        // Convert default sensitive tags to Tags for new chats
        let default_sensitive_tags = Tags::from(self.default_sensitive_tags.clone());

        // Upsert chat - new chats get enabled status based on bot mode
        let chat = self
            .repo
            .upsert_chat(
                chat_id,
                chat_type.to_string(),
                chat_title,
                self.is_public_mode,
                default_sensitive_tags,
            )
            .await
            .context("Failed to upsert chat")?;

        if let Some(user) = msg.from.as_ref() {
            let user_id = user.id.0 as i64;
            let username = user.username.clone();

            // Check if user already exists
            let user_model = match self
                .repo
                .get_user(user_id)
                .await
                .context("Failed to get user")?
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
                        .context("Failed to upsert user")?
                }
            };

            return Ok((user_model.role, chat.enabled));
        }

        // If no user info, return default user with chat enabled status
        Ok((UserRole::User, chat.enabled))
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
                error!("Failed to ensure user/chat: {:#}", e);
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

        // 处理每个链接
        for link in links {
            match link {
                PixivLink::Illust(illust_id) => {
                    self.handle_illust_link(
                        bot.clone(),
                        chat_id,
                        illust_id,
                        chat_settings.as_ref(),
                    )
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
        chat_settings: Option<&crate::db::entities::chats::Model>,
    ) -> ResponseResult<()> {
        info!("Fetching illust {} for chat {}", illust_id, chat_id);

        // 获取作品详情
        let pixiv = self.pixiv_client.read().await;
        let illust = match pixiv.get_illust_detail(illust_id).await {
            Ok(illust) => illust,
            Err(e) => {
                error!("Failed to get illust {}: {:#}", illust_id, e);
                bot.send_message(chat_id, format!("❌ 获取作品 {} 失败: {:#}", illust_id, e))
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

        let tags = tag::format_tags_escaped(&illust);

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

        // 检查是否有敏感标签 (使用 chat-level 设置)
        use crate::utils::sensitive;
        let blur_sensitive = chat_settings
            .map(|c| c.blur_sensitive_tags)
            .unwrap_or(false);
        let sensitive_tags = chat_settings
            .map(sensitive::get_chat_sensitive_tags)
            .unwrap_or_default();
        let has_spoiler =
            blur_sensitive && sensitive::contains_sensitive_tags(&illust, sensitive_tags);

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
                error!("Failed to get user {}: {:#}", user_id, e);
                bot.send_message(chat_id, format!("❌ 获取用户 {} 失败: {:#}", user_id, e))
                    .await?;
                return Ok(());
            }
        };
        drop(pixiv);

        // 创建或获取任务
        match self
            .repo
            .get_or_create_task(
                TaskType::Author,
                user_id.to_string(),
                Some(author.name.clone()),
            )
            .await
        {
            Ok(task) => {
                // 创建订阅
                match self
                    .repo
                    .upsert_subscription(chat_id.0, task.id, TagFilter::default())
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
                        error!("Failed to create subscription for {}: {:#}", user_id, e);
                        bot.send_message(chat_id, "❌ 创建订阅失败").await?;
                    }
                }
            }
            Err(e) => {
                error!("Failed to create task for {}: {:#}", user_id, e);
                bot.send_message(chat_id, "❌ 创建任务失败").await?;
            }
        }

        Ok(())
    }
}
