use super::caption::{individual_batch_caption, shared_batch_caption, CaptionStrategy};
use super::{ContinuationNumbering, Notifier};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardMarkup, InputFile, InputMedia, InputMediaPhoto, ParseMode};

impl Notifier {
    /// 底层发送：构建 InputMedia 并调用 API，返回第一条消息的ID
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn send_media_batch(
        &self,
        chat_id: ChatId,
        paths: &[PathBuf],
        strategy: &CaptionStrategy<'_>,
        batch_captions: Option<&[String]>,
        has_spoiler: bool,
        batch_idx: usize,
        continuation_numbering: ContinuationNumbering,
        silent: bool,
    ) -> Result<Option<i32>> {
        let media_group: Vec<InputMedia> = paths
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let mut photo = InputMediaPhoto::new(InputFile::file(path));

                let caption_text = match strategy {
                    CaptionStrategy::Shared(base_cap) => {
                        shared_batch_caption(*base_cap, i, batch_idx, continuation_numbering)
                    }
                    CaptionStrategy::Individual(_) => {
                        if let Some(caps) = batch_captions {
                            individual_batch_caption(&caps[i], i, batch_idx, continuation_numbering)
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
        let messages = req.await.context("Send media group failed")?;
        Ok(messages.first().map(|m| m.id.0))
    }

    pub(super) async fn send_photo_file_with_id(
        &self,
        chat_id: ChatId,
        path: &Path,
        caption: Option<&str>,
        has_spoiler: bool,
        keyboard: Option<InlineKeyboardMarkup>,
    ) -> Result<i32> {
        let mut req = self.bot.send_photo(chat_id, InputFile::file(path));
        if let Some(c) = caption {
            req = req.caption(c).parse_mode(ParseMode::MarkdownV2);
        }
        if has_spoiler {
            req = req.has_spoiler(true);
        }
        if let Some(kb) = keyboard {
            req = req.reply_markup(kb);
        }
        let message = req.await.context("Send photo failed")?;
        Ok(message.id.0)
    }

    /// 发送动画 (MP4/GIF) 文件并返回消息ID
    pub(super) async fn send_animation_file(
        &self,
        chat_id: ChatId,
        path: &Path,
        caption: Option<&str>,
        has_spoiler: bool,
        keyboard: Option<InlineKeyboardMarkup>,
    ) -> Result<i32> {
        let mut req = self.bot.send_animation(chat_id, InputFile::file(path));
        if let Some(c) = caption {
            req = req.caption(c).parse_mode(ParseMode::MarkdownV2);
        }
        if has_spoiler {
            req = req.has_spoiler(true);
        }
        if let Some(kb) = keyboard {
            req = req.reply_markup(kb);
        }
        let message = req.await.context("Send animation failed")?;
        Ok(message.id.0)
    }
}
