use super::{BatchSendResult, DownloadButtonConfig, Notifier};
use pixiv_client::UgoiraFrame;
use teloxide::prelude::*;
use teloxide::types::ChatAction;
use tracing::{error, warn};

impl Notifier {
    /// 发送 Ugoira (动图) 作品为 MP4 动画
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
}
