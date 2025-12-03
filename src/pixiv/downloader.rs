use anyhow::{anyhow, Context, Result};
use reqwest::Client;
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
}
