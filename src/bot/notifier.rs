use teloxide::prelude::*;
use teloxide::types::{ParseMode, InputFile};
use crate::error::AppResult;
use crate::pixiv::downloader::Downloader;
use tracing::{info, warn};
use std::sync::Arc;
use std::path::PathBuf;

pub struct Notifier {
    bot: Bot,
    downloader: Arc<Downloader>,
}

impl Notifier {
    pub fn new(bot: Bot, downloader: Arc<Downloader>) -> Self {
        Self { bot, downloader }
    }

    /// Send text notification with Markdown formatting
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
    
    /// Send plain text notification without formatting
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
    
    /// Download image and send as photo with caption
    pub async fn notify_with_image(
        &self,
        chat_id: ChatId,
        image_url: &str,
        caption: Option<&str>,
    ) -> AppResult<()> {
        info!("Downloading and sending image to chat {}: {}", chat_id, image_url);
        
        // Download the image first
        match self.downloader.download(image_url).await {
            Ok(local_path) => {
                self.send_photo_file(chat_id, local_path, caption).await
            }
            Err(e) => {
                warn!("Failed to download image {}: {}, falling back to text", image_url, e);
                // Fallback: send text message with URL
                let fallback_message = if let Some(cap) = caption {
                    format!("{}\n🔗 {}", cap, image_url)
                } else {
                    format!("🔗 {}", image_url)
                };
                self.notify_plain(chat_id, &fallback_message).await
            }
        }
    }
    
    /// Send a photo from local file path
    async fn send_photo_file(
        &self,
        chat_id: ChatId,
        file_path: PathBuf,
        caption: Option<&str>,
    ) -> AppResult<()> {
        info!("Sending photo from {:?} to chat {}", file_path, chat_id);
        
        let input_file = InputFile::file(&file_path);
        let mut request = self.bot.send_photo(chat_id, input_file);
        
        if let Some(cap) = caption {
            request = request.caption(cap);
        }
        
        match request.await {
            Ok(_) => {
                info!("✅ Photo sent successfully");
                Ok(())
            }
            Err(e) => {
                warn!("Failed to send photo to {}: {}", chat_id, e);
                Err(crate::error::AppError::Telegram(e.to_string()))
            }
        }
    }
}
