use crate::bot::handlers::DOWNLOAD_CALLBACK_PREFIX;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

/// Configuration for download button
/// Only applicable to non-channel chats (private and group chats)
#[derive(Clone, Debug, Default)]
pub struct DownloadButtonConfig {
    /// The illust ID to download when button is clicked
    pub illust_id: Option<u64>,
    /// Whether the target chat is a channel (channels don't support inline buttons)
    pub is_channel: bool,
}

impl DownloadButtonConfig {
    pub fn new(illust_id: u64) -> Self {
        Self {
            illust_id: Some(illust_id),
            is_channel: false,
        }
    }

    pub fn for_chat(illust_id: u64, chat: &crate::db::entities::chats::Model) -> Self {
        let config = Self::new(illust_id);
        if chat.r#type == "channel" {
            config.for_channel()
        } else {
            config
        }
    }

    pub fn for_channel(mut self) -> Self {
        self.is_channel = true;
        self
    }

    pub(super) fn should_show_button(&self) -> bool {
        self.illust_id.is_some() && !self.is_channel
    }

    pub(super) fn build_keyboard(&self) -> Option<InlineKeyboardMarkup> {
        if !self.should_show_button() {
            return None;
        }

        let illust_id = self.illust_id.unwrap();
        let callback_data = format!("{}{}", DOWNLOAD_CALLBACK_PREFIX, illust_id);
        let button = InlineKeyboardButton::callback(super::DOWNLOAD_BUTTON_LABEL, callback_data);
        Some(InlineKeyboardMarkup::new(vec![vec![button]]))
    }
}
