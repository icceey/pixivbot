use crate::error::AppResult;
use crate::pixiv::downloader::Downloader;
use std::path::PathBuf;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{InputFile, InputMedia, InputMediaPhoto, ParseMode};
use tracing::{info, warn};

pub struct Notifier {
    bot: Bot,
    downloader: Arc<Downloader>,
}

impl Notifier {
    pub fn new(bot: Bot, downloader: Arc<Downloader>) -> Self {
        Self { bot, downloader }
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
        has_spoiler: bool,
    ) -> AppResult<()> {
        info!(
            "Downloading and sending image to chat {}: {} (spoiler: {})",
            chat_id, image_url, has_spoiler
        );

        // Download the image first
        match self.downloader.download(image_url).await {
            Ok(local_path) => {
                self.send_photo_file(chat_id, local_path, caption, has_spoiler)
                    .await
            }
            Err(e) => {
                warn!(
                    "Failed to download image {}: {}, falling back to text",
                    image_url, e
                );
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

    /// 下载并发送多张图片 (媒体组)
    /// 超过10张时自动分批发送多条消息 (Telegram 单条限制10张)
    pub async fn notify_with_images(
        &self,
        chat_id: ChatId,
        image_urls: &[String],
        caption: Option<&str>,
        has_spoiler: bool,
    ) -> AppResult<()> {
        if image_urls.is_empty() {
            return Err(crate::error::AppError::Unknown(
                "No images to send".to_string(),
            ));
        }

        // 单图: 使用单图发送方式
        if image_urls.len() == 1 {
            let result = self
                .notify_with_image(chat_id, &image_urls[0], caption, has_spoiler)
                .await;
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            return result;
        }

        info!(
            "Downloading and sending {} images to chat {}",
            image_urls.len(),
            chat_id
        );

        // 批量下载
        let local_paths = match self.downloader.download_all(image_urls).await {
            Ok(paths) => paths,
            Err(e) => {
                warn!("Failed to download images: {}, falling back to text", e);
                let fallback_message = if let Some(cap) = caption {
                    format!("{}\n🔗 {} images", cap, image_urls.len())
                } else {
                    format!("🔗 {} images", image_urls.len())
                };
                return self.notify_plain(chat_id, &fallback_message).await;
            }
        };

        // Telegram 限制: 媒体组最多10张图片,超过则分批发送
        const MAX_IMAGES_PER_GROUP: usize = 10;
        let total_images = local_paths.len();
        let chunks: Vec<_> = local_paths.chunks(MAX_IMAGES_PER_GROUP).collect();
        let total_batches = chunks.len();

        info!(
            "Sending {} images in {} batch(es)",
            total_images, total_batches
        );

        for (batch_idx, chunk) in chunks.into_iter().enumerate() {
            // 第一批使用原始 caption,后续批次添加批次信息
            let batch_caption = if batch_idx == 0 {
                caption.map(|s| s.to_string())
            } else if total_batches > 1 {
                Some(format!(
                    "\\(continued {}/{}\\)",
                    batch_idx + 1,
                    total_batches
                ))
            } else {
                None
            };

            let batch_paths: Vec<PathBuf> = chunk.to_vec();
            let batch_size = batch_paths.len();

            if let Err(e) = self
                .send_media_group(chat_id, batch_paths, batch_caption.as_deref(), has_spoiler)
                .await
            {
                warn!(
                    "Failed to send batch {}/{}: {}",
                    batch_idx + 1,
                    total_batches,
                    e
                );
                // 继续发送剩余批次
                continue;
            }

            let cooldown_secs = (batch_size * 2) as u64;
            tokio::time::sleep(tokio::time::Duration::from_secs(cooldown_secs)).await;
        }

        info!(
            "✅ All {} image(s) sent in {} batch(es)",
            total_images, total_batches
        );
        Ok(())
    }

    /// Send a photo from local file path
    async fn send_photo_file(
        &self,
        chat_id: ChatId,
        file_path: PathBuf,
        caption: Option<&str>,
        has_spoiler: bool,
    ) -> AppResult<()> {
        info!(
            "Sending photo from {:?} to chat {} (spoiler: {})",
            file_path, chat_id, has_spoiler
        );

        let input_file = InputFile::file(&file_path);
        let mut request = self.bot.send_photo(chat_id, input_file);

        if let Some(cap) = caption {
            request = request.caption(cap).parse_mode(ParseMode::MarkdownV2);
        }

        if has_spoiler {
            request = request.has_spoiler(true);
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

    /// 发送媒体组 (多张图片)
    async fn send_media_group(
        &self,
        chat_id: ChatId,
        file_paths: Vec<PathBuf>,
        caption: Option<&str>,
        has_spoiler: bool,
    ) -> AppResult<()> {
        info!(
            "Sending media group with {} photos to chat {} (spoiler: {})",
            file_paths.len(),
            chat_id,
            has_spoiler
        );

        if file_paths.is_empty() {
            return Err(crate::error::AppError::Unknown(
                "No files to send".to_string(),
            ));
        }

        // 构建媒体组
        let media: Vec<InputMedia> = file_paths
            .into_iter()
            .enumerate()
            .map(|(idx, path)| {
                let input_file = InputFile::file(path);
                let mut photo = InputMediaPhoto::new(input_file);

                // 只在第一张图片上添加标题
                if idx == 0 {
                    if let Some(cap) = caption {
                        photo = photo.caption(cap).parse_mode(ParseMode::MarkdownV2);
                    }
                }

                // Apply has_spoiler to all photos in the media group
                if has_spoiler {
                    photo = photo.spoiler();
                }

                InputMedia::Photo(photo)
            })
            .collect();

        match self.bot.send_media_group(chat_id, media).await {
            Ok(_) => {
                info!("✅ Media group sent successfully");
                Ok(())
            }
            Err(e) => {
                warn!("Failed to send media group to {}: {}", chat_id, e);
                Err(crate::error::AppError::Telegram(e.to_string()))
            }
        }
    }

    /// Send media group with individual caption for each photo (for ranking push)
    /// Automatically splits into multiple messages when over 10 images (Telegram limit)
    pub async fn notify_with_individual_captions(
        &self,
        chat_id: ChatId,
        image_urls: &[String],
        captions: &[String],
        has_spoiler: bool,
    ) -> AppResult<()> {
        if image_urls.is_empty() {
            return Err(crate::error::AppError::Unknown(
                "No images to send".to_string(),
            ));
        }

        if image_urls.len() != captions.len() {
            return Err(crate::error::AppError::Unknown(
                "Image URLs and captions count mismatch".to_string(),
            ));
        }

        // Single image: use single image method
        if image_urls.len() == 1 {
            let result = self
                .notify_with_image(chat_id, &image_urls[0], Some(&captions[0]), has_spoiler)
                .await;
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            return result;
        }

        info!(
            "Downloading and sending {} images with individual captions to chat {}",
            image_urls.len(),
            chat_id
        );

        // Batch download
        let local_paths = match self.downloader.download_all(image_urls).await {
            Ok(paths) => paths,
            Err(e) => {
                warn!("Failed to download images: {}, falling back to text", e);
                let fallback_message = format!("🔗 {} images (download failed)", image_urls.len());
                return self.notify_plain(chat_id, &fallback_message).await;
            }
        };

        // Telegram limit: max 10 images per media group, split into batches if over
        const MAX_IMAGES_PER_GROUP: usize = 10;
        let total_images = local_paths.len();

        let chunks: Vec<_> = local_paths
            .chunks(MAX_IMAGES_PER_GROUP)
            .zip(captions.chunks(MAX_IMAGES_PER_GROUP))
            .collect();
        let total_batches = chunks.len();

        info!(
            "Sending {} images in {} batch(es)",
            total_images, total_batches
        );

        for (batch_idx, (path_chunk, caption_chunk)) in chunks.into_iter().enumerate() {
            let batch_size = path_chunk.len();

            if let Err(e) = self
                .send_media_group_with_individual_captions(
                    chat_id,
                    path_chunk,
                    caption_chunk,
                    has_spoiler,
                    batch_idx,
                    total_batches,
                )
                .await
            {
                warn!(
                    "Failed to send batch {}/{}: {}",
                    batch_idx + 1,
                    total_batches,
                    e
                );
                // Continue with remaining batches
                continue;
            }

            let cooldown_secs = (batch_size * 2) as u64;
            tokio::time::sleep(tokio::time::Duration::from_secs(cooldown_secs)).await;
        }

        info!(
            "✅ All {} image(s) sent in {} batch(es)",
            total_images, total_batches
        );
        Ok(())
    }

    /// Send media group with individual caption for each photo
    async fn send_media_group_with_individual_captions(
        &self,
        chat_id: ChatId,
        file_paths: &[PathBuf],
        captions: &[String],
        has_spoiler: bool,
        batch_idx: usize,
        total_batches: usize,
    ) -> AppResult<()> {
        info!(
            "Sending media group with {} photos (batch {}/{}) to chat {} (spoiler: {})",
            file_paths.len(),
            batch_idx + 1,
            total_batches,
            chat_id,
            has_spoiler
        );

        if file_paths.is_empty() {
            return Err(crate::error::AppError::Unknown(
                "No files to send".to_string(),
            ));
        }

        // Build media group with individual caption for each photo
        let media: Vec<InputMedia> = file_paths
            .iter()
            .zip(captions.iter())
            .enumerate()
            .map(|(idx, (path, caption))| {
                let input_file = InputFile::file(path);
                let mut photo = InputMediaPhoto::new(input_file);

                // Set individual caption for each photo
                let final_caption = if batch_idx > 0 && idx == 0 {
                    // Add batch marker to first photo of non-first batches
                    format!(
                        "\\(continued {}/{}\\)\n\n{}",
                        batch_idx + 1,
                        total_batches,
                        caption
                    )
                } else {
                    caption.clone()
                };

                photo = photo
                    .caption(final_caption)
                    .parse_mode(ParseMode::MarkdownV2);

                // Apply has_spoiler to all photos in the media group
                if has_spoiler {
                    photo = photo.spoiler();
                }

                InputMedia::Photo(photo)
            })
            .collect();

        match self.bot.send_media_group(chat_id, media).await {
            Ok(_) => {
                info!("✅ Media group sent successfully");
                Ok(())
            }
            Err(e) => {
                warn!("Failed to send media group to {}: {}", chat_id, e);
                Err(crate::error::AppError::Telegram(e.to_string()))
            }
        }
    }
}
