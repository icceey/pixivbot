pub mod commands;
pub mod notifier;
mod handler;

use teloxide::prelude::*;
use teloxide::types::BotCommandScope;
use crate::config::TelegramConfig;
use crate::error::AppResult;
use crate::db::repo::Repo;
use crate::db::entities::role::UserRole;
use crate::pixiv::client::PixivClient;
use crate::pixiv::downloader::Downloader;
use std::sync::Arc;
use tracing::info;

pub use commands::Command;
pub use handler::BotHandler;

pub async fn run(
    config: TelegramConfig,
    repo: Arc<Repo>,
    pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
    _downloader: Arc<Downloader>, // Not used in bot commands, only in scheduler
) -> AppResult<()> {
    info!("Starting Telegram Bot...");
    
    // Parse bot mode from config
    let is_public_mode = config.bot_mode.to_lowercase() == "public";
    
    info!("Bot mode: {} (new chats will be {} by default)", 
        config.bot_mode, 
        if is_public_mode { "enabled" } else { "disabled" }
    );
    
    let bot = Bot::new(config.bot_token.clone());
    let handler = BotHandler::new(bot.clone(), repo.clone(), pixiv_client, config.owner_id, is_public_mode);
    
    info!("✅ Bot initialized, starting command handler");

    // 设置命令可见性
    setup_commands(&bot, &repo).await;

    Command::repl(bot, move |bot: Bot, msg: Message, cmd: Command| {
        let handler = handler.clone();
        async move {
            handler.handle_command(bot, msg, cmd).await
        }
    })
    .await;
    
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
                    tracing::warn!("Failed to set commands for {:?} {}: {}", user.role, user.id, e);
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
