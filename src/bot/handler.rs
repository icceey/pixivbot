use crate::booru::BooruSiteRegistry;
use crate::bot::link_handler::{parse_pixiv_links, PixivLink};
use crate::bot::notifier::{DownloadButtonConfig, Notifier, ThrottledBot};
use crate::bot::Command;
use crate::config::EhentaiConfig;
use crate::db::repo::Repo;
use crate::db::types::{TagFilter, TaskType, UserRole};
use crate::pixiv::client::PixivClient;
use crate::utils::caption;
use booru_client::PopularScale;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use teloxide::utils::markdown;
use tracing::{error, info};

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
    /// 缓存目录路径 (用于查看磁盘占用)
    pub(crate) cache_dir: String,
    /// 日志目录路径 (用于查看磁盘占用)
    pub(crate) log_dir: String,
    pub(crate) booru_registry: Arc<BooruSiteRegistry>,
    pub(crate) eh_client: Option<Arc<eh_client::EhClient>>,
    pub(crate) eh_config: Arc<EhentaiConfig>,
    pub(crate) has_telegraph: bool,
}

impl BotHandler {
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
        booru_registry: Arc<BooruSiteRegistry>,
        eh_client: Option<Arc<eh_client::EhClient>>,
        eh_config: Arc<EhentaiConfig>,
        has_telegraph: bool,
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
            booru_registry,
            eh_client,
            eh_config,
            has_telegraph,
        }
    }

    pub async fn handle_command(
        &self,
        bot: ThrottledBot,
        msg: Message,
        cmd: Command,
        ctx: crate::bot::UserChatContext,
    ) -> ResponseResult<()> {
        if super::permissions::message_actor(&msg).is_none() {
            return Ok(());
        }
        let chat_id = msg.chat.id;
        let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);

        info!(
            "Received command from user {} in chat {}: {:?}",
            user_id, chat_id, cmd
        );

        self.dispatch_command(bot, msg, chat_id, cmd, &ctx.user.role)
            .await
    }

    async fn dispatch_command(
        &self,
        bot: ThrottledBot,
        msg: Message,
        chat_id: ChatId,
        cmd: Command,
        user_role: &UserRole,
    ) -> ResponseResult<()> {
        let user_id = msg.from.as_ref().map(|u| u.id);

        match cmd {
            Command::Help => self.handle_help(bot, chat_id).await,
            Command::Info => self.handle_info(bot, chat_id).await,

            Command::Sub(args) => self.handle_sub_author(bot, chat_id, user_id, args).await,
            Command::SubRank(args) => self.handle_sub_ranking(bot, chat_id, user_id, args).await,
            Command::SubTag(args) => self.handle_sub_tag(bot, chat_id, user_id, args).await,
            Command::UnsubTag(args) => self.handle_unsub_tag(bot, chat_id, user_id, args).await,
            Command::Unsub(args) => self.handle_unsub_author(bot, chat_id, user_id, args).await,
            Command::UnsubRank(args) => {
                self.handle_unsub_ranking(bot, chat_id, user_id, args).await
            }
            Command::UnsubThis => self.handle_unsub_this(bot, msg, chat_id).await,
            Command::List(args) => self.handle_list(bot, chat_id, user_id, args).await,

            Command::Settings => self.handle_settings(bot, chat_id).await,

            // Cancel command - handled via dialogue state, no-op here
            Command::Cancel => Ok(()),

            Command::Download(args) => self.handle_download(bot.clone(), msg, chat_id, args).await,

            Command::BSub(args) => self.handle_bsub(bot, chat_id, user_id, args).await,
            Command::BUnsub(args) => self.handle_bunsub(bot, chat_id, user_id, args).await,
            Command::BRank(args) => {
                self.handle_brank(bot, msg.chat.id, user_id, args, None)
                    .await
            }
            Command::BRankDay(args) => {
                self.handle_brank(bot, msg.chat.id, user_id, args, Some(PopularScale::Day))
                    .await
            }
            Command::BRankWeek(args) => {
                self.handle_brank(bot, msg.chat.id, user_id, args, Some(PopularScale::Week))
                    .await
            }
            Command::BRankMonth(args) => {
                self.handle_brank(bot, msg.chat.id, user_id, args, Some(PopularScale::Month))
                    .await
            }
            Command::BRand(args) => self.handle_brand(bot, msg.chat.id, user_id, args).await,

            Command::ESub(args) => self.handle_esub(bot, chat_id, user_id, args).await,
            Command::EUnsub(args) => self.handle_eunsub(bot, chat_id, user_id, args).await,
            Command::EDl(args) => self.handle_edl(bot, msg, chat_id, args).await,
            Command::EStatus {} => self.handle_estatus(bot, chat_id).await,
            Command::Telegraph(args) => self.handle_telegraph(bot, msg, chat_id, args).await,

            // Chat control checks global or current-group privileges at the operation entrypoint.
            Command::EnableChat(args) => self.handle_enable_chat(bot, &msg, args, true).await,
            Command::DisableChat(args) => self.handle_enable_chat(bot, &msg, args, false).await,

            Command::SetAdmin(args) if user_role.is_owner() => {
                self.handle_set_admin(bot, &msg, args, true).await
            }
            Command::UnsetAdmin(args) if user_role.is_owner() => {
                self.handle_set_admin(bot, &msg, args, false).await
            }

            // Silently ignore unauthorized commands
            _ => Ok(()),
        }
    }

    /// 处理普通消息（检查 Pixiv 链接）
    ///
    /// - 作品链接 (https://www.pixiv.net/artworks/xxx): 一次性推送作品
    /// - 作者链接 (https://www.pixiv.net/users/xxx): 订阅作者
    ///
    pub async fn handle_message(
        &self,
        bot: ThrottledBot,
        msg: Message,
        text: &str,
        ctx: crate::bot::UserChatContext,
    ) -> ResponseResult<()> {
        let links = parse_pixiv_links(text);
        if links.is_empty() {
            return Ok(());
        }

        let chat_id = msg.chat.id;
        let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);

        info!(
            "Processing Pixiv links from user {} in chat {}: {:?}",
            user_id, chat_id, links
        );

        let chat_settings = &ctx.chat;

        for link in links {
            match link {
                PixivLink::Illust(illust_id) => {
                    self.handle_illust_link(bot.clone(), chat_id, illust_id, chat_settings)
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
        chat_settings: &crate::db::entities::chats::Model,
    ) -> ResponseResult<()> {
        info!("Fetching illust {} for chat {}", illust_id, chat_id);

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

        let caption = if illust.is_ugoira() {
            caption::build_ugoira_caption(&illust)
        } else {
            caption::build_illust_caption(&illust)
        };

        let has_spoiler = crate::utils::sensitive::should_blur(chat_settings, &illust);
        let download_config = DownloadButtonConfig::for_pixiv_chat(illust.id, chat_settings);

        if illust.is_ugoira() {
            let pixiv = self.pixiv_client.read().await;
            let metadata_result = pixiv.get_ugoira_metadata(illust.id).await;
            drop(pixiv);

            let metadata = match metadata_result {
                Ok(metadata) => metadata,
                Err(e) => {
                    error!(
                        "Failed to get ugoira metadata for illust {}: {:#}",
                        illust.id, e
                    );
                    bot.send_message(chat_id, "❌ 获取动图信息失败").await?;
                    return Ok(());
                }
            };

            let _ = self
                .notifier
                .notify_ugoira(
                    chat_id,
                    &metadata.zip_urls.medium,
                    metadata.frames,
                    Some(&caption),
                    has_spoiler,
                    &download_config,
                )
                .await;

            return Ok(());
        }

        let image_urls = illust.get_all_image_urls_with_size(self.image_size);

        let _ = self
            .notifier
            .notify_with_images(
                chat_id,
                &image_urls,
                Some(&caption),
                has_spoiler,
                &download_config,
                None,
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
