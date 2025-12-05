pub mod commands;
mod handler;
pub mod link_handler;
pub mod notifier;

use crate::config::TelegramConfig;
use crate::db::repo::Repo;
use crate::db::types::UserRole;
use crate::pixiv::client::PixivClient;
use anyhow::Result;
use std::sync::Arc;
use teloxide::dispatching::{Dispatcher, UpdateFilterExt};
use teloxide::dptree;
use teloxide::prelude::*;
use teloxide::types::{BotCommandScope, Me};
use tracing::info;

pub use commands::Command;
pub use handler::BotHandler;

/// Handler 返回类型
type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

pub async fn run(
    bot: Bot,
    config: TelegramConfig,
    repo: Arc<Repo>,
    pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
    notifier: notifier::Notifier,
    sensitive_tags: Vec<String>,
) -> Result<()> {
    info!("Starting Telegram Bot...");

    // Parse bot mode from config
    let is_public_mode = config.bot_mode.to_lowercase() == "public";

    info!(
        "Bot mode: {} (new chats will be {} by default)",
        config.bot_mode,
        if is_public_mode {
            "enabled"
        } else {
            "disabled"
        }
    );

    let handler = BotHandler::new(
        repo.clone(),
        pixiv_client.clone(),
        notifier,
        sensitive_tags,
        config.owner_id,
        is_public_mode,
    );

    info!("✅ Bot initialized, starting command handler");

    // 设置命令可见性
    setup_commands(&bot, &repo).await;

    // 构建 handler 树
    let handler_tree = build_handler_tree();

    // 使用 Dispatcher
    Dispatcher::builder(bot, handler_tree)
        .dependencies(dptree::deps![handler])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

/// 构建消息处理树
fn build_handler_tree(
) -> teloxide::dispatching::UpdateHandler<Box<dyn std::error::Error + Send + Sync + 'static>> {
    use teloxide::dispatching::HandlerExt;

    // 使用 filter_mention_command: 在群组中需要 @bot 才能触发命令
    let command_handler = Update::filter_message()
        .filter_command::<Command>()
        .endpoint(handle_command);

    let message_handler = Update::filter_message().endpoint(handle_message);

    dptree::entry()
        .branch(command_handler)
        .branch(message_handler)
}

/// 处理命令
async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    handler: BotHandler,
) -> HandlerResult {
    handler.handle_command(bot, msg, cmd).await?;
    Ok(())
}

/// 处理普通消息（检查 Pixiv 链接）
async fn handle_message(bot: Bot, msg: Message, me: Me, handler: BotHandler) -> HandlerResult {
    handler.handle_message(bot, msg, me).await?;
    Ok(())
}

/// 设置命令可见性
/// - 普通用户看到基础命令
/// - 数据库中的 Admin 用户看到管理员命令
/// - 数据库中的 Owner 用户看到所有命令
async fn setup_commands(bot: &Bot, repo: &Repo) {
    // 1. 设置默认命令（所有用户都能看到的基础命令）
    if let Err(e) = bot
        .set_my_commands(Command::user_commands())
        .scope(BotCommandScope::Default)
        .await
    {
        tracing::warn!("Failed to set default commands: {}", e);
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
                        "Failed to set commands for {:?} {}: {}",
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
            tracing::warn!("Failed to get admin users from database: {}", e);
        }
    }
}
