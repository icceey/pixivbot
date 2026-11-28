mod delivery;

use crate::booru::{pool_image_batch, BooruSiteRegistry};
use crate::bot::{notifier::ThrottledBot, COLLECTION_MORE_PREFIX};
use crate::db::repo::{
    collection_jobs::{CollectionRetryStage, Model},
    Repo,
};
use crate::pixiv::{client::PixivClient, downloader::Downloader, PixivGallery};
use crate::scheduler::get_chat_if_should_notify;
use anyhow::{ensure, Context, Result};
use futures_util::{stream, StreamExt, TryStreamExt};
use media_publish::TelegraphPublisher;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup},
};
use tracing::{error, warn};

impl std::fmt::Display for CollectionRetryStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Preparing => "Collection preparation failed",
            Self::UploadReturned => "Image upload failed before Telegraph page creation",
        })
    }
}

const BATCH_SIZE: usize = 50;
const IMAGE_DOWNLOAD_CONCURRENCY: usize = 4;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum CollectionSource {
    Pixiv(PixivGallery),
    Pool { site: String, id: u64 },
}

impl CollectionSource {
    pub(crate) fn title(&self) -> String {
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

pub struct CollectionEngineConfig {
    pub repo: Arc<Repo>,
    pub bot: ThrottledBot,
    pub pixiv_client: PixivClient,
    pub downloader: Arc<Downloader>,
    pub booru_registry: Arc<BooruSiteRegistry>,
    pub telegraph: Option<TelegraphPublisher>,
    pub max_retry_count: u8,
    pub wakeup: Arc<tokio::sync::Notify>,
}

pub struct CollectionEngine {
    repo: Arc<Repo>,
    bot: ThrottledBot,
    pixiv_client: PixivClient,
    downloader: Arc<Downloader>,
    booru_registry: Arc<BooruSiteRegistry>,
    telegraph: Option<TelegraphPublisher>,
    max_retry_count: u8,
    wakeup: Arc<tokio::sync::Notify>,
}

impl CollectionEngine {
    pub fn new(config: CollectionEngineConfig) -> Self {
        Self {
            repo: config.repo,
            bot: config.bot,
            pixiv_client: config.pixiv_client,
            downloader: config.downloader,
            booru_registry: config.booru_registry,
            telegraph: config.telegraph,
            max_retry_count: config.max_retry_count,
            wakeup: config.wakeup,
        }
    }

    pub async fn recover(&self) -> Result<()> {
        for job in self.repo.recover_collection_jobs().await? {
            self.notify_collection_problem(
                &job,
                "发布时服务已重启，本批结果未确认，请核对聊天记录；不会自动重发。",
            )
            .await;
        }
        Ok(())
    }

    async fn notify_collection_problem(&self, job: &Model, text: &str) {
        let result: Result<()> = async {
            if get_chat_if_should_notify(&self.repo, job.chat_id)
                .await?
                .is_some()
            {
                self.bot
                    .send_message(
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

    pub async fn run(self) {
        loop {
            match self.repo.claim_collection_job().await {
                Ok(Some(job)) => {
                    if let Err(e) = self.process_collection_batch(&job).await {
                        self.handle_collection_failure(&job, e).await;
                    }
                }
                Ok(None) => match self.repo.next_collection_retry_at().await {
                    Ok(Some(deadline)) => {
                        let delay = (deadline - chrono::Utc::now().timestamp()).max(0) as u64;
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(delay)) => (),
                            _ = self.wakeup.notified() => (),
                        }
                    }
                    Ok(None) => self.wakeup.notified().await,
                    Err(e) => {
                        error!("Failed to read next collection retry deadline: {e:#}");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                },
                Err(e) => {
                    error!("Failed to claim a collection batch: {e:#}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn handle_collection_failure(&self, job: &Model, failure: anyhow::Error) {
        let notice = if let Some(&stage) = failure.downcast_ref::<CollectionRetryStage>() {
            match self
                .repo
                .schedule_collection_retry(job, stage, self.max_retry_count)
                .await
            {
                Ok(true) => {
                    warn!(
                        "Collection job {} batch {} scheduled retry {}: {failure:#}",
                        job.id,
                        job.generation,
                        job.retry_count + 1
                    );
                    return;
                }
                Ok(false) => "本批已达到重试上限，导出已停止。",
                Err(e) => {
                    error!(
                        "Could not schedule retry for collection {}: {e:#}; batch error: {failure:#}",
                        job.id
                    );
                    self.notify_collection_problem(job, "未能确认重试状态，请先核对聊天中的结果。")
                        .await;
                    return;
                }
            }
        } else {
            if let Err(e) = self.repo.fail_collection_job(job.id).await {
                error!("Could not close failed collection {}: {e:#}", job.id);
            }
            "本批未能完成；请核对已收到的结果，再决定是否重新导出。"
        };
        error!(
            "Collection job {} batch {} stopped: {failure:#}",
            job.id, job.generation
        );
        self.notify_collection_problem(job, notice).await;
    }

    async fn process_collection_batch(&self, job: &Model) -> Result<()> {
        if job.retry_count > 0 {
            ensure!(
                get_chat_if_should_notify(&self.repo, job.chat_id)
                    .await?
                    .is_some(),
                "Collection chat is no longer accessible"
            );
        }
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
                let cursor: Option<crate::pixiv::GalleryCursor> =
                    raw_cursor.map(serde_json::from_str).transpose()?;
                let (images, next) = self
                    .pixiv_client
                    .gallery_image_batch(gallery, cursor, limit)
                    .await
                    .context(CollectionRetryStage::Preparing)?;
                (images, next.map(|c| serde_json::to_string(&c)).transpose()?)
            }
            CollectionSource::Pool { site, id } => {
                let cursor: Option<crate::booru::PoolCursor> =
                    raw_cursor.map(serde_json::from_str).transpose()?;
                let (images, next) =
                    pool_image_batch(&self.booru_registry, site, *id, cursor, limit)
                        .await
                        .map_err(|e| {
                            if e.is::<booru_client::Error>() {
                                e.context(CollectionRetryStage::Preparing)
                            } else {
                                e
                            }
                        })?;
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
                self.bot
                    .send_message(
                        ChatId(job.chat_id),
                        format!("「{title}」：本段没有静态图片，可继续查找。"),
                    )
                    .reply_markup(markup)
                    .await?;
            } else {
                self.bot
                    .send_message(
                        ChatId(job.chat_id),
                        format!("「{title}」：没有更多可导出的静态图片。"),
                    )
                    .await?;
            }
        } else {
            let width = end.to_string().len();
            let first = job.delivered + 1;
            let downloader = &self.downloader;
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
                .await
                .context(CollectionRetryStage::Preparing)?;
            self.deliver_collection_files(
                job,
                &format!("{title}（第 {first}–{end} 张）"),
                &files,
                keyboard,
            )
            .await?;
        }
        self.repo.finish_collection_batch(job.id, end, next).await
    }

    async fn begin_collection_delivery(&self, job: &Model) -> Result<()> {
        ensure!(
            get_chat_if_should_notify(&self.repo, job.chat_id)
                .await?
                .is_some(),
            "Collection chat is no longer accessible"
        );
        self.repo.mark_collection_publishing(job.id).await
    }
}
