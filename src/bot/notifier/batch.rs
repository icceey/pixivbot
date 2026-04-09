use super::caption::CaptionStrategy;
use super::{
    BatchSendResult, ContinuationNumbering, DownloadButtonConfig, Notifier, MAX_PER_GROUP,
};
use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::{ChatAction, InlineKeyboardMarkup};
use tracing::{error, info, warn};

impl Notifier {
    /// 核心逻辑：下载 -> 分批 -> 发送
    pub(super) async fn process_batch_send(
        &self,
        chat_id: ChatId,
        image_urls: &[String],
        caption_strategy: CaptionStrategy<'_>,
        has_spoiler: bool,
        download_config: &DownloadButtonConfig,
        continuation_numbering: Option<ContinuationNumbering>,
    ) -> BatchSendResult {
        let total = image_urls.len();
        if total == 0 {
            return BatchSendResult::all_failed(0);
        }

        let keyboard = download_config.build_keyboard();

        if total == 1 {
            let numbering = continuation_numbering
                .unwrap_or_else(|| ContinuationNumbering::for_item_count(total));
            let effective_cap = match &caption_strategy {
                CaptionStrategy::Shared(c) => {
                    super::caption::shared_batch_caption(*c, 0, 0, numbering)
                }
                CaptionStrategy::Individual(cs) => {
                    super::caption::individual_batch_caption(&cs[0], 0, 0, numbering)
                }
            };

            match self
                .send_single_image(
                    chat_id,
                    &image_urls[0],
                    effective_cap.as_deref(),
                    has_spoiler,
                    keyboard,
                )
                .await
            {
                Ok(msg_id) => {
                    return BatchSendResult {
                        succeeded_indices: vec![0],
                        failed_indices: Vec::new(),
                        first_message_id: Some(msg_id),
                    };
                }
                Err(e) => {
                    error!("Single image send failed for chat {}: {:#}", chat_id, e);
                    return BatchSendResult::all_failed(1);
                }
            }
        }

        info!("Batch processing {} images for chat {}", total, chat_id);

        if let Err(e) = self
            .bot
            .send_chat_action(chat_id, ChatAction::UploadPhoto)
            .await
        {
            warn!("Failed to set chat action for chat {}: {:#}", chat_id, e);
        }

        let local_paths = match self.downloader.download_all(image_urls).await {
            Ok(paths) => paths,
            Err(e) => {
                error!("Batch download failed for chat {}: {:#}", chat_id, e);
                return BatchSendResult::all_failed(total);
            }
        };

        let chunks: Vec<_> = local_paths.chunks(MAX_PER_GROUP).collect();
        let continuation_numbering =
            continuation_numbering.unwrap_or_else(|| ContinuationNumbering::for_item_count(total));
        let total_batches = continuation_numbering.total_batches;

        let mut succeeded = Vec::new();
        let mut failed = Vec::new();
        let mut current_idx = 0;
        let mut first_message_id: Option<i32> = None;

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

            match self
                .send_media_batch(
                    chat_id,
                    path_chunk,
                    &caption_strategy,
                    batch_captions_slice,
                    has_spoiler,
                    batch_idx,
                    continuation_numbering,
                    silent,
                )
                .await
            {
                Ok(msg_id) => {
                    succeeded.extend(current_idx..batch_end_idx);
                    if first_message_id.is_none() {
                        first_message_id = msg_id;
                    }
                }
                Err(e) => {
                    warn!(
                        "Batch {}/{} failed for chat {}: {:#}",
                        continuation_numbering.display_batch_number(batch_idx),
                        total_batches,
                        chat_id,
                        e
                    );
                    failed.extend(current_idx..batch_end_idx);
                }
            }

            current_idx += batch_size;
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
            first_message_id,
        }
    }

    /// 发送单张图片并返回消息ID
    pub(super) async fn send_single_image(
        &self,
        chat_id: ChatId,
        image_url: &str,
        caption: Option<&str>,
        has_spoiler: bool,
        keyboard: Option<InlineKeyboardMarkup>,
    ) -> Result<i32> {
        info!(
            "Downloading and sending image to chat {}: {}",
            chat_id, image_url
        );
        if let Err(e) = self
            .bot
            .send_chat_action(chat_id, ChatAction::UploadPhoto)
            .await
        {
            warn!("Failed to set chat action for chat {}: {:#}", chat_id, e);
        }
        let local_path = self.downloader.download(image_url).await?;
        self.send_photo_file_with_id(chat_id, &local_path, caption, has_spoiler, keyboard)
            .await
    }
}
