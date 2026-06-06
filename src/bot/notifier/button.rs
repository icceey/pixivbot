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
        let cfg = Self::pixiv(illust_id);
        if chat.r#type == "channel" {
            cfg.for_channel()
        } else {
            cfg
        }
    }

    pub fn for_booru_chat(
        site_name: impl Into<String>,
        post_id: u64,
        chat: &crate::db::entities::chats::Model,
    ) -> Self {
        let cfg = Self::booru(site_name, post_id);
        if chat.r#type == "channel" {
            cfg.for_channel()
        } else {
            cfg
        }
    }

    pub fn for_channel(mut self) -> Self {
        self.is_channel = true;
        self
    }

    pub(super) fn should_show_button(&self) -> bool {
        self.target.is_some() && !self.is_channel
    }

    pub(super) fn build_keyboard(&self) -> Option<InlineKeyboardMarkup> {
        if !self.should_show_button() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(r#type: &str) -> crate::db::entities::chats::Model {
        crate::db::entities::chats::Model {
            id: 1,
            r#type: r#type.to_string(),
            title: None,
            enabled: true,
            blur_sensitive_tags: false,
            excluded_tags: Default::default(),
            sensitive_tags: Default::default(),
            created_at: Default::default(),
            allow_without_mention: false,
        }
    }

    #[test]
    fn pixiv_callback_data_format() {
        let cfg = DownloadButtonConfig::pixiv(12345);
        let kb = cfg.build_keyboard().expect("expected keyboard");
        let row = &kb.inline_keyboard[0];
        match &row[0].kind {
            teloxide::types::InlineKeyboardButtonKind::CallbackData(s) => {
                assert_eq!(s, "dl:12345");
            }
            _ => panic!("expected callback data"),
        }
    }

    #[test]
    fn booru_callback_data_format() {
        let cfg = DownloadButtonConfig::booru("yandere", 999);
        let kb = cfg.build_keyboard().expect("expected keyboard");
        let row = &kb.inline_keyboard[0];
        match &row[0].kind {
            teloxide::types::InlineKeyboardButtonKind::CallbackData(s) => {
                assert_eq!(s, "dlb:yandere:999");
            }
            _ => panic!("expected callback data"),
        }
    }

    #[test]
    fn channel_chat_hides_button_for_both_targets() {
        assert!(DownloadButtonConfig::for_pixiv_chat(1, &chat("channel"))
            .build_keyboard()
            .is_none());
        assert!(
            DownloadButtonConfig::for_booru_chat("y", 1, &chat("channel"))
                .build_keyboard()
                .is_none()
        );
        assert!(DownloadButtonConfig::for_pixiv_chat(1, &chat("private"))
            .build_keyboard()
            .is_some());
        assert!(
            DownloadButtonConfig::for_booru_chat("y", 1, &chat("private"))
                .build_keyboard()
                .is_some()
        );
    }

    #[test]
    fn booru_button_is_hidden_when_callback_data_exceeds_telegram_limit() {
        let long_site_name = "a".repeat(61);
        let cfg = DownloadButtonConfig::booru(long_site_name, 1);

        assert!(cfg.build_keyboard().is_none());
    }
}
