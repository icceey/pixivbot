use std::path::{Path, PathBuf};
use std::time::SystemTime;
use std::fs;
use md5;

use crate::error::{AppError, Result};

pub struct ImageCache {
    cache_dir: PathBuf,
}

impl ImageCache {
    pub fn new(cache_dir: PathBuf) -> Self {
        // Create cache directory if it doesn't exist
        fs::create_dir_all(&cache_dir).ok();
        
        Self { cache_dir }
    }
    
    pub fn get_path(&self, url: &str) -> PathBuf {
        let hash = md5::compute(url.as_bytes());
        let hash_hex = format!("{:x}", hash);
        
        // Use first 2 characters as subdirectory to prevent too many files in one directory
        let prefix = &hash_hex[0..2];
        let filename = &hash_hex[2..];
        
        let mut path = self.cache_dir.clone();
        path.push(prefix);
        fs::create_dir_all(&path).ok();
        path.push(format!("{}.jpg", filename));
        
        path
    }
    
    pub fn exists(&self, url: &str) -> bool {
        let path = self.get_path(url);
        path.exists()
    }
    
    pub fn get(&self, url: &str) -> Result<Vec<u8>> {
        let path = self.get_path(url);
        if !path.exists() {
            return Err(AppError::Custom("Image not found in cache".to_string()));
        }
        
        let data = fs::read(&path)?;
        // Update access time
        let now = SystemTime::now();
        fs::metadata(&path)?.modified()?;
        filetime::set_file_mtime(&path, now.into())?;
        
        Ok(data)
    }
    
    pub fn put(&self, url: &str, data: Vec<u8>) -> Result<()> {
        let path = self.get_path(url);
        fs::write(&path, data)?;
        Ok(())
    }
    
    pub async fn cleanup_old_files(&self, max_age_seconds: u64) -> Result<()> {
        let now = SystemTime::now();
        
        for entry in walkdir::WalkDir::new(&self.cache_dir)
            .into_iter()
            .flatten()
        {
            let path = entry.path();
            if path.is_file() {
                if let Ok(metadata) = fs::metadata(path) {
                    if let Ok(modified) = metadata.modified() {
                        let age = now.duration_since(modified).unwrap_or_default();
                        if age.as_secs() > max_age_seconds {
                            fs::remove_file(path)?;
                        }
                    }
                }
            }
        }
        
        Ok(())
    }
}