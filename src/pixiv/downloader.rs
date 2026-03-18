use anyhow::{anyhow, Context, Result};
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

    /// 下载 Ugoira (动图) 并转换为 MP4 文件
    ///
    /// 1. 下载 ZIP 文件 (包含各帧图片)
    /// 2. 从 ZIP 中提取帧到临时目录
    /// 3. 使用 ffmpeg 编码为无音频轨道的 MP4 (H.264 High profile, 画质优化)
    ///
    /// 使用 ZIP URL 作为缓存键 (后缀改为 .mp4)
    pub async fn download_ugoira_mp4(
        &self,
        zip_url: &str,
        frames: Vec<UgoiraFrame>,
    ) -> Result<PathBuf> {
        // Use a cache key derived from the ZIP URL but with .mp4 extension
        let mp4_cache_key = format!("{}.mp4", zip_url);

        // Check cache hit
        if let Some(path) = self.cache.get(&mp4_cache_key).await {
            info!("Cache hit for ugoira MP4: {}", zip_url);
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

        // Convert ZIP frames to MP4 in a blocking task (CPU-intensive)
        let zip_data = zip_bytes.to_vec();

        let mp4_data = tokio::task::spawn_blocking(move || encode_ugoira_mp4(&zip_data, &frames))
            .await
            .context("MP4 encoding task failed")??;

        // Save to cache
        let path = self.cache.save(&mp4_cache_key, &mp4_data).await?;
        info!("Ugoira MP4 saved to: {:?}", path);
        Ok(path)
    }
}

/// Extract frames from a ZIP archive and encode them as an MP4 video using ffmpeg.
///
/// Uses H.264 High profile with quality-optimized settings (CRF 18, slow preset)
/// and no audio track. Frames are extracted to a temporary directory and assembled
/// via ffmpeg's concat demuxer to preserve per-frame timing.
fn encode_ugoira_mp4(zip_data: &[u8], frames: &[UgoiraFrame]) -> Result<Vec<u8>> {
    let cursor = Cursor::new(zip_data);
    let mut archive = zip::ZipArchive::new(cursor).context("Failed to open ugoira ZIP")?;

    if frames.is_empty() {
        return Err(anyhow!("No frames found in ugoira ZIP"));
    }

    // Create a temporary directory for frame extraction
    let tmp_dir = tempfile::tempdir().context("Failed to create temp directory")?;
    let tmp_path = tmp_dir.path();

    // Extract each frame from the ZIP to the temp directory
    for frame_info in frames {
        let mut zip_file = archive
            .by_name(&frame_info.file)
            .with_context(|| format!("Frame '{}' not found in ZIP", frame_info.file))?;

        let frame_path = tmp_path.join(&frame_info.file);
        let mut out_file = std::fs::File::create(&frame_path)
            .with_context(|| format!("Failed to create temp frame '{}'", frame_info.file))?;
        std::io::copy(&mut zip_file, &mut out_file)
            .with_context(|| format!("Failed to write temp frame '{}'", frame_info.file))?;
    }

    // Build the ffmpeg concat demuxer input file
    let concat_path = tmp_path.join("concat.txt");
    let mut concat_content = String::new();
    for frame_info in frames {
        let frame_path = tmp_path.join(&frame_info.file);
        // Escape for ffmpeg concat demuxer: \ → \\, ' → \'
        let path_str = frame_path
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('\'', "\\'");
        let duration_sec = frame_info.delay as f64 / 1000.0;
        concat_content.push_str(&format!(
            "file '{}'\nduration {:.6}\n",
            path_str, duration_sec
        ));
    }
    // Repeat last frame entry to ensure its duration is applied
    if let Some(last) = frames.last() {
        let frame_path = tmp_path.join(&last.file);
        let path_str = frame_path
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('\'', "\\'");
        concat_content.push_str(&format!("file '{}'\n", path_str));
    }
    std::fs::write(&concat_path, &concat_content).context("Failed to write concat file")?;

    // Output MP4 path
    let output_path = tmp_path.join("output.mp4");

    // Run ffmpeg: H.264 High profile, quality-optimized, no audio
    let output = std::process::Command::new("ffmpeg")
        .args([
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
            &concat_path.to_string_lossy(),
            "-c:v",
            "libx264",
            "-profile:v",
            "high",
            "-preset",
            "slow",
            "-crf",
            "18",
            "-pix_fmt",
            "yuv420p",
            "-an",
            "-movflags",
            "+faststart",
            "-y",
            &output_path.to_string_lossy(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .context("Failed to execute ffmpeg command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("ffmpeg stderr: {}", stderr);
        return Err(anyhow!(
            "ffmpeg encoding failed (exit code: {:?})",
            output.status.code()
        ));
    }

    let mp4_data = std::fs::read(&output_path).context("Failed to read encoded MP4")?;

    let frame_count = frames.len();
    info!(
        "Encoded ugoira MP4: {} frames, {:.1} KB",
        frame_count,
        mp4_data.len() as f64 / 1024.0
    );

    Ok(mp4_data)
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

    /// Check if ffmpeg is available on the system (cached)
    fn ffmpeg_available() -> bool {
        use std::sync::LazyLock;
        static FFMPEG_AVAILABLE: LazyLock<bool> = LazyLock::new(|| {
            std::process::Command::new("ffmpeg")
                .arg("-version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok()
        });
        *FFMPEG_AVAILABLE
    }

    #[test]
    fn test_encode_ugoira_mp4_basic() {
        if !ffmpeg_available() {
            return;
        }

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

        let mp4_data = encode_ugoira_mp4(&zip_data, &frames).unwrap();

        // Verify it's a valid MP4: ftyp box type at offset 4 (after 4-byte size field)
        assert!(mp4_data.len() > 8);
        assert_eq!(
            &mp4_data[4..8],
            b"ftyp",
            "Output should be a valid MP4 file"
        );
    }

    #[test]
    fn test_encode_ugoira_mp4_single_frame() {
        if !ffmpeg_available() {
            return;
        }

        let frame0 = create_test_png(128, 128, 128);
        let zip_data = create_test_zip(&[("000000.png", &frame0)]);

        let frames = vec![UgoiraFrame {
            file: "000000.png".to_string(),
            delay: 50,
        }];

        let mp4_data = encode_ugoira_mp4(&zip_data, &frames).unwrap();
        assert!(mp4_data.len() > 8);
        assert_eq!(
            &mp4_data[4..8],
            b"ftyp",
            "Output should be a valid MP4 file"
        );
    }

    #[test]
    fn test_encode_ugoira_mp4_missing_frame() {
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

        let result = encode_ugoira_mp4(&zip_data, &frames);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing.png"));
    }

    #[test]
    fn test_encode_ugoira_mp4_empty_frames() {
        let frame0 = create_test_png(255, 0, 0);
        let zip_data = create_test_zip(&[("000000.png", &frame0)]);

        let frames: Vec<UgoiraFrame> = vec![];

        let result = encode_ugoira_mp4(&zip_data, &frames);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No frames"));
    }
}
