use super::collection_input::CollectionSource;
use crate::bot::{notifier::ThrottledBot, BotHandler};
use crate::db::repo::collection_jobs::ResumeResult;
use crate::pixiv::PixivGallery;
use crate::scheduler::get_chat_if_should_notify;
use teloxide::prelude::*;
use tracing::error;

pub(in crate::bot) const COLLECTION_MORE_PREFIX: &str = "col:";

impl CollectionSource {
    pub(super) fn title(&self) -> String {
        match self {
            Self::Pixiv(gallery) => match gallery {
                PixivGallery::Author(id) => format!("Pixiv 作者 {id}"),
                PixivGallery::Bookmarks(id) => format!("Pixiv 公开收藏 {id}"),
                PixivGallery::Collection(id) => format!("Pixiv 合集 {id}"),
                PixivGallery::Pixivision { id, .. } => format!("pixivision {id}"),
                PixivGallery::Ranking { mode, date, .. } => format!("Pixiv {mode} {date}"),
                PixivGallery::Tag(tag) => format!("Pixiv 标签 {tag}"),
            },
            Self::Pool { site, id } => format!("{site} pool {id}"),
        }
    }
}

impl BotHandler {
    pub(super) async fn enqueue_collection_export(
        &self,
        bot: &ThrottledBot,
        chat_id: ChatId,
        user_id: UserId,
        source: CollectionSource,
        telegraph: bool,
    ) -> ResponseResult<()> {
        let result = async {
            let json = serde_json::to_string(&source)?;
            self.repo
                .enqueue_collection_job(chat_id.0, user_id.0 as i64, json, telegraph)
                .await
        }
        .await;
        let text = match result {
            Ok(id) => {
                self.collection_wakeup.notify_one();
                format!(
                    "「{}」已加入后台队列（任务 #{id}），每批最多 50 张。",
                    source.title()
                )
            }
            Err(e) => {
                error!("Failed to queue collection for chat {chat_id}: {e:#}");
                "❌ 无法加入集合导出队列，请稍后重试。".to_string()
            }
        };
        bot.send_message(chat_id, text).await?;
        Ok(())
    }

    pub(crate) async fn handle_collection_more(
        &self,
        bot: &ThrottledBot,
        q: &CallbackQuery,
    ) -> ResponseResult<()> {
        let payload = q
            .data
            .as_deref()
            .and_then(|s| s.strip_prefix(COLLECTION_MORE_PREFIX))
            .and_then(|s| s.split_once(':'))
            .and_then(|(id, batch)| Some((id.parse::<i64>().ok()?, batch.parse::<i64>().ok()?)));
        let result = async {
            if let (Some((id, batch)), Some(message)) = (payload, &q.message) {
                let chat_id = message.chat().id.0;
                if !q.from.is_bot
                    && get_chat_if_should_notify(&self.repo, chat_id)
                        .await?
                        .is_some()
                {
                    return self
                        .repo
                        .resume_collection_job(id, chat_id, q.from.id.0 as i64, batch)
                        .await;
                }
            }
            Ok(ResumeResult::Unavailable)
        }
        .await;
        let text = match result {
            Ok(ResumeResult::Queued) => {
                self.collection_wakeup.notify_one();
                "下一批已加入队列。"
            }
            Ok(ResumeResult::Publishing) => "上一批正在确认，请稍后再按。",
            Ok(ResumeResult::Unavailable) => "此按钮不可用，或已处理。请由原发起人在原聊天中续取。",
            Err(e) => {
                error!("Could not resume collection: {e:#}");
                "暂时无法续取，请稍后再试。"
            }
        };
        bot.answer_callback_query(q.id.clone()).text(text).await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "collection_export_tests.rs"]
mod tests;
