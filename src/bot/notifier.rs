use teloxide::prelude::*;
use crate::error::AppResult;

pub struct Notifier {
    bot: Bot,
}

impl Notifier {
    pub fn new(bot: Bot) -> Self {
        Self { bot }
    }

    pub async fn notify(&self, chat_id: ChatId, message: &str) -> AppResult<()> {
        // Placeholder notification
        // self.bot.send_message(chat_id, message).await?;
        Ok(())
    }
}
