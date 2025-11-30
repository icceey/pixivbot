use crate::pixiv::downloader::Downloader;
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{InputFile, InputMedia, InputMediaPhoto, ParseMode};
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

/// 批量发送的结果，记录成功和失败的项目索引
#[derive(Debug, Clone)]
pub struct BatchSendResult {
    /// 总共要发送的项目数
    pub total: usize,
    /// 成功发送的项目索引 (基于原始输入的索引)
    pub succeeded_indices: Vec<usize>,
    /// 失败的项目索引
    pub failed_indices: Vec<usize>,
}

impl BatchSendResult {
    pub fn all_succeeded(total: usize) -> Self {
        Self {
            total,
            succeeded_indices: (0..total).collect(),
            failed_indices: Vec::new(),
        }
    }

    pub fn all_failed(total: usize) -> Self {
        Self {
            total,
            succeeded_indices: Vec::new(),
            failed_indices: (0..total).collect(),
        }
    }

    pub fn is_complete_success(&self) -> bool {
        self.failed_indices.is_empty()
    }

    pub fn is_complete_failure(&self) -> bool {
        self.succeeded_indices.is_empty()
    }

    pub fn has_failures(&self) -> bool {
        !self.failed_indices.is_empty()
    }
}

#[derive(Clone)]
pub struct Notifier {
    bot: Bot,
    downloader: Arc<Downloader>,
}

impl Notifier {
    pub fn new(bot: Bot, downloader: Arc<Downloader>) -> Self {
        Self { bot, downloader }
    }

    /// Send plain text notification without formatting
    pub async fn notify_plain(&self, chat_id: ChatId, message: &str) -> Result<()> {
        info!("Sending plain notification to chat {}", chat_id);

        self.bot
            .send_message(chat_id, message)
            .await
            .context("Failed to send Telegram message")?;
        info!("✅ Notification sent successfully");
        Ok(())
    }

    /// Download image and send as photo with caption
    pub async fn notify_with_image(
        &self,
        chat_id: ChatId,
        image_url: &str,
        caption: Option<&str>,
        has_spoiler: bool,
    ) -> Result<()> {
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
    /// 返回 BatchSendResult 表示哪些图片发送成功/失败
    pub async fn notify_with_images(
        &self,
        chat_id: ChatId,
        image_urls: &[String],
        caption: Option<&str>,
        has_spoiler: bool,
    ) -> BatchSendResult {
        let total = image_urls.len();

        if image_urls.is_empty() {
            return BatchSendResult::all_failed(0);
        }

        // 单图: 使用单图发送方式
        if image_urls.len() == 1 {
            let result = self
                .notify_with_image(chat_id, &image_urls[0], caption, has_spoiler)
                .await;
            sleep(Duration::from_secs(2)).await;
            return if result.is_ok() {
                BatchSendResult::all_succeeded(1)
            } else {
                BatchSendResult::all_failed(1)
            };
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
                // 下载失败时尝试发送文本，但仍标记为全部失败
                let _ = self.notify_plain(chat_id, &fallback_message).await;
                return BatchSendResult::all_failed(total);
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

        let mut succeeded_indices: Vec<usize> = Vec::new();
        let mut failed_indices: Vec<usize> = Vec::new();
        let mut current_idx: usize = 0;

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
            let batch_start_idx = current_idx;

            // 第一批带提醒，后续批次静默发送
            let disable_notification = batch_idx > 0;

            if let Err(e) = self
                .send_media_group(
                    chat_id,
                    batch_paths,
                    batch_caption.as_deref(),
                    has_spoiler,
                    disable_notification,
                )
                .await
            {
                warn!(
                    "Failed to send batch {}/{}: {}",
                    batch_idx + 1,
                    total_batches,
                    e
                );
                // 记录该批次所有图片为失败
                for i in batch_start_idx..(batch_start_idx + batch_size) {
                    failed_indices.push(i);
                }
                current_idx += batch_size;
                // 继续发送剩余批次
                continue;
            }

            // 记录该批次所有图片为成功
            for i in batch_start_idx..(batch_start_idx + batch_size) {
                succeeded_indices.push(i);
            }
            current_idx += batch_size;

            let cooldown_secs = (batch_size * 2) as u64;
            sleep(Duration::from_secs(cooldown_secs)).await;
        }

        if !failed_indices.is_empty() {
            error!(
                "❌ Failed to send {} of {} image(s)",
                failed_indices.len(),
                total_images
            );
        } else {
            info!(
                "✅ All {} image(s) sent in {} batch(es)",
                total_images, total_batches
            );
        }

        BatchSendResult {
            total: total_images,
            succeeded_indices,
            failed_indices,
        }
    }

    /// Send a photo from local file path
    async fn send_photo_file(
        &self,
        chat_id: ChatId,
        file_path: PathBuf,
        caption: Option<&str>,
        has_spoiler: bool,
    ) -> Result<()> {
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

        request.await.context("Failed to send photo")?;
        info!("✅ Photo sent successfully");
        Ok(())
    }

    /// 发送媒体组 (多张图片)
    async fn send_media_group(
        &self,
        chat_id: ChatId,
        file_paths: Vec<PathBuf>,
        caption: Option<&str>,
        has_spoiler: bool,
        disable_notification: bool,
    ) -> Result<()> {
        info!(
            "Sending media group with {} photos to chat {} (spoiler: {}, silent: {})",
            file_paths.len(),
            chat_id,
            has_spoiler,
            disable_notification
        );

        if file_paths.is_empty() {
            return Err(anyhow!("No files to send"));
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

        let mut request = self.bot.send_media_group(chat_id, media);
        if disable_notification {
            request = request.disable_notification(true);
        }

        request.await.context("Failed to send media group")?;
        info!("✅ Media group sent successfully");
        Ok(())
    }

    /// Send media group with individual caption for each photo (for ranking push)
    /// Automatically splits into multiple messages when over 10 images (Telegram limit)
    /// 返回 BatchSendResult 表示哪些图片发送成功/失败
    pub async fn notify_with_individual_captions(
        &self,
        chat_id: ChatId,
        image_urls: &[String],
        captions: &[String],
        has_spoiler: bool,
    ) -> BatchSendResult {
        let total = image_urls.len();

        if image_urls.is_empty() {
            return BatchSendResult::all_failed(0);
        }

        if image_urls.len() != captions.len() {
            warn!("Image URLs and captions count mismatch");
            return BatchSendResult::all_failed(total);
        }

        // Single image: use single image method
        if image_urls.len() == 1 {
            let result = self
                .notify_with_image(chat_id, &image_urls[0], Some(&captions[0]), has_spoiler)
                .await;
            sleep(Duration::from_secs(2)).await;
            return if result.is_ok() {
                BatchSendResult::all_succeeded(1)
            } else {
                BatchSendResult::all_failed(1)
            };
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
                let _ = self.notify_plain(chat_id, &fallback_message).await;
                return BatchSendResult::all_failed(total);
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

        let mut succeeded_indices: Vec<usize> = Vec::new();
        let mut failed_indices: Vec<usize> = Vec::new();
        let mut current_idx: usize = 0;

        for (batch_idx, (path_chunk, caption_chunk)) in chunks.into_iter().enumerate() {
            let batch_size = path_chunk.len();
            let batch_start_idx = current_idx;

            // 第一批带提醒，后续批次静默发送
            let disable_notification = batch_idx > 0;

            if let Err(e) = self
                .send_media_group_with_individual_captions(
                    chat_id,
                    path_chunk,
                    caption_chunk,
                    has_spoiler,
                    batch_idx,
                    total_batches,
                    disable_notification,
                )
                .await
            {
                warn!(
                    "Failed to send batch {}/{}: {}",
                    batch_idx + 1,
                    total_batches,
                    e
                );
                // 记录该批次所有图片为失败
                for i in batch_start_idx..(batch_start_idx + batch_size) {
                    failed_indices.push(i);
                }
                current_idx += batch_size;
                // Continue with remaining batches
                continue;
            }

            // 记录该批次所有图片为成功
            for i in batch_start_idx..(batch_start_idx + batch_size) {
                succeeded_indices.push(i);
            }
            current_idx += batch_size;

            let cooldown_secs = (batch_size * 2) as u64;
            sleep(Duration::from_secs(cooldown_secs)).await;
        }

        if !failed_indices.is_empty() {
            error!(
                "❌ Failed to send {} of {} image(s)",
                failed_indices.len(),
                total_images
            );
        } else {
            info!(
                "✅ All {} image(s) sent in {} batch(es)",
                total_images, total_batches
            );
        }

        BatchSendResult {
            total: total_images,
            succeeded_indices,
            failed_indices,
        }
    }

    /// Send media group with individual caption for each photo
    #[allow(clippy::too_many_arguments)]
    async fn send_media_group_with_individual_captions(
        &self,
        chat_id: ChatId,
        file_paths: &[PathBuf],
        captions: &[String],
        has_spoiler: bool,
        batch_idx: usize,
        total_batches: usize,
        disable_notification: bool,
    ) -> Result<()> {
        info!(
            "Sending media group with {} photos (batch {}/{}) to chat {} (spoiler: {}, silent: {})",
            file_paths.len(),
            batch_idx + 1,
            total_batches,
            chat_id,
            has_spoiler,
            disable_notification
        );

        if file_paths.is_empty() {
            return Err(anyhow!("No files to send"));
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

        let mut request = self.bot.send_media_group(chat_id, media);
        if disable_notification {
            request = request.disable_notification(true);
        }

        request.await.context("Failed to send media group")?;
        info!("✅ Media group sent successfully");
        Ok(())
    }
}
