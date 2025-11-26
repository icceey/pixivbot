pub mod commands;
pub mod notifier;
mod handler;

use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;
use crate::config::TelegramConfig;
use crate::error::AppResult;
use crate::db::repo::Repo;
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
    let handler = BotHandler::new(bot.clone(), repo, pixiv_client, config.owner_id, is_public_mode);
    
    info!("✅ Bot initialized, starting command handler");

    // TODO 临时设置所有命令可见
    let cmds = Command::bot_commands();
    let r = bot.set_my_commands(cmds);
    if let Err(e) = r.await {
        tracing::warn!("Failed to set bot commands: {}", e);
    }

    Command::repl(bot, move |bot: Bot, msg: Message, cmd: Command| {
        let handler = handler.clone();
        async move {
            handler.handle_command(bot, msg, cmd).await
        }
    })
    .await;
    
    Ok(())
}
