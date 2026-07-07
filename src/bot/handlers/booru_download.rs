use super::download::sanitize_filename;
use crate::bot::link_handler::BooruPostRef;
use crate::bot::notifier::ThrottledBot;
use crate::bot::BotHandler;
use anyhow::{Context, Result};
use std::future::Future;
use std::path::{Path, PathBuf};
use teloxide::prelude::*;
use teloxide::types::ChatAction;
use teloxide::utils::markdown;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

impl BotHandler {
    pub async fn handle_booru_download_callback(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        site_name: String,
        post_id: u64,
    ) -> ResponseResult<()> {
        info!(
            "Processing booru download callback {}#{} in chat {}",
            site_name, post_id, chat_id
        );

        let bot_clone = bot.clone();
        let action_task = tokio::spawn(async move {
            loop {
                if bot_clone
                    .send_chat_action(chat_id, ChatAction::UploadDocument)
                    .await
                    .is_err()
                {
                    break;
                }
                sleep(Duration::from_secs(4)).await;
            }
        });

        let result = self
            .process_booru_downloads(
                bot.clone(),
                chat_id,
                vec![BooruPostRef { site_name, post_id }],
            )
            .await;

        action_task.abort();
        result
    }

    pub(super) async fn process_booru_downloads(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        refs: Vec<BooruPostRef>,
    ) -> ResponseResult<()> {
        let mut files: Vec<(PathBuf, String)> = Vec::new();
        let mut titles: Vec<String> = Vec::new();
        let mut failed: Vec<String> = Vec::new();

        for r in &refs {
            match self.download_booru_post(&r.site_name, r.post_id).await {
                Ok((path, name, title)) => {
                    files.push((path, name));
                    titles.push(title);
                }
                Err(e) => {
                    error!(
                        "Failed to download booru post {}#{}: {:#}",
                        r.site_name, r.post_id, e
                    );
                    failed.push(format!("{}#{}", r.site_name, r.post_id));
                }
            }
        }

        if files.is_empty() {
            bot.send_message(chat_id, "❌ 下载失败").await?;
            return Ok(());
        }

        let caption = build_booru_caption(&titles, &failed);

        if files.len() <= self.download_original_threshold as usize {
            for (i, (path, name)) in files.iter().enumerate() {
                let cap = if i == 0 { caption.as_str() } else { "" };
                if let Err(e) = self.send_document(&bot, chat_id, path, name, cap).await {
                    warn!("Failed to send booru document {}: {:#}", name, e);
                }
            }
        } else {
            match self.create_zip_file(&files).await {
                Ok(zip_path) => {
                    let zip_name = format!(
                        "booru_{}_files_{}.zip",
                        files.len(),
                        chrono::Local::now().format("%Y%m%d_%H%M%S")
                    );
                    let send_result = remove_file_after(
                        &zip_path,
                        self.send_document(&bot, chat_id, &zip_path, &zip_name, &caption),
                    )
                    .await;
                    if let Err(e) = send_result {
                        warn!("Failed to send booru zip: {:#}", e);
                    }
                }
                Err(e) => {
                    error!("Failed to create booru zip: {:#}", e);
                    bot.send_message(chat_id, "❌ 打包失败").await?;
                }
            }
        }

        Ok(())
    }

    async fn download_booru_post(
        &self,
        site_name: &str,
        post_id: u64,
    ) -> Result<(PathBuf, String, String)> {
        let site = self
            .booru_registry
            .get(site_name)
            .with_context(|| format!("booru site '{}' is not configured", site_name))?;

        let posts = site
            .client
            .get_posts(&format!("id:{}", post_id), 1, 1)
            .await
            .with_context(|| format!("fetch booru post {}#{}", site_name, post_id))?;

        let post = posts
            .into_iter()
            .next()
            .with_context(|| format!("post {}#{} not found", site_name, post_id))?;

        let urls = booru_post_image_urls(&post);
        if urls.is_empty() {
            anyhow::bail!("post {}#{} has no downloadable url", site_name, post_id);
        }

        let downloader = self.notifier.get_downloader();
        let mut downloaded = None;
        let mut last_error = None;
        for url in urls {
            match downloader.download(url).await {
                Ok(path) => {
                    downloaded = Some((path, url.to_string()));
                    break;
                }
                Err(e) => {
                    warn!("Failed to download booru image {}: {:#}", url, e);
                    last_error = Some(e);
                }
            }
        }

        let (path, url) = downloaded
            .ok_or_else(|| last_error.unwrap_or_else(|| anyhow::anyhow!("no downloadable url")))?;

        let ext = url
            .rsplit('/')
            .next()
            .and_then(|seg| seg.split('?').next())
            .and_then(|seg| seg.rsplit_once('.').map(|(_, e)| e))
            .filter(|e| !e.is_empty() && e.len() <= 5)
            .unwrap_or("bin")
            .to_string();

        let safe_site = sanitize_filename(site_name);
        let filename = format!("{}_{}.{}", safe_site, post_id, ext);
        let title = format!("{} #{}", site_name, post_id);
        Ok((path, filename, title))
    }
}

fn booru_post_image_urls(post: &booru_client::BooruPost) -> Vec<&str> {
    // Downloads prefer the original file; jpeg_url is the only fallback.
    // sample_url is a downscaled variant and preview_url is only a thumbnail.
    [post.file_url.as_deref(), post.jpeg_url.as_deref()]
        .into_iter()
        .flatten()
        .collect()
}

async fn remove_file_after<T, E, Fut>(path: &Path, operation: Fut) -> std::result::Result<T, E>
where
    Fut: Future<Output = std::result::Result<T, E>>,
{
    let result = operation.await;
    if let Err(e) = tokio::fs::remove_file(path).await {
        warn!("Failed to remove temp ZIP file: {:#}", e);
    }
    result
}

fn build_booru_caption(titles: &[String], failed: &[String]) -> String {
    let mut s = String::from("📥 *下载完成*\n\n");
    if titles.len() == 1 {
        s.push_str(&format!("🖼 {}\n", markdown::escape(&titles[0])));
    } else if !titles.is_empty() {
        s.push_str(&format!("📦 包含 {} 个文件\n", titles.len()));
    }
    if !failed.is_empty() {
        s.push_str("\n⚠️ *部分文件下载失败*\n");
        for f in failed {
            s.push_str(&format!("• `{}`\n", markdown::escape(f)));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::{booru_post_image_urls, remove_file_after};
    use booru_client::{BooruPost, BooruRating};

    fn make_post() -> BooruPost {
        BooruPost {
            id: 1,
            tags: String::new(),
            score: 0,
            fav_count: 0,
            file_url: Some("file".to_string()),
            sample_url: Some("sample".to_string()),
            jpeg_url: Some("jpeg".to_string()),
            preview_url: Some("preview".to_string()),
            rating: BooruRating::Safe,
            width: 1,
            height: 1,
            md5: None,
            source: None,
            created_at: None,
            file_size: None,
            file_ext: None,
            status: None,
        }
    }

    #[test]
    fn booru_download_url_priority_prefers_file_then_jpeg() {
        let post = make_post();
        assert_eq!(booru_post_image_urls(&post), ["file", "jpeg"]);
    }

    #[test]
    fn booru_download_url_priority_accepts_jpeg_only_posts() {
        let mut post = make_post();
        post.sample_url = None;
        post.file_url = None;
        post.preview_url = None;

        assert_eq!(booru_post_image_urls(&post), ["jpeg"]);
    }

    #[tokio::test]
    async fn remove_file_after_cleans_zip_after_successful_send() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let path = temp.into_temp_path().to_path_buf();
        tokio::fs::write(&path, b"zip data").await.unwrap();

        remove_file_after(&path, async { Ok::<_, anyhow::Error>(()) })
            .await
            .unwrap();

        assert!(!path.exists());
    }
}
