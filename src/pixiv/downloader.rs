use crate::error::AppResult;
use std::path::PathBuf;

pub struct Downloader {
    cache_dir: PathBuf,
}

impl Downloader {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self { cache_dir }
    }

    pub async fn download(&self, url: &str) -> AppResult<PathBuf> {
        // Placeholder for download logic
        // Check cache
        // Download if miss
        // Save to cache
        // Return file path
        Ok(self.cache_dir.join("placeholder.jpg"))
    }
}
