use super::Repo;
use crate::db::entities::eh_download_queue;
use anyhow::{Context, Result};
use chrono::Local;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, Order, PaginatorTrait, QueryFilter, QueryOrder, Set,
};
use tracing::warn;

/// Status constants for eh_download_queue.
pub const STATUS_PENDING: &str = "pending";
pub const STATUS_DOWNLOADING: &str = "downloading";
pub const STATUS_DONE: &str = "done";
pub const STATUS_FAILED: &str = "failed";
pub const STATUS_DOWNLOADED: &str = "downloaded";
pub const STATUS_UPLOADING: &str = "uploading";
pub const STATUS_UPLOADED: &str = "uploaded";
pub const STATUS_PUBLISHING: &str = "publishing";

/// Source constants for eh_download_queue.
pub const SOURCE_SUBSCRIPTION: &str = "subscription";
pub const SOURCE_DIRECT: &str = "direct";

impl Repo {
    /// Enqueue a download request. Returns the created model.
    pub async fn enqueue_eh_download(
        &self,
        chat_id: i64,
        gid: i64,
        token: &str,
        title: &str,
        telegraph: bool,
        source: &str,
    ) -> Result<eh_download_queue::Model> {
        let now = Local::now().naive_local();

        let entry = eh_download_queue::ActiveModel {
            chat_id: Set(chat_id),
            gid: Set(gid),
            token: Set(token.to_string()),
            title: Set(title.to_string()),
            telegraph: Set(telegraph),
            source: Set(source.to_string()),
            status: Set(STATUS_PENDING.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            ..Default::default()
        };

        entry
            .insert(&self.db)
            .await
            .context("Failed to enqueue eh download")
    }

    /// Get the next pending download, atomically marking it as "downloading".
    ///
    /// Returns None if no pending downloads exist.
    #[allow(dead_code)]
    pub async fn get_next_pending_eh_download(&self) -> Result<Option<eh_download_queue::Model>> {
        let entry = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .order_by(eh_download_queue::Column::CreatedAt, Order::Asc)
            .one(&self.db)
            .await
            .context("Failed to fetch pending eh download")?;

        if let Some(model) = entry {
            let now = Local::now().naive_local();
            let mut active: eh_download_queue::ActiveModel = model.into();
            active.status = Set(STATUS_DOWNLOADING.to_string());
            active.started_at = Set(Some(now));
            let updated = active
                .update(&self.db)
                .await
                .context("Failed to mark eh download as downloading")?;
            Ok(Some(updated))
        } else {
            Ok(None)
        }
    }

    /// Mark a download as completed successfully.
    pub async fn mark_eh_download_done(
        &self,
        id: i32,
        file_size: i64,
    ) -> Result<eh_download_queue::Model> {
        let entry = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .context("Failed to fetch eh download")?
            .ok_or_else(|| anyhow::anyhow!("EH download {} not found", id))?;

        let now = Local::now().naive_local();
        let mut active: eh_download_queue::ActiveModel = entry.into();
        active.status = Set(STATUS_DONE.to_string());
        active.file_size = Set(file_size);
        active.completed_at = Set(Some(now));
        active.error = Set(None);
        active
            .update(&self.db)
            .await
            .context("Failed to mark eh download as done")
    }

    /// Mark a download as failed.
    #[allow(dead_code)]
    pub async fn mark_eh_download_failed(
        &self,
        id: i32,
        error: &str,
    ) -> Result<eh_download_queue::Model> {
        let entry = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .context("Failed to fetch eh download")?
            .ok_or_else(|| anyhow::anyhow!("EH download {} not found", id))?;

        let now = Local::now().naive_local();
        let new_retry_count = entry.retry_count + 1;
        let mut active: eh_download_queue::ActiveModel = entry.into();
        active.status = Set(STATUS_FAILED.to_string());
        active.error = Set(Some(error.to_string()));
        active.completed_at = Set(Some(now));
        active.retry_count = Set(new_retry_count);
        active
            .update(&self.db)
            .await
            .context("Failed to mark eh download as failed")
    }

    /// Get total bytes downloaded in the last `hours` window (completed_at >= cutoff).
    pub async fn get_eh_downloaded_bytes_in_window(&self, hours: u64) -> Result<i64> {
        let cutoff = Local::now().naive_local() - chrono::Duration::hours(hours as i64);

        let entries = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_DONE))
            .filter(eh_download_queue::Column::CompletedAt.gte(cutoff))
            .all(&self.db)
            .await
            .context("Failed to fetch eh downloads in window")?;

        let total: i64 = entries.iter().map(|e| e.file_size).sum();
        Ok(total)
    }

    /// Count pending downloads in the queue.
    #[allow(dead_code)]
    pub async fn count_pending_eh_downloads(&self) -> Result<u64> {
        eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .count(&self.db)
            .await
            .context("Failed to count pending eh downloads")
    }

    /// Reset stale "downloading" entries back to "pending" (crash recovery).
    pub async fn reset_stale_eh_downloads(&self) -> Result<u64> {
        let stale = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_DOWNLOADING))
            .all(&self.db)
            .await
            .context("Failed to fetch stale eh downloads")?;

        let count = stale.len();
        for entry in stale {
            let mut active: eh_download_queue::ActiveModel = entry.into();
            active.status = Set(STATUS_PENDING.to_string());
            active.started_at = Set(None);
            active
                .update(&self.db)
                .await
                .context("Failed to reset stale eh download")?;
        }

        Ok(count as u64)
    }

    /// Reset failed downloads back to pending if they haven't exceeded max_retry_count.
    #[allow(dead_code)]
    pub async fn retry_failed_eh_downloads(&self, max_retry_count: u8) -> Result<u64> {
        let failed = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_FAILED))
            .filter(eh_download_queue::Column::RetryCount.lte(max_retry_count as i32))
            .all(&self.db)
            .await
            .context("Failed to fetch failed eh downloads")?;

        let count = failed.len();
        for entry in failed {
            let mut active: eh_download_queue::ActiveModel = entry.into();
            active.status = Set(STATUS_PENDING.to_string());
            active.started_at = Set(None);
            active.completed_at = Set(None);
            active
                .update(&self.db)
                .await
                .context("Failed to reset failed eh download")?;
        }

        Ok(count as u64)
    }

    /// Calculate exponential backoff delay (seconds) for a given retry count.
    /// 1→60s, 2→300s, 3→900s, beyond→3600s.
    pub fn backoff_delay_secs(retry_count: i32) -> i64 {
        match retry_count {
            0 | 1 => 60,
            2 => 300,
            3 => 900,
            _ => 3600,
        }
    }

    /// Mark a download as downloaded (ZIP saved to cache). Transitions to `downloaded` status.
    pub async fn mark_eh_download_downloaded(
        &self,
        id: i32,
        file_size: i64,
        zip_path: &str,
    ) -> Result<eh_download_queue::Model> {
        let entry = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .context("Failed to fetch eh download")?
            .ok_or_else(|| anyhow::anyhow!("EH download {} not found", id))?;

        let now = Local::now().naive_local();
        let mut active: eh_download_queue::ActiveModel = entry.into();
        active.status = Set(STATUS_DOWNLOADED.to_string());
        active.file_size = Set(file_size);
        active.zip_path = Set(Some(zip_path.to_string()));
        active.completed_at = Set(Some(now));
        active.started_at = Set(None);
        active.error = Set(None);
        active.next_retry_at = Set(None);
        active
            .update(&self.db)
            .await
            .context("Failed to mark eh download as downloaded")
    }

    /// Mark a download as uploaded (Telegraph page created). Transitions to `uploaded` status.
    pub async fn mark_eh_download_uploaded(
        &self,
        id: i32,
        telegraph_url: &str,
    ) -> Result<eh_download_queue::Model> {
        let entry = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .context("Failed to fetch eh download")?
            .ok_or_else(|| anyhow::anyhow!("EH download {} not found", id))?;

        let now = Local::now().naive_local();
        let mut active: eh_download_queue::ActiveModel = entry.into();
        active.status = Set(STATUS_UPLOADED.to_string());
        active.telegraph_url = Set(Some(telegraph_url.to_string()));
        active.completed_at = Set(Some(now));
        active.error = Set(None);
        active.next_retry_at = Set(None);
        active
            .update(&self.db)
            .await
            .context("Failed to mark eh download as uploaded")
    }

    /// Get next entry for the download stage: status=pending, next_retry_at is NULL or <= now.
    /// Atomically marks it as 'downloading'.
    pub async fn get_next_for_download(&self) -> Result<Option<eh_download_queue::Model>> {
        let now = Local::now().naive_local();
        let entry = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(
                eh_download_queue::Column::NextRetryAt
                    .is_null()
                    .or(eh_download_queue::Column::NextRetryAt.lte(now)),
            )
            .order_by(eh_download_queue::Column::CreatedAt, Order::Asc)
            .one(&self.db)
            .await
            .context("Failed to fetch next for download")?;

        if let Some(model) = entry {
            let now = Local::now().naive_local();
            let mut active: eh_download_queue::ActiveModel = model.into();
            active.status = Set(STATUS_DOWNLOADING.to_string());
            active.started_at = Set(Some(now));
            active.next_retry_at = Set(None);
            let updated = active
                .update(&self.db)
                .await
                .context("Failed to mark as downloading")?;
            Ok(Some(updated))
        } else {
            Ok(None)
        }
    }

    /// Get next entry for the upload stage: status=downloaded, telegraph=true, next_retry_at ok.
    /// Atomically marks it as 'uploading'.
    pub async fn get_next_for_upload(&self) -> Result<Option<eh_download_queue::Model>> {
        let now = Local::now().naive_local();
        let entry = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_DOWNLOADED))
            .filter(eh_download_queue::Column::Telegraph.eq(true))
            .filter(
                eh_download_queue::Column::NextRetryAt
                    .is_null()
                    .or(eh_download_queue::Column::NextRetryAt.lte(now)),
            )
            .order_by(eh_download_queue::Column::CreatedAt, Order::Asc)
            .one(&self.db)
            .await
            .context("Failed to fetch next for upload")?;

        if let Some(model) = entry {
            let mut active: eh_download_queue::ActiveModel = model.into();
            active.status = Set(STATUS_UPLOADING.to_string());
            active.started_at = Set(Some(now));
            active.next_retry_at = Set(None);
            let updated = active
                .update(&self.db)
                .await
                .context("Failed to mark as uploading")?;
            Ok(Some(updated))
        } else {
            Ok(None)
        }
    }

    /// Get next entry for the publish stage: either (downloaded, telegraph=false) or (uploaded).
    /// Atomically marks it as 'publishing'.
    pub async fn get_next_for_publish(&self) -> Result<Option<eh_download_queue::Model>> {
        let now = Local::now().naive_local();
        let entry = eh_download_queue::Entity::find()
            .filter(
                sea_orm::Condition::any()
                    .add(
                        eh_download_queue::Column::Status
                            .eq(STATUS_DOWNLOADED)
                            .and(eh_download_queue::Column::Telegraph.eq(false)),
                    )
                    .add(eh_download_queue::Column::Status.eq(STATUS_UPLOADED)),
            )
            .filter(
                eh_download_queue::Column::NextRetryAt
                    .is_null()
                    .or(eh_download_queue::Column::NextRetryAt.lte(now)),
            )
            .order_by(eh_download_queue::Column::CreatedAt, Order::Asc)
            .one(&self.db)
            .await
            .context("Failed to fetch next for publish")?;

        if let Some(model) = entry {
            let mut active: eh_download_queue::ActiveModel = model.into();
            active.status = Set(STATUS_PUBLISHING.to_string());
            active.started_at = Set(Some(now));
            active.next_retry_at = Set(None);
            let updated = active
                .update(&self.db)
                .await
                .context("Failed to mark as publishing")?;
            Ok(Some(updated))
        } else {
            Ok(None)
        }
    }

    /// Schedule a retry for an entry: set status back to target_status, increment retry_count,
    /// set next_retry_at to now + backoff. If retry_count exceeds max, set status=failed.
    /// Returns (model, is_permanent_failure).
    pub async fn schedule_eh_retry(
        &self,
        id: i32,
        target_status: &str,
        error: &str,
        max_retry_count: u8,
    ) -> Result<(eh_download_queue::Model, bool)> {
        let entry = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .context("Failed to fetch eh download")?
            .ok_or_else(|| anyhow::anyhow!("EH download {} not found", id))?;

        let new_retry_count = entry.retry_count + 1;
        let is_permanent = new_retry_count > max_retry_count as i32;
        let now = Local::now().naive_local();

        let mut active: eh_download_queue::ActiveModel = entry.into();
        if is_permanent {
            active.status = Set(STATUS_FAILED.to_string());
            active.completed_at = Set(Some(now));
        } else {
            let delay = Self::backoff_delay_secs(new_retry_count);
            active.status = Set(target_status.to_string());
            active.next_retry_at = Set(Some(now + chrono::Duration::seconds(delay)));
            active.started_at = Set(None);
        }
        active.error = Set(Some(error.to_string()));
        active.retry_count = Set(new_retry_count);
        let model = active
            .update(&self.db)
            .await
            .context("Failed to schedule retry")?;
        Ok((model, is_permanent))
    }

    /// Delete ZIP files in the cache dir that have no corresponding queue entry
    /// in an active status (downloaded, uploading, uploaded, publishing).
    pub async fn cleanup_eh_cache_orphans(&self, cache_dir: &std::path::Path) -> Result<()> {
        if !cache_dir.exists() {
            return Ok(());
        }

        let active_paths: std::collections::HashSet<String> = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.is_in([
                STATUS_DOWNLOADED,
                STATUS_UPLOADING,
                STATUS_UPLOADED,
                STATUS_PUBLISHING,
            ]))
            .all(&self.db)
            .await?
            .into_iter()
            .filter_map(|e| e.zip_path)
            .collect();

        for entry in std::fs::read_dir(cache_dir).context("Failed to read eh_cache dir")? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("zip") {
                let path_str = path.to_string_lossy().to_string();
                if !active_paths.contains(&path_str) {
                    if let Err(e) = std::fs::remove_file(&path) {
                        warn!("Failed to remove orphan zip {}: {}", path.display(), e);
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repo::tests_helpers;

    #[tokio::test]
    async fn test_enqueue_and_get_next_pending() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        let model = repo
            .enqueue_eh_download(
                -100123,
                123456,
                "abcdef0123",
                "Test Gallery",
                false,
                SOURCE_SUBSCRIPTION,
            )
            .await
            .unwrap();

        assert_eq!(model.chat_id, -100123);
        assert_eq!(model.gid, 123456);
        assert_eq!(model.status, STATUS_PENDING);
        assert_eq!(model.source, SOURCE_SUBSCRIPTION);

        // get_next_pending should mark it as downloading
        let next = repo.get_next_pending_eh_download().await.unwrap().unwrap();
        assert_eq!(next.id, model.id);
        assert_eq!(next.status, STATUS_DOWNLOADING);
        assert!(next.started_at.is_some());

        // No more pending
        let none = repo.get_next_pending_eh_download().await.unwrap();
        assert!(none.is_none());
    }

    #[tokio::test]
    async fn test_mark_done() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        let model = repo
            .enqueue_eh_download(-100, 1, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();

        repo.get_next_pending_eh_download().await.unwrap();
        let done = repo.mark_eh_download_done(model.id, 50000).await.unwrap();

        assert_eq!(done.status, STATUS_DONE);
        assert_eq!(done.file_size, 50000);
        assert!(done.completed_at.is_some());
        assert!(done.error.is_none());
    }

    #[tokio::test]
    async fn test_mark_failed() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        let model = repo
            .enqueue_eh_download(-100, 1, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();

        repo.get_next_pending_eh_download().await.unwrap();
        let failed = repo
            .mark_eh_download_failed(model.id, "network error")
            .await
            .unwrap();

        assert_eq!(failed.status, STATUS_FAILED);
        assert_eq!(failed.error, Some("network error".to_string()));
        assert_eq!(failed.retry_count, 1);
        assert!(failed.completed_at.is_some());
    }

    #[tokio::test]
    async fn test_downloaded_bytes_in_window() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        // Enqueue and complete two downloads
        let m1 = repo
            .enqueue_eh_download(-100, 1, "tok1", "T1", false, SOURCE_DIRECT)
            .await
            .unwrap();
        repo.get_next_pending_eh_download().await.unwrap();
        repo.mark_eh_download_done(m1.id, 10000).await.unwrap();

        let m2 = repo
            .enqueue_eh_download(-100, 2, "tok2", "T2", false, SOURCE_DIRECT)
            .await
            .unwrap();
        repo.get_next_pending_eh_download().await.unwrap();
        repo.mark_eh_download_done(m2.id, 20000).await.unwrap();

        let bytes = repo.get_eh_downloaded_bytes_in_window(24).await.unwrap();
        assert_eq!(bytes, 30000);
    }

    #[tokio::test]
    async fn test_reset_stale_downloads() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        let m = repo
            .enqueue_eh_download(-100, 1, "tok", "T", false, SOURCE_DIRECT)
            .await
            .unwrap();
        repo.get_next_pending_eh_download().await.unwrap(); // marks as downloading

        // Simulate crash: entry is stuck in "downloading"
        let reset_count = repo.reset_stale_eh_downloads().await.unwrap();
        assert_eq!(reset_count, 1);

        // Should be pending again
        let next = repo.get_next_pending_eh_download().await.unwrap().unwrap();
        assert_eq!(next.id, m.id);
        assert_eq!(next.status, STATUS_DOWNLOADING); // got_next marks it downloading again
    }

    #[tokio::test]
    async fn test_count_pending() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        repo.enqueue_eh_download(-100, 1, "tok1", "T1", false, SOURCE_DIRECT)
            .await
            .unwrap();
        repo.enqueue_eh_download(-100, 2, "tok2", "T2", false, SOURCE_DIRECT)
            .await
            .unwrap();

        let count = repo.count_pending_eh_downloads().await.unwrap();
        assert_eq!(count, 2);
    }
}
