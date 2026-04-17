use anyhow::{anyhow, Context, Result};
use pixiv_client::UgoiraFrame;
use reqwest::Client;
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::OnceLock;
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
        let mut request = self.http_client.get(url);
        if let Some(referer) = download_referer(url) {
            request = request.header("Referer", referer);
        }

        let bytes = request
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
    /// 2. 从 ZIP 中解码各帧 PNG 图片
    /// 3. 使用 ffmpeg-next 库编码为无音频轨道的 MP4 (H.264 High profile, 画质优化)
    ///
    /// 使用 ZIP URL 的 MD5 哈希作为缓存键
    pub async fn download_ugoira_mp4(
        &self,
        zip_url: &str,
        frames: Vec<UgoiraFrame>,
    ) -> Result<PathBuf> {
        // Use a deterministic cache key derived from the ZIP URL (without timestamp)
        let url_hash = format!("{:x}", md5::compute(zip_url));
        let mp4_cache_key = format!("ugoira_{}.mp4", url_hash);

        // Check cache hit
        if let Some(path) = self.cache.get(&mp4_cache_key).await {
            info!("Cache hit for ugoira MP4: {}", zip_url);
            return Ok(path);
        }

        info!("Downloading ugoira ZIP: {}", zip_url);

        // Download the ZIP file
        let mut request = self.http_client.get(zip_url);
        if let Some(referer) = download_referer(zip_url) {
            request = request.header("Referer", referer);
        }

        let zip_bytes = request
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

fn download_referer(url: &str) -> Option<&'static str> {
    let host = url::Url::parse(url).ok()?.host_str()?.to_ascii_lowercase();

    if host == "pximg.net" || host.ends_with(".pximg.net") {
        Some("https://app-api.pixiv.net/")
    } else {
        None
    }
}

/// Read a named entry from a ZIP archive into a byte vector.
fn read_zip_entry(archive: &mut zip::ZipArchive<Cursor<&[u8]>>, name: &str) -> Result<Vec<u8>> {
    let mut entry = archive
        .by_name(name)
        .with_context(|| format!("Frame '{}' not found in ZIP", name))?;
    let mut buf = Vec::with_capacity(entry.size() as usize);
    entry
        .read_to_end(&mut buf)
        .with_context(|| format!("Failed to read frame '{}' from ZIP", name))?;
    Ok(buf)
}

/// Global FFmpeg initialization guard (runs only once per process).
static FFMPEG_INIT: OnceLock<Result<(), ffmpeg_next::Error>> = OnceLock::new();

fn h264_compatible_dimension(value: u32) -> u32 {
    if value.is_multiple_of(2) {
        value
    } else {
        value + 1
    }
}

/// Extract frames from a ZIP archive and encode them as an MP4 video using ffmpeg-next.
///
/// Uses H.264 High profile with quality-optimized settings (CRF 18, fast preset)
/// and no audio track. Frames are decoded from PNG in memory, scaled to YUV420P,
/// and encoded with per-frame timing from ugoira metadata.
fn encode_ugoira_mp4(zip_data: &[u8], frames: &[UgoiraFrame]) -> Result<Vec<u8>> {
    use ffmpeg_next::format::Pixel;
    use ffmpeg_next::software::scaling;
    use ffmpeg_next::util::error::{Error as FfmpegError, EAGAIN};
    use ffmpeg_next::{codec, encoder, format, frame, Dictionary, Packet, Rational};

    // Initialize FFmpeg library only once per process
    let init_result = FFMPEG_INIT.get_or_init(ffmpeg_next::init);
    if let Err(e) = init_result {
        return Err(anyhow!("Failed to initialize ffmpeg: {}", e));
    }

    let cursor = Cursor::new(zip_data);
    let mut archive = zip::ZipArchive::new(cursor).context("Failed to open ugoira ZIP")?;

    if frames.is_empty() {
        return Err(anyhow!("No frames found in ugoira ZIP"));
    }

    // Decode first frame to determine dimensions
    let first_bytes = read_zip_entry(&mut archive, &frames[0].file)?;
    let first_img = image::load_from_memory(&first_bytes)
        .context("Failed to decode first frame")?
        .to_rgba8();
    let source_width = first_img.width();
    let source_height = first_img.height();
    let encoded_width = h264_compatible_dimension(source_width);
    let encoded_height = h264_compatible_dimension(source_height);

    if encoded_width != source_width || encoded_height != source_height {
        info!(
            "Adjusting ugoira frame size from {}x{} to {}x{} for H.264 compatibility",
            source_width, source_height, encoded_width, encoded_height
        );
    }

    // Create temp file for output MP4 (ffmpeg format::output requires a path)
    let tmp_dir = tempfile::tempdir().context("Failed to create temp directory")?;
    let output_path = tmp_dir.path().join("output.mp4");

    // Create output context for MP4
    let mut octx = format::output(&output_path)
        .map_err(|e| anyhow!("Failed to create output context: {}", e))?;

    // Find H.264 codec and add video stream
    let codec = encoder::find(codec::Id::H264).ok_or_else(|| anyhow!("H.264 encoder not found"))?;

    let global_header = octx.format().flags().contains(format::Flags::GLOBAL_HEADER);

    let mut ost = octx
        .add_stream(codec)
        .map_err(|e| anyhow!("Failed to add output stream: {}", e))?;

    // Create and configure encoder
    let mut encoder_ctx = codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()
        .map_err(|e| anyhow!("Failed to create encoder context: {}", e))?;

    encoder_ctx.set_width(encoded_width);
    encoder_ctx.set_height(encoded_height);
    encoder_ctx.set_format(Pixel::YUV420P);
    encoder_ctx.set_time_base(Rational(1, 1000)); // millisecond timebase

    if global_header {
        encoder_ctx.set_flags(codec::Flags::GLOBAL_HEADER);
    }

    // H.264 quality options: High profile, CRF 18, fast preset
    let mut x264_opts = Dictionary::new();
    x264_opts.set("preset", "fast");
    x264_opts.set("crf", "18");
    x264_opts.set("profile", "high");

    let mut encoder = encoder_ctx
        .open_with(x264_opts)
        .map_err(|e| anyhow!("Failed to open H.264 encoder: {}", e))?;
    ost.set_parameters(&encoder);

    // Create RGBA → YUV420P pixel format scaler
    let mut scaler = scaling::Context::get(
        Pixel::RGBA,
        source_width,
        source_height,
        Pixel::YUV420P,
        encoded_width,
        encoded_height,
        scaling::Flags::BILINEAR,
    )
    .map_err(|e| anyhow!("Failed to create pixel format scaler: {}", e))?;

    // Write MP4 container header
    octx.write_header()
        .map_err(|e| anyhow!("Failed to write MP4 header: {}", e))?;
    let ost_time_base = octx.stream(0).unwrap().time_base();
    let encoder_time_base = Rational(1, 1000);

    // Pre-allocate reusable buffers for encoding loop
    let mut packet = Packet::empty();
    let mut rgba_frame = frame::Video::new(Pixel::RGBA, source_width, source_height);
    let mut yuv_frame = frame::Video::empty();
    let stride = rgba_frame.stride(0);
    let src_row_bytes = source_width as usize * 4;

    // Helper: receive encoded packets and write to output
    let receive_packets = |encoder: &mut encoder::Video,
                           packet: &mut Packet,
                           octx: &mut format::context::Output|
     -> Result<()> {
        loop {
            match encoder.receive_packet(packet) {
                Ok(()) => {
                    packet.set_stream(0);
                    packet.rescale_ts(encoder_time_base, ost_time_base);
                    packet
                        .write_interleaved(octx)
                        .map_err(|e| anyhow!("Failed to write packet: {}", e))?;
                }
                Err(FfmpegError::Other { errno }) if errno == EAGAIN => break,
                Err(FfmpegError::Eof) => break,
                Err(e) => return Err(anyhow!("Failed to receive packet from encoder: {}", e)),
            }
        }
        Ok(())
    };

    // Helper: encode a single RGBA image as a video frame at the given PTS
    let encode_rgba_frame = |img: &image::RgbaImage,
                             pts_ms: i64,
                             rgba_frame: &mut frame::Video,
                             yuv_frame: &mut frame::Video,
                             scaler: &mut scaling::Context,
                             encoder: &mut encoder::Video,
                             packet: &mut Packet,
                             octx: &mut format::context::Output|
     -> Result<()> {
        // Copy RGBA pixel data into reusable frame (respecting stride)
        let src_data = img.as_raw();
        let dst_data = rgba_frame.data_mut(0);
        for y in 0..source_height as usize {
            let dst_offset = y * stride;
            let src_offset = y * src_row_bytes;
            dst_data[dst_offset..dst_offset + src_row_bytes]
                .copy_from_slice(&src_data[src_offset..src_offset + src_row_bytes]);
        }

        // Scale RGBA → YUV420P
        scaler
            .run(rgba_frame, yuv_frame)
            .map_err(|e| anyhow!("Failed to scale frame: {}", e))?;
        yuv_frame.set_pts(Some(pts_ms));

        // Encode
        encoder
            .send_frame(yuv_frame)
            .map_err(|e| anyhow!("Failed to send frame to encoder: {}", e))?;
        receive_packets(encoder, packet, octx)
    };

    // Encode first frame (already decoded)
    let mut pts_ms: i64 = 0;
    encode_rgba_frame(
        &first_img,
        pts_ms,
        &mut rgba_frame,
        &mut yuv_frame,
        &mut scaler,
        &mut encoder,
        &mut packet,
        &mut octx,
    )?;
    pts_ms += frames[0].delay as i64;

    // Encode remaining frames
    for frame_info in &frames[1..] {
        let frame_bytes = read_zip_entry(&mut archive, &frame_info.file)?;
        let img = image::load_from_memory(&frame_bytes)
            .with_context(|| format!("Failed to decode frame '{}'", frame_info.file))?
            .to_rgba8();
        if img.width() != source_width || img.height() != source_height {
            return Err(anyhow!(
                "Frame '{}' has dimensions {}x{}, expected {}x{}",
                frame_info.file,
                img.width(),
                img.height(),
                source_width,
                source_height
            ));
        }
        encode_rgba_frame(
            &img,
            pts_ms,
            &mut rgba_frame,
            &mut yuv_frame,
            &mut scaler,
            &mut encoder,
            &mut packet,
            &mut octx,
        )?;
        pts_ms += frame_info.delay as i64;
    }

    // Flush encoder
    encoder
        .send_eof()
        .map_err(|e| anyhow!("Failed to flush encoder: {}", e))?;
    receive_packets(&mut encoder, &mut packet, &mut octx)?;

    // Write MP4 trailer
    octx.write_trailer()
        .map_err(|e| anyhow!("Failed to write MP4 trailer: {}", e))?;

    // Ensure output context is dropped so the file is fully flushed/unlocked
    drop(octx);

    // Read the output MP4
    let mp4_data = std::fs::read(&output_path).context("Failed to read encoded MP4")?;

    info!(
        "Encoded ugoira MP4: {} frames, {:.1} KB",
        frames.len(),
        mp4_data.len() as f64 / 1024.0
    );

    Ok(mp4_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, RgbaImage};
    use std::io::Write;

    #[test]
    fn pixiv_urls_keep_pixiv_referer() {
        assert_eq!(
            download_referer("https://i.pximg.net/img-original/img/2026/01/01/00/00/00/1_p0.jpg"),
            Some("https://app-api.pixiv.net/")
        );
    }

    #[test]
    fn non_pixiv_urls_do_not_use_pixiv_referer() {
        assert_eq!(
            download_referer("https://files.yande.re/sample/example.jpg"),
            None
        );
        assert_eq!(download_referer("https://example.com/image.jpg"), None);
        assert_eq!(download_referer("https://evilpximg.net/image.jpg"), None);
    }

    #[test]
    fn invalid_urls_do_not_use_pixiv_referer() {
        assert_eq!(download_referer("not a url"), None);
    }

    /// Create a minimal PNG image in memory (2x2 pixels with given color)
    fn create_test_png(r: u8, g: u8, b: u8) -> Vec<u8> {
        create_test_png_with_size(2, 2, r, g, b)
    }

    fn create_test_png_with_size(width: u32, height: u32, r: u8, g: u8, b: u8) -> Vec<u8> {
        let img = RgbaImage::from_pixel(width, height, image::Rgba([r, g, b, 255]));
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
    fn test_encode_ugoira_mp4_basic() {
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
    fn test_encode_ugoira_mp4_odd_dimensions() {
        let frame0 = create_test_png_with_size(3, 5, 255, 0, 0);
        let frame1 = create_test_png_with_size(3, 5, 0, 255, 0);
        let zip_data = create_test_zip(&[("000000.png", &frame0), ("000001.png", &frame1)]);

        let frames = vec![
            UgoiraFrame {
                file: "000000.png".to_string(),
                delay: 100,
            },
            UgoiraFrame {
                file: "000001.png".to_string(),
                delay: 100,
            },
        ];

        let mp4_data = encode_ugoira_mp4(&zip_data, &frames).unwrap();
        assert!(mp4_data.len() > 8);
        assert_eq!(
            &mp4_data[4..8],
            b"ftyp",
            "Output should remain a valid MP4 file for odd-sized frames"
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
