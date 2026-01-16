use crate::bot::link_handler::{parse_pixiv_links, PixivLink};
use crate::bot::notifier::{DownloadButtonConfig, Notifier, ThrottledBot};
use crate::bot::Command;
use crate::db::repo::Repo;
use crate::db::types::{TagFilter, TaskType, UserRole};
use crate::pixiv::client::PixivClient;
use crate::utils::tag;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
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
    pub(crate) image_size: pixiv_client::ImageSize,
    /// 下载原图阈值 (1-10): 图片数量不超过此值时逐张发送原图
    pub(crate) download_original_threshold: u8,
    /// 群组中是否需要 @bot 才响应 (默认: true)
    pub(crate) require_mention_in_group: bool,
    /// 缓存目录路径 (用于管理员查看磁盘占用)
    pub(crate) cache_dir: String,
    /// 日志目录路径 (用于管理员查看磁盘占用)
    pub(crate) log_dir: String,
}

impl BotHandler {
    // ------------------------------------------------------------------------
    // Constructor
    // ------------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: Arc<Repo>,
        pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
        notifier: Notifier,
        default_sensitive_tags: Vec<String>,
        owner_id: Option<i64>,
        is_public_mode: bool,
        image_size: pixiv_client::ImageSize,
        download_original_threshold: u8,
        require_mention_in_group: bool,
        cache_dir: String,
        log_dir: String,
    ) -> Self {
        Self {
            repo,
            pixiv_client,
            notifier,
            default_sensitive_tags,
            owner_id,
            is_public_mode,
            image_size,
            download_original_threshold,
            require_mention_in_group,
            cache_dir,
            log_dir,
        }
    }

    // ------------------------------------------------------------------------
    // Command Entry Point
    // ------------------------------------------------------------------------

    pub async fn handle_command(
        &self,
        bot: ThrottledBot,
        msg: Message,
        cmd: Command,
        ctx: crate::bot::UserChatContext,
    ) -> ResponseResult<()> {
        let chat_id = msg.chat.id;
        let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);

        info!(
            "Received command from user {} in chat {}: {:?}",
            user_id, chat_id, cmd
        );

        // Route command to appropriate handler
        self.dispatch_command(bot, msg, chat_id, cmd, ctx.user_role())
            .await
    }

    /// Dispatch command to the appropriate handler
    async fn dispatch_command(
        &self,
        bot: ThrottledBot,
        msg: Message,
        chat_id: ChatId,
        cmd: Command,
        user_role: &UserRole,
    ) -> ResponseResult<()> {
        // Get user_id for subscription commands that may need it for channel validation
        let user_id = msg.from.as_ref().map(|u| u.id);

        match cmd {
            // Help and Info commands (defined in handlers/info.rs)
            Command::Help => self.handle_help(bot, chat_id).await,
            Command::Info if user_role.is_admin() && chat_id.is_user() => {
                self.handle_info(bot, chat_id).await
            }

            // Subscription commands (defined in handlers/subscription.rs)
            Command::Sub(args) => self.handle_sub_author(bot, chat_id, user_id, args).await,
            Command::SubRank(args) => self.handle_sub_ranking(bot, chat_id, user_id, args).await,
            Command::Unsub(args) => self.handle_unsub_author(bot, chat_id, user_id, args).await,
            Command::UnsubRank(args) => {
                self.handle_unsub_ranking(bot, chat_id, user_id, args).await
            }
            Command::UnsubThis => self.handle_unsub_this(bot, msg, chat_id).await,
            Command::List(args) => self.handle_list(bot, chat_id, user_id, args).await,

            // Chat settings command (defined in handlers/settings.rs)
            // Note: The actual settings panel is shown via handle_settings which uses inline keyboards
            // Callback queries for settings buttons are handled in the dispatcher
            Command::Settings => self.handle_settings(bot, chat_id).await,

            // Cancel command - handled via dialogue state, no-op here
            Command::Cancel => Ok(()),

            // Download command (defined in handlers/download.rs)
            Command::Download(args) => self.handle_download(bot.clone(), msg, chat_id, args).await,

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
    // Message Handler (for Pixiv links)
    // ------------------------------------------------------------------------

    /// 处理普通消息（检查 Pixiv 链接）
    ///
    /// - 作品链接 (https://www.pixiv.net/artworks/xxx): 一次性推送作品
    /// - 作者链接 (https://www.pixiv.net/users/xxx): 订阅作者
    ///
    /// 群组中只在被 @ 时响应
    pub async fn handle_message(
        &self,
        bot: ThrottledBot,
        msg: Message,
        text: &str,
        ctx: crate::bot::UserChatContext,
    ) -> ResponseResult<()> {
        // 检查是否包含 Pixiv 链接
        let links = parse_pixiv_links(text);
        if links.is_empty() {
            return Ok(()); // 没有链接，忽略
        }

        let chat_id = msg.chat.id;
        let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);

        info!(
            "Processing Pixiv links from user {} in chat {}: {:?}",
            user_id, chat_id, links
        );

        // 获取聊天设置（用于模糊敏感内容）
        let chat_settings = &ctx.chat;

        // 处理每个链接
        for link in links {
            match link {
                PixivLink::Illust(illust_id) => {
                    self.handle_illust_link(bot.clone(), chat_id, illust_id, Some(chat_settings))
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
        bot: ThrottledBot,
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
                bot.send_message(chat_id, format!("❌ 获取作品 {} 失败", illust_id))
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

        // 获取所有图片 URL (使用配置的尺寸)
        let image_urls = illust.get_all_image_urls_with_size(self.image_size);

        // Build download button config
        // For one-off pushes via link, check chat type to skip channels
        let is_channel = chat_settings.is_some_and(|c| c.r#type == "channel");
        let download_config = if is_channel {
            DownloadButtonConfig::new(illust.id).for_channel()
        } else {
            DownloadButtonConfig::new(illust.id)
        };

        // 发送图片
        let _ = self
            .notifier
            .notify_with_images_and_button(
                chat_id,
                &image_urls,
                Some(&caption),
                has_spoiler,
                &download_config,
            )
            .await;

        Ok(())
    }

    /// 处理用户链接 - 订阅作者
    async fn handle_user_link(
        &self,
        bot: ThrottledBot,
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
                bot.send_message(chat_id, format!("❌ 获取用户 {} 失败", user_id))
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
