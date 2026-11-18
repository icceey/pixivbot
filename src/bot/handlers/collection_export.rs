use super::collection_input::CollectionSource;
use crate::bot::{notifier::ThrottledBot, BotHandler};
use crate::pixiv::PixivGallery;
use anyhow::{Context, Result};
use teloxide::prelude::*;
use tracing::error;

impl BotHandler {
    async fn collection_images(&self, source: CollectionSource) -> Result<Vec<(String, String)>> {
        match source {
            CollectionSource::Pixiv(gallery) => {
                self.pixiv_client
                    .read()
                    .await
                    .gallery_images(&gallery)
                    .await
            }
            CollectionSource::Pool { site, id } => {
                let site = self
                    .booru_registry
                    .get(&site)
                    .context("Booru site is not configured")?;
                let mut images = Vec::new();
                for post in site.client.pool_images(id).await? {
                    let parsed_url = post
                        .file_url
                        .as_deref()
                        .and_then(|s| url::Url::parse(s).ok());
                    let ext = post
                        .file_ext
                        .as_deref()
                        .or_else(|| {
                            parsed_url
                                .as_ref()?
                                .path_segments()?
                                .next_back()?
                                .rsplit_once('.')
                                .map(|(_, ext)| ext)
                        })
                        .with_context(|| {
                            format!("Missing original file type for post {}", post.id)
                        })?
                        .to_ascii_lowercase();
                    if matches!(
                        ext.as_str(),
                        "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif" | "jxl" | "bmp"
                    ) {
                        let url = post
                            .file_url
                            .with_context(|| format!("Missing original for post {}", post.id))?;
                        images.push((format!("{}.{ext}", post.id), url));
                    }
                }
                Ok(images)
            }
        }
    }

    pub(super) async fn send_collection_export(
        &self,
        bot: &ThrottledBot,
        chat_id: ChatId,
        source: CollectionSource,
        telegraph: bool,
    ) -> ResponseResult<()> {
        if telegraph && self.telegraph.is_none() {
            bot.send_message(chat_id, "❌ Telegraph 未配置，无法生成阅读链接。")
                .await?;
            return Ok(());
        }
        let title = match &source {
            CollectionSource::Pixiv(gallery) => match gallery {
                PixivGallery::Author(id) => format!("Pixiv 作者 {id}"),
                PixivGallery::Bookmarks(id) => format!("Pixiv 公开收藏 {id}"),
                PixivGallery::Collection(id) => format!("Pixiv 合集 {id}"),
                PixivGallery::Pixivision { id, .. } => format!("pixivision {id}"),
                PixivGallery::Ranking { mode, date, .. } => format!("Pixiv {mode} {date}"),
            },
            CollectionSource::Pool { site, id } => format!("{site} pool {id}"),
        };
        bot.send_message(chat_id, format!("正在准备「{title}」…"))
            .await?;
        let images = match self.collection_images(source).await {
            Ok(images) if images.is_empty() => {
                bot.send_message(chat_id, "合集中没有可导出的图片。")
                    .await?;
                return Ok(());
            }
            Ok(images) => images,
            Err(e) => {
                error!("Could not resolve {title}: {e:#}");
                bot.send_message(
                    chat_id,
                    "❌ 无法读取合集的完整图片列表，请检查链接和访问权限。",
                )
                .await?;
                return Ok(());
            }
        };
        let mut files = Vec::with_capacity(images.len());
        let width = images.len().to_string().len();
        for (i, (name, url)) in images.into_iter().enumerate() {
            match self.notifier.get_downloader().download(&url).await {
                Ok(path) => files.push((path, format!("{:0width$}_{name}", i + 1))),
                Err(e) => {
                    error!("Could not download {title} image {}: {e:#}", i + 1);
                    bot.send_message(
                        chat_id,
                        format!("❌ 第 {} 张图片下载失败，已停止导出。", i + 1),
                    )
                    .await?;
                    return Ok(());
                }
            }
        }
        if let Err(e) = self
            .deliver_collection_files(bot, chat_id, &title, &files, telegraph)
            .await
        {
            error!("Could not deliver {title}: {e:#}");
            let message = if telegraph {
                "❌ 发布 Telegraph 失败。"
            } else {
                "❌ 创建或发送压缩包失败，请检查文件大小限制。"
            };
            bot.send_message(chat_id, message).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "collection_export_tests.rs"]
mod tests;
