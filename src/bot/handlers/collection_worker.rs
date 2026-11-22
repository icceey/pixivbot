use super::{collection_export::COLLECTION_MORE_PREFIX, collection_input::CollectionSource};
use crate::booru::pool_image_batch;
use crate::bot::{notifier::ThrottledBot, BotHandler};
use crate::db::repo::collection_jobs::Model;
use crate::pixiv::PixivGallery;
use crate::scheduler::get_chat_if_should_notify;
use anyhow::{ensure, Context, Result};
use futures_util::{stream, StreamExt, TryStreamExt};
use std::time::Duration;
use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup},
};
use tracing::error;

const BATCH_SIZE: usize = 50;
const IMAGE_DOWNLOAD_CONCURRENCY: usize = 4;

impl BotHandler {
    pub(crate) async fn recover_collection_exports(&self, bot: &ThrottledBot) -> Result<()> {
        for job in self.repo.recover_collection_jobs().await? {
            self.notify_collection_problem(
                bot,
                &job,
                "发布时服务已重启，本批结果未确认，请核对聊天记录；不会自动重发。",
            )
            .await;
        }
        Ok(())
    }

    async fn notify_collection_problem(&self, bot: &ThrottledBot, job: &Model, text: &str) {
        let result: Result<()> = async {
            if get_chat_if_should_notify(&self.repo, job.chat_id)
                .await?
                .is_some()
            {
                bot.send_message(
                    ChatId(job.chat_id),
                    format!("❌ 集合任务 #{}：{text}", job.id),
                )
                .await?;
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            error!("Could not notify chat about collection {}: {e:#}", job.id);
        }
    }

    pub(crate) async fn run_collection_worker(&self, bot: ThrottledBot) {
        loop {
            match self.repo.claim_collection_job().await {
                Ok(Some(job)) => {
                    if let Err(e) = self.process_collection_batch(&bot, &job).await {
                        error!(
                            "Collection job {} batch {} failed: {e:#}",
                            job.id, job.generation
                        );
                        if let Err(e) = self.repo.fail_collection_job(job.id).await {
                            error!("Could not close failed collection {}: {e:#}", job.id);
                        }
                        self.notify_collection_problem(
                            &bot,
                            &job,
                            "本批未能完成；请核对已收到的结果，再决定是否重新导出。",
                        )
                        .await;
                    }
                }
                Ok(None) => self.collection_wakeup.notified().await,
                Err(e) => {
                    error!("Failed to claim a collection batch: {e:#}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn process_collection_batch(&self, bot: &ThrottledBot, job: &Model) -> Result<()> {
        let source: CollectionSource = serde_json::from_str(&job.source)?;
        let ranking_remaining = match &source {
            CollectionSource::Pixiv(PixivGallery::Ranking { limit, .. }) => {
                Some(limit - job.delivered as usize)
            }
            _ => None,
        };
        let limit = ranking_remaining.unwrap_or(BATCH_SIZE).min(BATCH_SIZE);
        let raw_cursor = job.cursor.as_deref();
        let (images, mut next) = match &source {
            CollectionSource::Pixiv(gallery) => {
                let client = self.pixiv_client.read().await.clone();
                let cursor: Option<crate::pixiv::GalleryCursor> =
                    raw_cursor.map(serde_json::from_str).transpose()?;
                let (images, next) = client.gallery_image_batch(gallery, cursor, limit).await?;
                (images, next.map(|c| serde_json::to_string(&c)).transpose()?)
            }
            CollectionSource::Pool { site, id } => {
                let cursor: Option<crate::booru::PoolCursor> =
                    raw_cursor.map(serde_json::from_str).transpose()?;
                let (images, next) =
                    pool_image_batch(&self.booru_registry, site, *id, cursor, limit).await?;
                (images, next.map(|c| serde_json::to_string(&c)).transpose()?)
            }
        };
        if ranking_remaining == Some(images.len()) {
            next = None;
        }
        let keyboard = next.as_ref().map(|_| {
            InlineKeyboardMarkup::new([[InlineKeyboardButton::callback(
                if job.telegraph {
                    "继续生成下一批"
                } else {
                    "下载更多"
                },
                format!("{COLLECTION_MORE_PREFIX}{}:{}", job.id, job.generation),
            )]])
        });
        let title = source.title();
        let end = job.delivered + images.len() as i64;
        if images.is_empty() {
            self.begin_collection_delivery(job).await?;
            if let Some(markup) = keyboard {
                bot.send_message(
                    ChatId(job.chat_id),
                    format!("「{title}」：本段没有静态图片，可继续查找。"),
                )
                .reply_markup(markup)
                .await?;
            } else {
                bot.send_message(
                    ChatId(job.chat_id),
                    format!("「{title}」：没有更多可导出的静态图片。"),
                )
                .await?;
            }
        } else {
            let width = end.to_string().len();
            let first = job.delivered + 1;
            let downloader = self.notifier.get_downloader();
            let files: Vec<_> = stream::iter(images.into_iter().enumerate())
                .map(|(i, (name, url))| async move {
                    let n = first + i as i64;
                    let path = downloader
                        .download(&url)
                        .await
                        .with_context(|| format!("Download collection image {n}"))?;
                    Ok::<_, anyhow::Error>((path, format!("{n:0width$}_{name}")))
                })
                .buffered(IMAGE_DOWNLOAD_CONCURRENCY)
                .try_collect()
                .await?;
            self.deliver_collection_files(
                bot,
                job,
                &format!("{title}（第 {first}–{end} 张）"),
                &files,
                keyboard,
            )
            .await?;
        }
        self.repo.finish_collection_batch(job.id, end, next).await
    }

    pub(super) async fn begin_collection_delivery(&self, job: &Model) -> Result<()> {
        ensure!(
            get_chat_if_should_notify(&self.repo, job.chat_id)
                .await?
                .is_some(),
            "Collection chat is no longer accessible"
        );
        self.repo.mark_collection_publishing(job.id).await
    }
}
