use super::download::sanitize_filename;
use crate::bot::notifier::ThrottledBot;
use crate::bot::BotHandler;
use crate::db::repo::collection_jobs::Model;
use anyhow::{Context, Result};
use eh_client::ImageUploadInput;
use std::io;
use std::path::PathBuf;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardMarkup, InputFile};
use tempfile::NamedTempFile;
use tracing::warn;

const TELEGRAPH_UPLOAD_BATCH_SIZE: usize = 5;

impl BotHandler {
    pub(super) async fn deliver_collection_files(
        &self,
        bot: &ThrottledBot,
        job: &Model,
        title: &str,
        files: &[(PathBuf, String)],
        keyboard: Option<InlineKeyboardMarkup>,
    ) -> Result<()> {
        let chat_id = ChatId(job.chat_id);
        if job.telegraph {
            self.begin_collection_delivery(job).await?;
            return self
                .deliver_collection_to_telegraph(bot, chat_id, title, files, keyboard)
                .await;
        }

        let archive = create_collection_zip(files).await?;
        self.begin_collection_delivery(job).await?;
        let archive_name = format!("{}.zip", sanitize_filename(title));
        let mut request = bot
            .send_document(
                chat_id,
                InputFile::file(archive.path()).file_name(archive_name),
            )
            .caption(format!("📦 {title}"));
        if let Some(markup) = keyboard {
            request = request.reply_markup(markup);
        }
        let send_result = request.await.context("Failed to send collection ZIP");

        if let Err(error) = archive.close() {
            warn!("Failed to remove collection ZIP temporary file: {error:#}");
        }
        send_result?;
        Ok(())
    }

    async fn deliver_collection_to_telegraph(
        &self,
        bot: &ThrottledBot,
        chat_id: ChatId,
        title: &str,
        files: &[(PathBuf, String)],
        keyboard: Option<InlineKeyboardMarkup>,
    ) -> Result<()> {
        let telegraph = self
            .telegraph
            .as_ref()
            .context("Telegraph is not configured")?;
        let mut public_urls = Vec::with_capacity(files.len());

        for batch in files.chunks(TELEGRAPH_UPLOAD_BATCH_SIZE) {
            let mut image_data = Vec::with_capacity(batch.len());
            for (path, filename) in batch {
                let bytes = tokio::fs::read(path)
                    .await
                    .with_context(|| format!("Failed to read collection image {filename}"))?;
                image_data.push((filename.clone(), bytes));
            }
            let inputs: Vec<_> = image_data
                .iter()
                .map(|(filename, bytes)| ImageUploadInput {
                    filename: filename.as_str(),
                    bytes: bytes.as_slice(),
                    resume_context: None,
                })
                .collect();
            let url_pairs = telegraph
                .image_uploader
                .upload_images_with_url_pairs(&inputs)
                .await
                .context("Failed to upload collection images")?;
            if url_pairs.len() != batch.len() {
                anyhow::bail!(
                    "Image uploader returned {} URL pairs for {} collection images",
                    url_pairs.len(),
                    batch.len()
                );
            }
            public_urls.extend(url_pairs.into_iter().map(|pair| pair.public_url));
        }

        let first_page_url = telegraph
            .client
            .create_gallery_page_strict(title, &public_urls)
            .await
            .context("Failed to create Telegraph collection page")?;
        // The chat may have been disabled during upload.
        anyhow::ensure!(
            crate::scheduler::get_chat_if_should_notify(&self.repo, chat_id.0)
                .await?
                .is_some(),
            "Collection chat was disabled during upload"
        );
        let mut request = bot.send_message(chat_id, format!("📖 {title}\n{first_page_url}"));
        if let Some(markup) = keyboard {
            request = request.reply_markup(markup);
        }
        request
            .await
            .context("Failed to send Telegraph collection link")?;
        Ok(())
    }
}

async fn create_collection_zip(files: &[(PathBuf, String)]) -> Result<NamedTempFile> {
    let files = files.to_vec();
    tokio::task::spawn_blocking(move || {
        let mut archive = tempfile::Builder::new()
            .prefix("pixivbot_collection_")
            .suffix(".zip")
            .tempfile()
            .context("Failed to create collection ZIP temporary file")?;
        {
            let mut zip = zip::ZipWriter::new(archive.as_file_mut());
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (path, filename) in files {
                zip.start_file(&filename, options)
                    .with_context(|| format!("Failed to add {filename} to collection ZIP"))?;
                let mut source = std::fs::File::open(&path)
                    .with_context(|| format!("Failed to open collection image {filename}"))?;
                io::copy(&mut source, &mut zip)
                    .with_context(|| format!("Failed to write {filename} to collection ZIP"))?;
            }
            zip.finish().context("Failed to finalize collection ZIP")?;
        }
        Ok::<_, anyhow::Error>(archive)
    })
    .await
    .context("Collection ZIP task panicked")?
}
