use teloxide::prelude::*;
use teloxide::types::ParseMode;
use crate::error::AppResult;
use tracing::{info, warn};

pub struct Notifier {
    bot: Bot,
}

impl Notifier {
    pub fn new(bot: Bot) -> Self {
        Self { bot }
    }

    pub async fn notify(&self, chat_id: ChatId, message: &str) -> AppResult<()> {
        info!("Sending notification to chat {}", chat_id);
        
        match self.bot
            .send_message(chat_id, message)
            .parse_mode(ParseMode::Markdown)
            .await
        {
            Ok(_) => {
                info!("✅ Notification sent successfully");
                Ok(())
            }
            Err(e) => {
                warn!("Failed to send notification to {}: {}", chat_id, e);
                Err(crate::error::AppError::Telegram(e.to_string()))
            }
        }
    }
    
    pub async fn notify_plain(&self, chat_id: ChatId, message: &str) -> AppResult<()> {
        info!("Sending plain notification to chat {}", chat_id);
        
        match self.bot.send_message(chat_id, message).await {
            Ok(_) => {
                info!("✅ Notification sent successfully");
                Ok(())
            }
            Err(e) => {
                warn!("Failed to send notification to {}: {}", chat_id, e);
                Err(crate::error::AppError::Telegram(e.to_string()))
            }
        }
    }
}
