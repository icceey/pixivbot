pub mod commands;
pub mod notifier;

use teloxide::prelude::*;
use crate::config::TelegramConfig;
use crate::error::AppResult;

pub async fn run(config: TelegramConfig) -> AppResult<()> {
    let bot = Bot::new(config.bot_token);
    
    // Placeholder for bot dispatching
    // teloxide::repl(bot, |bot: Bot, msg: Message| async move {
    //     bot.send_message(msg.chat.id, "Hello World!").await?;
    //     Ok(())
    // }).await;
    
    Ok(())
}
