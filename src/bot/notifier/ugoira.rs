use super::{BatchSendResult, DownloadButtonConfig, Notifier};
use pixiv_client::UgoiraFrame;
use teloxide::prelude::*;
#[cfg(feature = "ffmpeg-codec")]
use teloxide::types::ChatAction;
use tracing::error;
#[cfg(feature = "ffmpeg-codec")]
use tracing::warn;

impl Notifier {
    /// 发送 Ugoira (动图) 作品为 MP4 动画
    #[cfg(feature = "ffmpeg-codec")]
    pub async fn notify_ugoira(
        &self,
        chat_id: ChatId,
        zip_url: &str,
        frames: Vec<UgoiraFrame>,
        caption: Option<&str>,
        has_spoiler: bool,
        download_config: &DownloadButtonConfig,
    ) -> BatchSendResult {
        let keyboard = download_config.build_keyboard();

        if let Err(e) = self
            .bot
            .send_chat_action(chat_id, ChatAction::UploadVideo)
            .await
        {
            warn!("Failed to set chat action for chat {}: {:#}", chat_id, e);
        }

        let mp4_path = match self.downloader.download_ugoira_mp4(zip_url, frames).await {
            Ok(path) => path,
            Err(e) => {
                error!(
                    "Failed to download/convert ugoira for chat {}: {:#}",
                    chat_id, e
                );
                return BatchSendResult::all_failed(1);
            }
        };

        match self
            .send_animation_file(chat_id, &mp4_path, caption, has_spoiler, keyboard)
            .await
        {
            Ok(msg_id) => BatchSendResult {
                succeeded_indices: vec![0],
                failed_indices: Vec::new(),
                first_message_id: Some(msg_id),
            },
            Err(e) => {
                error!(
                    "Failed to send ugoira animation to chat {}: {:#}",
                    chat_id, e
                );
                BatchSendResult::all_failed(1)
            }
        }
    }

    /// Ugoira 发送的存根实现（未启用 ffmpeg-codec feature）。
    ///
    /// 返回全失败结果，调用方应记录错误并跳过。
    #[cfg(not(feature = "ffmpeg-codec"))]
    pub async fn notify_ugoira(
        &self,
        chat_id: ChatId,
        _zip_url: &str,
        _frames: Vec<UgoiraFrame>,
        _caption: Option<&str>,
        _has_spoiler: bool,
        _download_config: &DownloadButtonConfig,
    ) -> BatchSendResult {
        error!(
            "Cannot send ugoira to chat {}: ffmpeg-codec feature is not enabled, \
             MP4 encoding is unavailable. Build with --features ffmpeg-codec to enable.",
            chat_id
        );
        BatchSendResult::all_failed(1)
    }
}
