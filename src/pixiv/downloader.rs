use crate::error::AppResult;
use std::path::PathBuf;
use pixivrs::{HttpClient, utils};
use tracing::{info, warn};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

pub struct Downloader {
    cache_dir: PathBuf,
    http_client: HttpClient,
}

impl Downloader {
    pub fn new(http_client: HttpClient, cache_dir: impl Into<PathBuf>) -> Self {
        Self { 
            cache_dir: cache_dir.into(), 
            http_client 
        }
    }

    /// Download image and cache locally
    /// Returns the path to the downloaded file
    pub async fn download(&self, url: &str) -> AppResult<PathBuf> {
        // Generate cache key from URL
        let cache_key = self.generate_cache_key(url);
        let ext = utils::extract_extension(url).unwrap_or("jpg".to_string());
        
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
        match utils::download(&self.http_client, url, &filepath).await {
            Ok(_) => {
                info!("Downloaded to: {:?}", filepath);
                Ok(filepath)
            }
            Err(e) => {
                warn!("Failed to download {}: {}", url, e);
                Err(e.into())
            }
        }
    }

    /// Check if URL is already cached
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
            .last()
            .unwrap_or("image")
            .chars()
            .take(20)
            .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
            .collect()
    }

    /// Clean up old cache files (older than days_threshold)
    pub async fn cleanup_cache(&self, days_threshold: u64) -> AppResult<usize> {
        use std::time::{SystemTime, UNIX_EPOCH};
        
        info!("Starting cache cleanup (threshold: {} days)", days_threshold);
        let threshold_secs = days_threshold * 24 * 60 * 60;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
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
                                        if now - file_time > threshold_secs {
                                            if std::fs::remove_file(file.path()).is_ok() {
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
        }
        
        info!("Cache cleanup complete. Deleted {} files", deleted_count);
        Ok(deleted_count)
    }
}
