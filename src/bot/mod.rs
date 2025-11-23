pub mod commands;
pub mod notifier;
mod handler;

use teloxide::prelude::*;
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
    
    let bot = Bot::new(config.bot_token.clone());
    let handler = BotHandler::new(bot.clone(), repo, pixiv_client, config.owner_id);
    
    info!("✅ Bot initialized, starting command handler");
    
    Command::repl(bot, move |bot: Bot, msg: Message, cmd: Command| {
        let handler = handler.clone();
        async move {
            handler.handle_command(bot, msg, cmd).await
        }
    })
    .await;
    
    Ok(())
}
