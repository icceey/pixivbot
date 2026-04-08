use crate::pixiv::downloader::Downloader;
use crate::utils::caption::MAX_PER_GROUP;
use pixiv_client::UgoiraFrame;
use std::sync::Arc;
use teloxide::adaptors::Throttle;
use teloxide::prelude::*;
use tracing::warn;

mod batch;
mod button;
mod caption;
mod media;
mod numbering;
mod result;
mod ugoira;

/// Button label for download button
const DOWNLOAD_BUTTON_LABEL: &str = "📥 下载";

/// Type alias for the throttled bot
pub type ThrottledBot = Throttle<Bot>;

pub use button::DownloadButtonConfig;
pub use numbering::ContinuationNumbering;
pub use result::BatchSendResult;

use caption::CaptionStrategy;

#[derive(Clone)]
pub struct Notifier {
    bot: ThrottledBot,
    downloader: Arc<Downloader>,
}

impl Notifier {
    pub fn new(bot: ThrottledBot, downloader: Arc<Downloader>) -> Self {
        Self { bot, downloader }
    }

    /// Get reference to the downloader (used by download handler)
    pub fn get_downloader(&self) -> &Arc<Downloader> {
        &self.downloader
    }

    /// 发送多张图片（共享文案）
    #[allow(dead_code)]
    pub async fn notify_with_images(
        &self,
        chat_id: ChatId,
        image_urls: &[String],
        caption: Option<&str>,
        has_spoiler: bool,
    ) -> BatchSendResult {
        self.notify_with_images_and_button(
            chat_id,
            image_urls,
            caption,
            has_spoiler,
            &DownloadButtonConfig::default(),
        )
        .await
    }

    /// 发送多张图片（共享文案）并带有下载按钮
    pub async fn notify_with_images_and_button(
        &self,
        chat_id: ChatId,
        image_urls: &[String],
        caption: Option<&str>,
        has_spoiler: bool,
        download_config: &DownloadButtonConfig,
    ) -> BatchSendResult {
        self.process_batch_send(
            chat_id,
            image_urls,
            CaptionStrategy::Shared(caption),
            has_spoiler,
            download_config,
            None,
        )
        .await
    }

    pub async fn notify_with_images_and_button_and_continuation(
        &self,
        chat_id: ChatId,
        image_urls: &[String],
        caption: Option<&str>,
        has_spoiler: bool,
        download_config: &DownloadButtonConfig,
        continuation_numbering: ContinuationNumbering,
    ) -> BatchSendResult {
        self.process_batch_send(
            chat_id,
            image_urls,
            CaptionStrategy::Shared(caption),
            has_spoiler,
            download_config,
            Some(continuation_numbering),
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
        self.notify_with_individual_captions_and_button(
            chat_id,
            image_urls,
            captions,
            has_spoiler,
            &DownloadButtonConfig::default(),
        )
        .await
    }

    /// 发送多张图片（独立文案，用于榜单）并带有下载按钮
    /// Note: This method accepts `download_config` for API consistency, but
    /// ranking pushes typically use `DownloadButtonConfig::default()`, which
    /// means no download button will be shown.
    pub async fn notify_with_individual_captions_and_button(
        &self,
        chat_id: ChatId,
        image_urls: &[String],
        captions: &[String],
        has_spoiler: bool,
        download_config: &DownloadButtonConfig,
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
            download_config,
            None,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::caption::{individual_batch_caption, shared_batch_caption};
    use super::{BatchSendResult, ContinuationNumbering, DownloadButtonConfig};
    use crate::db::types::Tags;

    fn make_chat(chat_type: &str) -> crate::db::entities::chats::Model {
        crate::db::entities::chats::Model {
            id: 1,
            r#type: chat_type.to_string(),
            title: Some("test".to_string()),
            enabled: true,
            blur_sensitive_tags: false,
            excluded_tags: Tags::default(),
            sensitive_tags: Tags::default(),
            created_at: chrono::Utc::now().naive_utc(),
            allow_without_mention: false,
        }
    }

    #[test]
    fn shared_batch_caption_uses_global_numbering_for_resumed_multi_batch_send() {
        let numbering = ContinuationNumbering::new(2, 3);

        assert_eq!(
            shared_batch_caption(Some("base"), 0, 0, numbering),
            Some("\\(continued 2/3\\)".to_string())
        );
        assert_eq!(
            shared_batch_caption(Some("base"), 0, 1, numbering),
            Some("\\(continued 3/3\\)".to_string())
        );
        assert_eq!(shared_batch_caption(Some("base"), 1, 1, numbering), None);
    }

    #[test]
    fn individual_batch_caption_uses_global_numbering_for_later_batches() {
        let numbering = ContinuationNumbering::new(2, 3);

        assert_eq!(
            individual_batch_caption("ranking caption", 0, 0, numbering),
            Some("\\(continued 2/3\\)\n\nranking caption".to_string())
        );
        assert_eq!(
            individual_batch_caption("ranking caption", 0, 1, numbering),
            Some("\\(continued 3/3\\)\n\nranking caption".to_string())
        );
        assert_eq!(
            individual_batch_caption("ranking caption", 1, 1, numbering),
            Some("ranking caption".to_string())
        );
    }

    #[test]
    fn download_button_config_for_chat_marks_channels_only() {
        let private_chat = make_chat("private");
        let channel_chat = make_chat("channel");

        assert!(!DownloadButtonConfig::for_chat(123, &private_chat).is_channel);
        assert!(DownloadButtonConfig::for_chat(123, &channel_chat).is_channel);
    }

    #[test]
    fn batch_send_result_all_failed_marks_every_index_failed() {
        let result = BatchSendResult::all_failed(3);

        assert_eq!(result.succeeded_indices, Vec::<usize>::new());
        assert_eq!(result.failed_indices, vec![0, 1, 2]);
        assert_eq!(result.first_message_id, None);
        assert!(result.is_complete_failure());
        assert!(!result.is_complete_success());
    }

    #[test]
    fn batch_send_result_success_and_partial_flags_match_contents() {
        let success = BatchSendResult {
            succeeded_indices: vec![0, 1],
            failed_indices: Vec::new(),
            first_message_id: Some(42),
        };
        let partial = BatchSendResult {
            succeeded_indices: vec![0],
            failed_indices: vec![1],
            first_message_id: Some(7),
        };

        assert!(success.is_complete_success());
        assert!(!success.is_complete_failure());

        assert!(!partial.is_complete_success());
        assert!(!partial.is_complete_failure());
    }

    #[test]
    fn continuation_numbering_for_item_count_uses_shared_batch_limit() {
        let numbering = ContinuationNumbering::for_item_count(23);

        assert_eq!(numbering.first_batch_number, 1);
        assert_eq!(numbering.total_batches, 3);
        assert_eq!(numbering.display_batch_number(0), 1);
        assert_eq!(numbering.display_batch_number(2), 3);
    }

    #[test]
    fn download_button_config_hides_button_without_illust_or_for_channels() {
        let without_illust = DownloadButtonConfig::default();
        let for_channel = DownloadButtonConfig::new(123).for_channel();
        let normal = DownloadButtonConfig::new(123);

        assert!(!without_illust.should_show_button());
        assert!(!for_channel.should_show_button());
        assert!(normal.should_show_button());

        assert!(without_illust.build_keyboard().is_none());
        assert!(for_channel.build_keyboard().is_none());
        assert!(normal.build_keyboard().is_some());
    }
}
