use anyhow::{anyhow, Context, Result};
use image::codecs::gif::{GifEncoder, Repeat};
use image::Frame;
use pixiv_client::UgoiraFrame;
use reqwest::Client;
use std::io::Cursor;
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

    /// 下载 Ugoira (动图) 并转换为 GIF 文件
    ///
    /// 1. 下载 ZIP 文件 (包含各帧图片)
    /// 2. 从 ZIP 中提取帧
    /// 3. 根据帧延迟信息编码为动画 GIF
    ///
    /// 使用 ZIP URL 作为缓存键 (后缀改为 .gif)
    pub async fn download_ugoira_gif(
        &self,
        zip_url: &str,
        frames: &[UgoiraFrame],
    ) -> Result<PathBuf> {
        // Use a cache key derived from the ZIP URL but with .gif extension
        let gif_cache_key = format!("{}.gif", zip_url);

        // Check cache hit
        if let Some(path) = self.cache.get(&gif_cache_key).await {
            info!("Cache hit for ugoira GIF: {}", zip_url);
            return Ok(path);
        }

        info!("Downloading ugoira ZIP: {}", zip_url);

        // Download the ZIP file
        let zip_bytes = self
            .http_client
            .get(zip_url)
            .header("Referer", "https://app-api.pixiv.net/")
            .send()
            .await
            .context("Failed to download ugoira ZIP")?
            .error_for_status()
            .context("Ugoira ZIP download returned error status")?
            .bytes()
            .await
            .context("Failed to read ugoira ZIP bytes")?;

        // Convert ZIP frames to GIF in a blocking task (CPU-intensive)
        let frames_clone = frames.to_vec();
        let zip_data = zip_bytes.to_vec();

        let gif_data =
            tokio::task::spawn_blocking(move || encode_ugoira_gif(&zip_data, &frames_clone))
                .await
                .context("GIF encoding task panicked")??;

        // Save to cache
        let path = self.cache.save(&gif_cache_key, &gif_data).await?;
        info!("Ugoira GIF saved to: {:?}", path);
        Ok(path)
    }
}

/// Extract frames from a ZIP archive and encode them as an animated GIF.
///
/// Streams frames one at a time: each frame is decoded from the ZIP and
/// immediately encoded into the GIF output, avoiding buffering all decoded
/// frames in memory simultaneously.
fn encode_ugoira_gif(zip_data: &[u8], frames: &[UgoiraFrame]) -> Result<Vec<u8>> {
    let cursor = Cursor::new(zip_data);
    let mut archive = zip::ZipArchive::new(cursor).context("Failed to open ugoira ZIP")?;

    if frames.is_empty() {
        return Err(anyhow!("No frames found in ugoira ZIP"));
    }

    // Stream: decode each frame and immediately encode it into the GIF
    let mut gif_buf = Vec::new();
    let frame_count = frames.len();
    {
        let mut encoder = GifEncoder::new_with_speed(&mut gif_buf, 10);
        encoder
            .set_repeat(Repeat::Infinite)
            .context("Failed to set GIF repeat")?;

        for frame_info in frames {
            let mut zip_file = archive
                .by_name(&frame_info.file)
                .with_context(|| format!("Frame '{}' not found in ZIP", frame_info.file))?;

            let mut frame_data = Vec::new();
            std::io::Read::read_to_end(&mut zip_file, &mut frame_data)
                .with_context(|| format!("Failed to read frame '{}'", frame_info.file))?;

            let img = image::load_from_memory(&frame_data)
                .with_context(|| format!("Failed to decode frame '{}'", frame_info.file))?;

            let delay = image::Delay::from_numer_denom_ms(frame_info.delay, 1);
            let frame = Frame::from_parts(img.into_rgba8(), 0, 0, delay);
            encoder
                .encode_frame(frame)
                .context("Failed to encode GIF frame")?;
        }
    }

    info!(
        "Encoded ugoira GIF: {} frames, {:.1} KB",
        frame_count,
        gif_buf.len() as f64 / 1024.0
    );

    Ok(gif_buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, RgbaImage};
    use std::io::Write;

    /// Create a minimal PNG image in memory (2x2 pixels with given color)
    fn create_test_png(r: u8, g: u8, b: u8) -> Vec<u8> {
        let img = RgbaImage::from_pixel(2, 2, image::Rgba([r, g, b, 255]));
        let mut buf = Vec::new();
        let mut cursor = Cursor::new(&mut buf);
        img.write_to(&mut cursor, ImageFormat::Png).unwrap();
        buf
    }

    /// Create a ZIP archive in memory containing the given named files
    fn create_test_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(Cursor::new(&mut buf));
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (name, data) in files {
                zip.start_file(name.to_string(), options).unwrap();
                zip.write_all(data).unwrap();
            }
            zip.finish().unwrap();
        }
        buf
    }

    #[test]
    fn test_encode_ugoira_gif_basic() {
        let frame0 = create_test_png(255, 0, 0); // Red
        let frame1 = create_test_png(0, 255, 0); // Green
        let frame2 = create_test_png(0, 0, 255); // Blue

        let zip_data = create_test_zip(&[
            ("000000.png", &frame0),
            ("000001.png", &frame1),
            ("000002.png", &frame2),
        ]);

        let frames = vec![
            UgoiraFrame {
                file: "000000.png".to_string(),
                delay: 100,
            },
            UgoiraFrame {
                file: "000001.png".to_string(),
                delay: 100,
            },
            UgoiraFrame {
                file: "000002.png".to_string(),
                delay: 200,
            },
        ];

        let gif_data = encode_ugoira_gif(&zip_data, &frames).unwrap();

        // Verify it's a valid GIF (starts with "GIF89a" magic bytes)
        assert!(gif_data.len() > 6);
        assert_eq!(&gif_data[0..3], b"GIF");
    }

    #[test]
    fn test_encode_ugoira_gif_single_frame() {
        let frame0 = create_test_png(128, 128, 128);
        let zip_data = create_test_zip(&[("000000.png", &frame0)]);

        let frames = vec![UgoiraFrame {
            file: "000000.png".to_string(),
            delay: 50,
        }];

        let gif_data = encode_ugoira_gif(&zip_data, &frames).unwrap();
        assert!(gif_data.len() > 6);
        assert_eq!(&gif_data[0..3], b"GIF");
    }

    #[test]
    fn test_encode_ugoira_gif_missing_frame() {
        let frame0 = create_test_png(255, 0, 0);
        let zip_data = create_test_zip(&[("000000.png", &frame0)]);

        let frames = vec![
            UgoiraFrame {
                file: "000000.png".to_string(),
                delay: 100,
            },
            UgoiraFrame {
                file: "missing.png".to_string(),
                delay: 100,
            },
        ];

        let result = encode_ugoira_gif(&zip_data, &frames);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing.png"));
    }

    #[test]
    fn test_encode_ugoira_gif_empty_frames() {
        let frame0 = create_test_png(255, 0, 0);
        let zip_data = create_test_zip(&[("000000.png", &frame0)]);

        let frames: Vec<UgoiraFrame> = vec![];

        let result = encode_ugoira_gif(&zip_data, &frames);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No frames"));
    }
}
