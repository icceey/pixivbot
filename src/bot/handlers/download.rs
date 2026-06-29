//! Download handler - downloads and sends Pixiv artwork as files
//!
//! Supports:
//! - /download <url|id>
//! - /download (as reply to bot message)

use crate::bot::link_handler::{
    parse_booru_post_links, parse_pixiv_links, BooruPostRef, PixivLink,
};
use crate::bot::notifier::ThrottledBot;
use crate::bot::BotHandler;
use anyhow::{Context, Result};
use chrono::Local;
use regex::Regex;
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
        bot: ThrottledBot,
        msg: Message,
        chat_id: ChatId,
        args: String,
    ) -> ResponseResult<()> {
        info!("Processing /download command from chat {}", chat_id);

        let (illust_ids, booru_refs) = self.extract_targets(&msg, &args).await;

        // Check for e-hentai/exhentai gallery links
        let eh_galleries = self.extract_eh_galleries(&msg, &args);

        // Reject multiple EH gallery links
        if eh_galleries.len() > 1 {
            bot.send_message(
                chat_id,
                "❌ 一次只能处理一个 E-Hentai 链接，请使用 /edl <url>。",
            )
            .await?;
            return Ok(());
        }

        // Reject mixed EH + Pixiv/Booru targets
        if eh_galleries.len() == 1 && (!illust_ids.is_empty() || !booru_refs.is_empty()) {
            bot.send_message(
                chat_id,
                "❌ 请不要把 E-Hentai 链接和 Pixiv/Booru 链接混在同一次 /download 中；E-Hentai 请使用 /edl <url>。",
            )
            .await?;
            return Ok(());
        }

        if illust_ids.is_empty() && booru_refs.is_empty() && eh_galleries.is_empty() {
            bot.send_message(
                chat_id,
                "❌ 请提供作品 ID 或 URL，或回复包含作品链接的消息\n\n例如：\n\
                 • `/download 123456789`\n\
                 • `/download https://www.pixiv.net/artworks/123456789`\n\
                 • `/download https://e-hentai.org/g/12345/token/`\n\
                 • `/download https://yande.re/post/show/123456`（需先在配置中启用）\n\
                 • 回复包含链接的消息并使用 `/download`",
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
            return Ok(());
        }

        // Handle eh gallery download via the download queue
        if let Some((gid, token)) = eh_galleries.into_iter().next() {
            if let Some(eh_client) = &self.eh_client {
                let metadata = eh_client.get_metadata(&[(gid, &token)]).await;
                let title = match &metadata {
                    Ok(m) if !m.is_empty() => m[0].title.clone(),
                    _ => format!("gallery_{}", gid),
                };
                if let Err(e) = self
                    .repo
                    .enqueue_eh_download(
                        chat_id.0,
                        gid as i64,
                        &token,
                        &title,
                        false,
                        crate::db::repo::eh_download_queue::SOURCE_DIRECT,
                    )
                    .await
                {
                    error!("Failed to enqueue eh download from /download: {:#}", e);
                    bot.send_message(chat_id, "❌ 加入 E-Hentai 下载队列失败")
                        .await?;
                } else {
                    bot.send_message(chat_id, "⏳ 已加入 E-Hentai 下载队列")
                        .await?;
                }
                return Ok(());
            }
        }

        info!(
            "Found {} pixiv ids and {} booru refs to download",
            illust_ids.len(),
            booru_refs.len()
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

        let mut result: ResponseResult<()> = Ok(());
        if !illust_ids.is_empty() {
            result = self
                .process_downloads(bot.clone(), chat_id, illust_ids)
                .await;
        }
        if result.is_ok() && !booru_refs.is_empty() {
            result = self
                .process_booru_downloads(bot.clone(), chat_id, booru_refs)
                .await;
        }

        action_task.abort();

        result
    }

    async fn extract_targets(&self, msg: &Message, args: &str) -> (Vec<u64>, Vec<BooruPostRef>) {
        let mut ids = HashSet::new();
        let mut booru_seen: HashSet<(String, u64)> = HashSet::new();
        let mut booru_refs: Vec<BooruPostRef> = Vec::new();

        let absorb = |text: &str,
                      ids: &mut HashSet<u64>,
                      booru_seen: &mut HashSet<(String, u64)>,
                      booru_refs: &mut Vec<BooruPostRef>| {
            for link in parse_pixiv_links(text) {
                if let PixivLink::Illust(id) = link {
                    ids.insert(id);
                }
            }
            for r in parse_booru_post_links(text, &self.booru_registry) {
                if booru_seen.insert((r.site_name.clone(), r.post_id)) {
                    booru_refs.push(r);
                }
            }
        };

        if !args.trim().is_empty() {
            if let Ok(id) = args.trim().parse::<u64>() {
                ids.insert(id);
            } else {
                absorb(args, &mut ids, &mut booru_seen, &mut booru_refs);
            }
        }

        if ids.is_empty() && booru_refs.is_empty() {
            if let Some(reply_msg) = msg.reply_to_message() {
                if let Some(text) = reply_msg.text().or_else(|| reply_msg.caption()) {
                    absorb(text, &mut ids, &mut booru_seen, &mut booru_refs);
                }

                let entities: Vec<MessageEntityRef<'_>> = reply_msg
                    .parse_entities()
                    .into_iter()
                    .flatten()
                    .chain(reply_msg.parse_caption_entities().into_iter().flatten())
                    .collect();

                for entity in entities {
                    match &entity.kind() {
                        MessageEntityKind::TextLink { url } => {
                            absorb(url.as_str(), &mut ids, &mut booru_seen, &mut booru_refs);
                        }
                        MessageEntityKind::Url => {
                            if let Some(text) = reply_msg.text() {
                                if let Some(url_text) = text.get(entity.range()) {
                                    absorb(url_text, &mut ids, &mut booru_seen, &mut booru_refs);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        (ids.into_iter().collect(), booru_refs)
    }

    /// Extract all e-hentai/exhentai gallery URLs from args or replied message.
    fn extract_eh_galleries(&self, msg: &Message, args: &str) -> Vec<(u64, String)> {
        if self.eh_client.is_none() {
            return Vec::new();
        }

        let mut text = String::new();
        if !args.trim().is_empty() {
            text.push_str(args);
            text.push(' ');
        }
        if let Some(reply_msg) = msg.reply_to_message() {
            if let Some(reply_text) = reply_msg.text().or_else(|| reply_msg.caption()) {
                text.push_str(reply_text);
            }
        }

        if text.is_empty() {
            return Vec::new();
        }

        extract_eh_galleries_from_text(&text)
    }

    /// Process downloads for multiple illusts
    async fn process_downloads(
        &self,
        bot: ThrottledBot,
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
            for (idx, (path, filename)) in all_files.iter().enumerate() {
                // Only show caption on first file
                let cap = if idx == 0 { caption.as_str() } else { "" };
                if let Err(e) = self.send_document(&bot, chat_id, path, filename, cap).await {
                    error!("Failed to send document {}: {:#}", filename, e);
                    let _ = bot.send_message(chat_id, "❌ 发送文件失败").await;
                    break;
                }
                // Rate limiting is now handled by the Throttle adaptor
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

        // For ugoira works, download as MP4 instead of static images
        if illust.is_ugoira() {
            #[cfg(feature = "ffmpeg-codec")]
            {
                let metadata = pixiv
                    .get_ugoira_metadata(illust_id)
                    .await
                    .context("Failed to fetch ugoira metadata")?;
                drop(pixiv);

                let title = illust.title.clone();
                let artist = illust.user.name.clone();
                let downloader = self.notifier.get_downloader();

                let mp4_path = downloader
                    .download_ugoira_mp4(&metadata.zip_urls.medium, metadata.frames)
                    .await
                    .context("Failed to download ugoira MP4")?;

                let sanitized_title = sanitize_filename(&title);
                let filename = format!("{}_{}.mp4", sanitized_title, illust_id);

                return Ok((vec![(mp4_path, filename)], title, artist));
            }

            #[cfg(not(feature = "ffmpeg-codec"))]
            {
                drop(pixiv);
                anyhow::bail!(
                    "Ugoira MP4 download requires the ffmpeg-codec feature, \
                     which is not enabled in this build"
                );
            }
        }

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
    pub(super) async fn create_zip_file(&self, files: &[(PathBuf, String)]) -> Result<PathBuf> {
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
    pub(super) async fn send_document(
        &self,
        bot: &ThrottledBot,
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

    /// Handle download callback from inline button
    ///
    /// Called when user clicks the "📥 下载" button on a pushed image.
    /// This method processes the download for a single illust ID.
    pub async fn handle_download_callback(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        illust_id: u64,
    ) -> ResponseResult<()> {
        info!(
            "Processing download callback for illust {} in chat {}",
            illust_id, chat_id
        );

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

        // Process download for single illust
        let result = self
            .process_downloads(bot.clone(), chat_id, vec![illust_id])
            .await;

        // Stop the chat action task
        action_task.abort();

        result
    }
}

/// Sanitize filename by replacing illegal filesystem characters with underscore
pub(super) fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}

/// Extract all E-Hentai/ExHentai gallery URLs from text, returning (gid, token) pairs.
fn extract_eh_galleries_from_text(text: &str) -> Vec<(u64, String)> {
    let re = Regex::new(r"https?://(?:e-|ex)hentai\.org/g/(\d+)/([A-Za-z0-9_-]+)/?")
        .expect("valid EH gallery regex");
    re.captures_iter(text)
        .filter_map(|cap| {
            let gid = cap.get(1)?.as_str().parse::<u64>().ok()?;
            let token = cap.get(2)?.as_str().to_string();
            Some((gid, token))
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

    #[test]
    fn test_extract_eh_galleries_finds_multiple_links() {
        let text = "https://e-hentai.org/g/1/aaaaaaaaaa/ https://e-hentai.org/g/2/bbbbbbbbbb/";
        let galleries = extract_eh_galleries_from_text(text);
        assert_eq!(galleries.len(), 2);
        assert_eq!(galleries[0], (1, "aaaaaaaaaa".to_string()));
        assert_eq!(galleries[1], (2, "bbbbbbbbbb".to_string()));
    }
}
