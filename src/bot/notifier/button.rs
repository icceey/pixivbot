use crate::bot::handlers::{BOORU_DOWNLOAD_CALLBACK_PREFIX, DOWNLOAD_CALLBACK_PREFIX};
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

const TELEGRAM_CALLBACK_DATA_MAX_BYTES: usize = 64;

#[derive(Clone, Debug)]
pub enum DownloadTarget {
    Pixiv(u64),
    Booru { site_name: String, post_id: u64 },
}

#[derive(Clone, Debug, Default)]
pub struct DownloadButtonConfig {
    target: Option<DownloadTarget>,
    is_channel: bool,
}

impl DownloadButtonConfig {
    pub fn pixiv(illust_id: u64) -> Self {
        Self {
            target: Some(DownloadTarget::Pixiv(illust_id)),
            is_channel: false,
        }
    }

    pub fn booru(site_name: impl Into<String>, post_id: u64) -> Self {
        Self {
            target: Some(DownloadTarget::Booru {
                site_name: site_name.into(),
                post_id,
            }),
            is_channel: false,
        }
    }

    pub fn for_pixiv_chat(illust_id: u64, chat: &crate::db::entities::chats::Model) -> Self {
        let mut cfg = Self::pixiv(illust_id);
        cfg.is_channel = chat.r#type == "channel";
        cfg
    }

    pub fn for_booru_chat(
        site_name: impl Into<String>,
        post_id: u64,
        chat: &crate::db::entities::chats::Model,
    ) -> Self {
        let mut cfg = Self::booru(site_name, post_id);
        cfg.is_channel = chat.r#type == "channel";
        cfg
    }

    pub(super) fn build_keyboard(&self) -> Option<InlineKeyboardMarkup> {
        if self.is_channel {
            return None;
        }

        let callback_data = match self.target.as_ref()? {
            DownloadTarget::Pixiv(id) => format!("{}{}", DOWNLOAD_CALLBACK_PREFIX, id),
            DownloadTarget::Booru { site_name, post_id } => format!(
                "{}{}:{}",
                BOORU_DOWNLOAD_CALLBACK_PREFIX, site_name, post_id
            ),
        };

        if callback_data.len() > TELEGRAM_CALLBACK_DATA_MAX_BYTES {
            return None;
        }

        let button = InlineKeyboardButton::callback(super::DOWNLOAD_BUTTON_LABEL, callback_data);
        Some(InlineKeyboardMarkup::new(vec![vec![button]]))
    }
}
