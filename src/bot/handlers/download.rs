//! Download handler - downloads and sends Pixiv artwork as files
//!
//! Supports:
//! - /download <url|id>
//! - /download (as reply to bot message)

use crate::bot::link_handler::{parse_pixiv_links, PixivLink};
use crate::bot::BotHandler;
use anyhow::{Context, Result};
use chrono::Local;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use teloxide::prelude::*;
use teloxide::types::{ChatAction, InputFile, MessageEntityKind, MessageEntityRef, ParseMode};
use teloxide::utils::markdown;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

/// Page number prefix for multi-page artworks in filenames
const PAGE_PREFIX: &str = "p";

impl BotHandler {
    /// Handle /download command
    ///
    /// Priority: Command arguments > Reply message
    /// - Parse arguments for URLs/IDs
    /// - If reply, parse message entities (TextLink and Url) for hidden links
    /// - Deduplicate IDs
    /// - Download images and send as files (single or ZIP)
    pub async fn handle_download(
        &self,
        bot: Bot,
        msg: Message,
        chat_id: ChatId,
        args: String,
    ) -> ResponseResult<()> {
        info!("Processing /download command from chat {}", chat_id);

        // Extract Pixiv IDs from arguments or reply message
        let illust_ids = self.extract_illust_ids(&msg, &args).await;

        if illust_ids.is_empty() {
            bot.send_message(
                chat_id,
                "❌ 请提供作品 ID 或 URL，或回复包含作品链接的消息\n\n例如：\n\
                 • `/download 123456789`\n\
                 • `/download https://www.pixiv.net/artworks/123456789`\n\
                 • 回复包含链接的消息并使用 `/download`",
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
            return Ok(());
        }

        info!("Found {} unique illust IDs to download", illust_ids.len());

        // Spawn background task to keep chat action alive
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

        // Process downloads
        let result = self
            .process_downloads(bot.clone(), chat_id, illust_ids)
            .await;

        // Stop the chat action task
        action_task.abort();

        result
    }

    /// Extract illust IDs from command arguments or reply message
    async fn extract_illust_ids(&self, msg: &Message, args: &str) -> Vec<u64> {
        let mut ids = HashSet::new();

        // Priority 1: Parse from command arguments
        if !args.trim().is_empty() {
            // Try parsing as direct ID first
            if let Ok(id) = args.trim().parse::<u64>() {
                ids.insert(id);
            } else {
                // Try parsing as URL
                for link in parse_pixiv_links(args) {
                    if let PixivLink::Illust(id) = link {
                        ids.insert(id);
                    }
                }
            }
        }

        // Priority 2: Parse from reply message
        if ids.is_empty() {
            if let Some(reply_msg) = msg.reply_to_message() {
                // Parse text content for links
                if let Some(text) = reply_msg.text().or_else(|| reply_msg.caption()) {
                    for link in parse_pixiv_links(text) {
                        if let PixivLink::Illust(id) = link {
                            ids.insert(id);
                        }
                    }
                }

                // Parse message entities for hidden links (TextLink and Url)
                let entities: Vec<MessageEntityRef<'_>> = reply_msg
                    .parse_entities()
                    .into_iter()
                    .flatten()
                    .chain(reply_msg.parse_caption_entities().into_iter().flatten())
                    .collect();

                for entity in entities {
                    match &entity.kind() {
                        MessageEntityKind::TextLink { url } => {
                            for link in parse_pixiv_links(url.as_str()) {
                                if let PixivLink::Illust(id) = link {
                                    ids.insert(id);
                                }
                            }
                        }
                        MessageEntityKind::Url => {
                            if let Some(text) = reply_msg.text() {
                                // text.get() safely handles out-of-bounds ranges
                                if let Some(url_text) = text.get(entity.range()) {
                                    for link in parse_pixiv_links(url_text) {
                                        if let PixivLink::Illust(id) = link {
                                            ids.insert(id);
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        ids.into_iter().collect()
    }

    /// Process downloads for multiple illusts
    async fn process_downloads(
        &self,
        bot: Bot,
        chat_id: ChatId,
        illust_ids: Vec<u64>,
    ) -> ResponseResult<()> {
        let mut failed_ids = Vec::new();
        let mut all_files: Vec<(PathBuf, String)> = Vec::new(); // (path, sanitized_filename)
        let mut work_info: Vec<(String, String)> = Vec::new(); // (title, artist)

        // Download all illusts
        for illust_id in &illust_ids {
            match self.download_illust(*illust_id).await {
                Ok((files, title, artist)) => {
                    all_files.extend(files);
                    work_info.push((title, artist));
                }
                Err(e) => {
                    error!("Failed to download illust {}: {:#}", illust_id, e);
                    failed_ids.push(*illust_id);
                }
            }
        }

        if all_files.is_empty() {
            bot.send_message(chat_id, "❌ 所有作品下载失败").await?;
            return Ok(());
        }

        // Build caption with work info and errors
        let caption = self.build_download_caption(&work_info, &failed_ids);

        // Send files based on threshold
        let threshold = self.download_original_threshold as usize;
        if all_files.len() <= threshold {
            // Within threshold - send each file separately
            let mut first = true;
            for (path, filename) in &all_files {
                // Only show caption on first file
                let cap = if first {
                    first = false;
                    caption.as_str()
                } else {
                    ""
                };
                if let Err(e) = self.send_document(&bot, chat_id, path, filename, cap).await {
                    error!("Failed to send document {}: {:#}", filename, e);
                    bot.send_message(chat_id, "❌ 发送文件失败").await?;
                    break;
                }
                // Small delay between files to avoid rate limiting
                if all_files.len() > 1 {
                    sleep(Duration::from_millis(500)).await;
                }
            }
        } else {
            // Exceeds threshold - create ZIP and send
            match self.create_zip_file(&all_files).await {
                Ok(zip_path) => {
                    let zip_filename =
                        format!("pixiv_{}_works.zip", Local::now().format("%Y%m%d_%H%M%S"));
                    if let Err(e) = self
                        .send_document(&bot, chat_id, &zip_path, &zip_filename, &caption)
                        .await
                    {
                        error!("Failed to send document: {:#}", e);
                        bot.send_message(chat_id, "❌ 发送文件失败").await?;
                    }

                    // Clean up temp ZIP file
                    if let Err(e) = tokio::fs::remove_file(&zip_path).await {
                        warn!("Failed to remove temp ZIP file: {:#}", e);
                    }
                }
                Err(e) => {
                    error!("Failed to create ZIP file: {:#}", e);
                    bot.send_message(chat_id, "❌ 创建压缩文件失败").await?;
                }
            }
        }

        Ok(())
    }

    /// Download a single illust and return file paths with metadata
    async fn download_illust(
        &self,
        illust_id: u64,
    ) -> Result<(Vec<(PathBuf, String)>, String, String)> {
        info!("Downloading illust {}", illust_id);

        // Get illust details
        let pixiv = self.pixiv_client.read().await;
        let illust = pixiv
            .get_illust_detail(illust_id)
            .await
            .context("Failed to fetch illust details")?;
        drop(pixiv);

        let title = illust.title.clone();
        let artist = illust.user.name.clone();
        let urls = illust.get_all_image_urls();

        // Download all pages
        let downloader = &self.notifier.get_downloader();
        let mut files = Vec::new();

        for (page_idx, url) in urls.iter().enumerate() {
            match downloader.download(url).await {
                Ok(local_path) => {
                    // Extract extension from URL
                    let ext = url
                        .rsplit('.')
                        .next()
                        .and_then(|s| s.split('?').next())
                        .unwrap_or("jpg");

                    // Create sanitized filename
                    let sanitized_title = sanitize_filename(&title);
                    let filename = if urls.len() > 1 {
                        format!(
                            "{}_{}_{}{}.{}",
                            sanitized_title, illust_id, PAGE_PREFIX, page_idx, ext
                        )
                    } else {
                        format!("{}_{}.{}", sanitized_title, illust_id, ext)
                    };

                    files.push((local_path, filename));
                }
                Err(e) => {
                    warn!(
                        "Failed to download page {} of illust {}: {:#}",
                        page_idx, illust_id, e
                    );
                }
            }
        }

        if files.is_empty() {
            anyhow::bail!("All pages failed to download");
        }

        Ok((files, title, artist))
    }

    /// Create a ZIP file from multiple files
    async fn create_zip_file(&self, files: &[(PathBuf, String)]) -> Result<PathBuf> {
        let temp_dir = std::env::temp_dir();
        let zip_filename = format!(
            "pixivbot_download_{}.zip",
            Local::now().format("%Y%m%d_%H%M%S%3f")
        );
        let zip_path = temp_dir.join(zip_filename);

        // Clone data needed for the blocking task
        let files_clone: Vec<(PathBuf, String)> = files.to_vec();
        let zip_path_clone = zip_path.clone();

        // Run synchronous ZIP operations in a blocking task
        tokio::task::spawn_blocking(move || {
            let zip_file =
                std::fs::File::create(&zip_path_clone).context("Failed to create ZIP file")?;
            let mut zip = zip::ZipWriter::new(zip_file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);

            for (local_path, filename) in files_clone {
                zip.start_file(&filename, options)
                    .context("Failed to start ZIP file entry")?;
                let file_data = std::fs::read(&local_path)
                    .context(format!("Failed to read file {:?}", local_path))?;
                zip.write_all(&file_data)
                    .context("Failed to write to ZIP")?;
            }

            zip.finish().context("Failed to finalize ZIP")?;
            Ok::<PathBuf, anyhow::Error>(zip_path_clone)
        })
        .await
        .context("ZIP creation task panicked")?
    }

    /// Send a document file
    async fn send_document(
        &self,
        bot: &Bot,
        chat_id: ChatId,
        path: &Path,
        filename: &str,
        caption: &str,
    ) -> Result<()> {
        let input_file = InputFile::file(path).file_name(filename.to_string());

        bot.send_document(chat_id, input_file)
            .caption(caption)
            .parse_mode(ParseMode::MarkdownV2)
            .await
            .context("Failed to send document")?;

        Ok(())
    }

    /// Build caption with work info and error report
    fn build_download_caption(&self, work_info: &[(String, String)], failed_ids: &[u64]) -> String {
        let mut caption = String::from("📥 *下载完成*\n\n");

        // Add work info
        if work_info.len() == 1 {
            let (title, artist) = &work_info[0];
            caption.push_str(&format!(
                "🎨 {}\nby *{}*\n",
                markdown::escape(title),
                markdown::escape(artist)
            ));
        } else if !work_info.is_empty() {
            caption.push_str(&format!("📦 包含 {} 个作品\n", work_info.len()));
        }

        // Add error report
        if !failed_ids.is_empty() {
            caption.push_str("\n⚠️ *部分作品下载失败*\n");
            for id in failed_ids {
                caption.push_str(&format!("• ID: `{}`\n", id));
            }
        }

        caption
    }
}

/// Sanitize filename by replacing illegal filesystem characters with underscore
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename("normal_title"), "normal_title");
        assert_eq!(sanitize_filename("title:with/slash"), "title_with_slash");
        assert_eq!(sanitize_filename("title<>:\""), "title____");
        assert_eq!(
            sanitize_filename("title|with?many*bad\\chars"),
            "title_with_many_bad_chars"
        );
    }
}
