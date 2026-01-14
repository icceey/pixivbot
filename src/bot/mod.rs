pub mod commands;
mod handler;
mod handlers;
pub mod link_handler;
pub mod middleware;
pub mod notifier;

use crate::config::TelegramConfig;
use crate::db::repo::Repo;
use crate::db::types::UserRole;
use crate::pixiv::client::PixivClient;
use anyhow::Result;
use handlers::{DOWNLOAD_CALLBACK_PREFIX, LIST_CALLBACK_PREFIX};
use notifier::ThrottledBot;
use std::sync::Arc;
use teloxide::dispatching::{Dispatcher, UpdateFilterExt};
use teloxide::dptree;
use teloxide::prelude::*;
use teloxide::types::BotCommandScope;
use tracing::{error, info, warn};

pub use commands::Command;
pub use handler::BotHandler;
pub use middleware::UserChatContext;

/// Handler 返回类型
type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[allow(clippy::too_many_arguments)]
pub async fn run(
    bot: ThrottledBot,
    config: TelegramConfig,
    repo: Arc<Repo>,
    pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
    notifier: notifier::Notifier,
    sensitive_tags: Vec<String>,
    image_size: pixiv_client::ImageSize,
    download_original_threshold: u8,
    cache_dir: String,
    log_dir: String,
) -> Result<()> {
    info!("Starting Telegram Bot...");

    // Parse bot mode from config
    let is_public_mode = config.bot_mode.is_public();

    info!(
        "Bot mode: {:?} (new chats will be {} by default)",
        config.bot_mode,
        if is_public_mode {
            "enabled"
        } else {
            "disabled"
        }
    );

    info!(
        "Require mention in group: {}",
        config.require_mention_in_group
    );

    let handler = BotHandler::new(
        repo.clone(),
        pixiv_client.clone(),
        notifier.clone(),
        sensitive_tags,
        config.owner_id,
        is_public_mode,
        image_size,
        download_original_threshold,
        config.require_mention_in_group,
        cache_dir,
        log_dir,
    );

    info!("✅ Bot initialized, starting command handler");

    // 设置命令可见性
    setup_commands(&bot, &repo).await;

    // 构建 handler 树
    let handler_tree = build_handler_tree();

    // 使用 Dispatcher
    Dispatcher::builder(bot, handler_tree)
        .dependencies(dptree::deps![handler, repo, notifier])
        .default_handler(|_| async {})
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

/// 构建消息处理树
fn build_handler_tree(
) -> teloxide::dispatching::UpdateHandler<Box<dyn std::error::Error + Send + Sync + 'static>> {
    // 管理员启用/禁用聊天命令 - 只检查用户权限，不检查聊天是否启用
    // 这允许管理员在禁用的聊天中使用 /enablechat 命令
    // 注意：此处的 is_admin() 检查与 handler 中 dispatch_command 的 pattern guard 是有意重复的（纵深防御）
    let admin_chat_control_handler = Message::filter_text()
        .chain(middleware::filter_hybrid_command::<Command, HandlerResult>())
        .chain(middleware::filter_user_chat())
        .filter(|cmd: Command, ctx: UserChatContext| {
            // 仅当用户是管理员且命令是 EnableChat 或 DisableChat 时处理
            ctx.user_role().is_admin()
                && matches!(cmd, Command::EnableChat(_) | Command::DisableChat(_))
        })
        .endpoint(handle_command);

    // 常规命令 - 保持原有的聊天可访问性检查
    let command_handler = Message::filter_text()
        .chain(middleware::filter_hybrid_command::<Command, HandlerResult>())
        .chain(middleware::filter_user_chat())
        .chain(middleware::filter_chat_accessible())
        .endpoint(handle_command);

    let message_handler = Message::filter_text()
        .chain(middleware::filter_relevant_message::<HandlerResult>())
        .chain(middleware::filter_user_chat())
        .chain(middleware::filter_chat_accessible())
        .endpoint(handle_message);

    // Callback query handler for pagination
    let callback_handler = Update::filter_callback_query()
        .filter_map(|q: CallbackQuery| {
            // Filter for list pagination callbacks
            q.data
                .as_ref()
                .filter(|data| data.starts_with(LIST_CALLBACK_PREFIX))
                .cloned()
        })
        .endpoint(handle_list_callback);

    // Callback query handler for download button
    let download_callback_handler = Update::filter_callback_query()
        .filter_map(|q: CallbackQuery| {
            // Filter for download callbacks
            q.data
                .as_ref()
                .filter(|data| data.starts_with(DOWNLOAD_CALLBACK_PREFIX))
                .cloned()
        })
        .endpoint(handle_download_callback);

    // Chat migration handler - handles group to supergroup upgrades
    let migration_handler = dptree::filter(|msg: Message| {
        // Only process migration messages
        msg.migrate_to_chat_id().is_some() || msg.migrate_from_chat_id().is_some()
    })
    .endpoint(handle_chat_migration);

    dptree::entry()
        .branch(callback_handler)
        .branch(download_callback_handler)
        .branch(
            Update::filter_message()
                .branch(migration_handler)
                .branch(admin_chat_control_handler)
                .branch(command_handler)
                .branch(message_handler),
        )
}

/// 处理命令
async fn handle_command(
    bot: ThrottledBot,
    msg: Message,
    cmd: Command,
    handler: BotHandler,
    ctx: UserChatContext,
) -> HandlerResult {
    handler.handle_command(bot, msg, cmd, ctx).await?;
    Ok(())
}

/// 处理普通消息（检查 Pixiv 链接）
async fn handle_message(
    bot: ThrottledBot,
    msg: Message,
    handler: BotHandler,
    text: String,
    ctx: UserChatContext,
) -> HandlerResult {
    handler.handle_message(bot, msg, &text, ctx).await?;
    Ok(())
}

/// 处理列表分页回调
async fn handle_list_callback(
    bot: ThrottledBot,
    q: CallbackQuery,
    callback_data: String,
    handler: BotHandler,
) -> HandlerResult {
    // Answer the callback to remove loading indicator
    if let Err(e) = bot.answer_callback_query(q.id.clone()).await {
        warn!("Failed to answer callback query: {:#}", e);
    }

    // Parse page number from callback data (format: "list:N" or "list:noop")
    let page_str = callback_data
        .strip_prefix(LIST_CALLBACK_PREFIX)
        .unwrap_or("0");

    // Handle noop (page indicator button)
    if page_str == "noop" {
        return Ok(());
    }

    let page: usize = page_str.parse().unwrap_or_else(|_| {
        warn!("Invalid page number in callback data: {}", page_str);
        0
    });

    // Get message and chat info from the callback query
    if let Some(msg) = q.message {
        let chat_id = msg.chat().id;
        let message_id = msg.id();

        // Update the subscription list message
        // For pagination callback, use same chat_id for reply and target (no channel support in pagination)
        handler
            .send_subscription_list(bot, chat_id, chat_id, page, Some(message_id), false)
            .await?;
    }

    Ok(())
}

/// 处理下载按钮回调
async fn handle_download_callback(
    bot: ThrottledBot,
    q: CallbackQuery,
    callback_data: String,
    handler: BotHandler,
) -> HandlerResult {
    // Answer the callback immediately to prevent timeout and handle concurrent clicks
    // Using show_alert=false and cache_time to reduce flickering on repeated clicks
    if let Err(e) = bot
        .answer_callback_query(q.id.clone())
        .cache_time(5) // Cache for 5 seconds to reduce duplicate processing
        .await
    {
        warn!("Failed to answer callback query: {:#}", e);
    }

    // Parse illust_id from callback data (format: "dl:12345678")
    let illust_id_str = callback_data
        .strip_prefix(DOWNLOAD_CALLBACK_PREFIX)
        .unwrap_or("0");

    let illust_id: u64 = match illust_id_str.parse() {
        Ok(id) => id,
        Err(_) => {
            warn!("Invalid illust_id in callback data: {}", illust_id_str);
            return Ok(());
        }
    };

    // Get chat_id from the callback query message
    let chat_id = match &q.message {
        Some(msg) => msg.chat().id,
        None => {
            warn!("No message found in download callback query");
            return Ok(());
        }
    };

    info!(
        "Download button clicked: illust_id={} chat_id={} user={:?}",
        illust_id, chat_id, q.from.id
    );

    // Process the download
    handler
        .handle_download_callback(bot, chat_id, illust_id)
        .await?;

    Ok(())
}

/// 处理聊天迁移（普通群组升级为超级群组）
async fn handle_chat_migration(msg: Message, repo: Arc<Repo>) -> HandlerResult {
    let chat_id = msg.chat.id;

    // Handle migrate_to_chat_id (old group → new supergroup)
    if let Some(new_chat_id) = msg.migrate_to_chat_id() {
        info!(
            "Chat migration detected: old chat {} → new supergroup {}",
            chat_id, new_chat_id
        );

        // Migrate all data from old chat_id to new chat_id
        if let Err(e) = repo.migrate_chat(chat_id.0, new_chat_id.0).await {
            error!(
                "Failed to migrate chat {} to {}: {:#}",
                chat_id, new_chat_id, e
            );
        } else {
            info!(
                "✅ Successfully migrated chat data from {} to {}",
                chat_id, new_chat_id
            );
        }
    }

    // Handle migrate_from_chat_id (new supergroup ← old group)
    if let Some(old_chat_id) = msg.migrate_from_chat_id() {
        info!(
            "Supergroup {} created from old chat {}",
            chat_id, old_chat_id
        );
        // This is the notification from the new supergroup side
        // The actual migration is already handled by migrate_to_chat_id
        // We just log it for debugging purposes
    }

    Ok(())
}

/// 设置命令可见性
/// - 普通用户看到基础命令
/// - 数据库中的 Admin 用户看到管理员命令
/// - 数据库中的 Owner 用户看到所有命令
async fn setup_commands(bot: &ThrottledBot, repo: &Repo) {
    // 1. 设置默认命令（所有用户都能看到的基础命令）
    if let Err(e) = bot
        .set_my_commands(Command::user_commands())
        .scope(BotCommandScope::Default)
        .await
    {
        tracing::warn!("Failed to set default commands: {:#}", e);
    } else {
        info!("✅ Set default commands for all users");
    }

    // 2. 从数据库获取所有管理员用户，为他们设置对应的命令可见性
    match repo.get_admin_users().await {
        Ok(admin_users) => {
            for user in admin_users {
                let commands = match user.role {
                    UserRole::Owner => Command::owner_commands(),
                    UserRole::Admin => Command::admin_commands(),
                    UserRole::User => continue, // 不应该出现，但以防万一
                };

                if let Err(e) = bot
                    .set_my_commands(commands)
                    .scope(BotCommandScope::Chat {
                        chat_id: teloxide::types::Recipient::Id(teloxide::types::ChatId(user.id)),
                    })
                    .await
                {
                    tracing::warn!(
                        "Failed to set commands for {:?} {}: {:#}",
                        user.role,
                        user.id,
                        e
                    );
                } else {
                    info!("✅ Set {:?} commands for user_id: {}", user.role, user.id);
                }
            }
        }
        Err(e) => {
            tracing::warn!("Failed to get admin users from database: {:#}", e);
        }
    }
}
