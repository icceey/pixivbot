use anyhow::{Context, Result};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio::time::Duration;
use tracing::{error, info};

/// File cache manager for storing and retrieving cached files.
///
/// This manager handles:
/// - Path management: Generates unique local paths from URLs
/// - Storage strategy: Uses hash-prefixed directories (bucketing)
/// - Persistence: Async file read/write operations
/// - Lifecycle: Automatic cleanup of expired files
#[derive(Clone, Debug)]
pub struct FileCacheManager {
    /// Cache root directory (e.g., "./data/cache")
    root_dir: PathBuf,
}

impl FileCacheManager {
    /// Initialize the cache manager and start background cleanup task.
    ///
    /// # Arguments
    /// * `root_dir` - Cache root directory. Created on first write if not exists.
    /// * `retention_days` - Maximum file retention period in days.
    ///
    /// # Background Cleanup
    /// A background task is spawned that runs every 24 hours,
    /// deleting files older than `retention_days`.
    pub fn new(root_dir: impl Into<PathBuf>, retention_days: u64) -> Self {
        let root_dir = root_dir.into();

        // Start background cleanup task
        Self::start_background_cleanup(root_dir.clone(), retention_days);

        Self { root_dir }
    }

    /// Check if URL is cached.
    ///
    /// # Returns
    /// * `Some(PathBuf)` - Cache hit, returns absolute path
    /// * `None` - Cache miss
    pub async fn get(&self, url: &str) -> Option<PathBuf> {
        let path = self.resolve_path(url);
        tokio::fs::metadata(&path).await.ok().map(|_| path)
    }

    /// Save data to cache.
    ///
    /// # Arguments
    /// * `url` - Original URL (used to generate key and filename)
    /// * `data` - Binary data to cache
    ///
    /// # Behavior
    /// 1. Calculates target path
    /// 2. Creates parent directories if needed
    /// 3. Writes data asynchronously
    /// 4. Returns the written file path
    pub async fn save(&self, url: &str, data: &[u8]) -> Result<PathBuf> {
        let path = self.resolve_path(url);

        // Create parent directory if needed
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .context("Failed to create cache directory")?;
        }

        // Write file
        let mut file = tokio::fs::File::create(&path)
            .await
            .context("Failed to create cache file")?;
        file.write_all(data)
            .await
            .context("Failed to write cache data")?;

        Ok(path)
    }

    /// Start background cleanup task.
    fn start_background_cleanup(root_dir: PathBuf, retention_days: u64) {
        tokio::spawn(async move {
            const STARTUP_DELAY: Duration = Duration::from_secs(60);
            const CLEANUP_PERIOD: Duration = Duration::from_secs(24 * 3600);

            // Initial delay to avoid startup contention
            tokio::time::sleep(STARTUP_DELAY).await;

            let mut interval = tokio::time::interval(CLEANUP_PERIOD);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;

                match Self::cleanup_dir(&root_dir, retention_days).await {
                    Ok(count) if count > 0 => {
                        info!("✅ Cache cleanup complete: {} files deleted", count)
                    }
                    Ok(_) => (),
                    Err(e) => error!("❌ Cache cleanup failed: {}", e),
                }
            }
        });
    }

    /// Execute cleanup logic (static helper).
    async fn cleanup_dir(root_dir: &Path, retention_days: u64) -> Result<usize> {
        let threshold = Duration::from_hours(retention_days * 24);
        let mut deleted_count = 0;

        // Use std::fs for directory iteration (async not needed for metadata-only ops)
        let mut entries = match tokio::fs::read_dir(root_dir).await {
            Ok(e) => e,
            Err(_) => return Ok(0), // Directory doesn't exist yet
        };

        // check subdirectories
        while let Ok(Some(entry)) = entries.next_entry().await {
            // Check if entry is a directory
            if !entry.file_type().await?.is_dir() {
                continue;
            }

            // check files in subdirectory
            let mut sub_entries = match tokio::fs::read_dir(entry.path()).await {
                Ok(e) => e,
                Err(_) => continue, // Skip if cannot read
            };

            // check files in subdirectory
            while let Ok(Some(file_entry)) = sub_entries.next_entry().await {
                let metadata = match file_entry.metadata().await {
                    Ok(m) => m,
                    Err(_) => continue, // Skip if cannot read metadata
                };

                if let Ok(elapsed) = metadata.modified()?.elapsed() {
                    if elapsed > threshold
                        && tokio::fs::remove_file(file_entry.path()).await.is_ok()
                    {
                        deleted_count += 1;
                    }
                };
            }
        }

        Ok(deleted_count)
    }

    /// Generate a deterministic cache key from URL.
    fn generate_key(&self, url: &str) -> String {
        let mut hasher = DefaultHasher::new();
        url.hash(&mut hasher);
        format!("{:x}", hasher.finish())
    }

    /// Create a safe URL slug (last part of path).
    fn safe_url_slug(&self, url: &str) -> String {
        url.split('/')
            .next_back()
            .unwrap_or("image")
            .chars()
            .take(20)
            .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
            .collect()
    }

    /// Extract file extension from URL.
    fn extract_extension<'a>(&self, url: &'a str) -> &'a str {
        url.split('.')
            .next_back()
            .and_then(|ext| ext.split('?').next())
            .filter(|ext| ext.len() <= 4)
            .unwrap_or("jpg")
    }

    /// Resolve full path for a URL.
    ///
    /// Directory structure: `{root_dir}/{prefix}/{hash}_{slug}.{ext}`
    /// - `prefix`: First 2 characters of hash (00-ff)
    fn resolve_path(&self, url: &str) -> PathBuf {
        let key = self.generate_key(url);
        let prefix = &key[..2];
        let slug = self.safe_url_slug(url);
        let ext = self.extract_extension(url);

        let filename = format!("{}_{}.{}", key, slug, ext);
        self.root_dir.join(prefix).join(filename)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_key_deterministic() {
        let cache = FileCacheManager {
            root_dir: PathBuf::from("/tmp/cache"),
        };

        let url = "https://example.com/image.jpg";
        let key1 = cache.generate_key(url);
        let key2 = cache.generate_key(url);

        assert_eq!(key1, key2);
        assert!(!key1.is_empty());
    }

    #[test]
    fn test_safe_url_slug() {
        let cache = FileCacheManager {
            root_dir: PathBuf::from("/tmp/cache"),
        };

        assert_eq!(
            cache.safe_url_slug("https://example.com/path/image_123.jpg"),
            "image_123jpg"
        );
        assert_eq!(
            cache.safe_url_slug("https://example.com/very_long_filename_that_exceeds_limit.png"),
            "very_long_filename_t"
        );
        assert_eq!(cache.safe_url_slug("https://example.com/"), "");
    }

    #[test]
    fn test_extract_extension() {
        let cache = FileCacheManager {
            root_dir: PathBuf::from("/tmp/cache"),
        };

        assert_eq!(
            cache.extract_extension("https://example.com/image.jpg"),
            "jpg"
        );
        assert_eq!(
            cache.extract_extension("https://example.com/image.png?v=123"),
            "png"
        );
        assert_eq!(cache.extract_extension("https://example.com/image"), "jpg");
        // fallback
    }

    #[test]
    fn test_resolve_path() {
        let cache = FileCacheManager {
            root_dir: PathBuf::from("/tmp/cache"),
        };

        let path = cache.resolve_path("https://example.com/test.jpg");

        // Path should be: /tmp/cache/{prefix}/{hash}_{slug}.{ext}
        assert!(path.starts_with("/tmp/cache"));
        assert!(path.to_string_lossy().ends_with(".jpg"));
    }
}
