use anyhow::{Context, Result};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio::time::Duration;
use tracing::{error, info};

/// File cache with URL-derived paths and periodic expiry cleanup.
#[derive(Clone, Debug)]
pub struct FileCacheManager {
    root_dir: PathBuf,
}

impl FileCacheManager {
    /// Start daily cleanup of files older than `retention_days`.
    /// The cache directory is created on first write.
    pub fn new(root_dir: impl Into<PathBuf>, retention_days: u64) -> Self {
        let root_dir = root_dir.into();

        Self::start_background_cleanup(root_dir.clone(), retention_days);

        Self { root_dir }
    }

    /// Return the cached file path under the configured root.
    pub async fn get(&self, url: &str) -> Option<PathBuf> {
        let path = self.resolve_path(url);
        tokio::fs::metadata(&path).await.ok().map(|_| path)
    }

    /// Cache data under the URL-derived path.
    pub async fn save(&self, url: &str, data: &[u8]) -> Result<PathBuf> {
        let path = self.resolve_path(url);

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .context("Failed to create cache directory")?;
        }

        let mut file = tokio::fs::File::create(&path)
            .await
            .context("Failed to create cache file")?;
        file.write_all(data)
            .await
            .context("Failed to write cache data")?;

        Ok(path)
    }

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
                    Err(e) => error!("❌ Cache cleanup failed: {:#}", e),
                }
            }
        });
    }

    async fn cleanup_dir(root_dir: &Path, retention_days: u64) -> Result<usize> {
        let threshold = Duration::from_hours(retention_days * 24);
        let mut deleted_count = 0;

        let mut entries = match tokio::fs::read_dir(root_dir).await {
            Ok(e) => e,
            Err(_) => return Ok(0), // Directory doesn't exist yet
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            if !entry.file_type().await?.is_dir() {
                continue;
            }

            let mut sub_entries = match tokio::fs::read_dir(entry.path()).await {
                Ok(e) => e,
                Err(_) => continue, // Skip if cannot read
            };

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

    /// Directory structure: `{root_dir}/{hash_prefix}/{hash}_{slug}.{ext}`.
    fn resolve_path(&self, url: &str) -> PathBuf {
        let mut hasher = DefaultHasher::new();
        url.hash(&mut hasher);
        let key = format!("{:x}", hasher.finish());
        let prefix = &key[..2];
        let slug: String = url
            .split('/')
            .next_back()
            .unwrap_or("image")
            .chars()
            .take(20)
            .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        let ext = url
            .split('.')
            .next_back()
            .and_then(|ext| ext.split('?').next())
            .filter(|ext| ext.len() <= 4)
            .unwrap_or("jpg");

        let filename = format!("{}_{}.{}", key, slug, ext);
        self.root_dir.join(prefix).join(filename)
    }
}
