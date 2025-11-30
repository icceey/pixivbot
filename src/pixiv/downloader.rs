use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use tracing::info;

pub struct Downloader {
    cache_dir: PathBuf,
    http_client: Client,
}

impl Downloader {
    pub fn new(http_client: Client, cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
            http_client,
        }
    }

    /// Download image and cache locally
    /// Returns the path to the downloaded file
    pub async fn download(&self, url: &str) -> Result<PathBuf> {
        // Generate cache key from URL
        let cache_key = self.generate_cache_key(url);
        let ext = self.extract_extension(url).unwrap_or("jpg");

        // Create hash-prefixed path (first 2 chars of hash for bucketing)
        let prefix = &cache_key[..2];
        let cache_subdir = self.cache_dir.join(prefix);
        std::fs::create_dir_all(&cache_subdir)?;

        let filename = format!("{}_{}.{}", cache_key, self.safe_url_slug(url), ext);
        let filepath = cache_subdir.join(filename);

        // Check cache hit
        if filepath.exists() {
            info!("Cache hit for: {}", url);
            return Ok(filepath);
        }

        // Cache miss - download
        info!("Downloading: {}", url);
        let response = self
            .http_client
            .get(url)
            .header("Referer", "https://app-api.pixiv.net/")
            .send()
            .await
            .context("Failed to send download request")?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "Download failed with status: {}",
                response.status()
            ));
        }

        let bytes = response
            .bytes()
            .await
            .context("Failed to read response bytes")?;

        let mut file = tokio::fs::File::create(&filepath).await?;
        file.write_all(&bytes).await?;

        info!("Downloaded to: {:?}", filepath);
        Ok(filepath)
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
                    tracing::warn!(
                        "Failed to download image {}/{} ({}): {}",
                        idx + 1,
                        urls.len(),
                        url,
                        e
                    );
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

    /// Extract file extension from URL
    fn extract_extension<'a>(&self, url: &'a str) -> Option<&'a str> {
        url.split('.')
            .next_back()
            .and_then(|ext| ext.split('?').next())
            .filter(|ext| ext.len() <= 4)
    }

    /// Check if URL is already cached
    #[allow(dead_code)]
    pub fn is_cached(&self, url: &str) -> bool {
        let cache_key = self.generate_cache_key(url);
        let prefix = &cache_key[..2];
        let cache_subdir = self.cache_dir.join(prefix);

        if let Ok(entries) = std::fs::read_dir(cache_subdir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with(&cache_key) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Generate a deterministic cache key from URL
    fn generate_cache_key(&self, url: &str) -> String {
        let mut hasher = DefaultHasher::new();
        url.hash(&mut hasher);
        format!("{:x}", hasher.finish())
    }

    /// Create a safe URL slug (last part of path)
    fn safe_url_slug(&self, url: &str) -> String {
        url.split('/')
            .next_back()
            .unwrap_or("image")
            .chars()
            .take(20)
            .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
            .collect()
    }

    /// Clean up old cache files (older than days_threshold)
    pub async fn cleanup_cache(&self, days_threshold: u64) -> Result<usize> {
        use std::time::{SystemTime, UNIX_EPOCH};

        info!(
            "Starting cache cleanup (threshold: {} days)",
            days_threshold
        );
        let threshold_secs = days_threshold * 24 * 60 * 60;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_secs();

        let mut deleted_count = 0;

        if let Ok(entries) = std::fs::read_dir(&self.cache_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    // Check files in subdirectory
                    if let Ok(files) = std::fs::read_dir(entry.path()) {
                        for file in files.flatten() {
                            if let Ok(metadata) = file.metadata() {
                                if let Ok(modified) = metadata.modified() {
                                    if let Ok(duration) = modified.duration_since(UNIX_EPOCH) {
                                        let file_time = duration.as_secs();
                                        if now - file_time > threshold_secs
                                            && std::fs::remove_file(file.path()).is_ok()
                                        {
                                            deleted_count += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        info!("Cache cleanup complete. Deleted {} files", deleted_count);
        Ok(deleted_count)
    }
}
