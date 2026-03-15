use anyhow::{anyhow, Context, Result};
use image::{
    codecs::gif::{GifEncoder, Repeat},
    Delay, Frame as ImageFrame,
};
use pixiv_client::UgoiraFrame;
use reqwest::Client;
use std::io::Read;
use std::path::PathBuf;
use tracing::{info, warn};

use crate::cache::FileCacheManager;

pub struct Downloader {
    http_client: Client,
    cache: FileCacheManager,
}

impl Downloader {
    pub fn new(http_client: Client, cache: FileCacheManager) -> Self {
        Self { http_client, cache }
    }

    /// Download image and cache locally
    /// Returns the path to the downloaded file
    pub async fn download(&self, url: &str) -> Result<PathBuf> {
        // Check cache hit
        if let Some(path) = self.cache.get(url).await {
            info!("Cache hit for: {}", url);
            return Ok(path);
        }

        // Cache miss - download
        let bytes = self
            .http_client
            .get(url)
            .header("Referer", "https://app-api.pixiv.net/")
            .send()
            .await
            .context("Failed to send download request")?
            .error_for_status()
            .context("Download returned error status")?
            .bytes()
            .await
            .context("Failed to read response bytes")?;

        // Save to cache
        let path = self.cache.save(url, &bytes).await?;
        info!("Downloaded to: {:?}", path);
        Ok(path)
    }

    /// 批量下载多张图片 (用于多图作品)
    /// 返回所有下载成功的文件路径
    pub async fn download_all(&self, urls: &[String]) -> Result<Vec<PathBuf>> {
        info!("Batch downloading {} images", urls.len());

        let mut paths = Vec::with_capacity(urls.len());

        for (idx, url) in urls.iter().enumerate() {
            match self.download(url).await {
                Ok(path) => {
                    info!("Downloaded {}/{}: {:?}", idx + 1, urls.len(), path);
                    paths.push(path);
                }
                Err(e) => {
                    // 继续下载其他图片,不因一张失败而中断
                    warn!("Failed to download image[{}] ({}): {:#}", idx + 1, url, e);
                }
            }
        }

        if paths.is_empty() {
            return Err(anyhow!("All images failed to download"));
        }

        info!(
            "Batch download complete: {}/{} successful",
            paths.len(),
            urls.len()
        );
        Ok(paths)
    }

    /// Download a ugoira animation as an animated GIF.
    ///
    /// Downloads the ugoira zip archive, extracts frames in order, and
    /// encodes them into a single animated GIF using the per-frame delay
    /// information from the Pixiv API.  The result is cached by zip URL.
    pub async fn download_ugoira_gif(
        &self,
        zip_url: &str,
        frames: &[UgoiraFrame],
    ) -> Result<PathBuf> {
        // Check cache first (keyed on zip_url, stored as .gif)
        if let Some(path) = self.cache.get_as(zip_url, "gif").await {
            info!("Cache hit for ugoira GIF: {}", zip_url);
            return Ok(path);
        }

        info!("Downloading ugoira zip: {}", zip_url);
        let zip_bytes = self
            .http_client
            .get(zip_url)
            .header("Referer", "https://app-api.pixiv.net/")
            .send()
            .await
            .context("Failed to send ugoira zip request")?
            .error_for_status()
            .context("Ugoira zip download returned error status")?
            .bytes()
            .await
            .context("Failed to read ugoira zip bytes")?
            .to_vec();

        // Encode GIF in a blocking thread (CPU-bound)
        let frames_owned = frames.to_vec();
        let gif_bytes =
            tokio::task::spawn_blocking(move || create_gif_from_zip(&zip_bytes, &frames_owned))
                .await
                .context("GIF encoding task panicked")?
                .context("Failed to create animated GIF from ugoira frames")?;

        let path = self
            .cache
            .save_as(zip_url, &gif_bytes, "gif")
            .await
            .context("Failed to cache ugoira GIF")?;

        info!("Ugoira GIF created and cached: {:?}", path);
        Ok(path)
    }
}

/// Encode a ugoira zip archive as an animated GIF.
///
/// Runs synchronously (intended to be called from `spawn_blocking`).
fn create_gif_from_zip(zip_bytes: &[u8], frames: &[UgoiraFrame]) -> Result<Vec<u8>> {
    use std::io::Cursor;

    if frames.is_empty() {
        return Err(anyhow!("Ugoira has no frames"));
    }

    let cursor = Cursor::new(zip_bytes);
    let mut archive = zip::ZipArchive::new(cursor).context("Failed to open ugoira zip")?;

    let mut output: Vec<u8> = Vec::new();
    let mut encoder = GifEncoder::new_with_speed(&mut output, 10 /* fast quantisation */);
    encoder
        .set_repeat(Repeat::Infinite)
        .context("Failed to set GIF repeat")?;

    for frame_info in frames {
        let frame_bytes = {
            let mut zip_file = archive
                .by_name(&frame_info.file)
                .with_context(|| format!("Frame '{}' not found in zip", frame_info.file))?;
            let mut bytes = Vec::new();
            zip_file
                .read_to_end(&mut bytes)
                .context("Failed to read frame bytes")?;
            bytes
        };

        let img = image::load_from_memory(&frame_bytes)
            .with_context(|| format!("Failed to decode frame '{}'", frame_info.file))?
            .to_rgba8();

        // `image::Delay` accepts milliseconds.  Clamp to at least 10 ms (= 1 centisecond)
        // because GIF delays are stored in centiseconds and a zero-delay frame is invalid.
        let delay_ms = (frame_info.delay as u64).max(10);
        let delay = Delay::from_numer_denom_ms(delay_ms as u32, 1);
        let frame = ImageFrame::from_parts(img, 0, 0, delay);
        encoder
            .encode_frame(frame)
            .context("Failed to encode GIF frame")?;
    }

    drop(encoder);
    Ok(output)
}
