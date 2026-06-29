use super::Repo;
use crate::db::entities::eh_download_queue;
use anyhow::{Context, Result};
use chrono::Local;
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

    /// Mark a publish as completed successfully (archive and/or Telegraph sent).
    /// Only allowed when current status is `STATUS_PUBLISHING`.
    /// Preserves `completed_at` from the download stage if already set;
    /// sets it to now only if it hasn't been set yet.
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

        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_DONE),
            )
            .col_expr(eh_download_queue::Column::FileSize, Expr::value(file_size))
            .col_expr(
                eh_download_queue::Column::CompletedAt,
                Expr::value(completed_at),
            )
            .col_expr(
                eh_download_queue::Column::Error,
                Expr::value(None::<String>),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .exec(&self.db)
            .await
            .context("Failed to mark eh download as done")?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot mark EH download {} as done: expected status '{}', but it was changed by another worker",
                id,
                STATUS_PUBLISHING
            );
        }

        let model = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after mark done")?;
        Ok(model)
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
    /// Only allowed when current status is `STATUS_DOWNLOADING`.
    pub async fn mark_eh_download_downloaded(
        &self,
        id: i32,
        file_size: i64,
        zip_path: &str,
    ) -> Result<eh_download_queue::Model> {
        let now = Local::now().naive_local();

        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_DOWNLOADED),
            )
            .col_expr(eh_download_queue::Column::FileSize, Expr::value(file_size))
            .col_expr(
                eh_download_queue::Column::ZipPath,
                Expr::value(Some(zip_path.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::CompletedAt,
                Expr::value(now),
            )
            .col_expr(
                eh_download_queue::Column::StartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::Error,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_DOWNLOADING))
            .exec(&self.db)
            .await
            .context("Failed to mark eh download as downloaded")?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot mark EH download {} as downloaded: expected status '{}', but it was changed by another worker",
                id,
                STATUS_DOWNLOADING
            );
        }

        let model = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after mark downloaded")?;
        Ok(model)
    }

    /// Mark a download as uploaded (Telegraph page created). Transitions to `uploaded` status.
    /// Only allowed when current status is `STATUS_UPLOADING`.
    pub async fn mark_eh_download_uploaded(
        &self,
        id: i32,
        telegraph_url: &str,
    ) -> Result<eh_download_queue::Model> {
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_UPLOADED),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphUrl,
                Expr::value(Some(telegraph_url.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::Error,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_UPLOADING))
            .exec(&self.db)
            .await
            .context("Failed to mark eh download as uploaded")?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot mark EH download {} as uploaded: expected status '{}', but it was changed by another worker",
                id,
                STATUS_UPLOADING
            );
        }

        let model = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after mark uploaded")?;
        Ok(model)
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

        // Atomic claim: only flip if still pending with valid next_retry_at
        // (guards against concurrent workers and defers)
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
            .filter(
                sea_orm::Condition::any()
                    .add(eh_download_queue::Column::NextRetryAt.is_null())
                    .add(eh_download_queue::Column::NextRetryAt.lte(now)),
            )
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

        // Atomic claim: only flip if still downloaded+telegraph with valid next_retry_at
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
            .filter(eh_download_queue::Column::Telegraph.eq(true))
            .filter(
                sea_orm::Condition::any()
                    .add(eh_download_queue::Column::NextRetryAt.is_null())
                    .add(eh_download_queue::Column::NextRetryAt.lte(now)),
            )
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

        // Atomically claim: only flip if status is still the original AND next_retry_at is valid.
        // Also guard against row changes between select and update (telegraph toggle, re-enqueue).
        let original_status = model.status.clone();
        let status_filter = if original_status == STATUS_DOWNLOADED {
            // Must still be downloaded with telegraph=false (prevent claim of upgraded row)
            sea_orm::Condition::all()
                .add(eh_download_queue::Column::Status.eq(STATUS_DOWNLOADED))
                .add(eh_download_queue::Column::Telegraph.eq(false))
        } else {
            // Must still be uploaded
            sea_orm::Condition::all()
                .add(eh_download_queue::Column::Status.eq(STATUS_UPLOADED))
        };
        let retry_filter = sea_orm::Condition::any()
            .add(eh_download_queue::Column::NextRetryAt.is_null())
            .add(eh_download_queue::Column::NextRetryAt.lte(now));

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
            .filter(status_filter)
            .filter(retry_filter)
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
    /// Only updates rows currently in `STATUS_PUBLISHING`.
    pub async fn mark_eh_archive_sent(&self, id: i32) -> Result<()> {
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::ArchiveSentAt,
                Expr::value(Local::now().naive_local()),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .exec(&self.db)
            .await?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot mark archive sent for EH download {}: expected status '{}', but it was changed",
                id,
                STATUS_PUBLISHING
            );
        }
        Ok(())
    }

    /// Mark the Telegraph link as sent (publish stage progress marker).
    /// Only updates rows currently in `STATUS_PUBLISHING`.
    #[allow(dead_code)]
    pub async fn mark_eh_telegraph_sent(&self, id: i32) -> Result<()> {
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::TelegraphSentAt,
                Expr::value(Local::now().naive_local()),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .exec(&self.db)
            .await?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot mark telegraph sent for EH download {}: expected status '{}', but it was changed",
                id,
                STATUS_PUBLISHING
            );
        }
        Ok(())
    }

    /// Defer an entry: set status to `target_status` and delay next retry by `delay_secs`.
    /// Does NOT increment `retry_count` and does NOT set `error`.
    ///
    /// Legal target statuses: `STATUS_PENDING`, `STATUS_DOWNLOADED`, `STATUS_UPLOADED`.
    /// Current-status guards:
    /// - target `STATUS_PENDING`: current must be `STATUS_DOWNLOADING`.
    /// - target `STATUS_DOWNLOADED`: current must be `STATUS_UPLOADING` or `STATUS_PUBLISHING`.
    /// - target `STATUS_UPLOADED`: current must be `STATUS_PUBLISHING`.
    pub async fn defer_eh_download(
        &self,
        id: i32,
        target_status: &str,
        delay_secs: i64,
    ) -> Result<()> {
        let current_filter = match target_status {
            STATUS_PENDING => eh_download_queue::Column::Status.eq(STATUS_DOWNLOADING),
            STATUS_DOWNLOADED => eh_download_queue::Column::Status
                .is_in([STATUS_UPLOADING, STATUS_PUBLISHING]),
            STATUS_UPLOADED => eh_download_queue::Column::Status.eq(STATUS_PUBLISHING),
            _ => anyhow::bail!(
                "defer_eh_download: invalid target status '{}' (expected pending, downloaded, or uploaded)",
                target_status
            ),
        };

        let result = eh_download_queue::Entity::update_many()
            .col_expr(eh_download_queue::Column::Status, Expr::value(target_status))
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(Local::now().naive_local() + chrono::Duration::seconds(delay_secs)),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(current_filter)
            .exec(&self.db)
            .await?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot defer EH download {} to '{}': expected in-flight status, but it was changed by another worker",
                id,
                target_status
            );
        }
        Ok(())
    }

    /// Schedule a retry for an entry: set status back to target_status, increment retry_count,
    /// set next_retry_at to now + backoff. If retry_count exceeds max, set status=failed.
    /// Returns (model, is_permanent_failure).
    ///
    /// CAS guard: only updates from the expected in-flight status for the given target:
    /// - target `STATUS_PENDING`: current must be `STATUS_DOWNLOADING`.
    /// - target `STATUS_DOWNLOADED`: current may be `STATUS_UPLOADING` or `STATUS_PUBLISHING`.
    /// - target `STATUS_UPLOADED`: current must be `STATUS_PUBLISHING`.
    /// - any other target is an error.
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

        if is_permanent {
            // Permanent failure: no CAS needed — just mark failed.
            let _result = eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::Status,
                    Expr::value(STATUS_FAILED),
                )
                .col_expr(
                    eh_download_queue::Column::CompletedAt,
                    Expr::value(now),
                )
                .col_expr(
                    eh_download_queue::Column::Error,
                    Expr::value(Some(error.to_string())),
                )
                .col_expr(
                    eh_download_queue::Column::RetryCount,
                    Expr::value(new_retry_count),
                )
                .filter(eh_download_queue::Column::Id.eq(id))
                .exec(&self.db)
                .await
                .context("Failed to schedule retry (permanent)")?;

            let model = eh_download_queue::Entity::find_by_id(id)
                .one(&self.db)
                .await?
                .context("Entry disappeared after retry")?;
            return Ok((model, true));
        }

        // Determine the valid current-status filter for the target.
        let current_filter = match target_status {
            STATUS_PENDING => eh_download_queue::Column::Status.eq(STATUS_DOWNLOADING),
            STATUS_DOWNLOADED => eh_download_queue::Column::Status
                .is_in([STATUS_UPLOADING, STATUS_PUBLISHING]),
            STATUS_UPLOADED => eh_download_queue::Column::Status.eq(STATUS_PUBLISHING),
            _ => anyhow::bail!(
                "schedule_eh_retry: invalid target status '{}'",
                target_status
            ),
        };

        let delay = Self::backoff_delay_secs(new_retry_count);
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(target_status),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(now + chrono::Duration::seconds(delay)),
            )
            .col_expr(
                eh_download_queue::Column::StartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::Error,
                Expr::value(Some(error.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::RetryCount,
                Expr::value(new_retry_count),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(current_filter)
            .exec(&self.db)
            .await
            .context("Failed to schedule retry")?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot schedule retry for EH download {} to '{}': expected in-flight status, but it was changed by another worker",
                id,
                target_status
            );
        }

        let model = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after retry")?;
        Ok((model, false))
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
    async fn test_reenqueue_during_downloading_blocks_stale_download_completion() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 40, "tok", "Title", false, SOURCE_SUBSCRIPTION)
            .await
            .unwrap();

        // Claim for download (status -> downloading)
        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);
        assert_eq!(claimed.status, STATUS_DOWNLOADING);

        // Re-enqueue with source upgrade causes full reset (status -> pending)
        let reset = repo
            .enqueue_eh_download(-100, 40, "new", "New", false, SOURCE_DIRECT)
            .await
            .unwrap();
        assert_eq!(reset.id, model.id);
        assert_eq!(reset.status, STATUS_PENDING);

        // Stale worker tries to mark the old claimed row as downloaded — must fail
        let err = repo
            .mark_eh_download_downloaded(claimed.id, 9999, "/tmp/40.zip")
            .await;
        assert!(err.is_err(), "stale downloaded completion should be blocked");

        // Verify final state is still pending, not overwritten
        let final_row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(final_row.status, STATUS_PENDING);
        assert_ne!(final_row.file_size, 9999);
    }

    #[tokio::test]
    async fn test_publish_claim_requires_telegraph_false_for_downloaded() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 45, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();

        // Download stage: claim -> downloading -> downloaded
        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);
        repo.mark_eh_download_downloaded(model.id, 5000, "/tmp/45.zip")
            .await
            .unwrap();

        // Row is now downloaded, telegraph=false — publish should claim it
        let pub_claimed = repo.get_next_for_publish().await.unwrap().unwrap();
        assert_eq!(pub_claimed.id, model.id);
        assert_eq!(pub_claimed.status, STATUS_PUBLISHING);
        // Reset back to downloaded for the next check
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_DOWNLOADED))
            .col_expr(Column::Telegraph, Expr::value(true))
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
            .await
            .unwrap();

        // Now row is downloaded, telegraph=true — publish should NOT claim it
        let none = repo.get_next_for_publish().await.unwrap();
        assert!(
            none.is_none(),
            "publish should not claim downloaded row with telegraph=true"
        );
    }

    #[tokio::test]
    async fn test_marker_methods_require_publishing_status() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 50, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();

        // Row is pending — archive marker should fail
        let err = repo.mark_eh_archive_sent(model.id).await;
        assert!(
            err.is_err(),
            "mark_eh_archive_sent should fail on non-publishing row"
        );

        // Telegraph marker should also fail
        let err = repo.mark_eh_telegraph_sent(model.id).await;
        assert!(
            err.is_err(),
            "mark_eh_telegraph_sent should fail on non-publishing row"
        );

        // Move to publishing
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_PUBLISHING))
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
            .await
            .unwrap();

        // Now markers should succeed
        repo.mark_eh_archive_sent(model.id).await.unwrap();
        repo.mark_eh_telegraph_sent(model.id).await.unwrap();
    }

    #[tokio::test]
    async fn test_defer_rejects_invalid_status_transition() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 55, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();

        // Defer from pending to publishing — invalid (not an in-flight status)
        let err = repo
            .defer_eh_download(model.id, STATUS_PUBLISHING, 60)
            .await;
        assert!(
            err.is_err(),
            "defer to publishing from pending should fail"
        );

        // Defer from pending to failed — invalid (not a legal target)
        let err = repo
            .defer_eh_download(model.id, STATUS_FAILED, 60)
            .await;
        assert!(
            err.is_err(),
            "defer to failed should be rejected as invalid target"
        );

        // Defer from pending to pending — invalid (must be from downloading)
        let err = repo
            .defer_eh_download(model.id, STATUS_PENDING, 60)
            .await;
        assert!(
            err.is_err(),
            "defer to pending from pending should fail (must be from downloading)"
        );
    }


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

        // Move to publishing via the normal pipeline so CAS guards pass.
        // download: claim -> downloading -> downloaded
        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);
        repo.mark_eh_download_downloaded(model.id, 5000, "/tmp/20.zip")
            .await
            .unwrap();

        // Simulate upload: set telegraph_url so publish claims from uploaded branch
        Entity::update_many()
            .col_expr(Column::TelegraphUrl, Expr::value(Some("https://telegra.ph/20".to_string())))
            .col_expr(Column::Status, Expr::value(STATUS_UPLOADED))
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
            .await
            .unwrap();

        // Claim for publish -> publishing
        let pub_claimed = repo.get_next_for_publish().await.unwrap().unwrap();
        assert_eq!(pub_claimed.id, model.id);
        assert_eq!(pub_claimed.status, STATUS_PUBLISHING);

        // Mark archive sent while publishing
        repo.mark_eh_archive_sent(model.id).await.unwrap();

        // Defer back to publishing (simulates stale worker)
        repo.defer_eh_download(model.id, STATUS_UPLOADED, 60)
            .await
            .unwrap();
        // Move back to publishing manually for reset
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_PUBLISHING))
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
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
        // Claim to downloading so defer-to-pending passes the CAS guard.
        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);
        assert_eq!(claimed.status, STATUS_DOWNLOADING);

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
        // Claim to downloading so defer-to-pending passes the CAS guard.
        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);

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

        // Download stage: claim -> downloading -> downloaded
        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);
        repo.mark_eh_download_downloaded(model.id, 50000, "/tmp/1.zip")
            .await
            .unwrap();

        // Publish stage: claim -> publishing -> done
        let pub_claimed = repo.get_next_for_publish().await.unwrap().unwrap();
        assert_eq!(pub_claimed.id, model.id);
        assert_eq!(pub_claimed.status, STATUS_PUBLISHING);

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

        // Enqueue and complete two downloads through the full pipeline
        let m1 = repo
            .enqueue_eh_download(-100, 1, "tok1", "T1", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let c1 = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(c1.id, m1.id);
        repo.mark_eh_download_downloaded(m1.id, 10000, "/tmp/1.zip")
            .await
            .unwrap();
        let p1 = repo.get_next_for_publish().await.unwrap().unwrap();
        assert_eq!(p1.id, m1.id);
        repo.mark_eh_download_done(m1.id, 10000).await.unwrap();

        let m2 = repo
            .enqueue_eh_download(-100, 2, "tok2", "T2", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let c2 = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(c2.id, m2.id);
        repo.mark_eh_download_downloaded(m2.id, 20000, "/tmp/2.zip")
            .await
            .unwrap();
        let p2 = repo.get_next_for_publish().await.unwrap().unwrap();
        assert_eq!(p2.id, m2.id);
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
