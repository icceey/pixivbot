use crate::pixiv::downloader::Downloader;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{ChatAction, InputFile, InputMedia, InputMediaPhoto, ParseMode};
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
pub struct BatchSendResult {
    pub succeeded_indices: Vec<usize>,
    pub failed_indices: Vec<usize>,
}

impl BatchSendResult {
    pub fn all_succeeded(total: usize) -> Self {
        Self {
            succeeded_indices: (0..total).collect(),
            failed_indices: Vec::new(),
        }
    }
    pub fn all_failed(total: usize) -> Self {
        Self {
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
}

/// 文案策略：区分“共享文案”和“独立文案”
enum CaptionStrategy<'a> {
    /// 所有图片共享一个 Caption (仅第一张显示)
    Shared(Option<&'a str>),
    /// 每张图片有独立的 Caption
    Individual(&'a [String]),
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

    /// 下载并发送单张图片
    pub async fn notify_with_image(
        &self,
        chat_id: ChatId,
        image_url: &str,
        caption: Option<&str>,
        has_spoiler: bool,
    ) -> Result<()> {
        info!(
            "Downloading and sending image to chat {}: {}",
            chat_id, image_url
        );
        // Set bot status to uploading photo
        if let Err(e) = self
            .bot
            .send_chat_action(chat_id, ChatAction::UploadPhoto)
            .await
        {
            warn!("Failed to set chat action for chat {}: {:#}", chat_id, e);
        }
        let local_path = self.downloader.download(image_url).await?;
        self.send_photo_file(chat_id, &local_path, caption, has_spoiler)
            .await
    }

    /// 发送多张图片（共享文案）
    pub async fn notify_with_images(
        &self,
        chat_id: ChatId,
        image_urls: &[String],
        caption: Option<&str>,
        has_spoiler: bool,
    ) -> BatchSendResult {
        self.process_batch_send(
            chat_id,
            image_urls,
            CaptionStrategy::Shared(caption),
            has_spoiler,
        )
        .await
    }

    /// 发送多张图片（独立文案，用于榜单）
    pub async fn notify_with_individual_captions(
        &self,
        chat_id: ChatId,
        image_urls: &[String],
        captions: &[String],
        has_spoiler: bool,
    ) -> BatchSendResult {
        if image_urls.len() != captions.len() {
            warn!("Image URLs and captions count mismatch");
            return BatchSendResult::all_failed(image_urls.len());
        }
        self.process_batch_send(
            chat_id,
            image_urls,
            CaptionStrategy::Individual(captions),
            has_spoiler,
        )
        .await
    }

    // ==================== 私有通用逻辑 ====================

    /// 核心逻辑：下载 -> 分批 -> 发送
    async fn process_batch_send(
        &self,
        chat_id: ChatId,
        image_urls: &[String],
        caption_strategy: CaptionStrategy<'_>,
        has_spoiler: bool,
    ) -> BatchSendResult {
        let total = image_urls.len();
        if total == 0 {
            return BatchSendResult::all_failed(0);
        }

        // 1. 优化：单图特例处理
        if total == 1 {
            let cap = match &caption_strategy {
                CaptionStrategy::Shared(c) => *c,
                CaptionStrategy::Individual(cs) => Some(cs[0].as_str()),
            };

            match self
                .notify_with_image(chat_id, &image_urls[0], cap, has_spoiler)
                .await
            {
                Ok(_) => {
                    sleep(Duration::from_secs(2)).await;
                    return BatchSendResult::all_succeeded(1);
                }
                Err(e) => {
                    error!("Single image send failed for chat {}: {:#}", chat_id, e);
                    return BatchSendResult::all_failed(1);
                }
            }
        }

        info!("Batch processing {} images for chat {}", total, chat_id);

        // Set bot status to uploading photo before downloading
        if let Err(e) = self
            .bot
            .send_chat_action(chat_id, ChatAction::UploadPhoto)
            .await
        {
            warn!("Failed to set chat action for chat {}: {:#}", chat_id, e);
        }

        // 2. 批量下载
        let local_paths = match self.downloader.download_all(image_urls).await {
            Ok(paths) => paths,
            Err(e) => {
                error!("Batch download failed for chat {}: {:#}", chat_id, e);
                return BatchSendResult::all_failed(total);
            }
        };

        // 3. 分批处理
        const MAX_PER_GROUP: usize = 10;
        let chunks: Vec<_> = local_paths.chunks(MAX_PER_GROUP).collect();
        let total_batches = chunks.len();

        let mut succeeded = Vec::new();
        let mut failed = Vec::new();
        let mut current_idx = 0;

        for (batch_idx, path_chunk) in chunks.into_iter().enumerate() {
            let batch_size = path_chunk.len();
            let batch_end_idx = current_idx + batch_size;

            let batch_captions_slice = match &caption_strategy {
                CaptionStrategy::Individual(all_captions) => {
                    Some(&all_captions[current_idx..batch_end_idx])
                }
                CaptionStrategy::Shared(_) => None,
            };

            let silent = batch_idx > 0;

            if let Err(e) = self
                .send_media_batch(
                    chat_id,
                    path_chunk,
                    &caption_strategy,
                    batch_captions_slice,
                    has_spoiler,
                    batch_idx,
                    total_batches,
                    silent,
                )
                .await
            {
                warn!(
                    "Batch {}/{} failed for chat {}: {:#}",
                    batch_idx + 1,
                    total_batches,
                    chat_id,
                    e
                );
                failed.extend(current_idx..batch_end_idx);
            } else {
                succeeded.extend(current_idx..batch_end_idx);
            }

            current_idx += batch_size;

            if batch_idx < total_batches - 1 {
                let cooldown = Duration::from_secs((batch_size * 2) as u64);
                sleep(cooldown).await;
            }
        }

        if !failed.is_empty() {
            error!(
                "❌ Sent {}/{} images to chat {}",
                succeeded.len(),
                total,
                chat_id
            );
        } else {
            info!("✅ All {} images sent to chat {}", total, chat_id);
        }

        BatchSendResult {
            succeeded_indices: succeeded,
            failed_indices: failed,
        }
    }

    /// 底层发送：构建 InputMedia 并调用 API
    #[allow(clippy::too_many_arguments)]
    async fn send_media_batch(
        &self,
        chat_id: ChatId,
        paths: &[PathBuf], // 接收切片
        strategy: &CaptionStrategy<'_>,
        batch_captions: Option<&[String]>, // 仅当 Individual 时有值
        has_spoiler: bool,
        batch_idx: usize,
        total_batches: usize,
        silent: bool,
    ) -> Result<()> {
        let media_group: Vec<InputMedia> = paths
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let mut photo = InputMediaPhoto::new(InputFile::file(path));

                // 文案逻辑
                let caption_text = match strategy {
                    // 1. 共享文案：只有第一批的第一张图有文案
                    CaptionStrategy::Shared(base_cap) => {
                        if i == 0 {
                            if batch_idx == 0 {
                                // 首批首图：原始文案
                                base_cap.map(|s| s.to_string())
                            } else if total_batches > 1 {
                                // 后续批次首图：添加页码标记
                                Some(format!(
                                    "\\(continued {}/{}\\)",
                                    batch_idx + 1,
                                    total_batches
                                ))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    }
                    // 2. 独立文案：每张图都有，且需处理批次标记
                    CaptionStrategy::Individual(_) => {
                        if let Some(caps) = batch_captions {
                            let raw_cap = &caps[i];
                            if batch_idx > 0 && i == 0 {
                                // 后续批次首图：添加页码标记 + 原始文案
                                Some(format!(
                                    "\\(continued {}/{}\\)\n\n{}",
                                    batch_idx + 1,
                                    total_batches,
                                    raw_cap
                                ))
                            } else {
                                Some(raw_cap.clone())
                            }
                        } else {
                            None
                        }
                    }
                };

                if let Some(c) = caption_text {
                    photo = photo.caption(c).parse_mode(ParseMode::MarkdownV2);
                }
                if has_spoiler {
                    photo = photo.spoiler();
                }
                InputMedia::Photo(photo)
            })
            .collect();

        let mut req = self.bot.send_media_group(chat_id, media_group);
        if silent {
            req = req.disable_notification(true);
        }
        req.await.context("Send media group failed")?;
        Ok(())
    }

    async fn send_photo_file(
        &self,
        chat_id: ChatId,
        path: &Path,
        caption: Option<&str>,
        has_spoiler: bool,
    ) -> Result<()> {
        let mut req = self.bot.send_photo(chat_id, InputFile::file(path));
        if let Some(c) = caption {
            req = req.caption(c).parse_mode(ParseMode::MarkdownV2);
        }
        if has_spoiler {
            req = req.has_spoiler(true);
        }
        req.await.context("Send photo failed")?;
        Ok(())
    }
}
