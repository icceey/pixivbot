use super::Repo;
use crate::db::entities::eh_download_queue;
use anyhow::{Context, Result};
use chrono::{Local, Utc};
use sea_orm::prelude::DateTime;
use sea_orm::sea_query::Expr;
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
    /// Enqueue a download request. Returns the created/updated model.
    ///
    /// If an entry for the same (chat_id, gid) already exists:
    /// - If it's `done` or `failed`, reset to `pending` (re-download).
    /// - Otherwise, return the existing entry (already in queue).
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

        // Check for existing entry
        let existing = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::ChatId.eq(chat_id))
            .filter(eh_download_queue::Column::Gid.eq(gid))
            .one(&self.db)
            .await?;

        if let Some(model) = existing {
            return self
                .merge_eh_download(model, token, title, telegraph, source)
                .await;
        }

        // No existing entry — insert new; handle race on unique conflict
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

        match entry.insert(&self.db).await {
            Ok(model) => Ok(model),
            Err(db_err) => {
                // Race: another caller may have inserted the same (chat_id, gid).
                // Re-select and merge instead of failing on unique constraint.
                match eh_download_queue::Entity::find()
                    .filter(eh_download_queue::Column::ChatId.eq(chat_id))
                    .filter(eh_download_queue::Column::Gid.eq(gid))
                    .one(&self.db)
                    .await
                {
                    Ok(Some(raced)) => {
                        self.merge_eh_download(raced, token, title, telegraph, source)
                            .await
                    }
                    Ok(None) => Err(db_err).context("Failed to enqueue eh download"),
                    Err(select_err) => Err(select_err)
                        .context("Failed to re-select after insert conflict"),
                }
            }
        }
    }

    /// Merge an existing queue entry with new request parameters.
    ///
    /// - Terminal (`done`/`failed`): reset to `pending` with full transient clear.
    /// - Non-terminal: update token/title, merge telegraph (OR) and source (direct wins).
    ///   If the merge upgrades source to direct or adds telegraph to an already-uploaded
    ///   entry, reset to `pending` with full transient clear.
    async fn merge_eh_download(
        &self,
        existing: eh_download_queue::Model,
        token: &str,
        title: &str,
        telegraph: bool,
        source: &str,
    ) -> Result<eh_download_queue::Model> {
        let is_terminal = existing.status == STATUS_DONE || existing.status == STATUS_FAILED;
        let merged_telegraph = existing.telegraph || telegraph;
        let merged_source =
            if existing.source == SOURCE_DIRECT || source == SOURCE_DIRECT {
                SOURCE_DIRECT
            } else {
                SOURCE_SUBSCRIPTION
            };
        let source_upgraded_to_direct =
            existing.source != SOURCE_DIRECT && merged_source == SOURCE_DIRECT;
        let telegraph_upgraded = !existing.telegraph && merged_telegraph;
        let reset_for_new_requirement = source_upgraded_to_direct
            || (telegraph_upgraded
                && matches!(
                    existing.status.as_str(),
                    STATUS_UPLOADED | STATUS_PUBLISHING
                ));

        if is_terminal || reset_for_new_requirement {
            // Full reset to pending
            let mut active: eh_download_queue::ActiveModel = existing.into();
            active.status = Set(STATUS_PENDING.to_string());
            active.token = Set(token.to_string());
            active.title = Set(title.to_string());
            active.telegraph = Set(merged_telegraph);
            active.source = Set(merged_source.to_string());
            active.file_size = Set(0);
            active.error = Set(None);
            active.retry_count = Set(0);
            active.started_at = Set(None);
            active.completed_at = Set(None);
            active.zip_path = Set(None);
            active.telegraph_url = Set(None);
            active.next_retry_at = Set(None);
            active.archive_sent_at = Set(None);
            active.telegraph_sent_at = Set(None);
            return active
                .update(&self.db)
                .await
                .context("Failed to reset eh download for re-enqueue");
        }

        // Non-terminal: update in place, preserve progress
        let mut active: eh_download_queue::ActiveModel = existing.into();
        active.token = Set(token.to_string());
        active.title = Set(title.to_string());
        active.telegraph = Set(merged_telegraph);
        active.source = Set(merged_source.to_string());
        active
            .update(&self.db)
            .await
            .context("Failed to update eh download in place")
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
    /// Preserves `completed_at` from the download stage if already set;
    /// sets it to now only if it hasn't been set yet (e.g. direct pending→done path).
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
        let completed_at = entry.completed_at.unwrap_or(now);
        let mut active: eh_download_queue::ActiveModel = entry.into();
        active.status = Set(STATUS_DONE.to_string());
        active.file_size = Set(file_size);
        active.completed_at = Set(Some(completed_at));
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

    /// Get total bytes downloaded in the last `hours` window.
    /// Uses `completed_at` from the download stage (not overwritten by upload/publish stages).
    /// Uses SQL aggregate for efficiency.
    pub async fn get_eh_downloaded_bytes_in_window(&self, hours: u64) -> Result<i64> {
        let cutoff = Local::now().naive_local() - chrono::Duration::hours(hours as i64);

        let result = eh_download_queue::Entity::find()
            .filter(
                eh_download_queue::Column::Status.is_in([
                    STATUS_DOWNLOADED,
                    STATUS_UPLOADING,
                    STATUS_UPLOADED,
                    STATUS_PUBLISHING,
                    STATUS_DONE,
                ]),
            )
            .filter(eh_download_queue::Column::CompletedAt.gte(cutoff))
            .all(&self.db)
            .await
            .context("Failed to fetch eh downloads in window")?;

        Ok(result.iter().map(|e| e.file_size).sum())
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

    /// Reset stale in-flight entries back to their pre-claim status (crash recovery).
    ///
    /// Resets ALL three transient statuses:
    /// - `downloading` → `pending`
    /// - `uploading`   → `downloaded`
    /// - `publishing`  → `downloaded` (if telegraph=false) or `uploaded` (if telegraph_url is set)
    ///
    /// Should be called once at startup before any worker begins.
    pub async fn reset_stale_eh_downloads(&self) -> Result<u64> {
        let mut count = 0u64;

        // downloading → pending
        let stale_downloading = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_DOWNLOADING))
            .all(&self.db)
            .await
            .context("Failed to fetch stale downloading entries")?;
        for entry in stale_downloading {
            let mut active: eh_download_queue::ActiveModel = entry.into();
            active.status = Set(STATUS_PENDING.to_string());
            active.started_at = Set(None);
            active
                .update(&self.db)
                .await
                .context("Failed to reset stale downloading entry")?;
            count += 1;
        }

        // uploading → downloaded
        let stale_uploading = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_UPLOADING))
            .all(&self.db)
            .await
            .context("Failed to fetch stale uploading entries")?;
        for entry in stale_uploading {
            let mut active: eh_download_queue::ActiveModel = entry.into();
            active.status = Set(STATUS_DOWNLOADED.to_string());
            active.started_at = Set(None);
            active
                .update(&self.db)
                .await
                .context("Failed to reset stale uploading entry")?;
            count += 1;
        }

        // publishing → downloaded (telegraph=false) or uploaded (telegraph_url set)
        let stale_publishing = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .all(&self.db)
            .await
            .context("Failed to fetch stale publishing entries")?;
        for entry in stale_publishing {
            let target = if entry.telegraph_url.is_some() {
                STATUS_UPLOADED
            } else {
                STATUS_DOWNLOADED
            };
            let mut active: eh_download_queue::ActiveModel = entry.into();
            active.status = Set(target.to_string());
            active.started_at = Set(None);
            active
                .update(&self.db)
                .await
                .context("Failed to reset stale publishing entry")?;
            count += 1;
        }

        Ok(count)
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

        let mut active: eh_download_queue::ActiveModel = entry.into();
        active.status = Set(STATUS_UPLOADED.to_string());
        active.telegraph_url = Set(Some(telegraph_url.to_string()));
        active.error = Set(None);
        active.next_retry_at = Set(None);
        active
            .update(&self.db)
            .await
            .context("Failed to mark eh download as uploaded")
    }

    /// Get next entry for the download stage: status=pending, next_retry_at is NULL or <= now.
    /// Uses a conditional UPDATE to atomically claim the entry.
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

        let Some(model) = entry else {
            return Ok(None);
        };

        // Atomic claim: only flip if still pending (guards against concurrent workers)
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_DOWNLOADING),
            )
            .col_expr(eh_download_queue::Column::StartedAt, Expr::value(now))
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .exec(&self.db)
            .await
            .context("Failed to atomically claim download entry")?;

        if result.rows_affected == 0 {
            return Ok(None); // someone else claimed it
        }

        // Re-fetch the updated model
        let updated = eh_download_queue::Entity::find_by_id(model.id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after claim")?;
        Ok(Some(updated))
    }

    /// Get next entry for the upload stage: status=downloaded, telegraph=true, next_retry_at ok.
    /// Uses a conditional UPDATE to atomically claim the entry.
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

        let Some(model) = entry else {
            return Ok(None);
        };

        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_UPLOADING),
            )
            .col_expr(eh_download_queue::Column::StartedAt, Expr::value(now))
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_DOWNLOADED))
            .exec(&self.db)
            .await
            .context("Failed to atomically claim upload entry")?;

        if result.rows_affected == 0 {
            return Ok(None);
        }

        let updated = eh_download_queue::Entity::find_by_id(model.id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after claim")?;
        Ok(Some(updated))
    }

    /// Get next entry for the publish stage: either (downloaded, telegraph=false) or (uploaded).
    /// Uses a conditional UPDATE to atomically claim the entry.
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

        let Some(model) = entry else {
            return Ok(None);
        };

        // Atomically claim: only flip if status is still the original
        let original_status = model.status.clone();
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_PUBLISHING),
            )
            .col_expr(eh_download_queue::Column::StartedAt, Expr::value(now))
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(eh_download_queue::Column::Status.eq(&original_status))
            .exec(&self.db)
            .await
            .context("Failed to atomically claim publish entry")?;

        if result.rows_affected == 0 {
            return Ok(None);
        }

        let updated = eh_download_queue::Entity::find_by_id(model.id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after claim")?;
        Ok(Some(updated))
    }

    /// Mark the archive ZIP as sent (publish stage progress marker).
    pub async fn mark_eh_archive_sent(&self, id: i32) -> Result<()> {
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::ArchiveSentAt,
                Expr::value(Utc::now().naive_utc()),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// Mark the Telegraph link as sent (publish stage progress marker).
    #[allow(dead_code)]
    pub async fn mark_eh_telegraph_sent(&self, id: i32) -> Result<()> {
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::TelegraphSentAt,
                Expr::value(Utc::now().naive_utc()),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// Defer an entry: set status to `target_status` and delay next retry by `delay_secs`.
    /// Does NOT increment `retry_count` and does NOT set `error`.
    pub async fn defer_eh_download(
        &self,
        id: i32,
        target_status: &str,
        delay_secs: i64,
    ) -> Result<()> {
        eh_download_queue::Entity::update_many()
            .col_expr(eh_download_queue::Column::Status, Expr::value(target_status))
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(Local::now().naive_local() + chrono::Duration::seconds(delay_secs)),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .exec(&self.db)
            .await?;
        Ok(())
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
    use crate::db::entities::eh_download_queue::{Column, Entity};
    use crate::db::repo::tests_helpers;
    use chrono::Utc;
    use sea_orm::sea_query::Expr;

    #[tokio::test]
    async fn test_enqueue_merges_telegraph_and_direct_source() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let first = repo
            .enqueue_eh_download(-100, 10, "old", "Old", false, SOURCE_SUBSCRIPTION)
            .await
            .unwrap();
        let merged = repo
            .enqueue_eh_download(-100, 10, "new", "New", true, SOURCE_DIRECT)
            .await
            .unwrap();

        assert_eq!(first.id, merged.id);
        assert!(merged.telegraph);
        assert_eq!(merged.source, SOURCE_DIRECT);
        assert_eq!(merged.token, "new");
        assert_eq!(merged.title, "New");
    }

    #[tokio::test]
    async fn test_downloaded_bytes_window_counts_all_downloaded_states() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        for (gid, status, size) in [
            (1, STATUS_DOWNLOADED, 100),
            (2, STATUS_UPLOADING, 200),
            (3, STATUS_UPLOADED, 300),
            (4, STATUS_PUBLISHING, 400),
            (5, STATUS_DONE, 500),
        ] {
            let model = repo
                .enqueue_eh_download(-100, gid, "tok", "Title", false, SOURCE_DIRECT)
                .await
                .unwrap();
            Entity::update_many()
                .col_expr(Column::Status, Expr::value(status))
                .col_expr(Column::FileSize, Expr::value(size))
                .col_expr(
                    Column::CompletedAt,
                    Expr::value(Utc::now().naive_utc()),
                )
                .filter(Column::Id.eq(model.id))
                .exec(&repo.db)
                .await
                .unwrap();
        }

        let bytes = repo.get_eh_downloaded_bytes_in_window(24).await.unwrap();
        assert_eq!(bytes, 1500);
    }

    #[tokio::test]
    async fn test_publish_markers_survive_stale_reset_and_clear_on_terminal_reset() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 20, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();
        repo.mark_eh_archive_sent(model.id).await.unwrap();
        repo.defer_eh_download(model.id, STATUS_PUBLISHING, 60)
            .await
            .unwrap();
        repo.reset_stale_eh_downloads().await.unwrap();
        let preserved = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert!(preserved.archive_sent_at.is_some());

        repo.mark_eh_download_failed(model.id, "failed")
            .await
            .unwrap();
        let reset = repo
            .enqueue_eh_download(-100, 20, "new", "New", false, SOURCE_DIRECT)
            .await
            .unwrap();
        assert!(reset.archive_sent_at.is_none());
        assert!(reset.telegraph_sent_at.is_none());
    }

    #[tokio::test]
    async fn test_defer_does_not_increment_retry_count() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 30, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();
        repo.defer_eh_download(model.id, STATUS_PENDING, 60)
            .await
            .unwrap();
        let deferred = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(deferred.status, STATUS_PENDING);
        assert_eq!(deferred.retry_count, 0);
        assert!(deferred.next_retry_at.is_some());
    }

    #[tokio::test]
    async fn test_deferred_item_not_claimable_before_delay_expires() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 35, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();

        // Defer with a long delay — the item should not be picked up
        repo.defer_eh_download(model.id, STATUS_PENDING, 3600)
            .await
            .unwrap();

        // get_next_for_download filters on next_retry_at <= now, so should return None
        let next = repo.get_next_for_download().await.unwrap();
        assert!(next.is_none(), "deferred item should not be claimable before delay expires");

        let reloaded = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.retry_count, 0);
        assert!(reloaded.next_retry_at.is_some());
    }


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

    #[tokio::test]
    async fn test_queue_schema_has_publish_marker_columns() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let entry = repo
            .enqueue_eh_download(-100, 42, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();
        assert!(entry.archive_sent_at.is_none());
        assert!(entry.telegraph_sent_at.is_none());
    }
}
