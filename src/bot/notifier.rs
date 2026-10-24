use crate::pixiv::downloader::Downloader;
use crate::utils::caption::MAX_PER_GROUP;
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

const DOWNLOAD_BUTTON_LABEL: &str = "📥 下载";

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

    pub fn get_downloader(&self) -> &Arc<Downloader> {
        &self.downloader
    }

    /// 发送多张图片（共享文案）
    pub async fn notify_with_images(
        &self,
        chat_id: ChatId,
        image_urls: &[String],
        caption: Option<&str>,
        has_spoiler: bool,
        download_config: &DownloadButtonConfig,
        continuation_numbering: Option<ContinuationNumbering>,
    ) -> BatchSendResult {
        self.process_batch_send(
            chat_id,
            image_urls,
            CaptionStrategy::Shared(caption),
            has_spoiler,
            download_config,
            continuation_numbering,
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
            &DownloadButtonConfig::default(),
            None,
        )
        .await
    }
}
