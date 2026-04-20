use super::download::sanitize_filename;
use crate::bot::link_handler::BooruPostRef;
use crate::bot::notifier::ThrottledBot;
use crate::bot::BotHandler;
use anyhow::{Context, Result};
use std::path::PathBuf;
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
                    if let Err(e) = self
                        .send_document(&bot, chat_id, &zip_path, &zip_name, &caption)
                        .await
                    {
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

        let url = post
            .file_url
            .clone()
            .or_else(|| post.sample_url.clone())
            .or_else(|| post.preview_url.clone())
            .with_context(|| format!("post {}#{} has no downloadable url", site_name, post_id))?;

        let path = self
            .notifier
            .get_downloader()
            .download(&url)
            .await
            .with_context(|| format!("download booru image {}", url))?;

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
