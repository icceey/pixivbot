use super::Repo;
use crate::db::entities::{eh_download_queue, eh_gallery_jobs};
use crate::db::repo::eh_gallery_jobs::{
    eh_gallery_job_artifact_path, CLEANUP_STATUS_FAILED, CLEANUP_STATUS_NONE,
    CLEANUP_STATUS_PENDING, CLEANUP_STATUS_RUNNING, JOB_STATUS_DOWNLOADED, JOB_STATUS_DOWNLOADING,
    JOB_STATUS_PENDING, JOB_STATUS_RETIRED, TELEGRAPH_STATUS_PENDING, TELEGRAPH_STATUS_READY,
    TELEGRAPH_STATUS_UPLOADING,
};
use anyhow::{Context, Result};
use chrono::{Local, Timelike};
use eh_client::{ArchiveArtifacts, ImageUploader};
use sea_orm::prelude::DateTime;
use sea_orm::sea_query::{Expr, Query, SimpleExpr};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, Order, PaginatorTrait, QueryFilter, QueryOrder,
    QueryTrait, Set, TransactionTrait,
};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use tokio::sync::OwnedMutexGuard;
use tracing::warn;

use crate::db::entities::subscriptions;

/// Per-chat serialization for EH delivery mutations.
///
/// The registry retains only weak references.  A returned guard owns the
/// mutex while it is held, and removes its idle map entry when it was the last
/// user, so inactive chats cannot accumulate permanent mutexes.
#[derive(Default)]
pub struct EhChatLockRegistry {
    locks: Mutex<HashMap<i64, Weak<tokio::sync::Mutex<()>>>>,
}

pub struct EhChatLockGuard<'a> {
    registry: &'a EhChatLockRegistry,
    chat_id: i64,
    mutex: Arc<tokio::sync::Mutex<()>>,
    guard: Option<OwnedMutexGuard<()>>,
}

impl EhChatLockRegistry {
    pub async fn lock_chat(&self, chat_id: i64) -> EhChatLockGuard<'_> {
        let mutex = {
            let mut locks = self.locks.lock().expect("EH chat lock map poisoned");
            locks.retain(|_, mutex| mutex.strong_count() > 0);
            locks
                .entry(chat_id)
                .or_default()
                .upgrade()
                .unwrap_or_else(|| {
                    let mutex = Arc::new(tokio::sync::Mutex::new(()));
                    locks.insert(chat_id, Arc::downgrade(&mutex));
                    mutex
                })
        };
        let guard = mutex.clone().lock_owned().await;
        EhChatLockGuard {
            registry: self,
            chat_id,
            mutex,
            guard: Some(guard),
        }
    }

    pub async fn lock_chats(&self, chat_ids: &[i64]) -> Vec<EhChatLockGuard<'_>> {
        let mut chat_ids = chat_ids.to_vec();
        chat_ids.sort_unstable();
        chat_ids.dedup();

        let mut guards = Vec::with_capacity(chat_ids.len());
        for chat_id in chat_ids {
            guards.push(self.lock_chat(chat_id).await);
        }
        guards
    }
}

impl Drop for EhChatLockGuard<'_> {
    fn drop(&mut self) {
        // Release the asynchronous mutex while its map entry still exists, so
        // a caller that arrives during Drop can only reuse this mutex.
        drop(self.guard.take());

        let mut locks = self
            .registry
            .locks
            .lock()
            .expect("EH chat lock map poisoned");
        // `lock_chat` upgrades/creates entries while holding this same map
        // mutex.  Checking the matching weak entry and all remaining strong
        // users together therefore cannot split one chat into two mutexes.
        let should_remove = locks.get(&self.chat_id).is_some_and(|registered| {
            registered.ptr_eq(&Arc::downgrade(&self.mutex)) && Arc::strong_count(&self.mutex) == 1
        });
        if should_remove {
            locks.remove(&self.chat_id);
        }
    }
}

pub static EH_CHAT_LOCKS: LazyLock<EhChatLockRegistry> = LazyLock::new(EhChatLockRegistry::default);

/// Status constants for eh_download_queue.
pub const STATUS_PENDING: &str = "pending";
pub const STATUS_DOWNLOADING: &str = "downloading";
pub const STATUS_DONE: &str = "done";
pub const STATUS_FAILED: &str = "failed";
pub const STATUS_DOWNLOADED: &str = "downloaded";
pub const STATUS_UPLOADING: &str = "uploading";
pub const STATUS_UPLOADED: &str = "uploaded";
pub const STATUS_PUBLISHING: &str = "publishing";
pub const STATUS_CANCELED: &str = "canceled";
pub const BACKGROUND_STATUS_PENDING: &str = "pending";
pub const BACKGROUND_STATUS_RUNNING: &str = "running";
pub const TELEGRAPH_REWRITE_STATUS_PENDING: &str = "pending";
#[allow(dead_code)] // Historical queue-row rewrite state is query-compatible but no longer claimed.
pub const TELEGRAPH_REWRITE_STATUS_REWRITING: &str = "rewriting";
#[allow(dead_code)] // Historical queue-row rewrite state is query-compatible but no longer claimed.
pub const TELEGRAPH_REWRITE_STATUS_FAILED: &str = "failed";
#[allow(dead_code)]
const MAIN_DOWNLOAD_RECENT_WINDOW_HOURS: i64 = 2;

#[allow(dead_code)]
enum ArchivePolicyClaim {
    Main { started_at: DateTime },
}

/// `started_at` is a row-level claim generation after the first claim. Keeping
/// it monotonic at whole-second precision prevents same-tick ABA without a
/// schema migration, including on databases that truncate timestamp fractions.
fn next_claim_generation(now: DateTime, previous: Option<DateTime>) -> Result<DateTime> {
    let now_second = now
        .with_nanosecond(0)
        .context("Cannot normalize EH claim generation timestamp")?;
    let Some(previous) = previous else {
        return Ok(now_second);
    };
    let previous_second = previous
        .with_nanosecond(0)
        .context("Cannot normalize previous EH claim generation timestamp")?;
    let following_generation = previous_second
        .checked_add_signed(chrono::Duration::seconds(1))
        .context("EH claim generation timestamp overflow")?;

    Ok(now_second.max(following_generation))
}

fn claim_generation_filter(previous: Option<DateTime>) -> sea_orm::Condition {
    match previous {
        Some(generation) => {
            sea_orm::Condition::all().add(eh_download_queue::Column::StartedAt.eq(generation))
        }
        None => sea_orm::Condition::all().add(eh_download_queue::Column::StartedAt.is_null()),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EhQueueStatusItem {
    pub gid: i64,
    pub title: String,
    pub status: String,
    pub background_download_status: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EhQueueSnapshot {
    pub active: Vec<EhQueueStatusItem>,
    pub recent_terminal: Option<EhQueueStatusItem>,
}

/// A delivery claim paired with the shared-gallery job state that made it
/// publishable. The worker re-reads both rows under the chat lock before it
/// sends anything, so this snapshot is only the atomic claim hand-off.
#[derive(Clone, Debug)]
pub struct EhDeliveryClaim {
    pub delivery: eh_download_queue::Model,
    pub job: eh_gallery_jobs::Model,
}

fn eh_delivery_is_ready_for_publish(
    delivery: &eh_download_queue::Model,
    job: &eh_gallery_jobs::Model,
    send_archive: bool,
) -> bool {
    if job.cleanup_status != CLEANUP_STATUS_NONE {
        return false;
    }
    let archive_required = send_archive && delivery.archive_sent_at.is_none();
    let telegraph_required = delivery.telegraph && delivery.telegraph_sent_at.is_none();
    let telegraph_ready =
        job.telegraph_status == TELEGRAPH_STATUS_READY && job.telegraph_url.is_some();

    // A Telegraph consumer may not be claimed until the shared page is ready.
    // Archive-only consumers intentionally do not wait for a pending, running,
    // or terminally failed Telegraph upload.
    if telegraph_required && !telegraph_ready {
        return false;
    }

    let archive_ready = job.status == JOB_STATUS_DOWNLOADED && job.zip_path.is_some();
    (archive_required && archive_ready)
        || (telegraph_required && telegraph_ready)
        || (!archive_required && !telegraph_required)
}

fn eh_job_cleanup_is_none_filter(job_id: i32) -> SimpleExpr {
    Expr::exists(
        Query::select()
            .expr(Expr::value(1))
            .from(eh_gallery_jobs::Entity)
            .and_where(eh_gallery_jobs::Column::Id.eq(job_id))
            .and_where(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .to_owned(),
    )
}

impl EhQueueStatusItem {
    fn from_delivery(model: eh_download_queue::Model) -> Self {
        Self {
            gid: model.gid,
            title: model.title,
            status: model.status,
            background_download_status: model.background_download_status,
        }
    }

    /// Project the private shared-job state into the historical, user-facing
    /// delivery stages. The returned type deliberately contains no job ID,
    /// artifact path, token, or internal error text.
    fn from_delivery_and_job(
        delivery: eh_download_queue::Model,
        job: Option<eh_gallery_jobs::Model>,
    ) -> Self {
        let gid = delivery.gid;
        let title = delivery.title.clone();
        let own_status = delivery.status.clone();

        // Delivery state has precedence whenever it represents a concrete
        // publish or terminal outcome for this chat.
        if matches!(
            own_status.as_str(),
            STATUS_PUBLISHING | STATUS_DONE | STATUS_FAILED | STATUS_CANCELED
        ) {
            return Self {
                gid,
                title,
                status: own_status,
                background_download_status: None,
            };
        }

        // Only waiting shared deliveries derive their in-flight stage from the
        // shared job. Historical unbound active rows retain their own stage.
        if own_status != STATUS_WAITING {
            return Self::from_delivery(delivery);
        }
        let Some(job) = job else {
            return Self::from_delivery(delivery);
        };

        let (status, background_download_status) = if job.status == JOB_STATUS_RETIRED
            && matches!(
                job.cleanup_status.as_str(),
                CLEANUP_STATUS_PENDING | CLEANUP_STATUS_RUNNING | CLEANUP_STATUS_FAILED
            ) {
            (STATUS_PENDING, None)
        } else if matches!(
            job.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_PENDING | BACKGROUND_STATUS_RUNNING)
        ) {
            (STATUS_PENDING, job.background_download_status.as_deref())
        } else if job.status == JOB_STATUS_PENDING {
            (STATUS_PENDING, None)
        } else if job.status == JOB_STATUS_DOWNLOADING {
            (STATUS_DOWNLOADING, None)
        } else if delivery.telegraph
            && matches!(
                job.telegraph_status.as_str(),
                TELEGRAPH_STATUS_PENDING | TELEGRAPH_STATUS_UPLOADING
            )
        {
            (
                if job.telegraph_status == TELEGRAPH_STATUS_UPLOADING {
                    STATUS_UPLOADING
                } else {
                    STATUS_DOWNLOADED
                },
                None,
            )
        } else if job.status == JOB_STATUS_DOWNLOADED {
            (
                if delivery.telegraph && job.telegraph_status == TELEGRAPH_STATUS_READY {
                    STATUS_UPLOADED
                } else {
                    STATUS_DOWNLOADED
                },
                None,
            )
        } else {
            (STATUS_WAITING, None)
        };

        Self {
            gid,
            title,
            status: status.to_string(),
            background_download_status: background_download_status.map(str::to_string),
        }
    }
}

/// Source constants for eh_download_queue.
pub const SOURCE_SUBSCRIPTION: &str = "subscription";
pub const SOURCE_DIRECT: &str = "direct";
pub const STATUS_WAITING: &str = "waiting";

fn parse_subscription_ids(value: Option<&str>) -> BTreeSet<i32> {
    value
        .unwrap_or_default()
        .split(',')
        .filter_map(|part| part.parse::<i32>().ok())
        .collect()
}

fn format_subscription_ids(ids: &BTreeSet<i32>) -> Option<String> {
    if ids.is_empty() {
        None
    } else {
        Some(ids.iter().map(i32::to_string).collect::<Vec<_>>().join(","))
    }
}

pub(crate) fn merge_subscription_ids(current: Option<&str>, new_id: Option<i32>) -> Option<String> {
    let mut ids = parse_subscription_ids(current);
    if let Some(id) = new_id {
        ids.insert(id);
    }
    format_subscription_ids(&ids)
}

pub(crate) fn merge_telegraph_subscription_ids(
    current: Option<&str>,
    new_id: Option<i32>,
    telegraph: bool,
) -> Option<String> {
    let mut ids = parse_subscription_ids(current);
    if telegraph {
        if let Some(id) = new_id {
            ids.insert(id);
        }
    }
    format_subscription_ids(&ids)
}

fn is_active_subscription_queue_status(status: &str) -> bool {
    matches!(
        status,
        STATUS_WAITING
            | STATUS_PENDING
            | STATUS_DOWNLOADING
            | STATUS_DOWNLOADED
            | STATUS_UPLOADING
            | STATUS_UPLOADED
            | STATUS_PUBLISHING
    )
}

fn is_cancelable_subscription_queue_status(status: &str) -> bool {
    is_active_subscription_queue_status(status)
        || matches!(status, STATUS_DONE | STATUS_FAILED | STATUS_CANCELED)
}

fn subscription_ids_filter(expected: Option<String>) -> sea_orm::sea_query::SimpleExpr {
    match expected {
        Some(value) => eh_download_queue::Column::SubscriptionIds.eq(value),
        None => eh_download_queue::Column::SubscriptionIds.is_null(),
    }
}

fn telegraph_subscription_ids_filter(expected: Option<String>) -> sea_orm::sea_query::SimpleExpr {
    match expected {
        Some(value) => eh_download_queue::Column::TelegraphSubscriptionIds.eq(value),
        None => eh_download_queue::Column::TelegraphSubscriptionIds.is_null(),
    }
}

fn job_id_filter(expected: Option<i32>) -> sea_orm::sea_query::SimpleExpr {
    match expected {
        Some(value) => eh_download_queue::Column::JobId.eq(value),
        None => eh_download_queue::Column::JobId.is_null(),
    }
}

impl Repo {
    /// Merge an existing queue entry with new request parameters.
    ///
    /// - Terminal (`done`/`failed`): reset to `pending` with full transient clear.
    /// - Non-terminal: update token/title, merge telegraph (OR) and source (direct wins).
    ///   If the merge upgrades source to direct or adds telegraph to an already-uploaded
    ///   entry, reset to `pending` with full transient clear.
    ///
    /// Uses a retry loop with CAS guards: the in-place update checks that the row is
    /// still in the expected status (and expected telegraph for downloaded rows).
    /// If a concurrent worker changed the row between select and update, re-read and
    /// recompute the merge decision up to 3 attempts.
    #[cfg_attr(not(test), allow(dead_code))]
    async fn merge_eh_download(
        &self,
        existing: eh_download_queue::Model,
        token: &str,
        title: &str,
        telegraph: bool,
        source: &str,
        subscription_id: Option<i32>,
    ) -> Result<eh_download_queue::Model> {
        const MAX_RETRIES: usize = 3;
        let mut current = existing;

        for attempt in 0..MAX_RETRIES {
            let is_terminal = matches!(
                current.status.as_str(),
                STATUS_DONE | STATUS_FAILED | STATUS_CANCELED
            );
            let merged_source = if current.source == SOURCE_DIRECT || source == SOURCE_DIRECT {
                SOURCE_DIRECT
            } else {
                SOURCE_SUBSCRIPTION
            };
            let merged_subscription_ids = if merged_source == SOURCE_DIRECT {
                None
            } else {
                merge_subscription_ids(current.subscription_ids.as_deref(), subscription_id)
            };
            let merged_telegraph_subscription_ids = if merged_source == SOURCE_DIRECT {
                None
            } else {
                merge_telegraph_subscription_ids(
                    current.telegraph_subscription_ids.as_deref(),
                    subscription_id,
                    telegraph,
                )
            };
            let merged_telegraph = if merged_source == SOURCE_SUBSCRIPTION {
                merged_telegraph_subscription_ids.is_some()
            } else {
                current.telegraph || telegraph
            };
            let source_upgraded_to_direct =
                current.source != SOURCE_DIRECT && merged_source == SOURCE_DIRECT;
            let telegraph_upgraded = !current.telegraph && merged_telegraph;
            let reset_for_new_requirement = source_upgraded_to_direct
                || (telegraph_upgraded
                    && matches!(current.status.as_str(), STATUS_UPLOADED | STATUS_PUBLISHING));

            if is_terminal || reset_for_new_requirement {
                // Full reset to pending — CAS-guarded so a stale snapshot does not
                // blindly overwrite a row that was changed by another worker.
                let id = current.id;
                let expected_status = current.status.clone();
                let expected_telegraph = current.telegraph;
                let expected_source = current.source.clone();
                let expected_subscription_ids = current.subscription_ids.clone();

                let result = eh_download_queue::Entity::update_many()
                    .col_expr(
                        eh_download_queue::Column::Status,
                        Expr::value(STATUS_PENDING),
                    )
                    .col_expr(
                        eh_download_queue::Column::Token,
                        Expr::value(token.to_string()),
                    )
                    .col_expr(
                        eh_download_queue::Column::Title,
                        Expr::value(title.to_string()),
                    )
                    .col_expr(
                        eh_download_queue::Column::Telegraph,
                        Expr::value(merged_telegraph),
                    )
                    .col_expr(
                        eh_download_queue::Column::Source,
                        Expr::value(merged_source.to_string()),
                    )
                    .col_expr(
                        eh_download_queue::Column::SubscriptionIds,
                        Expr::value(merged_subscription_ids.clone()),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphSubscriptionIds,
                        Expr::value(merged_telegraph_subscription_ids.clone()),
                    )
                    .col_expr(eh_download_queue::Column::FileSize, Expr::value(0))
                    .col_expr(
                        eh_download_queue::Column::Error,
                        Expr::value(None::<String>),
                    )
                    .col_expr(eh_download_queue::Column::RetryCount, Expr::value(0))
                    .col_expr(
                        eh_download_queue::Column::CompletedAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::ZipPath,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphUrl,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::NextRetryAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::ArchiveSentAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphSentAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::BackgroundDownloadStatus,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::BackgroundDownloadStartedAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::BackgroundDownloadAttemptCount,
                        Expr::value(0),
                    )
                    .col_expr(
                        eh_download_queue::Column::BackgroundDownloadError,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteData,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteStatus,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteAfter,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteStartedAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteRetryCount,
                        Expr::value(0),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteError,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewrittenAt,
                        Expr::value(None::<DateTime>),
                    )
                    .filter(eh_download_queue::Column::Id.eq(id))
                    .filter(eh_download_queue::Column::Status.eq(&expected_status))
                    .filter(eh_download_queue::Column::Telegraph.eq(expected_telegraph))
                    .filter(eh_download_queue::Column::Source.eq(&expected_source))
                    .filter(subscription_ids_filter(expected_subscription_ids))
                    .exec(&self.db)
                    .await
                    .context("Failed to reset eh download for re-enqueue")?;

                if result.rows_affected == 1 {
                    let model = eh_download_queue::Entity::find_by_id(id)
                        .one(&self.db)
                        .await?
                        .context("Entry disappeared after reset")?;
                    return Ok(model);
                }

                // CAS failed — row changed between select and update.
                // Re-read and retry.
                if attempt + 1 < MAX_RETRIES {
                    current = match eh_download_queue::Entity::find_by_id(id)
                        .one(&self.db)
                        .await?
                    {
                        Some(fresh) => fresh,
                        None => anyhow::bail!("EH download {} disappeared during merge", id),
                    };
                    continue;
                }

                anyhow::bail!(
                    "Failed to reset EH download {} after {} attempts: row changed too frequently",
                    id,
                    MAX_RETRIES
                );
            }

            // Non-terminal: conditional update with CAS on expected status.
            // For downloaded rows, also guard on telegraph to prevent racing with
            // a publish worker that claimed the row between our select and update.
            let id = current.id;
            let expected_status = current.status.clone();
            let expected_telegraph = current.telegraph;
            let expected_source = current.source.clone();
            let expected_subscription_ids = current.subscription_ids.clone();

            let result = eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::Token,
                    Expr::value(token.to_string()),
                )
                .col_expr(
                    eh_download_queue::Column::Title,
                    Expr::value(title.to_string()),
                )
                .col_expr(
                    eh_download_queue::Column::Telegraph,
                    Expr::value(merged_telegraph),
                )
                .col_expr(
                    eh_download_queue::Column::Source,
                    Expr::value(merged_source.to_string()),
                )
                .col_expr(
                    eh_download_queue::Column::SubscriptionIds,
                    Expr::value(merged_subscription_ids.clone()),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphSubscriptionIds,
                    Expr::value(merged_telegraph_subscription_ids.clone()),
                )
                .filter(eh_download_queue::Column::Id.eq(id))
                .filter(eh_download_queue::Column::Status.eq(&expected_status))
                .filter(eh_download_queue::Column::Telegraph.eq(expected_telegraph))
                .filter(eh_download_queue::Column::Source.eq(&expected_source))
                .filter(subscription_ids_filter(expected_subscription_ids))
                .exec(&self.db)
                .await
                .context("Failed to update eh download in place")?;

            if result.rows_affected == 1 {
                // Success — re-read and return
                let model = eh_download_queue::Entity::find_by_id(id)
                    .one(&self.db)
                    .await?
                    .context("Entry disappeared after merge update")?;
                return Ok(model);
            }

            // CAS failed — row was changed between our select and update.
            // Re-read and retry.
            if attempt + 1 < MAX_RETRIES {
                current = match eh_download_queue::Entity::find_by_id(id)
                    .one(&self.db)
                    .await?
                {
                    Some(fresh) => fresh,
                    None => anyhow::bail!("EH download {} disappeared during merge", id),
                };
            }
        }

        anyhow::bail!(
            "Failed to merge EH download {} after {} attempts: row changed too frequently",
            current.id,
            MAX_RETRIES
        );
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
            let generation = next_claim_generation(now, model.started_at)?;
            let result = eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::Status,
                    Expr::value(STATUS_DOWNLOADING),
                )
                .col_expr(
                    eh_download_queue::Column::StartedAt,
                    Expr::value(generation),
                )
                .filter(eh_download_queue::Column::Id.eq(model.id))
                .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
                .filter(claim_generation_filter(model.started_at))
                .exec(&self.db)
                .await
                .context("Failed to mark eh download as downloading")?;
            if result.rows_affected == 0 {
                return Ok(None);
            }

            let updated = eh_download_queue::Entity::find()
                .filter(eh_download_queue::Column::Id.eq(model.id))
                .filter(eh_download_queue::Column::Status.eq(STATUS_DOWNLOADING))
                .filter(eh_download_queue::Column::StartedAt.eq(generation))
                .one(&self.db)
                .await
                .context("Failed to re-fetch legacy EH download claim")?;
            Ok(updated)
        } else {
            Ok(None)
        }
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

    /// Permanently fail a main download after its archive cost violates the
    /// configured policy. The original claim generation prevents a stale worker
    /// from overwriting a newer re-enqueued claim.
    #[allow(dead_code)]
    pub async fn fail_eh_download_for_archive_policy(
        &self,
        entry: &eh_download_queue::Model,
        error: &str,
    ) -> Result<eh_download_queue::Model> {
        let started_at = entry
            .started_at
            .context("Cannot fail EH download for archive policy: missing main claim started_at")?;
        self.fail_eh_download_for_archive_policy_claim(
            entry.id,
            error,
            ArchivePolicyClaim::Main { started_at },
        )
        .await
    }

    async fn fail_eh_download_for_archive_policy_claim(
        &self,
        id: i32,
        error: &str,
        claim: ArchivePolicyClaim,
    ) -> Result<eh_download_queue::Model> {
        let (expected_claim, claim_name): (SimpleExpr, &str) = match claim {
            ArchivePolicyClaim::Main { started_at } => (
                sea_orm::Condition::all()
                    .add(eh_download_queue::Column::Status.eq(STATUS_DOWNLOADING))
                    .add(eh_download_queue::Column::StartedAt.eq(started_at))
                    .into(),
                "main downloading claim",
            ),
        };
        let now = Local::now().naive_local();
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_FAILED),
            )
            .col_expr(
                eh_download_queue::Column::Error,
                Expr::value(Some(error.to_string())),
            )
            .col_expr(eh_download_queue::Column::CompletedAt, Expr::value(now))
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadAttemptCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadError,
                Expr::value(None::<String>),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(expected_claim)
            .exec(&self.db)
            .await
            .context("Failed to fail EH download for archive policy")?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot fail EH download {} for archive policy: expected {} claim, but it was changed by another worker",
                id,
                claim_name
            );
        }

        eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .context("Failed to fetch EH download after archive policy failure")?
            .context("Entry disappeared after archive policy failure")
    }

    /// Get the current queue status for one chat.
    pub async fn get_eh_queue_snapshot(&self, chat_id: i64) -> Result<EhQueueSnapshot> {
        let active = eh_download_queue::Entity::find()
            .find_also_related(eh_gallery_jobs::Entity)
            .filter(eh_download_queue::Column::ChatId.eq(chat_id))
            .filter(eh_download_queue::Column::Status.is_in([
                STATUS_WAITING,
                STATUS_PENDING,
                STATUS_DOWNLOADING,
                STATUS_DOWNLOADED,
                STATUS_UPLOADING,
                STATUS_UPLOADED,
                STATUS_PUBLISHING,
            ]))
            .order_by(eh_download_queue::Column::CreatedAt, Order::Asc)
            .all(&self.db)
            .await
            .context("Failed to fetch active EH queue entries")?
            .into_iter()
            .map(|(delivery, job)| EhQueueStatusItem::from_delivery_and_job(delivery, job))
            .collect();

        let recent_terminal = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::ChatId.eq(chat_id))
            .filter(eh_download_queue::Column::Status.is_in([
                STATUS_DONE,
                STATUS_FAILED,
                STATUS_CANCELED,
            ]))
            .order_by(eh_download_queue::Column::CreatedAt, Order::Desc)
            .one(&self.db)
            .await
            .context("Failed to fetch recent terminal EH queue entry")?
            .map(EhQueueStatusItem::from_delivery);

        Ok(EhQueueSnapshot {
            active,
            recent_terminal,
        })
    }

    /// Count pending downloads in the queue.
    #[allow(dead_code)]
    pub async fn count_pending_eh_downloads(&self) -> Result<u64> {
        eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.is_in([STATUS_WAITING, STATUS_PENDING]))
            .count(&self.db)
            .await
            .context("Failed to count pending eh downloads")
    }

    /// Cancel delivery ownership created for a subscription that has just been
    /// removed. Direct deliveries and other subscription owners are left
    /// untouched.
    pub async fn cancel_eh_subscription_queue_entries(
        &self,
        subscription_id: i32,
        send_archive: bool,
    ) -> Result<u64> {
        let chat_ids = self
            .find_eh_subscription_delivery_rows(subscription_id)
            .await?
            .into_iter()
            .map(|row| row.chat_id)
            .collect::<Vec<_>>();
        let _guards = EH_CHAT_LOCKS.lock_chats(&chat_ids).await;
        self.cancel_eh_subscription_queue_entries_under_chat_locks(subscription_id, send_archive)
            .await
    }

    /// Cancel legacy subscription queue rows that predate `subscription_ids`.
    ///
    /// They cannot be attributed to a specific subscription safely, so they are
    /// canceled instead of being published after an unsubscribe or after the
    /// migration introduces owner-aware queue semantics.  Already-canceled
    /// legacy rows are included so startup can repair rows from an older
    /// migration attempt that canceled them without clearing Telegraph state.
    #[cfg(test)] // Runtime cancellation is shared-job ownership based.
    pub async fn cancel_legacy_eh_subscription_queue_entries(&self) -> Result<u64> {
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_CANCELED),
            )
            .col_expr(eh_download_queue::Column::Telegraph, Expr::value(false))
            .col_expr(
                eh_download_queue::Column::TelegraphUrl,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphSubscriptionIds,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::ArchiveSentAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphSentAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteData,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteAfter,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteRetryCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteError,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewrittenAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Source.eq(SOURCE_SUBSCRIPTION))
            .filter(eh_download_queue::Column::SubscriptionIds.is_null())
            .filter(eh_download_queue::Column::Status.is_in([
                STATUS_WAITING,
                STATUS_PENDING,
                STATUS_DOWNLOADING,
                STATUS_DOWNLOADED,
                STATUS_UPLOADING,
                STATUS_UPLOADED,
                STATUS_PUBLISHING,
                STATUS_CANCELED,
                STATUS_DONE,
                STATUS_FAILED,
            ]))
            .exec(&self.db)
            .await
            .context("Failed to cancel legacy EH subscription queue entries")?;
        Ok(result.rows_affected)
    }

    /// Delete an EH subscription and cancel/prune its deliveries while holding
    /// that subscription's chat lock.
    pub async fn delete_eh_subscription_and_cancel_queue(
        &self,
        subscription_id: i32,
        send_archive: bool,
    ) -> Result<()> {
        let subscription = subscriptions::Entity::find_by_id(subscription_id)
            .one(&self.db)
            .await
            .context("Failed to load EH subscription before cancellation")?
            .context("EH subscription disappeared before cancellation")?;
        let _guard = EH_CHAT_LOCKS.lock_chat(subscription.chat_id).await;
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin EH subscription deletion transaction")?;
        let result: Result<()> = async {
            let deleted = subscriptions::Entity::delete_by_id(subscription_id)
                .exec(&txn)
                .await
                .context("Failed to delete EH subscription")?;
            anyhow::ensure!(
                deleted.rows_affected == 1,
                "EH subscription disappeared during cancellation"
            );
            self.cancel_eh_subscription_queue_entries_in_txn(&txn, subscription_id, send_archive)
                .await?;
            Ok(())
        }
        .await;
        match result {
            Ok(()) => txn
                .commit()
                .await
                .context("Failed to commit EH subscription deletion transaction"),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back EH subscription deletion transaction")?;
                Err(error)
            }
        }
    }

    async fn find_eh_subscription_delivery_rows(
        &self,
        subscription_id: i32,
    ) -> Result<Vec<eh_download_queue::Model>> {
        let rows = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Source.eq(SOURCE_SUBSCRIPTION))
            .filter(eh_download_queue::Column::SubscriptionIds.is_not_null())
            .filter(eh_download_queue::Column::Status.is_in([
                STATUS_WAITING,
                STATUS_PENDING,
                STATUS_DOWNLOADING,
                STATUS_DOWNLOADED,
                STATUS_UPLOADING,
                STATUS_UPLOADED,
                STATUS_PUBLISHING,
                STATUS_DONE,
                STATUS_FAILED,
                STATUS_CANCELED,
            ]))
            .all(&self.db)
            .await
            .context("Failed to cancel eh subscription queue entries")?;
        Ok(rows
            .into_iter()
            .filter(|row| {
                parse_subscription_ids(row.subscription_ids.as_deref()).contains(&subscription_id)
            })
            .collect())
    }

    async fn find_eh_subscription_delivery_rows_in_txn(
        &self,
        txn: &sea_orm::DatabaseTransaction,
        subscription_id: i32,
    ) -> Result<Vec<eh_download_queue::Model>> {
        let rows = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Source.eq(SOURCE_SUBSCRIPTION))
            .filter(eh_download_queue::Column::SubscriptionIds.is_not_null())
            .filter(eh_download_queue::Column::Status.is_in([
                STATUS_WAITING,
                STATUS_PENDING,
                STATUS_DOWNLOADING,
                STATUS_DOWNLOADED,
                STATUS_UPLOADING,
                STATUS_UPLOADED,
                STATUS_PUBLISHING,
                STATUS_DONE,
                STATUS_FAILED,
                STATUS_CANCELED,
            ]))
            .all(txn)
            .await
            .context("Failed to select EH subscription deliveries for cancellation")?;
        Ok(rows
            .into_iter()
            .filter(|row| {
                parse_subscription_ids(row.subscription_ids.as_deref()).contains(&subscription_id)
            })
            .collect())
    }

    async fn cancel_eh_subscription_queue_entries_under_chat_locks(
        &self,
        subscription_id: i32,
        send_archive: bool,
    ) -> Result<u64> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin EH subscription cancellation transaction")?;
        let result = self
            .cancel_eh_subscription_queue_entries_in_txn(&txn, subscription_id, send_archive)
            .await;
        match result {
            Ok(changed) => txn
                .commit()
                .await
                .context("Failed to commit EH subscription cancellation transaction")
                .map(|()| changed),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back EH subscription cancellation transaction")?;
                Err(error)
            }
        }
    }

    async fn cancel_eh_subscription_queue_entries_in_txn(
        &self,
        txn: &sea_orm::DatabaseTransaction,
        subscription_id: i32,
        send_archive: bool,
    ) -> Result<u64> {
        // Re-query after the caller has acquired every affected chat lock so a
        // send/cancel operation for the same chat observes one order.
        let active_rows = self
            .find_eh_subscription_delivery_rows_in_txn(txn, subscription_id)
            .await?;
        let mut changed = 0u64;
        let mut affected_job_ids = BTreeSet::new();
        for row in active_rows {
            let job_id = row.job_id;
            let row_changed = self
                .remove_subscription_owner_from_eh_row_in_txn(txn, row, subscription_id)
                .await?;
            changed += row_changed;
            if row_changed == 1 {
                if let Some(job_id) = job_id {
                    affected_job_ids.insert(job_id);
                }
            }
        }
        for job_id in affected_job_ids {
            self.recompute_eh_job_telegraph_requirement_in_txn(txn, job_id)
                .await?;
            self.evaluate_eh_job_liveness_in_txn(txn, job_id, send_archive)
                .await?;
        }
        Ok(changed)
    }

    async fn remove_subscription_owner_from_eh_row_in_txn(
        &self,
        txn: &sea_orm::DatabaseTransaction,
        mut row: eh_download_queue::Model,
        subscription_id: i32,
    ) -> Result<u64> {
        const MAX_RETRIES: usize = 3;
        for attempt in 0..MAX_RETRIES {
            if row.source != SOURCE_SUBSCRIPTION
                || !is_cancelable_subscription_queue_status(&row.status)
            {
                return Ok(0);
            }

            let mut ids = parse_subscription_ids(row.subscription_ids.as_deref());
            if !ids.remove(&subscription_id) {
                return Ok(0);
            }
            let remaining_ids = format_subscription_ids(&ids);
            let mut telegraph_ids =
                parse_subscription_ids(row.telegraph_subscription_ids.as_deref());
            telegraph_ids.remove(&subscription_id);
            let remaining_telegraph_ids = format_subscription_ids(&telegraph_ids);
            let canceled = remaining_ids.is_none();
            let telegraph_still_required = remaining_telegraph_ids.is_some();
            let new_status = if canceled && is_active_subscription_queue_status(&row.status) {
                STATUS_CANCELED.to_string()
            } else {
                row.status.clone()
            };
            let result = eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::SubscriptionIds,
                    Expr::value(remaining_ids),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphSubscriptionIds,
                    Expr::value(remaining_telegraph_ids),
                )
                .col_expr(eh_download_queue::Column::Status, Expr::value(new_status))
                .col_expr(
                    eh_download_queue::Column::Telegraph,
                    Expr::value(telegraph_still_required),
                )
                .filter(eh_download_queue::Column::Id.eq(row.id))
                .filter(eh_download_queue::Column::Status.eq(&row.status))
                .filter(eh_download_queue::Column::Source.eq(SOURCE_SUBSCRIPTION))
                .filter(job_id_filter(row.job_id))
                .filter(eh_download_queue::Column::SubscriptionIds.eq(row.subscription_ids.clone()))
                .filter(eh_download_queue::Column::Telegraph.eq(row.telegraph))
                .filter(telegraph_subscription_ids_filter(
                    row.telegraph_subscription_ids.clone(),
                ))
                .exec(txn)
                .await
                .context("Failed to cancel eh subscription queue entry")?;
            if result.rows_affected == 1 {
                return Ok(1);
            }

            if attempt + 1 == MAX_RETRIES {
                anyhow::bail!(
                    "Failed to cancel EH subscription queue entry {} after {} attempts: row changed too frequently",
                    row.id,
                    MAX_RETRIES
                );
            }

            match eh_download_queue::Entity::find_by_id(row.id)
                .one(txn)
                .await?
            {
                Some(fresh) => row = fresh,
                None => return Ok(0),
            }
        }
        Ok(0)
    }

    /// True if a claimed queue row is still active and has not been canceled
    /// after its originating subscription was removed.
    pub async fn eh_download_is_active(
        &self,
        id: i32,
        expected_status: &str,
        send_archive: bool,
    ) -> Result<bool> {
        let row = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(expected_status))
            .one(&self.db)
            .await
            .context("Failed to check eh download activity")?;
        let Some(row) = row else {
            return Ok(false);
        };
        if row.source != SOURCE_SUBSCRIPTION {
            return Ok(true);
        }
        self.eh_download_has_live_owner_or_cancel(row.id, expected_status, send_archive)
            .await
    }

    async fn eh_download_has_live_owner_or_cancel(
        &self,
        id: i32,
        expected_status: &str,
        send_archive: bool,
    ) -> Result<bool> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin inactive EH download soft-cancel transaction")?;
        let result: Result<bool> = async {
            let Some(mut row) = eh_download_queue::Entity::find()
                .filter(eh_download_queue::Column::Id.eq(id))
                .filter(eh_download_queue::Column::Status.eq(expected_status))
                .one(&txn)
                .await
                .context("Failed to re-read EH download activity in transaction")?
            else {
                return Ok(false);
            };
            const MAX_RETRIES: usize = 3;
            for attempt in 0..MAX_RETRIES {
                if row.status != expected_status {
                    return Ok(false);
                }
                if row.source != SOURCE_SUBSCRIPTION {
                    return Ok(true);
                }
                let ids = parse_subscription_ids(row.subscription_ids.as_deref());
                let alive = if ids.is_empty() {
                    false
                } else {
                    subscriptions::Entity::find()
                        .filter(subscriptions::Column::Id.is_in(ids.iter().copied()))
                        .count(&txn)
                        .await
                        .context("Failed to check EH subscription owners")?
                        > 0
                };
                if alive {
                    return Ok(true);
                }

                let result = eh_download_queue::Entity::update_many()
                    .col_expr(
                        eh_download_queue::Column::Status,
                        Expr::value(STATUS_CANCELED),
                    )
                    .col_expr(eh_download_queue::Column::Telegraph, Expr::value(false))
                    .col_expr(
                        eh_download_queue::Column::SubscriptionIds,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphSubscriptionIds,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphUrl,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::ArchiveSentAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphSentAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteData,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteStatus,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteAfter,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteStartedAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteRetryCount,
                        Expr::value(0),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteError,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewrittenAt,
                        Expr::value(None::<DateTime>),
                    )
                    .filter(eh_download_queue::Column::Id.eq(row.id))
                    .filter(eh_download_queue::Column::Status.eq(&row.status))
                    .filter(eh_download_queue::Column::Source.eq(SOURCE_SUBSCRIPTION))
                    .filter(eh_download_queue::Column::Telegraph.eq(row.telegraph))
                    .filter(subscription_ids_filter(row.subscription_ids.clone()))
                    .filter(telegraph_subscription_ids_filter(
                        row.telegraph_subscription_ids.clone(),
                    ))
                    .exec(&txn)
                    .await
                    .context("Failed to soft-cancel inactive EH download")?;
                if result.rows_affected == 1 {
                    if let Some(job_id) = row.job_id {
                        self.recompute_eh_job_telegraph_requirement_in_txn(&txn, job_id)
                            .await?;
                        self.evaluate_eh_job_liveness_in_txn(&txn, job_id, send_archive)
                            .await?;
                    }
                    return Ok(false);
                }

                if attempt + 1 == MAX_RETRIES {
                    anyhow::bail!(
                        "Failed to soft-cancel inactive EH download {} after {} attempts: row changed too frequently",
                        row.id,
                        MAX_RETRIES
                    );
                }

                match eh_download_queue::Entity::find_by_id(row.id).one(&txn).await? {
                    Some(fresh) => row = fresh,
                    None => return Ok(false),
                }
            }
            Ok(false)
        }
        .await;
        match result {
            Ok(active) => txn
                .commit()
                .await
                .context("Failed to commit inactive EH download soft-cancel transaction")
                .map(|()| active),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back inactive EH download soft-cancel transaction")?;
                Err(error)
            }
        }
    }

    /// Reset stale Telegraph rewrite claims back to pending rewrite work.
    #[allow(dead_code)] // Runtime stale recovery moved to shared eh_gallery_jobs.
    pub async fn reset_stale_eh_telegraph_rewrites(&self, stale_sec: i64) -> Result<u64> {
        let cutoff = Local::now().naive_local() - chrono::Duration::seconds(stale_sec);
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(Some(TELEGRAPH_REWRITE_STATUS_PENDING.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(None::<DateTime>),
            )
            .filter(
                eh_download_queue::Column::TelegraphRewriteStatus
                    .eq(TELEGRAPH_REWRITE_STATUS_REWRITING),
            )
            .filter(eh_download_queue::Column::TelegraphRewriteData.is_not_null())
            .filter(
                sea_orm::Condition::any()
                    .add(eh_download_queue::Column::TelegraphRewriteStartedAt.is_null())
                    .add(eh_download_queue::Column::TelegraphRewriteStartedAt.lte(cutoff)),
            )
            .exec(&self.db)
            .await
            .context("Failed to reset stale EH Telegraph rewrites")?;

        Ok(result.rows_affected)
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
    ///
    /// `gp_cost` is compatibility/display metadata for the most recent successful
    /// archive download (0 for free / unlocked). The append-only
    /// `eh_gp_spend_attempts` ledger calculates rolling GP budgets.
    #[allow(dead_code)]
    pub async fn mark_eh_download_downloaded(
        &self,
        id: i32,
        file_size: i64,
        zip_path: &str,
        gp_cost: i64,
    ) -> Result<eh_download_queue::Model> {
        let now = Local::now().naive_local();

        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_DOWNLOADED),
            )
            .col_expr(eh_download_queue::Column::FileSize, Expr::value(file_size))
            .col_expr(eh_download_queue::Column::GpCost, Expr::value(gp_cost))
            .col_expr(
                eh_download_queue::Column::ZipPath,
                Expr::value(Some(zip_path.to_string())),
            )
            .col_expr(eh_download_queue::Column::CompletedAt, Expr::value(now))
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
    #[allow(dead_code)]
    pub async fn mark_eh_download_uploaded(
        &self,
        id: i32,
        telegraph_url: &str,
    ) -> Result<eh_download_queue::Model> {
        self.mark_eh_download_uploaded_with_rewrite(id, telegraph_url, None)
            .await
    }

    /// Mark a download as uploaded and store optional post-send Telegraph rewrite metadata.
    pub async fn mark_eh_download_uploaded_with_rewrite(
        &self,
        id: i32,
        telegraph_url: &str,
        rewrite_data_json: Option<&str>,
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
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteData,
                Expr::value(rewrite_data_json.map(str::to_string)),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteAfter,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteRetryCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteError,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewrittenAt,
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

    /// Fallback a permanently failed Telegraph upload to archive-only delivery.
    /// Sets telegraph=false, status=downloaded, clears next_retry_at,
    /// telegraph_url, archive_sent_at, and telegraph_sent_at so publish
    /// workers do not send stale Telegraph links.
    /// Only updates rows currently in `STATUS_UPLOADING`.
    #[allow(dead_code)] // Historical per-delivery upload compatibility API; runtime claims jobs.
    pub async fn fallback_eh_upload_to_archive(
        &self,
        id: i32,
        error: &str,
    ) -> Result<eh_download_queue::Model> {
        let result = eh_download_queue::Entity::update_many()
            .col_expr(eh_download_queue::Column::Telegraph, Expr::value(false))
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_DOWNLOADED),
            )
            .col_expr(
                eh_download_queue::Column::Error,
                Expr::value(Some(error.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(eh_download_queue::Column::RetryCount, Expr::value(0))
            .col_expr(
                eh_download_queue::Column::TelegraphUrl,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphSubscriptionIds,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::ArchiveSentAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphSentAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteData,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteAfter,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteRetryCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteError,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewrittenAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_UPLOADING))
            .exec(&self.db)
            .await?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot fallback EH upload {} to archive: expected status '{}', but it was changed",
                id,
                STATUS_UPLOADING
            );
        }

        let model = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after upload fallback")?;
        Ok(model)
    }

    /// Disable Telegraph delivery for rows that have not produced a Telegraph URL yet.
    ///
    /// This is used at startup when no Telegraph token is configured.  Rows with an
    /// existing `telegraph_url` are left untouched because they are already publishable;
    /// rows without a URL are downgraded to archive-only so they can be downloaded or
    /// published without an upload worker.  Terminal rows have only their Telegraph flag
    /// cleared so a later plain re-enqueue does not OR-merge the stale preference back in.
    #[allow(dead_code)] // Legacy compatibility API; startup owns only shared job state.
    pub async fn disable_eh_telegraph_for_unuploaded_entries(&self) -> Result<u64> {
        let mut changed = 0u64;

        // Pre-download in-flight work should restart from the download queue.
        let pending = eh_download_queue::Entity::update_many()
            .col_expr(eh_download_queue::Column::Telegraph, Expr::value(false))
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_PENDING),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphUrl,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphSubscriptionIds,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::ArchiveSentAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphSentAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteData,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteAfter,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteRetryCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteError,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewrittenAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Telegraph.eq(true))
            .filter(eh_download_queue::Column::TelegraphUrl.is_null())
            .filter(eh_download_queue::Column::Status.is_in([STATUS_PENDING, STATUS_DOWNLOADING]))
            .exec(&self.db)
            .await
            .context("Failed to disable unuploaded EH Telegraph pending entries")?;
        changed += pending.rows_affected;

        // ZIP already exists or should exist: publish as archive-only.
        let downloaded = eh_download_queue::Entity::update_many()
            .col_expr(eh_download_queue::Column::Telegraph, Expr::value(false))
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_DOWNLOADED),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphUrl,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphSubscriptionIds,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::ArchiveSentAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphSentAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteData,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteAfter,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteRetryCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteError,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewrittenAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Telegraph.eq(true))
            .filter(eh_download_queue::Column::TelegraphUrl.is_null())
            .filter(eh_download_queue::Column::Status.is_in([
                STATUS_DOWNLOADED,
                STATUS_UPLOADING,
                STATUS_UPLOADED,
                STATUS_PUBLISHING,
            ]))
            .exec(&self.db)
            .await
            .context("Failed to disable unuploaded EH Telegraph downloaded entries")?;
        changed += downloaded.rows_affected;

        // Terminal rows do not need status changes, but clearing the stale flag prevents
        // future plain `/edl` re-enqueues from OR-merging Telegraph back to true.
        let terminal = eh_download_queue::Entity::update_many()
            .col_expr(eh_download_queue::Column::Telegraph, Expr::value(false))
            .col_expr(
                eh_download_queue::Column::TelegraphSubscriptionIds,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteData,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteAfter,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteRetryCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteError,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewrittenAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Telegraph.eq(true))
            .filter(eh_download_queue::Column::Status.is_in([
                STATUS_DONE,
                STATUS_FAILED,
                STATUS_CANCELED,
            ]))
            .exec(&self.db)
            .await
            .context("Failed to disable unuploaded EH Telegraph terminal entries")?;
        changed += terminal.rows_affected;

        Ok(changed)
    }

    /// Get next entry for the download stage: status=pending, next_retry_at is NULL or <= now.
    /// Uses a conditional UPDATE to atomically claim the entry.
    #[allow(dead_code)]
    pub async fn get_next_for_download(&self) -> Result<Option<eh_download_queue::Model>> {
        let now = Local::now().naive_local();
        self.get_next_for_download_at(now).await
    }

    #[allow(dead_code)]
    async fn get_next_for_download_at(
        &self,
        now: DateTime,
    ) -> Result<Option<eh_download_queue::Model>> {
        let cutoff = now - chrono::Duration::hours(MAIN_DOWNLOAD_RECENT_WINDOW_HOURS);
        let is_recent = Expr::col(eh_download_queue::Column::CreatedAt).gt(cutoff);
        let recent_priority: SimpleExpr = Expr::case(is_recent.clone(), 0).finally(1).into();
        let recent_created_at: SimpleExpr = Expr::case(
            is_recent.clone(),
            Expr::col(eh_download_queue::Column::CreatedAt),
        )
        .finally(Expr::value(None::<DateTime>))
        .into();
        let recent_id: SimpleExpr =
            Expr::case(is_recent.clone(), Expr::col(eh_download_queue::Column::Id))
                .finally(Expr::value(None::<i32>))
                .into();
        let old_created_at: SimpleExpr =
            Expr::case(is_recent.clone(), Expr::value(None::<DateTime>))
                .finally(Expr::col(eh_download_queue::Column::CreatedAt))
                .into();
        let old_id: SimpleExpr = Expr::case(is_recent, Expr::value(None::<i32>))
            .finally(Expr::col(eh_download_queue::Column::Id))
            .into();
        let mut query = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(eh_download_queue::Column::BackgroundDownloadStatus.is_null())
            .filter(
                eh_download_queue::Column::NextRetryAt
                    .is_null()
                    .or(eh_download_queue::Column::NextRetryAt.lte(now)),
            );
        QueryTrait::query(&mut query)
            .order_by_expr(recent_priority, Order::Asc)
            .order_by_expr(recent_created_at, Order::Asc)
            .order_by_expr(recent_id, Order::Asc)
            .order_by_expr(old_created_at, Order::Desc)
            .order_by_expr(old_id, Order::Desc);
        let entry = query
            .one(&self.db)
            .await
            .context("Failed to fetch next for download")?;

        let Some(model) = entry else {
            return Ok(None);
        };

        self.claim_main_download_from_snapshot_at(&model, now).await
    }

    #[allow(dead_code)]
    async fn claim_main_download_from_snapshot_at(
        &self,
        model: &eh_download_queue::Model,
        now: DateTime,
    ) -> Result<Option<eh_download_queue::Model>> {
        let generation = next_claim_generation(now, model.started_at)?;
        // Atomic claim: only flip if still pending with valid next_retry_at
        // and the selected previous generation (guards stale selectors too).
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_DOWNLOADING),
            )
            .col_expr(
                eh_download_queue::Column::StartedAt,
                Expr::value(generation),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(eh_download_queue::Column::BackgroundDownloadStatus.is_null())
            .filter(claim_generation_filter(model.started_at))
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

        // Confirm this worker's status and generation survived until readback.
        let updated = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_DOWNLOADING))
            .filter(eh_download_queue::Column::BackgroundDownloadStatus.is_null())
            .filter(eh_download_queue::Column::StartedAt.eq(generation))
            .one(&self.db)
            .await?;
        Ok(updated)
    }

    /// Get next entry for the upload stage: status=downloaded, telegraph=true, next_retry_at ok.
    /// Uses a conditional UPDATE to atomically claim the entry.
    #[allow(dead_code)] // Historical per-delivery upload compatibility API; runtime claims jobs.
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

        let generation = next_claim_generation(now, model.started_at)?;
        // Atomic claim: only flip if still downloaded+telegraph with valid next_retry_at
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_UPLOADING),
            )
            .col_expr(
                eh_download_queue::Column::StartedAt,
                Expr::value(generation),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_DOWNLOADED))
            .filter(eh_download_queue::Column::Telegraph.eq(true))
            .filter(claim_generation_filter(model.started_at))
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

        let updated = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_UPLOADING))
            .filter(eh_download_queue::Column::StartedAt.eq(generation))
            .one(&self.db)
            .await?;
        Ok(updated)
    }

    /// Claim the next due shared-gallery delivery that has at least one ready
    /// publish surface. Legacy rows without a `job_id` are deliberately never
    /// claimed by this runtime lane.
    pub async fn get_next_eh_delivery_for_publish(
        &self,
        send_archive: bool,
    ) -> Result<Option<EhDeliveryClaim>> {
        let now = Local::now().naive_local();
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH delivery publish claim transaction")?;
        let result = async {
            let candidates = eh_download_queue::Entity::find()
                .find_also_related(eh_gallery_jobs::Entity)
                .filter(eh_download_queue::Column::Status.eq(STATUS_WAITING))
                .filter(eh_download_queue::Column::JobId.is_not_null())
                .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
                .filter(
                    eh_download_queue::Column::NextRetryAt
                        .is_null()
                        .or(eh_download_queue::Column::NextRetryAt.lte(now)),
                )
                .order_by(eh_download_queue::Column::CreatedAt, Order::Asc)
                .order_by(eh_download_queue::Column::Id, Order::Asc)
                .all(&txn)
                .await
                .context("Failed to fetch due shared EH deliveries for publish")?;

            for (delivery, job) in candidates {
                let Some(job) = job else {
                    continue;
                };
                if !eh_delivery_is_ready_for_publish(&delivery, &job, send_archive) {
                    continue;
                }

                let generation = next_claim_generation(now, delivery.started_at)?;
                let claimed = eh_download_queue::Entity::update_many()
                    .col_expr(
                        eh_download_queue::Column::Status,
                        Expr::value(STATUS_PUBLISHING),
                    )
                    .col_expr(
                        eh_download_queue::Column::StartedAt,
                        Expr::value(generation),
                    )
                    .col_expr(
                        eh_download_queue::Column::NextRetryAt,
                        Expr::value(None::<DateTime>),
                    )
                    .filter(eh_download_queue::Column::Id.eq(delivery.id))
                    .filter(eh_download_queue::Column::JobId.eq(job.id))
                    .filter(eh_download_queue::Column::Status.eq(STATUS_WAITING))
                    .filter(eh_job_cleanup_is_none_filter(job.id))
                    .filter(claim_generation_filter(delivery.started_at))
                    .filter(
                        eh_download_queue::Column::NextRetryAt
                            .is_null()
                            .or(eh_download_queue::Column::NextRetryAt.lte(now)),
                    )
                    .exec(&txn)
                    .await
                    .context("Failed to atomically claim shared EH delivery for publish")?;
                if claimed.rows_affected == 0 {
                    continue;
                }

                let claimed_delivery = eh_download_queue::Entity::find()
                    .filter(eh_download_queue::Column::Id.eq(delivery.id))
                    .filter(eh_download_queue::Column::JobId.eq(job.id))
                    .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
                    .filter(eh_download_queue::Column::StartedAt.eq(generation))
                    .one(&txn)
                    .await
                    .context("Failed to reread shared EH delivery publish claim")?
                    .context("Shared EH delivery changed before publish claim readback")?;
                let claimed_job = eh_gallery_jobs::Entity::find_by_id(job.id)
                    .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
                    .one(&txn)
                    .await
                    .context("Failed to reread shared EH job for delivery claim")?
                    .context("Shared EH job changed before delivery claim readback")?;
                return Ok(Some(EhDeliveryClaim {
                    delivery: claimed_delivery,
                    job: claimed_job,
                }));
            }

            Ok(None)
        }
        .await;

        match result {
            Ok(Some(claim)) => {
                txn.commit()
                    .await
                    .context("Failed to commit shared EH delivery publish claim transaction")?;
                Ok(Some(claim))
            }
            Ok(None) => {
                txn.rollback().await.context(
                    "Failed to roll back empty shared EH delivery publish claim transaction",
                )?;
                Ok(None)
            }
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back shared EH delivery publish claim transaction")?;
                Err(error)
            }
        }
    }

    /// Re-read a currently publishing delivery with its shared job. Publish
    /// workers call this after acquiring the keyed chat lock so cancellation,
    /// marker progress, and job readiness are observed as one fresh snapshot.
    pub async fn get_eh_delivery_publish_claim(
        &self,
        delivery_id: i32,
    ) -> Result<Option<EhDeliveryClaim>> {
        let Some((delivery, job)) = eh_download_queue::Entity::find()
            .find_also_related(eh_gallery_jobs::Entity)
            .filter(eh_download_queue::Column::Id.eq(delivery_id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .one(&self.db)
            .await
            .context("Failed to reread shared EH publishing delivery")?
        else {
            return Ok(None);
        };
        let Some(job) = job else {
            return Ok(None);
        };
        if job.cleanup_status != CLEANUP_STATUS_NONE {
            eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::Status,
                    Expr::value(STATUS_WAITING),
                )
                .col_expr(
                    eh_download_queue::Column::NextRetryAt,
                    Expr::value(None::<DateTime>),
                )
                .filter(eh_download_queue::Column::Id.eq(delivery.id))
                .filter(eh_download_queue::Column::JobId.eq(job.id))
                .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
                .exec(&self.db)
                .await
                .context("Failed to release dirty shared EH publishing delivery")?;
            return Ok(None);
        }
        Ok(Some(EhDeliveryClaim { delivery, job }))
    }

    /// Check whether a claimed delivery still has a live owner. Subscription
    /// ownership uses the existing cancellation-safe check; direct deliveries
    /// remain active while their expected claim status survives.
    pub async fn eh_delivery_is_active(
        &self,
        delivery_id: i32,
        expected_status: &str,
        send_archive: bool,
    ) -> Result<bool> {
        self.eh_download_is_active(delivery_id, expected_status, send_archive)
            .await
    }

    /// Release a claimed delivery without burning a retry. A positive delay is
    /// required so a tick that is refilling workers cannot reclaim it forever.
    pub async fn defer_eh_delivery_publish(&self, delivery_id: i32, delay_secs: i64) -> Result<()> {
        let retry_at = Local::now().naive_local() + chrono::Duration::seconds(delay_secs.max(1));
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_WAITING),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(Some(retry_at)),
            )
            .filter(eh_download_queue::Column::Id.eq(delivery_id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .exec(&self.db)
            .await
            .context("Failed to defer shared EH delivery publish")?;
        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot defer shared EH delivery {}: publishing claim changed",
                delivery_id
            );
        }
        Ok(())
    }

    /// Retry only the failed chat delivery. Shared download/upload/page state
    /// remains on its job and is intentionally untouched.
    pub async fn schedule_eh_delivery_retry(
        &self,
        delivery_id: i32,
        error: &str,
        max_retry_count: u8,
        send_archive: bool,
    ) -> Result<(eh_download_queue::Model, bool)> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH delivery retry transaction")?;
        let result: Result<(eh_download_queue::Model, bool)> = async {
            let delivery = eh_download_queue::Entity::find_by_id(delivery_id)
                .one(&txn)
                .await
                .context("Failed to fetch shared EH delivery for retry")?
                .context("Shared EH delivery disappeared before retry")?;
            let retry_count = delivery
                .retry_count
                .checked_add(1)
                .context("Shared EH delivery retry count overflow")?;
            let terminal = retry_count > i32::from(max_retry_count);
            let now = Local::now().naive_local();
            let mut update = eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::Status,
                    Expr::value(if terminal {
                        STATUS_FAILED
                    } else {
                        STATUS_WAITING
                    }),
                )
                .col_expr(
                    eh_download_queue::Column::Error,
                    Expr::value(Some(error.to_string())),
                )
                .col_expr(
                    eh_download_queue::Column::RetryCount,
                    Expr::value(retry_count),
                )
                .filter(eh_download_queue::Column::Id.eq(delivery_id))
                .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
                .filter(job_id_filter(delivery.job_id));
            if terminal {
                update = update
                    .col_expr(
                        eh_download_queue::Column::CompletedAt,
                        Expr::value(Some(now)),
                    )
                    .col_expr(
                        eh_download_queue::Column::NextRetryAt,
                        Expr::value(None::<DateTime>),
                    );
            } else {
                let retry_at = now
                    .checked_add_signed(chrono::Duration::seconds(Self::backoff_delay_secs(
                        retry_count,
                    )))
                    .context("Shared EH delivery retry deadline overflow")?;
                update = update.col_expr(
                    eh_download_queue::Column::NextRetryAt,
                    Expr::value(Some(retry_at)),
                );
            }
            let result = update
                .exec(&txn)
                .await
                .context("Failed to schedule shared EH delivery retry")?;
            if result.rows_affected != 1 {
                anyhow::bail!(
                    "Cannot retry shared EH delivery {}: publishing claim changed",
                    delivery_id
                );
            }
            if terminal {
                if let Some(job_id) = delivery.job_id {
                    self.recompute_eh_job_telegraph_requirement_in_txn(&txn, job_id)
                        .await?;
                    self.evaluate_eh_job_liveness_in_txn(&txn, job_id, send_archive)
                        .await?;
                }
            }
            let updated = eh_download_queue::Entity::find_by_id(delivery_id)
                .one(&txn)
                .await
                .context("Failed to reread shared EH delivery after retry")?
                .context("Shared EH delivery disappeared after retry")?;
            Ok((updated, terminal))
        }
        .await;
        match result {
            Ok(outcome) => txn
                .commit()
                .await
                .context("Failed to commit shared EH delivery retry transaction")
                .map(|()| outcome),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back shared EH delivery retry transaction")?;
                Err(error)
            }
        }
    }

    /// Persist the archive marker for one delivery immediately after Telegram
    /// accepts its document. The marker is deliberately delivery-local.
    pub async fn mark_eh_archive_delivery_sent(&self, delivery_id: i32) -> Result<()> {
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::ArchiveSentAt,
                Expr::value(Some(Local::now().naive_local())),
            )
            .filter(eh_download_queue::Column::Id.eq(delivery_id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .filter(eh_download_queue::Column::ArchiveSentAt.is_null())
            .exec(&self.db)
            .await
            .context("Failed to mark shared EH archive delivery sent")?;
        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot mark archive sent for shared EH delivery {}: publishing claim changed",
                delivery_id
            );
        }
        Ok(())
    }

    /// Finish only this delivery after all of its enabled/requested surfaces
    /// have durable sent markers.
    pub async fn mark_eh_delivery_done(
        &self,
        delivery_id: i32,
        expected_job_id: i32,
        send_archive: bool,
    ) -> Result<eh_download_queue::Model> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH delivery completion transaction")?;
        let result: Result<eh_download_queue::Model> = async {
            let updated = eh_download_queue::Entity::update_many()
                .col_expr(eh_download_queue::Column::Status, Expr::value(STATUS_DONE))
                .col_expr(
                    eh_download_queue::Column::CompletedAt,
                    Expr::value(Some(Local::now().naive_local())),
                )
                .col_expr(
                    eh_download_queue::Column::NextRetryAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::Error,
                    Expr::value(None::<String>),
                )
                .filter(eh_download_queue::Column::Id.eq(delivery_id))
                .filter(eh_download_queue::Column::JobId.eq(expected_job_id))
                .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
                .exec(&txn)
                .await
                .context("Failed to mark shared EH delivery done")?;
            if updated.rows_affected != 1 {
                anyhow::bail!(
                    "Cannot complete shared EH delivery {}: publishing claim changed",
                    delivery_id
                );
            }
            self.recompute_eh_job_telegraph_requirement_in_txn(&txn, expected_job_id)
                .await?;
            self.evaluate_eh_job_liveness_in_txn(&txn, expected_job_id, send_archive)
                .await?;
            eh_download_queue::Entity::find_by_id(delivery_id)
                .one(&txn)
                .await
                .context("Failed to reread completed shared EH delivery")?
                .filter(|delivery| delivery.job_id == Some(expected_job_id))
                .context("Shared EH delivery disappeared or changed jobs after completion")
        }
        .await;
        match result {
            Ok(delivery) => txn
                .commit()
                .await
                .context("Failed to commit shared EH delivery completion transaction")
                .map(|()| delivery),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back shared EH delivery completion transaction")?;
                Err(error)
            }
        }
    }

    /// Get next entry for the legacy per-row publish stage: either
    /// (downloaded, telegraph=false) or (uploaded). Runtime workers use
    /// `get_next_eh_delivery_for_publish` instead.
    #[cfg(test)]
    #[allow(dead_code)] // Retained for legacy-row compatibility tests.
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

        let generation = next_claim_generation(now, model.started_at)?;
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
            sea_orm::Condition::all().add(eh_download_queue::Column::Status.eq(STATUS_UPLOADED))
        };
        let retry_filter = sea_orm::Condition::any()
            .add(eh_download_queue::Column::NextRetryAt.is_null())
            .add(eh_download_queue::Column::NextRetryAt.lte(now));

        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_PUBLISHING),
            )
            .col_expr(
                eh_download_queue::Column::StartedAt,
                Expr::value(generation),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(status_filter)
            .filter(claim_generation_filter(model.started_at))
            .filter(retry_filter)
            .exec(&self.db)
            .await
            .context("Failed to atomically claim publish entry")?;

        if result.rows_affected == 0 {
            return Ok(None);
        }

        let updated = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .filter(eh_download_queue::Column::StartedAt.eq(generation))
            .one(&self.db)
            .await?;
        Ok(updated)
    }

    /// Mark the archive ZIP as sent (publish stage progress marker).
    /// Only updates rows currently in `STATUS_PUBLISHING`.
    #[cfg(test)]
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
        self.mark_eh_telegraph_sent_and_schedule_rewrite(id, None)
            .await
    }

    /// Mark the Telegraph link as sent and schedule rewrite metadata in the same DB update.
    pub async fn mark_eh_telegraph_sent_and_schedule_rewrite(
        &self,
        id: i32,
        rewrite_delay_secs: Option<i64>,
    ) -> Result<()> {
        let now = Local::now().naive_local();
        if rewrite_delay_secs.is_none() {
            let result = eh_download_queue::Entity::update_many()
                .col_expr(eh_download_queue::Column::TelegraphSentAt, Expr::value(now))
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
            return Ok(());
        }

        if let Some(delay_secs) = rewrite_delay_secs {
            let result = eh_download_queue::Entity::update_many()
                .col_expr(eh_download_queue::Column::TelegraphSentAt, Expr::value(now))
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteStatus,
                    Expr::value(Some(TELEGRAPH_REWRITE_STATUS_PENDING.to_string())),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteAfter,
                    Expr::value(now + chrono::Duration::seconds(delay_secs)),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteStartedAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteRetryCount,
                    Expr::value(0),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteError,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewrittenAt,
                    Expr::value(None::<DateTime>),
                )
                .filter(eh_download_queue::Column::Id.eq(id))
                .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
                .filter(eh_download_queue::Column::TelegraphRewriteData.is_not_null())
                .exec(&self.db)
                .await
                .context("Failed to mark EH Telegraph sent and schedule rewrite")?;
            if result.rows_affected == 1 {
                return Ok(());
            }
        }

        let result = eh_download_queue::Entity::update_many()
            .col_expr(eh_download_queue::Column::TelegraphSentAt, Expr::value(now))
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .filter(eh_download_queue::Column::TelegraphRewriteData.is_null())
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

    /// Schedule stored Telegraph rewrite data after the link has been sent.
    #[allow(dead_code)] // Runtime scheduling moved to shared eh_gallery_jobs.
    pub async fn schedule_eh_telegraph_rewrite_after_send(
        &self,
        id: i32,
        delay_secs: i64,
    ) -> Result<()> {
        let now = Local::now().naive_local();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(Some(TELEGRAPH_REWRITE_STATUS_PENDING.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteAfter,
                Expr::value(now + chrono::Duration::seconds(delay_secs)),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteRetryCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteError,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewrittenAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .filter(eh_download_queue::Column::TelegraphSentAt.is_not_null())
            .filter(eh_download_queue::Column::TelegraphRewriteData.is_not_null())
            .filter(eh_download_queue::Column::TelegraphRewriteStatus.is_null())
            .filter(eh_download_queue::Column::TelegraphRewriteAfter.is_null())
            .filter(eh_download_queue::Column::TelegraphRewriteNextRetryAt.is_null())
            .filter(eh_download_queue::Column::TelegraphRewrittenAt.is_null())
            .exec(&self.db)
            .await
            .context("Failed to schedule EH Telegraph rewrite")?;
        Ok(())
    }

    /// Claim the next due Telegraph rewrite job.
    #[allow(dead_code)] // Retained only for historical queue-row compatibility queries.
    pub async fn get_next_for_telegraph_rewrite(&self) -> Result<Option<eh_download_queue::Model>> {
        let now = Local::now().naive_local();
        let entry = eh_download_queue::Entity::find()
            .filter(
                eh_download_queue::Column::TelegraphRewriteStatus
                    .eq(TELEGRAPH_REWRITE_STATUS_PENDING),
            )
            .filter(eh_download_queue::Column::TelegraphRewriteData.is_not_null())
            .filter(eh_download_queue::Column::TelegraphSentAt.is_not_null())
            .filter(eh_download_queue::Column::TelegraphRewrittenAt.is_null())
            .filter(
                eh_download_queue::Column::TelegraphRewriteAfter
                    .is_null()
                    .or(eh_download_queue::Column::TelegraphRewriteAfter.lte(now)),
            )
            .filter(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt
                    .is_null()
                    .or(eh_download_queue::Column::TelegraphRewriteNextRetryAt.lte(now)),
            )
            .order_by(eh_download_queue::Column::TelegraphRewriteAfter, Order::Asc)
            .one(&self.db)
            .await
            .context("Failed to fetch next EH Telegraph rewrite")?;

        let Some(model) = entry else {
            return Ok(None);
        };

        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(Some(TELEGRAPH_REWRITE_STATUS_REWRITING.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(now),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(
                eh_download_queue::Column::TelegraphRewriteStatus
                    .eq(TELEGRAPH_REWRITE_STATUS_PENDING),
            )
            .filter(eh_download_queue::Column::TelegraphRewriteData.is_not_null())
            .filter(
                sea_orm::Condition::any()
                    .add(eh_download_queue::Column::TelegraphRewriteAfter.is_null())
                    .add(eh_download_queue::Column::TelegraphRewriteAfter.lte(now)),
            )
            .filter(
                sea_orm::Condition::any()
                    .add(eh_download_queue::Column::TelegraphRewriteNextRetryAt.is_null())
                    .add(eh_download_queue::Column::TelegraphRewriteNextRetryAt.lte(now)),
            )
            .exec(&self.db)
            .await
            .context("Failed to atomically claim EH Telegraph rewrite")?;

        if result.rows_affected == 0 {
            return Ok(None);
        }

        let updated = eh_download_queue::Entity::find_by_id(model.id)
            .one(&self.db)
            .await?
            .context("EH Telegraph rewrite entry disappeared after claim")?;
        Ok(Some(updated))
    }

    /// Mark a claimed Telegraph rewrite as complete and clear rewrite payload.
    #[allow(dead_code)] // Runtime completion moved to shared eh_gallery_jobs.
    pub async fn mark_eh_telegraph_rewritten(&self, id: i32) -> Result<()> {
        let now = Local::now().naive_local();
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteData,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteAfter,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteRetryCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteError,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewrittenAt,
                Expr::value(Some(now)),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(
                eh_download_queue::Column::TelegraphRewriteStatus
                    .eq(TELEGRAPH_REWRITE_STATUS_REWRITING),
            )
            .exec(&self.db)
            .await
            .context("Failed to mark EH Telegraph rewrite complete")?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot mark EH Telegraph rewrite {} complete: expected status '{}'",
                id,
                TELEGRAPH_REWRITE_STATUS_REWRITING
            );
        }

        Ok(())
    }

    /// Retry a claimed Telegraph rewrite with backoff, or stop retrying after `max_retry_count`.
    #[allow(dead_code)] // Runtime retry state moved to shared eh_gallery_jobs.
    pub async fn schedule_eh_telegraph_rewrite_retry(
        &self,
        id: i32,
        error: &str,
        max_retry_count: u8,
    ) -> Result<bool> {
        let entry = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .context("Failed to fetch EH Telegraph rewrite for retry")?
            .ok_or_else(|| anyhow::anyhow!("EH Telegraph rewrite {} not found", id))?;
        let retry_count = entry.telegraph_rewrite_retry_count + 1;
        let is_permanent = retry_count > max_retry_count as i32;
        let now = Local::now().naive_local();

        if is_permanent {
            let result = eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteStatus,
                    Expr::value(Some(TELEGRAPH_REWRITE_STATUS_FAILED.to_string())),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteStartedAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteRetryCount,
                    Expr::value(retry_count),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteError,
                    Expr::value(Some(error.to_string())),
                )
                .filter(eh_download_queue::Column::Id.eq(id))
                .filter(
                    eh_download_queue::Column::TelegraphRewriteStatus
                        .eq(TELEGRAPH_REWRITE_STATUS_REWRITING),
                )
                .exec(&self.db)
                .await
                .context("Failed to mark EH Telegraph rewrite failed")?;

            if result.rows_affected != 1 {
                anyhow::bail!(
                    "Cannot fail EH Telegraph rewrite {}: expected status '{}'",
                    id,
                    TELEGRAPH_REWRITE_STATUS_REWRITING
                );
            }
            return Ok(true);
        }

        let delay = Self::backoff_delay_secs(retry_count);
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(Some(TELEGRAPH_REWRITE_STATUS_PENDING.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(now + chrono::Duration::seconds(delay)),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteRetryCount,
                Expr::value(retry_count),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteError,
                Expr::value(Some(error.to_string())),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(
                eh_download_queue::Column::TelegraphRewriteStatus
                    .eq(TELEGRAPH_REWRITE_STATUS_REWRITING),
            )
            .exec(&self.db)
            .await
            .context("Failed to schedule EH Telegraph rewrite retry")?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot retry EH Telegraph rewrite {}: expected status '{}'",
                id,
                TELEGRAPH_REWRITE_STATUS_REWRITING
            );
        }

        Ok(false)
    }

    /// Defer an entry: set status to `target_status` and delay next retry by `delay_secs`.
    /// Does NOT increment `retry_count` and does NOT set `error`.
    ///
    /// Legal target statuses: `STATUS_PENDING`, `STATUS_DOWNLOADED`, `STATUS_UPLOADED`.
    /// Current-status guards:
    /// - target `STATUS_PENDING`: current must be `STATUS_DOWNLOADING`.
    /// - target `STATUS_DOWNLOADED`: current must be `STATUS_UPLOADING` or `STATUS_PUBLISHING`.
    /// - target `STATUS_UPLOADED`: current must be `STATUS_PUBLISHING`.
    #[cfg(test)]
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
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(target_status),
            )
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

    /// Recover an upload-stage Abort failure without spending a retry.
    ///
    /// This intentionally accepts only `STATUS_UPLOADING`: publish-stage rows
    /// have different completion and retry semantics and must not be released
    /// through the upload fallback path.
    #[allow(dead_code)] // Historical per-delivery upload compatibility API; runtime claims jobs.
    pub async fn defer_eh_upload_after_abort_failure(
        &self,
        id: i32,
        delay_secs: i64,
    ) -> Result<()> {
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_DOWNLOADED),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(Local::now().naive_local() + chrono::Duration::seconds(delay_secs)),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_UPLOADING))
            .exec(&self.db)
            .await
            .context("Failed to defer EH upload after multipart Abort failure")?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot defer EH upload {} after multipart Abort failure: expected status '{}'",
                id,
                STATUS_UPLOADING
            );
        }
        Ok(())
    }

    /// Recover a publish-stage Abort-gate failure without spending a retry.
    ///
    /// Only a claimed publish row may use this path. The target is selected
    /// from whether Telegraph upload succeeded, so sent markers remain intact
    /// and the next publish claim only completes local cleanup.
    #[cfg(test)]
    pub async fn defer_eh_publish_after_abort_failure(
        &self,
        id: i32,
        target_status: &str,
        delay_secs: i64,
    ) -> Result<()> {
        if !matches!(target_status, STATUS_DOWNLOADED | STATUS_UPLOADED) {
            anyhow::bail!(
                "defer_eh_publish_after_abort_failure: invalid target status '{}' (expected downloaded or uploaded)",
                target_status
            );
        }

        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(target_status),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(Local::now().naive_local() + chrono::Duration::seconds(delay_secs)),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .exec(&self.db)
            .await
            .context("Failed to defer EH publish after multipart Abort failure")?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot defer EH publish {} after multipart Abort failure: expected status '{}'",
                id,
                STATUS_PUBLISHING
            );
        }
        Ok(())
    }

    /// Schedule a retry for an entry: set status back to target_status, increment retry_count,
    /// set next_retry_at to now + backoff. If retry_count exceeds max, set status=failed.
    /// Returns (model, is_permanent_failure).
    ///
    /// Compatibility wrapper for unambiguous retry targets.  Use
    /// `schedule_eh_retry_from()` when multiple stages can retry to the same
    /// target status.
    #[allow(dead_code)]
    pub async fn schedule_eh_retry(
        &self,
        id: i32,
        target_status: &str,
        error: &str,
        max_retry_count: u8,
    ) -> Result<(eh_download_queue::Model, bool)> {
        let expected_status = match target_status {
            STATUS_PENDING => STATUS_DOWNLOADING,
            STATUS_UPLOADED => STATUS_PUBLISHING,
            STATUS_DOWNLOADED => anyhow::bail!(
                "schedule_eh_retry target '{}' is ambiguous; use schedule_eh_retry_from with the claimed status",
                target_status
            ),
            _ => anyhow::bail!(
                "schedule_eh_retry: invalid target status '{}'",
                target_status
            ),
        };
        self.schedule_eh_retry_from(id, expected_status, target_status, error, max_retry_count)
            .await
    }

    /// Schedule a retry from a specific in-flight status.  The explicit
    /// `expected_status` is required because both upload and publish failures can
    /// target `downloaded`; without it a stale worker could overwrite a newer
    /// in-flight stage that happens to share the same retry target.
    pub async fn schedule_eh_retry_from(
        &self,
        id: i32,
        expected_status: &str,
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

        // Determine the valid current-status filter for the expected stage and
        // retry target (same for transient and permanent failure).
        let current_filter = match (expected_status, target_status) {
            (STATUS_DOWNLOADING, STATUS_PENDING) => {
                eh_download_queue::Column::Status.eq(STATUS_DOWNLOADING)
            }
            (STATUS_UPLOADING, STATUS_DOWNLOADED) => {
                eh_download_queue::Column::Status.eq(STATUS_UPLOADING)
            }
            (STATUS_PUBLISHING, STATUS_DOWNLOADED) => {
                eh_download_queue::Column::Status.eq(STATUS_PUBLISHING)
            }
            (STATUS_PUBLISHING, STATUS_UPLOADED) => {
                eh_download_queue::Column::Status.eq(STATUS_PUBLISHING)
            }
            (STATUS_PUBLISHING, STATUS_PENDING) => {
                eh_download_queue::Column::Status.eq(STATUS_PUBLISHING)
            }
            _ => anyhow::bail!(
                "schedule_eh_retry_from: invalid transition from '{}' to '{}'",
                expected_status,
                target_status
            ),
        };

        if is_permanent {
            // Permanent failure: CAS-guarded so stale workers don't overwrite re-enqueued rows.
            let result = eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::Status,
                    Expr::value(STATUS_FAILED),
                )
                .col_expr(eh_download_queue::Column::CompletedAt, Expr::value(now))
                .col_expr(
                    eh_download_queue::Column::Error,
                    Expr::value(Some(error.to_string())),
                )
                .col_expr(
                    eh_download_queue::Column::RetryCount,
                    Expr::value(new_retry_count),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteData,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteStatus,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteAfter,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteStartedAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteRetryCount,
                    Expr::value(0),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteError,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewrittenAt,
                    Expr::value(None::<DateTime>),
                )
                .filter(eh_download_queue::Column::Id.eq(id))
                .filter(current_filter)
                .exec(&self.db)
                .await
                .context("Failed to schedule retry (permanent)")?;

            if result.rows_affected != 1 {
                anyhow::bail!(
                    "Cannot schedule permanent retry for EH download {}: expected in-flight status, but it was changed by another worker",
                    id
                );
            }

            let model = eh_download_queue::Entity::find_by_id(id)
                .one(&self.db)
                .await?
                .context("Entry disappeared after retry")?;
            return Ok((model, true));
        }

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

    #[allow(dead_code)] // Legacy delivery lane retained for historical rows; workers use job claims.
    pub async fn schedule_eh_background_download_from(
        &self,
        id: i32,
        expected_status: &str,
        error: &str,
    ) -> Result<eh_download_queue::Model> {
        let now = Local::now().naive_local();
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_PENDING),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::Error,
                Expr::value(Some(error.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStatus,
                Expr::value(Some(BACKGROUND_STATUS_PENDING.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                Expr::value(Some(now)),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadAttemptCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadError,
                Expr::value(None::<String>),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(expected_status))
            .exec(&self.db)
            .await
            .context("Failed to schedule EH background download")?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot schedule EH background download {}: expected status '{}', but it was changed",
                id,
                expected_status
            );
        }

        let model = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after background handoff")?;
        Ok(model)
    }

    /// Defer a background download that is currently running without treating
    /// the defer as a retryable failure.
    ///
    /// Sets `background_download_status` back to `pending`, schedules
    /// `background_download_next_retry_at = now + delay_secs`, and crucially
    /// does NOT increment `background_download_attempt_count` and does NOT
    /// mark the entry as failed. Used by the background worker when the GP /
    /// byte-rate guard defers a download - the entry should wait out the
    /// configured window, not burn retry attempts.
    ///
    /// Requires `status = STATUS_PENDING` and
    /// `background_download_status = BACKGROUND_STATUS_RUNNING` (i.e. the
    /// background worker currently owns the entry). Returns the updated model.
    #[allow(dead_code)] // Legacy delivery lane retained for historical rows; workers use job claims.
    pub async fn defer_eh_background_download(
        &self,
        id: i32,
        delay_secs: i64,
        reason: &str,
    ) -> Result<eh_download_queue::Model> {
        let now = Local::now().naive_local();
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStatus,
                Expr::value(Some(BACKGROUND_STATUS_PENDING.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                Expr::value(Some(now + chrono::Duration::seconds(delay_secs))),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadError,
                Expr::value(Some(reason.to_string())),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(
                eh_download_queue::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING),
            )
            .exec(&self.db)
            .await
            .context("Failed to defer EH background download")?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot defer EH background download {}: expected status '{}' with background_download_status='{}', but it was changed by another worker",
                id,
                STATUS_PENDING,
                BACKGROUND_STATUS_RUNNING
            );
        }

        let model = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after background defer")?;
        Ok(model)
    }

    #[allow(dead_code)] // Legacy delivery lane retained for historical rows; workers use job claims.
    pub async fn reset_stale_background_downloads(&self, stale_sec: u64) -> Result<u64> {
        let cutoff = Local::now().naive_local() - chrono::Duration::seconds(stale_sec as i64);
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStatus,
                Expr::value(Some(BACKGROUND_STATUS_PENDING.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(
                eh_download_queue::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING),
            )
            .filter(
                eh_download_queue::Column::BackgroundDownloadStartedAt
                    .is_null()
                    .or(eh_download_queue::Column::BackgroundDownloadStartedAt.lte(cutoff)),
            )
            .exec(&self.db)
            .await
            .context("Failed to reset stale EH background downloads")?;
        Ok(result.rows_affected)
    }

    #[allow(dead_code)] // Legacy delivery lane retained for historical rows; workers use job claims.
    pub async fn release_background_downloads_to_main_queue(&self) -> Result<u64> {
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadAttemptCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadError,
                Expr::value(None::<String>),
            )
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(eh_download_queue::Column::BackgroundDownloadStatus.is_not_null())
            .exec(&self.db)
            .await
            .context("Failed to release EH background downloads to main queue")?;
        Ok(result.rows_affected)
    }

    #[allow(dead_code)] // Called only by the retained legacy delivery transitions below.
    async fn clear_background_download_if_inactive(&self, id: i32) -> Result<()> {
        let Some(row) = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await?
        else {
            return Ok(());
        };
        if row.background_download_status.is_none()
            || matches!(row.status.as_str(), STATUS_PENDING | STATUS_DOWNLOADING)
        {
            return Ok(());
        }
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadAttemptCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadError,
                Expr::value(None::<String>),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(row.status))
            .filter(eh_download_queue::Column::BackgroundDownloadStatus.is_not_null())
            .exec(&self.db)
            .await
            .context("Failed to clear stale EH background download state")?;
        Ok(())
    }

    #[allow(dead_code)] // Legacy delivery lane retained for historical rows; workers use job claims.
    pub async fn get_next_for_background_download(
        &self,
    ) -> Result<Option<eh_download_queue::Model>> {
        let now = Local::now().naive_local();
        self.get_next_for_background_download_at(now).await
    }

    #[allow(dead_code)] // Called only by the retained legacy delivery transitions above.
    async fn get_next_for_background_download_at(
        &self,
        now: DateTime,
    ) -> Result<Option<eh_download_queue::Model>> {
        let entry = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(
                eh_download_queue::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_PENDING),
            )
            .filter(
                eh_download_queue::Column::BackgroundDownloadNextRetryAt
                    .is_null()
                    .or(eh_download_queue::Column::BackgroundDownloadNextRetryAt.lte(now)),
            )
            .order_by(eh_download_queue::Column::CreatedAt, Order::Asc)
            .one(&self.db)
            .await
            .context("Failed to fetch next background EH download")?;

        let Some(model) = entry else {
            return Ok(None);
        };

        let generation = next_claim_generation(now, model.started_at)?;
        let lease_started_at = next_claim_generation(now, None)?;
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStatus,
                Expr::value(Some(BACKGROUND_STATUS_RUNNING.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStartedAt,
                Expr::value(Some(lease_started_at)),
            )
            .col_expr(
                eh_download_queue::Column::StartedAt,
                Expr::value(generation),
            )
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(
                eh_download_queue::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_PENDING),
            )
            .filter(claim_generation_filter(model.started_at))
            .filter(
                sea_orm::Condition::any()
                    .add(eh_download_queue::Column::BackgroundDownloadNextRetryAt.is_null())
                    .add(eh_download_queue::Column::BackgroundDownloadNextRetryAt.lte(now)),
            )
            .exec(&self.db)
            .await
            .context("Failed to atomically claim background EH download")?;

        if result.rows_affected == 0 {
            return Ok(None);
        }

        let updated = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(
                eh_download_queue::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING),
            )
            .filter(eh_download_queue::Column::StartedAt.eq(generation))
            .filter(eh_download_queue::Column::BackgroundDownloadStartedAt.eq(lease_started_at))
            .one(&self.db)
            .await?;
        Ok(updated)
    }

    #[allow(dead_code)] // Legacy delivery lane retained for historical rows; workers use job claims.
    pub async fn mark_eh_background_download_downloaded(
        &self,
        id: i32,
        file_size: i64,
        zip_path: &str,
        gp_cost: i64,
    ) -> Result<eh_download_queue::Model> {
        anyhow::ensure!(
            file_size >= 0,
            "EH background download file size must be non-negative"
        );
        let now = Local::now().naive_local();
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin background EH completion transaction")?;
        let claimed = eh_download_queue::Entity::find_by_id(id)
            .one(&txn)
            .await
            .context("Failed to fetch claimed background EH download")?
            .context("Background EH download disappeared before completion")?;
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_DOWNLOADED),
            )
            .col_expr(eh_download_queue::Column::FileSize, Expr::value(file_size))
            .col_expr(eh_download_queue::Column::GpCost, Expr::value(gp_cost))
            .col_expr(
                eh_download_queue::Column::ZipPath,
                Expr::value(Some(zip_path.to_string())),
            )
            .col_expr(eh_download_queue::Column::CompletedAt, Expr::value(now))
            .col_expr(
                eh_download_queue::Column::Error,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadError,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadAttemptCount,
                Expr::value(0),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(
                eh_download_queue::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING),
            )
            .exec(&txn)
            .await
            .context("Failed to mark background EH download as downloaded")?;

        if result.rows_affected != 1 {
            txn.rollback().await?;
            self.clear_background_download_if_inactive(id).await?;
            anyhow::bail!("Cannot mark background EH download {} as downloaded", id);
        }

        let model = eh_download_queue::Entity::find_by_id(id)
            .one(&txn)
            .await?
            .context("Entry disappeared after mark background downloaded")?;
        if let Some(job_id) = claimed.job_id {
            crate::db::repo::eh_download_completions::append_eh_download_completion_in_txn(
                &txn,
                job_id,
                claimed.gid,
                file_size,
                now,
            )
            .await?;
        } else {
            crate::db::entities::eh_download_completions::ActiveModel {
                job_id: Set(None),
                gid: Set(claimed.gid),
                file_size: Set(file_size),
                created_at: Set(now),
                ..Default::default()
            }
            .insert(&txn)
            .await
            .context("Failed to append legacy background EH download completion")?;
        }
        txn.commit()
            .await
            .context("Failed to commit background EH completion transaction")?;
        Ok(model)
    }

    #[allow(dead_code)] // Legacy delivery lane retained for historical rows; workers use job claims.
    pub async fn schedule_eh_background_download_retry(
        &self,
        id: i32,
        error: &str,
        max_attempts: u8,
    ) -> Result<(eh_download_queue::Model, bool)> {
        let entry = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .context("Failed to fetch background EH download")?
            .ok_or_else(|| anyhow::anyhow!("EH download {} not found", id))?;
        let new_attempts = entry.background_download_attempt_count + 1;
        let permanent = new_attempts >= max_attempts as i32;
        let now = Local::now().naive_local();

        let mut update = eh_download_queue::Entity::update_many();
        if permanent {
            update = update
                .col_expr(
                    eh_download_queue::Column::Status,
                    Expr::value(STATUS_FAILED),
                )
                .col_expr(eh_download_queue::Column::CompletedAt, Expr::value(now))
                .col_expr(
                    eh_download_queue::Column::Error,
                    Expr::value(Some(error.to_string())),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadStatus,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadStartedAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadError,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadAttemptCount,
                    Expr::value(0),
                );
        } else {
            let delay = Self::backoff_delay_secs(new_attempts);
            update = update
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadStatus,
                    Expr::value(Some(BACKGROUND_STATUS_PENDING.to_string())),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadStartedAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                    Expr::value(Some(now + chrono::Duration::seconds(delay))),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadError,
                    Expr::value(Some(error.to_string())),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadAttemptCount,
                    Expr::value(new_attempts),
                );
        }

        let result = update
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(
                eh_download_queue::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING),
            )
            .exec(&self.db)
            .await
            .context("Failed to schedule background EH download retry")?;

        if result.rows_affected != 1 {
            self.clear_background_download_if_inactive(id).await?;
            anyhow::bail!("Cannot schedule background EH download retry {}", id);
        }

        let model = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after background retry")?;
        Ok((model, permanent))
    }

    /// Delete cache artifact families which have no persisted shared-job owner.
    ///
    /// Delivery compatibility columns are deliberately excluded from the
    /// keep-set: only a shared job owns its deterministic artifact family.
    /// Families carrying remote multipart state are Abort-gated before every
    /// local deletion and remain intact when that gate cannot be satisfied.
    pub async fn cleanup_eh_cache_orphans(
        &self,
        cache_dir: &std::path::Path,
        abort_uploader: Option<&dyn ImageUploader>,
    ) -> Result<()> {
        if !cache_dir.exists() {
            return Ok(());
        }

        let mut owned_final_zips: HashSet<std::path::PathBuf> = HashSet::new();
        let job_artifacts = eh_gallery_jobs::Entity::find()
            .filter(
                eh_gallery_jobs::Column::Status
                    .ne(crate::db::repo::eh_gallery_jobs::JOB_STATUS_RETIRED)
                    .or(eh_gallery_jobs::Column::CleanupStatus
                        .ne(crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_NONE)),
            )
            .all(&self.db)
            .await
            .context("Failed to fetch shared EH jobs for cache cleanup")?;
        for job in job_artifacts {
            if let Some(zip_path) = job.zip_path.as_deref() {
                owned_final_zips.insert(std::path::PathBuf::from(zip_path));
            }
            if matches!(
                job.status.as_str(),
                crate::db::repo::eh_gallery_jobs::JOB_STATUS_PENDING
                    | crate::db::repo::eh_gallery_jobs::JOB_STATUS_DOWNLOADING
            ) {
                owned_final_zips.insert(eh_gallery_job_artifact_path(cache_dir, &job));
            }
        }

        let mut artifact_families: HashMap<std::path::PathBuf, ArchiveArtifacts> = HashMap::new();
        for entry in std::fs::read_dir(cache_dir).context("Failed to read eh_cache dir")? {
            let entry = entry?;
            let path = entry.path();
            let Some(artifacts) = ArchiveArtifacts::from_member(&path) else {
                continue;
            };
            artifact_families
                .entry(artifacts.final_zip().to_path_buf())
                .or_insert(artifacts);
        }

        for (final_zip, artifacts) in artifact_families {
            let result = if !owned_final_zips.contains(&final_zip) {
                if artifacts.uploads_dir().exists() {
                    let Some(uploader) = abort_uploader else {
                        warn!(
                            "Preserving EH orphan upload state because no S3/ipfS3 abort uploader is configured"
                        );
                        continue;
                    };
                    if uploader
                        .abort_upload_state(artifacts.uploads_dir())
                        .await
                        .is_err()
                    {
                        warn!(
                            "Failed to abort EH orphan upload state; preserving local archive family"
                        );
                        continue;
                    }
                }
                artifacts.remove_all().await
            } else {
                continue;
            };
            if let Err(e) = result {
                warn!("Failed to cleanup EH archive artifacts: {}", e);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::entities::eh_download_completions;
    use crate::db::entities::eh_download_queue::{Column, Entity};
    use crate::db::entities::{eh_gallery_jobs, subscriptions};
    use crate::db::repo::eh_gallery_jobs::{
        EhGalleryVariant, CLEANUP_STATUS_FAILED, CLEANUP_STATUS_PENDING, JOB_STATUS_DOWNLOADED,
        JOB_STATUS_DOWNLOADING, JOB_STATUS_PENDING, JOB_STATUS_RETIRED,
    };
    use crate::db::repo::tests_helpers;
    use crate::db::types::{TagFilter, TaskType};
    use chrono::{Duration, NaiveDate};
    use sea_orm::{sea_query::Expr, ConnectionTrait, DbBackend, Statement};

    #[derive(Default)]
    struct RecordingAbortUploader {
        aborts: std::sync::Mutex<Vec<(std::path::PathBuf, bool)>>,
        fail_abort: bool,
    }

    #[async_trait::async_trait]
    impl ImageUploader for RecordingAbortUploader {
        async fn upload_images(
            &self,
            _images: &[eh_client::ImageUploadInput<'_>],
        ) -> eh_client::Result<Vec<String>> {
            Err(eh_client::Error::Other(
                "recording uploader upload_images must not be called".to_string(),
            ))
        }

        async fn abort_upload_state(&self, uploads_dir: &std::path::Path) -> eh_client::Result<()> {
            self.aborts
                .lock()
                .unwrap()
                .push((uploads_dir.to_path_buf(), uploads_dir.exists()));
            if self.fail_abort {
                return Err(eh_client::Error::Other(
                    "recording uploader abort failure".to_string(),
                ));
            }
            Ok(())
        }
    }

    async fn job_for_delivery(
        repo: &Repo,
        delivery: &eh_download_queue::Model,
    ) -> eh_gallery_jobs::Model {
        eh_gallery_jobs::Entity::find_by_id(delivery.job_id.unwrap())
            .one(repo.db())
            .await
            .unwrap()
            .unwrap()
    }

    async fn seed_publishing_archive_delivery(
        repo: &Repo,
        gid: i64,
    ) -> (eh_download_queue::Model, eh_gallery_jobs::Model) {
        let delivery = repo
            .enqueue_eh_download(
                -100,
                gid,
                "terminal-transition",
                "Terminal transition",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            1,
            "/tmp/terminal-transition.zip",
            0,
        )
        .await
        .unwrap();
        let claim = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claim.delivery.id, delivery.id);
        (claim.delivery, claim.job)
    }

    async fn set_eh_job_claim_fields(
        repo: &Repo,
        job_id: i32,
        created_at: DateTime,
        next_retry_at: Option<DateTime>,
    ) {
        eh_gallery_jobs::Entity::update_many()
            .col_expr(eh_gallery_jobs::Column::CreatedAt, Expr::value(created_at))
            .col_expr(
                eh_gallery_jobs::Column::NextRetryAt,
                Expr::value(next_retry_at),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .exec(repo.db())
            .await
            .unwrap();
    }

    async fn completion_count_for_job(repo: &Repo, job_id: i32) -> u64 {
        eh_download_completions::Entity::find()
            .filter(eh_download_completions::Column::JobId.eq(job_id))
            .count(repo.db())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn chat_locks_serialize_same_chat_but_not_different_chats() {
        let first = EH_CHAT_LOCKS.lock_chat(-100).await;
        let blocked_same = tokio::spawn(async { EH_CHAT_LOCKS.lock_chat(-100).await });
        let free_other = tokio::spawn(async { EH_CHAT_LOCKS.lock_chat(-200).await });

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), free_other)
                .await
                .is_ok()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), blocked_same)
                .await
                .is_err()
        );
        drop(first);

        let guards = EH_CHAT_LOCKS.lock_chats(&[-1, -3, -2, -1]).await;
        assert_eq!(guards.len(), 3);
    }

    #[tokio::test]
    async fn chat_lock_drop_with_queued_waiter_never_splits_and_cleans_idle_key() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let registry = Arc::new(EhChatLockRegistry::default());
        let first = registry.lock_chat(-100).await;
        let active = Arc::new(AtomicUsize::new(0));
        let second_acquired = Arc::new(tokio::sync::Notify::new());
        let release_second = Arc::new(tokio::sync::Notify::new());
        let third_acquired = Arc::new(tokio::sync::Notify::new());

        let second_registry = Arc::clone(&registry);
        let second_active = Arc::clone(&active);
        let second_acquired_signal = Arc::clone(&second_acquired);
        let second_release = Arc::clone(&release_second);
        let second = tokio::spawn(async move {
            let _guard = second_registry.lock_chat(-100).await;
            assert_eq!(second_active.fetch_add(1, Ordering::SeqCst), 0);
            second_acquired_signal.notify_one();
            second_release.notified().await;
            assert_eq!(second_active.fetch_sub(1, Ordering::SeqCst), 1);
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let waiter_is_queued = registry
                    .locks
                    .lock()
                    .unwrap()
                    .get(&-100)
                    .is_some_and(|mutex| mutex.strong_count() >= 3);
                if waiter_is_queued {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("second caller should queue on the first mutex");

        drop(first);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            second_acquired.notified(),
        )
        .await
        .expect("queued caller should acquire after the current guard drops");

        let third_registry = Arc::clone(&registry);
        let third_active = Arc::clone(&active);
        let third_acquired_signal = Arc::clone(&third_acquired);
        let third = tokio::spawn(async move {
            let _guard = third_registry.lock_chat(-100).await;
            assert_eq!(third_active.fetch_add(1, Ordering::SeqCst), 0);
            third_acquired_signal.notify_one();
            assert_eq!(third_active.fetch_sub(1, Ordering::SeqCst), 1);
        });
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                third_acquired.notified(),
            )
            .await
            .is_err(),
            "a third caller must wait for the queued caller's same-chat critical section"
        );

        release_second.notify_one();
        second.await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), third_acquired.notified())
            .await
            .expect("third caller should acquire after the second caller releases");
        third.await.unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if registry.locks.lock().unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("idle chat lock should be removed from the registry");
    }

    #[tokio::test]
    async fn cancel_one_shared_delivery_keeps_sibling_job_and_artifact_live() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let canceled = repo
            .enqueue_eh_subscription_download(-100, 123, 700, "tok", "Title", true, &variant)
            .await
            .unwrap();
        let sibling = repo
            .enqueue_eh_subscription_download(-200, 456, 700, "tok", "Title", false, &variant)
            .await
            .unwrap();
        assert_eq!(canceled.job_id, sibling.job_id);

        let temp = tempfile::NamedTempFile::new().unwrap();
        let job = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            job.id,
            job.started_at.unwrap(),
            1,
            temp.path().to_str().unwrap(),
            0,
        )
        .await
        .unwrap();

        repo.cancel_eh_subscription_queue_entries(123, true)
            .await
            .unwrap();

        let canceled = Entity::find_by_id(canceled.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let sibling = Entity::find_by_id(sibling.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let job = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(canceled.status, STATUS_CANCELED);
        assert_eq!(sibling.status, STATUS_WAITING);
        assert_ne!(job.status, JOB_STATUS_RETIRED);
        assert_eq!(job.telegraph_required, sibling.telegraph);
        assert!(temp.path().exists(), "liveness must not delete the ZIP");
    }

    #[tokio::test]
    async fn cancellation_before_and_after_upload_claim_obeys_aggregate_boundary() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");

        let before = repo
            .enqueue_eh_subscription_download(-100, 123, 701, "before", "Before", true, &variant)
            .await
            .unwrap();
        let before_job = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            before_job.id,
            before_job.started_at.unwrap(),
            1,
            "/tmp/before.zip",
            0,
        )
        .await
        .unwrap();
        repo.cancel_eh_subscription_queue_entries(123, true)
            .await
            .unwrap();
        let before_job = eh_gallery_jobs::Entity::find_by_id(before.job_id.unwrap())
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            before_job.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_NOT_REQUIRED
        );

        let after = repo
            .enqueue_eh_subscription_download(-200, 456, 702, "after", "After", true, &variant)
            .await
            .unwrap();
        let after_job = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            after_job.id,
            after_job.started_at.unwrap(),
            1,
            "/tmp/after.zip",
            0,
        )
        .await
        .unwrap();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphStatus,
                Expr::value(crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_UPLOADING),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(after.job_id.unwrap()))
            .exec(repo.db())
            .await
            .unwrap();
        repo.cancel_eh_subscription_queue_entries(456, true)
            .await
            .unwrap();
        let after_job = eh_gallery_jobs::Entity::find_by_id(after.job_id.unwrap())
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after_job.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_UPLOADING
        );
    }

    #[tokio::test]
    async fn liveness_keeps_pending_rewrite_payload_without_scheduling_archive_cleanup() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let delivery = repo
            .enqueue_eh_subscription_download(
                -100,
                123,
                703,
                "rewrite",
                "Rewrite",
                false,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let job = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(job.id, job.started_at.unwrap(), 1, "/tmp/rewrite.zip", 0)
            .await
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_CANCELED))
            .filter(Column::Id.eq(delivery.id))
            .exec(repo.db())
            .await
            .unwrap();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteData,
                Expr::value(Some("payload".to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteStatus,
                Expr::value(Some(
                    crate::db::repo::eh_gallery_jobs::TELEGRAPH_REWRITE_STATUS_PENDING.to_string(),
                )),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .exec(repo.db())
            .await
            .unwrap();

        let decision = repo.evaluate_eh_job_liveness(job.id, true).await.unwrap();
        let job = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert!(!decision.retire);
        assert!(!decision.remove_archive_family);
        assert!(decision.preserve_rewrite_payload);
        assert_ne!(job.status, JOB_STATUS_RETIRED);
        assert_eq!(job.cleanup_status, "none");
        assert_eq!(job.telegraph_rewrite_data.as_deref(), Some("payload"));
        assert_eq!(
            job.telegraph_rewrite_status.as_deref(),
            Some(crate::db::repo::eh_gallery_jobs::TELEGRAPH_REWRITE_STATUS_PENDING)
        );
        assert_eq!(job.zip_path.as_deref(), Some("/tmp/rewrite.zip"));
    }

    #[tokio::test]
    async fn cancellation_rolls_back_owner_and_subscription_when_job_update_fails() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let now = Local::now().naive_local();
        repo.upsert_chat(-100, "private".to_string(), None, true, Default::default())
            .await
            .unwrap();
        let task = crate::db::entities::tasks::ActiveModel {
            r#type: Set(crate::db::types::TaskType::Ehentai),
            value: Set("eh:transactional-cancel".to_string()),
            author_name: Set(None),
            next_poll_at: Set(now),
            last_polled_at: Set(None),
            ..Default::default()
        }
        .insert(repo.db())
        .await
        .unwrap();
        let subscription = crate::db::entities::subscriptions::ActiveModel {
            chat_id: Set(-100),
            task_id: Set(task.id),
            created_at: Set(now),
            ..Default::default()
        }
        .insert(repo.db())
        .await
        .unwrap();
        let delivery = repo
            .enqueue_eh_subscription_download(
                -100,
                subscription.id,
                704,
                "transactional",
                "Transactional",
                true,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let job_before = eh_gallery_jobs::Entity::find_by_id(delivery.job_id.unwrap())
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();

        repo.db()
            .execute_unprepared(
                "CREATE TRIGGER fail_eh_gallery_job_update \
                 BEFORE UPDATE OF telegraph_required ON eh_gallery_jobs \
                 BEGIN SELECT RAISE(ABORT, 'forced job update failure'); END",
            )
            .await
            .unwrap();

        assert!(repo
            .cancel_eh_subscription_queue_entries(subscription.id, true)
            .await
            .is_err());
        let after_standalone_failure = Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let job_after_standalone_failure = eh_gallery_jobs::Entity::find_by_id(job_before.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after_standalone_failure.subscription_ids,
            delivery.subscription_ids
        );
        assert_eq!(after_standalone_failure.status, STATUS_WAITING);
        assert_eq!(
            job_after_standalone_failure.telegraph_required,
            job_before.telegraph_required
        );
        assert_eq!(job_after_standalone_failure.status, job_before.status);
        assert_eq!(
            job_after_standalone_failure.cleanup_status,
            job_before.cleanup_status
        );
        repo.db()
            .execute_unprepared("DROP TRIGGER fail_eh_gallery_job_update")
            .await
            .unwrap();
        repo.db()
            .execute_unprepared(
                "CREATE TRIGGER fail_eh_gallery_job_liveness \
                 BEFORE UPDATE OF status ON eh_gallery_jobs \
                 WHEN NEW.status = 'retired' \
                 BEGIN SELECT RAISE(ABORT, 'forced liveness update failure'); END",
            )
            .await
            .unwrap();

        assert!(repo
            .delete_eh_subscription_and_cancel_queue(subscription.id, true)
            .await
            .is_err());
        assert!(repo.subscription_exists(subscription.id).await.unwrap());
        let after_delete_failure = Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after_delete_failure.subscription_ids,
            delivery.subscription_ids
        );
        assert_eq!(after_delete_failure.status, STATUS_WAITING);
        let job_after_delete_failure = eh_gallery_jobs::Entity::find_by_id(job_before.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            job_after_delete_failure.telegraph_required,
            job_before.telegraph_required
        );
        assert_eq!(job_after_delete_failure.status, job_before.status);
        assert_eq!(
            job_after_delete_failure.cleanup_status,
            job_before.cleanup_status
        );

        repo.db()
            .execute_unprepared("DROP TRIGGER fail_eh_gallery_job_liveness")
            .await
            .unwrap();
        repo.delete_eh_subscription_and_cancel_queue(subscription.id, true)
            .await
            .unwrap();
        assert!(!repo.subscription_exists(subscription.id).await.unwrap());
        let canceled = Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let retired = eh_gallery_jobs::Entity::find_by_id(job_before.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(canceled.status, STATUS_CANCELED);
        assert_eq!(canceled.subscription_ids, None);
        assert!(!retired.telegraph_required);
        assert_eq!(retired.status, JOB_STATUS_RETIRED);
    }

    #[test]
    fn test_next_claim_generation_is_monotonic_at_second_precision() {
        let second = NaiveDate::from_ymd_opt(2026, 7, 22)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let fractional_now = second + Duration::milliseconds(900);
        let fractional_previous = second + Duration::milliseconds(100);

        assert_eq!(next_claim_generation(fractional_now, None).unwrap(), second);
        assert_eq!(
            next_claim_generation(fractional_now, Some(fractional_previous)).unwrap(),
            second + Duration::seconds(1)
        );
        assert_eq!(
            next_claim_generation(second, Some(second + Duration::seconds(8))).unwrap(),
            second + Duration::seconds(9)
        );
        assert_eq!(
            next_claim_generation(second + Duration::seconds(8), Some(second)).unwrap(),
            second + Duration::seconds(8)
        );
        assert!(next_claim_generation(DateTime::MAX, Some(DateTime::MAX)).is_err());
    }

    #[tokio::test]
    async fn test_subscription_enqueue_records_origin_subscription() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_subscription_download(
                -100,
                123,
                40,
                "tok",
                "Title",
                false,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        assert_eq!(model.source, SOURCE_SUBSCRIPTION);
        assert_eq!(model.subscription_ids.as_deref(), Some("123"));

        let direct = repo
            .enqueue_eh_download(
                -100,
                41,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        assert_eq!(direct.subscription_ids, None);
    }

    #[tokio::test]
    async fn test_cancel_subscription_queue_entries_cancels_only_active_subscription_rows() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let sub_row = repo
            .enqueue_eh_subscription_download(
                -100,
                123,
                40,
                "tok",
                "Title",
                false,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let other_sub_row = repo
            .enqueue_eh_subscription_download(
                -100,
                456,
                41,
                "tok",
                "Title",
                false,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let direct_row = repo
            .enqueue_eh_download(
                -100,
                42,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let done_row = repo
            .enqueue_eh_subscription_download(
                -100,
                123,
                43,
                "tok",
                "Title",
                false,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_DONE))
            .filter(Column::Id.eq(done_row.id))
            .exec(&repo.db)
            .await
            .unwrap();

        let changed = repo
            .cancel_eh_subscription_queue_entries(123, true)
            .await
            .unwrap();
        assert_eq!(changed, 2);
        let canceled = Entity::find_by_id(sub_row.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(canceled.status, STATUS_CANCELED);
        assert_eq!(canceled.subscription_ids, None);
        assert!(Entity::find_by_id(other_sub_row.id)
            .one(&repo.db)
            .await
            .unwrap()
            .is_some());
        assert!(Entity::find_by_id(direct_row.id)
            .one(&repo.db)
            .await
            .unwrap()
            .is_some());
        let done = Entity::find_by_id(done_row.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.status, STATUS_DONE);
        assert_eq!(done.subscription_ids, None);
    }

    #[tokio::test]
    async fn test_cancel_subscription_queue_entries_keeps_other_subscription_owners() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let row = repo
            .enqueue_eh_subscription_download(
                -100,
                123,
                44,
                "tok",
                "Title",
                false,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let merged = repo
            .enqueue_eh_subscription_download(
                -100,
                456,
                44,
                "tok2",
                "Title 2",
                false,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        assert_eq!(merged.id, row.id);
        assert_eq!(merged.subscription_ids.as_deref(), Some("123,456"));

        let changed = repo
            .cancel_eh_subscription_queue_entries(123, true)
            .await
            .unwrap();
        assert_eq!(changed, 1);
        let remaining = Entity::find_by_id(row.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(remaining.status, STATUS_WAITING);
        assert_eq!(remaining.subscription_ids.as_deref(), Some("456"));

        let changed = repo
            .cancel_eh_subscription_queue_entries(456, true)
            .await
            .unwrap();
        assert_eq!(changed, 1);
        let canceled = Entity::find_by_id(row.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(canceled.status, STATUS_CANCELED);
        assert_eq!(canceled.subscription_ids, None);
    }

    #[tokio::test]
    async fn test_cancel_subscription_queue_entries_removes_stale_telegraph_requirement() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let telegraph_owner = repo
            .enqueue_eh_subscription_download(
                -100,
                123,
                52,
                "tok",
                "Title",
                true,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let merged = repo
            .enqueue_eh_subscription_download(
                -100,
                456,
                52,
                "tok2",
                "Title 2",
                false,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        assert_eq!(merged.id, telegraph_owner.id);
        assert!(merged.telegraph);
        assert_eq!(merged.subscription_ids.as_deref(), Some("123,456"));
        assert_eq!(merged.telegraph_subscription_ids.as_deref(), Some("123"));
        Entity::update_many()
            .col_expr(
                Column::NextRetryAt,
                Expr::value(Some(
                    chrono::Local::now().naive_local() + chrono::Duration::hours(1),
                )),
            )
            .filter(Column::Id.eq(merged.id))
            .exec(&repo.db)
            .await
            .unwrap();

        let changed = repo
            .cancel_eh_subscription_queue_entries(123, true)
            .await
            .unwrap();
        assert_eq!(changed, 1);
        let row = Entity::find_by_id(telegraph_owner.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_WAITING);
        assert_eq!(row.subscription_ids.as_deref(), Some("456"));
        assert_eq!(row.telegraph_subscription_ids, None);
        assert!(!row.telegraph);
        assert!(
            row.next_retry_at.is_some(),
            "owner removal must not erase delivery retry state"
        );
    }

    #[tokio::test]
    async fn test_cancel_subscription_queue_entries_preserves_concurrent_telegraph_upgrade() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        repo.enqueue_eh_subscription_download(
            -100,
            123,
            54,
            "tok",
            "Title",
            false,
            &EhGalleryVariant::archive("1280x"),
        )
        .await
        .unwrap();
        let stale = repo
            .enqueue_eh_subscription_download(
                -100,
                456,
                54,
                "tok2",
                "Title 2",
                false,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        assert_eq!(stale.subscription_ids.as_deref(), Some("123,456"));
        assert_eq!(stale.telegraph_subscription_ids, None);

        Entity::update_many()
            .col_expr(Column::Telegraph, Expr::value(true))
            .col_expr(
                Column::TelegraphSubscriptionIds,
                Expr::value(Some("456".to_string())),
            )
            .filter(Column::Id.eq(stale.id))
            .exec(&repo.db)
            .await
            .unwrap();

        let row_id = stale.id;
        let txn = repo.db.begin().await.unwrap();
        let changed = repo
            .remove_subscription_owner_from_eh_row_in_txn(&txn, stale, 123)
            .await
            .unwrap();
        txn.commit().await.unwrap();
        assert_eq!(changed, 1);
        let row = Entity::find_by_id(row_id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.subscription_ids.as_deref(), Some("456"));
        assert_eq!(row.telegraph_subscription_ids.as_deref(), Some("456"));
        assert!(row.telegraph);
    }

    #[tokio::test]
    async fn test_cancel_subscription_queue_entries_scrubs_terminal_telegraph_owner() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let row = repo
            .enqueue_eh_subscription_download(
                -100,
                123,
                53,
                "tok",
                "Title",
                true,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_DONE))
            .col_expr(
                Column::TelegraphUrl,
                Expr::value(Some("https://telegra.ph/old".to_string())),
            )
            .filter(Column::Id.eq(row.id))
            .exec(&repo.db)
            .await
            .unwrap();

        let changed = repo
            .cancel_eh_subscription_queue_entries(123, true)
            .await
            .unwrap();
        assert_eq!(changed, 1);
        let scrubbed = Entity::find_by_id(row.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(scrubbed.status, STATUS_DONE);
        assert_eq!(scrubbed.subscription_ids, None);
        assert_eq!(scrubbed.telegraph_subscription_ids, None);
        assert!(!scrubbed.telegraph);
        assert_eq!(
            scrubbed.telegraph_url.as_deref(),
            Some("https://telegra.ph/old"),
            "owner removal must not erase delivery state owned by later job migration tasks"
        );

        let reenqueued = repo
            .enqueue_eh_download(
                -100,
                53,
                "new",
                "New",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        assert_eq!(reenqueued.status, STATUS_WAITING);
        assert!(!reenqueued.telegraph);
    }

    #[tokio::test]
    async fn test_merge_preserves_concurrent_subscription_owner_updates() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let row = repo
            .enqueue_eh_subscription_download(
                -100,
                123,
                45,
                "tok",
                "Title",
                false,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::SubscriptionIds,
                Expr::value(Some("123,789".to_string())),
            )
            .filter(eh_download_queue::Column::Id.eq(row.id))
            .exec(&repo.db)
            .await
            .unwrap();

        let merged = repo
            .merge_eh_download(
                row,
                "tok2",
                "Title 2",
                false,
                SOURCE_SUBSCRIPTION,
                Some(456),
            )
            .await
            .unwrap();
        assert_eq!(merged.subscription_ids.as_deref(), Some("123,456,789"));
    }

    #[tokio::test]
    async fn test_inactive_check_preserves_concurrent_live_subscription_owner() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let now = chrono::Local::now().naive_local();
        repo.upsert_chat(-100, "private".to_string(), None, true, Default::default())
            .await
            .unwrap();
        let task = crate::db::entities::tasks::ActiveModel {
            r#type: Set(crate::db::types::TaskType::Ehentai),
            value: Set("eh:test".to_string()),
            author_name: Set(None),
            next_poll_at: Set(now),
            last_polled_at: Set(None),
            ..Default::default()
        }
        .insert(&repo.db)
        .await
        .unwrap();
        let live_sub = crate::db::entities::subscriptions::ActiveModel {
            chat_id: Set(-100),
            task_id: Set(task.id),
            created_at: Set(now),
            ..Default::default()
        }
        .insert(&repo.db)
        .await
        .unwrap();
        let row = eh_download_queue::ActiveModel {
            chat_id: Set(-100i64),
            gid: Set(46i64),
            token: Set("tok".to_string()),
            title: Set("Title".to_string()),
            telegraph: Set(false),
            source: Set(SOURCE_SUBSCRIPTION.to_string()),
            subscription_ids: Set(Some("123".to_string())),
            status: Set(STATUS_PUBLISHING.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            ..Default::default()
        }
        .insert(&repo.db)
        .await
        .unwrap();

        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::SubscriptionIds,
                Expr::value(Some(format!("123,{}", live_sub.id))),
            )
            .filter(eh_download_queue::Column::Id.eq(row.id))
            .exec(&repo.db)
            .await
            .unwrap();

        let row_id = row.id;
        let active = repo
            .eh_download_has_live_owner_or_cancel(row.id, STATUS_PUBLISHING, true)
            .await
            .unwrap();
        assert!(active, "fresh live owner should prevent soft cancel");
        let persisted = Entity::find_by_id(row_id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.status, STATUS_PUBLISHING);
    }

    #[tokio::test]
    async fn test_inactive_check_cancels_row_without_live_subscription_owner() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let now = chrono::Local::now().naive_local();
        let row = eh_download_queue::ActiveModel {
            chat_id: Set(-100i64),
            gid: Set(47i64),
            token: Set("tok".to_string()),
            title: Set("Title".to_string()),
            telegraph: Set(false),
            source: Set(SOURCE_SUBSCRIPTION.to_string()),
            subscription_ids: Set(Some("123".to_string())),
            status: Set(STATUS_PUBLISHING.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            ..Default::default()
        }
        .insert(&repo.db)
        .await
        .unwrap();
        let row_id = row.id;

        let active = repo
            .eh_download_has_live_owner_or_cancel(row.id, STATUS_PUBLISHING, true)
            .await
            .unwrap();
        assert!(!active, "missing owner should make row inactive");
        let persisted = Entity::find_by_id(row_id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.status, STATUS_CANCELED);
        assert_eq!(persisted.subscription_ids, None);
    }

    #[tokio::test]
    async fn test_cancel_legacy_subscription_queue_entries_without_owner_tracking() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let now = chrono::Local::now().naive_local();
        let legacy = eh_download_queue::ActiveModel {
            chat_id: Set(-100i64),
            gid: Set(48i64),
            token: Set("tok".to_string()),
            title: Set("Title".to_string()),
            telegraph: Set(true),
            source: Set(SOURCE_SUBSCRIPTION.to_string()),
            subscription_ids: Set(None),
            status: Set(STATUS_DOWNLOADED.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            telegraph_url: Set(Some("https://telegra.ph/stale".to_string())),
            ..Default::default()
        }
        .insert(&repo.db)
        .await
        .unwrap();
        let direct = eh_download_queue::ActiveModel {
            chat_id: Set(-100i64),
            gid: Set(49i64),
            token: Set("tok".to_string()),
            title: Set("Title".to_string()),
            telegraph: Set(false),
            source: Set(SOURCE_DIRECT.to_string()),
            subscription_ids: Set(None),
            status: Set(STATUS_DOWNLOADED.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            ..Default::default()
        }
        .insert(&repo.db)
        .await
        .unwrap();
        let terminal = eh_download_queue::ActiveModel {
            chat_id: Set(-100i64),
            gid: Set(50i64),
            token: Set("tok".to_string()),
            title: Set("Title".to_string()),
            telegraph: Set(false),
            source: Set(SOURCE_SUBSCRIPTION.to_string()),
            subscription_ids: Set(None),
            status: Set(STATUS_DONE.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            ..Default::default()
        }
        .insert(&repo.db)
        .await
        .unwrap();
        let already_canceled = eh_download_queue::ActiveModel {
            chat_id: Set(-100i64),
            gid: Set(51i64),
            token: Set("tok".to_string()),
            title: Set("Title".to_string()),
            telegraph: Set(true),
            source: Set(SOURCE_SUBSCRIPTION.to_string()),
            subscription_ids: Set(None),
            status: Set(STATUS_CANCELED.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            telegraph_url: Set(Some("https://telegra.ph/stale-canceled".to_string())),
            ..Default::default()
        }
        .insert(&repo.db)
        .await
        .unwrap();

        let count = repo
            .cancel_legacy_eh_subscription_queue_entries()
            .await
            .unwrap();
        assert_eq!(count, 3);
        let legacy = Entity::find_by_id(legacy.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(legacy.status, STATUS_CANCELED);
        assert!(!legacy.telegraph);
        assert!(legacy.telegraph_url.is_none());
        let already_canceled = Entity::find_by_id(already_canceled.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(already_canceled.status, STATUS_CANCELED);
        assert!(!already_canceled.telegraph);
        assert!(already_canceled.telegraph_url.is_none());
        assert_eq!(
            Entity::find_by_id(direct.id)
                .one(&repo.db)
                .await
                .unwrap()
                .unwrap()
                .status,
            STATUS_DOWNLOADED
        );
        assert_eq!(
            Entity::find_by_id(terminal.id)
                .one(&repo.db)
                .await
                .unwrap()
                .unwrap()
                .status,
            STATUS_CANCELED
        );
    }

    #[tokio::test]
    async fn test_reenqueue_during_downloading_settles_unbound_completion_for_cleanup() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let delivery = repo
            .enqueue_eh_download(
                -100,
                40,
                "tok",
                "Title",
                false,
                SOURCE_SUBSCRIPTION,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        let claimed_started_at = claimed.started_at.unwrap();
        assert_eq!(claimed.id, delivery.job_id.unwrap());
        assert_eq!(claimed.status, JOB_STATUS_DOWNLOADING);

        // The updated token creates the next canonical job generation and
        // rebinds the delivery away from the claimed job.
        let rebound = repo
            .enqueue_eh_download(
                -100,
                40,
                "new",
                "New",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        assert_eq!(rebound.id, delivery.id);
        assert_eq!(rebound.status, STATUS_WAITING);
        let replacement_job = job_for_delivery(&repo, &rebound).await;
        assert_ne!(replacement_job.id, claimed.id);
        assert_eq!(replacement_job.status, JOB_STATUS_PENDING);

        // The claimed worker may finish after its last consumer rebounded. Its
        // real completion is retained for the owned family and scheduled for
        // cleanup; it must never alter the replacement job.
        let settled = repo
            .mark_eh_job_downloaded(claimed.id, claimed_started_at, 9999, "/tmp/40.zip", 0)
            .await
            .unwrap();
        assert_eq!(settled.status, JOB_STATUS_RETIRED);
        assert_eq!(settled.cleanup_status, CLEANUP_STATUS_PENDING);
        assert_eq!(completion_count_for_job(&repo, claimed.id).await, 1);
        assert_eq!(
            job_for_delivery(&repo, &rebound).await.status,
            JOB_STATUS_PENDING
        );
    }

    #[tokio::test]
    async fn test_publish_claim_waits_for_required_telegraph_readiness() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("45.zip");
        std::fs::write(&zip_path, b"zip").unwrap();
        let model = repo
            .enqueue_eh_download(
                -100,
                45,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.job_id.unwrap());
        repo.mark_eh_job_downloaded(
            claimed.id,
            claimed.started_at.unwrap(),
            3,
            zip_path.to_str().unwrap(),
            0,
        )
        .await
        .unwrap();

        let publish_claim = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(publish_claim.delivery.id, model.id);
        repo.defer_eh_delivery_publish(model.id, 1).await.unwrap();

        // A delivery that now needs Telegraph is not claimable until the shared
        // Telegraph page is ready, even though its archive remains downloaded.
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_WAITING))
            .col_expr(Column::Telegraph, Expr::value(true))
            .col_expr(Column::NextRetryAt, Expr::value(None::<DateTime>))
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
            .await
            .unwrap();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRequired,
                Expr::value(true),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphStatus,
                Expr::value(crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_PENDING),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(model.job_id.unwrap()))
            .exec(repo.db())
            .await
            .unwrap();

        let none = repo.get_next_eh_delivery_for_publish(true).await.unwrap();
        assert!(
            none.is_none(),
            "publish must not claim a Telegraph delivery before its shared page is ready"
        );
    }

    #[tokio::test]
    async fn test_dirty_cleanup_job_cannot_be_claimed_or_reread_for_publish() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let delivery = repo
            .enqueue_eh_download(
                -100,
                46,
                "tok",
                "Title",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            3,
            "dirty-cleanup.zip",
            0,
        )
        .await
        .unwrap();
        let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        repo.mark_eh_job_telegraph_ready(
            upload.id,
            upload.started_at.unwrap(),
            "https://telegra.ph/ready",
            Some("payload"),
            false,
        )
        .await
        .unwrap();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::CleanupStatus,
                Expr::value(CLEANUP_STATUS_PENDING),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(download.id))
            .exec(repo.db())
            .await
            .unwrap();

        assert!(
            repo.get_next_eh_delivery_for_publish(false)
                .await
                .unwrap()
                .is_none(),
            "dirty cleanup must block publish claims even with a ready Telegraph URL"
        );

        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_PUBLISHING))
            .filter(Column::Id.eq(delivery.id))
            .exec(repo.db())
            .await
            .unwrap();
        assert!(
            repo.get_eh_delivery_publish_claim(delivery.id)
                .await
                .unwrap()
                .is_none(),
            "dirty cleanup must also reject a claimed delivery reread"
        );
    }

    #[tokio::test]
    async fn test_marker_methods_require_publishing_status() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(
                -100,
                50,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
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
            .enqueue_eh_download(
                -100,
                55,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        // Defer from pending to publishing — invalid (not an in-flight status)
        let err = repo
            .defer_eh_download(model.id, STATUS_PUBLISHING, 60)
            .await;
        assert!(err.is_err(), "defer to publishing from pending should fail");

        // Defer from pending to failed — invalid (not a legal target)
        let err = repo.defer_eh_download(model.id, STATUS_FAILED, 60).await;
        assert!(
            err.is_err(),
            "defer to failed should be rejected as invalid target"
        );

        // Defer from pending to pending — invalid (must be from downloading)
        let err = repo.defer_eh_download(model.id, STATUS_PENDING, 60).await;
        assert!(
            err.is_err(),
            "defer to pending from pending should fail (must be from downloading)"
        );
    }

    #[tokio::test]
    async fn test_defer_eh_upload_after_abort_failure_only_accepts_uploading() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let uploading = repo
            .enqueue_eh_download(
                -100,
                56,
                "uploading",
                "Title",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_UPLOADING))
            .filter(Column::Id.eq(uploading.id))
            .exec(repo.db())
            .await
            .unwrap();

        repo.defer_eh_upload_after_abort_failure(uploading.id, 60)
            .await
            .expect("uploading row should be deferred after an Abort failure");
        let uploading = Entity::find_by_id(uploading.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(uploading.status, STATUS_DOWNLOADED);
        assert_eq!(uploading.retry_count, 0);
        assert!(uploading.next_retry_at.is_some());

        let publishing = repo
            .enqueue_eh_download(
                -100,
                57,
                "publishing",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_PUBLISHING))
            .filter(Column::Id.eq(publishing.id))
            .exec(repo.db())
            .await
            .unwrap();

        let error = repo
            .defer_eh_upload_after_abort_failure(publishing.id, 60)
            .await
            .expect_err("publish rows must not use the upload-stage Abort recovery CAS");
        assert!(error.to_string().contains("expected status 'uploading'"));
        let publishing = Entity::find_by_id(publishing.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(publishing.status, STATUS_PUBLISHING);
    }

    #[tokio::test]
    async fn test_defer_eh_publish_after_abort_failure_only_accepts_publishing_targets() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let downloaded = repo
            .enqueue_eh_download(
                -100,
                58,
                "downloaded",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_PUBLISHING))
            .col_expr(Column::RetryCount, Expr::value(7))
            .col_expr(Column::Error, Expr::value("existing publish error"))
            .filter(Column::Id.eq(downloaded.id))
            .exec(repo.db())
            .await
            .unwrap();
        repo.mark_eh_archive_sent(downloaded.id).await.unwrap();

        repo.defer_eh_publish_after_abort_failure(downloaded.id, STATUS_DOWNLOADED, 60)
            .await
            .expect("publishing row should defer to downloaded after an Abort failure");
        let downloaded = Entity::find_by_id(downloaded.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(downloaded.status, STATUS_DOWNLOADED);
        assert_eq!(downloaded.retry_count, 7);
        assert_eq!(downloaded.error.as_deref(), Some("existing publish error"));
        assert!(downloaded.archive_sent_at.is_some());
        assert!(downloaded.next_retry_at.is_some());

        let uploaded = repo
            .enqueue_eh_download(
                -100,
                59,
                "uploaded",
                "Title",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_PUBLISHING))
            .filter(Column::Id.eq(uploaded.id))
            .exec(repo.db())
            .await
            .unwrap();
        repo.defer_eh_publish_after_abort_failure(uploaded.id, STATUS_UPLOADED, 60)
            .await
            .expect("publishing row should defer to uploaded after an Abort failure");
        let uploaded = Entity::find_by_id(uploaded.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(uploaded.status, STATUS_UPLOADED);
        assert_eq!(uploaded.retry_count, 0);

        let uploading = repo
            .enqueue_eh_download(
                -100,
                60,
                "uploading",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_UPLOADING))
            .filter(Column::Id.eq(uploading.id))
            .exec(repo.db())
            .await
            .unwrap();
        let error = repo
            .defer_eh_publish_after_abort_failure(uploading.id, STATUS_DOWNLOADED, 60)
            .await
            .expect_err("uploading rows must not use the publish-stage Abort recovery CAS");
        assert!(error.to_string().contains("expected status 'publishing'"));

        let error = repo
            .defer_eh_publish_after_abort_failure(uploading.id, STATUS_PENDING, 60)
            .await
            .expect_err("publish Abort recovery must reject invalid targets");
        assert!(error.to_string().contains("invalid target status"));
        let uploading = Entity::find_by_id(uploading.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(uploading.status, STATUS_UPLOADING);
    }

    #[tokio::test]
    async fn test_enqueue_merges_telegraph_and_direct_source() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let first = repo
            .enqueue_eh_download(
                -100,
                10,
                "old",
                "Old",
                false,
                SOURCE_SUBSCRIPTION,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let merged = repo
            .enqueue_eh_download(
                -100,
                10,
                "new",
                "New",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
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
            (6, STATUS_FAILED, 600),
        ] {
            let delivery = repo
                .enqueue_eh_download(
                    -100,
                    gid,
                    "tok",
                    "Title",
                    false,
                    SOURCE_DIRECT,
                    &EhGalleryVariant::archive("1280x"),
                )
                .await
                .unwrap();
            let job = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
            assert_eq!(job.id, delivery.job_id.unwrap());
            repo.mark_eh_job_downloaded(
                job.id,
                job.started_at.unwrap(),
                size,
                &format!("/tmp/{gid}.zip"),
                0,
            )
            .await
            .unwrap();
            Entity::update_many()
                .col_expr(Column::Status, Expr::value(status))
                .filter(Column::Id.eq(delivery.id))
                .exec(&repo.db)
                .await
                .unwrap();
        }

        let bytes = repo.get_eh_downloaded_bytes_in_window(24).await.unwrap();
        assert_eq!(bytes, 2100);
    }

    #[tokio::test]
    async fn test_cleanup_eh_cache_orphans_aborts_orphan_upload_state_before_removal_and_preserves_active_state(
    ) {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let temp = tempfile::tempdir().unwrap();
        let cache_dir = temp.path();

        let orphan_zip = cache_dir.join("orphan.zip");
        let orphan_part = cache_dir.join("orphan.zip.part");
        let orphan_parts = cache_dir.join("orphan.zip.parts");
        let orphan_uploads = cache_dir.join("orphan.zip.uploads");
        let active_zip = cache_dir.join("active.zip");
        let active_part = cache_dir.join("active.zip.part");
        let active_parts = cache_dir.join("active.zip.parts");
        let active_uploads = cache_dir.join("active.zip.uploads");
        let unrelated = cache_dir.join("notes").join("keep.txt");
        std::fs::write(&orphan_zip, b"zip").unwrap();
        std::fs::write(&orphan_part, b"partial").unwrap();
        std::fs::create_dir_all(orphan_parts.join("nested")).unwrap();
        std::fs::write(orphan_parts.join("manifest.json"), b"manifest").unwrap();
        std::fs::write(orphan_parts.join("nested").join("part-0001"), b"part").unwrap();
        std::fs::create_dir_all(orphan_uploads.join("nested")).unwrap();
        std::fs::write(orphan_uploads.join("archive.json"), b"archive").unwrap();
        std::fs::write(orphan_uploads.join("nested").join("image-0.json"), b"image").unwrap();
        std::fs::write(&active_zip, b"zip").unwrap();
        std::fs::write(&active_part, b"partial").unwrap();
        std::fs::create_dir_all(active_parts.join("nested")).unwrap();
        std::fs::write(active_parts.join("manifest.json"), b"manifest").unwrap();
        std::fs::write(active_parts.join("nested").join("part-0001"), b"part").unwrap();
        std::fs::create_dir_all(active_uploads.join("nested")).unwrap();
        std::fs::write(active_uploads.join("archive.json"), b"archive").unwrap();
        std::fs::write(active_uploads.join("nested").join("image-0.json"), b"image").unwrap();
        std::fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
        std::fs::write(&unrelated, b"keep").unwrap();

        let model = repo
            .enqueue_eh_download(
                -100,
                77,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.job_id.unwrap());
        repo.mark_eh_job_downloaded(
            claimed.id,
            claimed.started_at.unwrap(),
            5000,
            active_zip.to_str().unwrap(),
            0,
        )
        .await
        .unwrap();

        let uploader = RecordingAbortUploader::default();
        repo.cleanup_eh_cache_orphans(cache_dir, Some(&uploader))
            .await
            .unwrap();

        assert_eq!(
            *uploader.aborts.lock().unwrap(),
            vec![(orphan_uploads.clone(), true)],
            "only the orphan upload state should be aborted before removal"
        );

        assert!(!orphan_zip.exists(), "orphan final ZIP should be removed");
        assert!(
            !orphan_part.exists(),
            "orphan partial ZIP should be removed"
        );
        assert!(
            !orphan_parts.exists(),
            "orphan multipart state should be removed recursively"
        );
        assert!(
            !orphan_uploads.exists(),
            "orphan upload state should be removed recursively"
        );
        assert!(active_zip.exists(), "active final ZIP should be kept");
        assert!(
            active_part.exists(),
            "a persisted shared-job owner keeps its entire artifact family"
        );
        assert!(
            active_parts.exists(),
            "a persisted shared-job owner keeps its assembly state"
        );
        assert!(
            active_uploads.join("archive.json").exists()
                && active_uploads.join("nested").join("image-0.json").exists(),
            "active final ZIP should keep resumable upload state"
        );
        assert!(
            unrelated.exists(),
            "unrelated directories should be ignored"
        );
    }

    #[tokio::test]
    async fn test_cleanup_eh_cache_orphans_keeps_pending_resume_partial() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let temp = tempfile::tempdir().unwrap();
        let cache_dir = temp.path();

        let model = repo
            .enqueue_eh_download(
                -100,
                88,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.job_id.unwrap());

        let artifacts = ArchiveArtifacts::new(eh_gallery_job_artifact_path(cache_dir, &claimed));
        let part = artifacts.assembly_scratch().to_path_buf();
        let parts_dir = artifacts.parts_dir().to_path_buf();
        let uploads_dir = artifacts.uploads_dir().to_path_buf();
        std::fs::write(&part, b"partial").unwrap();
        std::fs::create_dir_all(parts_dir.join("nested")).unwrap();
        std::fs::write(parts_dir.join("manifest.json"), b"manifest").unwrap();
        std::fs::write(parts_dir.join("nested").join("part-0001"), b"part").unwrap();
        std::fs::create_dir_all(uploads_dir.join("nested")).unwrap();
        std::fs::write(uploads_dir.join("archive.json"), b"archive").unwrap();
        std::fs::write(uploads_dir.join("nested").join("image-0.json"), b"image").unwrap();

        let reset = repo.reset_stale_eh_shared_work(60, 60).await.unwrap();
        assert_eq!(reset.downloads, 1);
        let uploader = RecordingAbortUploader::default();
        repo.cleanup_eh_cache_orphans(cache_dir, Some(&uploader))
            .await
            .unwrap();
        assert!(
            uploader.aborts.lock().unwrap().is_empty(),
            "pending resume state must not be aborted"
        );

        assert!(
            part.exists(),
            "pending retry partial should be kept for resumable download"
        );
        assert!(
            parts_dir.exists(),
            "pending retry multipart state should be kept for resumable download"
        );
        assert!(
            uploads_dir.exists(),
            "pending retry upload state should be kept for resumable upload"
        );

        let retry = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        std::fs::write(artifacts.final_zip(), b"zip").unwrap();
        repo.mark_eh_job_downloaded(
            retry.id,
            retry.started_at.unwrap(),
            3,
            &artifacts.final_zip().to_string_lossy(),
            0,
        )
        .await
        .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_CANCELED))
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
            .await
            .unwrap();
        repo.evaluate_eh_job_liveness(model.job_id.unwrap(), true)
            .await
            .unwrap();

        repo.cleanup_eh_cache_orphans(cache_dir, Some(&uploader))
            .await
            .unwrap();
        assert!(
            part.exists(),
            "dirty retired job state belongs to its durable cleanup generation"
        );
        assert!(
            parts_dir.exists(),
            "orphan cleanup must not race durable cleanup ownership"
        );
        assert!(
            uploads_dir.exists(),
            "durable cleanup performs the required Abort before local removal"
        );
    }

    #[tokio::test]
    async fn test_cleanup_eh_cache_orphans_retains_upload_state_without_abort_uploader() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let temp = tempfile::tempdir().unwrap();
        let cache_dir = temp.path();

        let retained_zip = cache_dir.join("retained.zip");
        let retained_part = cache_dir.join("retained.zip.part");
        let retained_uploads = cache_dir.join("retained.zip.uploads");
        let removable_zip = cache_dir.join("removable.zip");
        std::fs::write(&retained_zip, b"zip").unwrap();
        std::fs::write(&retained_part, b"partial").unwrap();
        std::fs::create_dir_all(&retained_uploads).unwrap();
        std::fs::write(retained_uploads.join("archive.json"), b"manifest").unwrap();
        std::fs::write(&removable_zip, b"zip").unwrap();

        repo.cleanup_eh_cache_orphans(cache_dir, None)
            .await
            .unwrap();

        assert!(
            retained_zip.exists(),
            "upload-state family must be retained"
        );
        assert!(retained_part.exists(), "family scratch must be retained");
        assert!(
            retained_uploads.exists(),
            "the only remote upload IDs must not be discarded"
        );
        assert!(
            !removable_zip.exists(),
            "an orphan without upload state must retain existing cleanup behavior"
        );
    }

    #[tokio::test]
    async fn test_cleanup_eh_cache_orphans_preserves_family_when_abort_fails() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let temp = tempfile::tempdir().unwrap();
        let cache_dir = temp.path();
        let orphan_zip = cache_dir.join("abort-fails.zip");
        let orphan_part = cache_dir.join("abort-fails.zip.part");
        let orphan_uploads = cache_dir.join("abort-fails.zip.uploads");
        let manifest = orphan_uploads.join("archive.json");
        std::fs::write(&orphan_zip, b"zip").unwrap();
        std::fs::write(&orphan_part, b"partial").unwrap();
        std::fs::create_dir_all(&orphan_uploads).unwrap();
        std::fs::write(&manifest, b"manifest").unwrap();
        let uploader = RecordingAbortUploader {
            fail_abort: true,
            ..Default::default()
        };

        repo.cleanup_eh_cache_orphans(cache_dir, Some(&uploader))
            .await
            .unwrap();

        assert_eq!(
            *uploader.aborts.lock().unwrap(),
            vec![(orphan_uploads.clone(), true)],
            "Abort must observe the manifest directory before cleanup considers deletion"
        );
        assert!(
            orphan_zip.exists(),
            "Abort failure must retain the final ZIP"
        );
        assert!(
            orphan_part.exists(),
            "Abort failure must retain family scratch"
        );
        assert!(
            orphan_uploads.exists(),
            "Abort failure must retain upload state"
        );
        assert!(
            manifest.exists(),
            "Abort failure must retain the upload manifest"
        );
    }

    #[tokio::test]
    async fn orphan_cleanup_uses_active_job_paths_and_never_crosses_variants() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let temp = tempfile::tempdir().unwrap();
        let cache_dir = temp.path();
        let first = repo
            .enqueue_eh_download(
                -100,
                89,
                "variants",
                "First",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let second = repo
            .enqueue_eh_download(
                -200,
                89,
                "variants",
                "Second",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("original"),
            )
            .await
            .unwrap();
        let first_job = job_for_delivery(&repo, &first).await;
        let second_job = job_for_delivery(&repo, &second).await;
        let first_artifacts =
            ArchiveArtifacts::new(eh_gallery_job_artifact_path(cache_dir, &first_job));
        let second_artifacts =
            ArchiveArtifacts::new(eh_gallery_job_artifact_path(cache_dir, &second_job));
        assert_ne!(first_artifacts.final_zip(), second_artifacts.final_zip());
        for artifacts in [&first_artifacts, &second_artifacts] {
            std::fs::write(artifacts.final_zip(), b"zip").unwrap();
            std::fs::create_dir_all(artifacts.uploads_dir()).unwrap();
            std::fs::write(artifacts.uploads_dir().join("archive.json"), b"state").unwrap();
        }
        let orphan = ArchiveArtifacts::new(cache_dir.join("orphan.zip"));
        std::fs::write(orphan.final_zip(), b"zip").unwrap();
        std::fs::create_dir_all(orphan.uploads_dir()).unwrap();
        std::fs::write(orphan.uploads_dir().join("archive.json"), b"state").unwrap();
        let uploader = RecordingAbortUploader::default();

        repo.cleanup_eh_cache_orphans(cache_dir, Some(&uploader))
            .await
            .unwrap();

        assert_eq!(
            *uploader.aborts.lock().unwrap(),
            vec![(orphan.uploads_dir().to_path_buf(), true)]
        );
        for artifacts in [&first_artifacts, &second_artifacts] {
            assert!(artifacts.final_zip().exists());
            assert!(artifacts.uploads_dir().join("archive.json").exists());
        }
        assert!(!orphan.final_zip().exists());
        assert!(!orphan.uploads_dir().exists());
    }

    #[tokio::test]
    async fn test_publish_markers_survive_delivery_retry_and_marker_safe_reenqueue() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(
                -100,
                20,
                "tok",
                "Title",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            claimed.id,
            claimed.started_at.unwrap(),
            5000,
            "/tmp/20.zip",
            0,
        )
        .await
        .unwrap();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphUrl,
                Expr::value(Some("https://telegra.ph/20".to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphStatus,
                Expr::value(crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_READY),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRequired,
                Expr::value(true),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(model.job_id.unwrap()))
            .exec(repo.db())
            .await
            .unwrap();

        let claim = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claim.delivery.id, model.id);
        repo.mark_eh_archive_delivery_sent(model.id).await.unwrap();
        repo.mark_eh_telegraph_delivery_sent(model.id, claim.job.id, None)
            .await
            .unwrap();

        repo.schedule_eh_delivery_retry(model.id, "telegram failed", 3, true)
            .await
            .unwrap();
        let preserved = Entity::find_by_id(model.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert!(preserved.archive_sent_at.is_some());
        assert!(preserved.telegraph_sent_at.is_some());

        let rebound = repo
            .enqueue_eh_download(
                -100,
                20,
                "new",
                "New",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        assert_eq!(rebound.job_id, model.job_id);
        assert!(rebound.archive_sent_at.is_some());
        assert!(rebound.telegraph_sent_at.is_some());
    }

    #[tokio::test]
    async fn test_defer_does_not_increment_retry_count() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let delivery = repo
            .enqueue_eh_download(
                -100,
                30,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, delivery.job_id.unwrap());
        assert_eq!(claimed.status, JOB_STATUS_DOWNLOADING);

        repo.defer_eh_job_download(claimed.id, 60).await.unwrap();
        let deferred = job_for_delivery(&repo, &delivery).await;
        assert_eq!(deferred.status, JOB_STATUS_PENDING);
        assert_eq!(deferred.retry_count, 0);
        assert!(deferred.next_retry_at.is_some());
        assert_eq!(delivery.status, STATUS_WAITING);
    }

    #[tokio::test]
    async fn test_deferred_item_not_claimable_before_delay_expires() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let delivery = repo
            .enqueue_eh_download(
                -100,
                35,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, delivery.job_id.unwrap());

        // Defer with a long delay — the item should not be picked up
        repo.defer_eh_job_download(claimed.id, 3600).await.unwrap();

        let next = repo.get_next_eh_job_for_download().await.unwrap();
        assert!(
            next.is_none(),
            "deferred item should not be claimable before delay expires"
        );

        let reloaded = job_for_delivery(&repo, &delivery).await;
        assert_eq!(reloaded.retry_count, 0);
        assert!(reloaded.next_retry_at.is_some());
    }

    #[tokio::test]
    async fn test_background_owned_item_is_excluded_from_main_download_queue() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let slow = repo
            .enqueue_eh_download(
                -100,
                40,
                "slow",
                "Slow",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let fast = repo
            .enqueue_eh_download(
                -100,
                41,
                "fast",
                "Fast",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, slow.job_id.unwrap());
        let background = repo
            .schedule_eh_job_background_download(claimed.id, JOB_STATUS_DOWNLOADING, "too slow")
            .await
            .unwrap();
        assert_eq!(background.status, STATUS_PENDING);
        assert_eq!(
            background.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_PENDING)
        );

        let next = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(next.id, fast.job_id.unwrap());
    }

    #[tokio::test]
    async fn test_main_download_claim_prioritizes_recent_fifo_then_old_lifo() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let anchor = chrono::Local::now().naive_local();

        let recent_first = repo
            .enqueue_eh_download(
                -100,
                100,
                "tok",
                "Recent first",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let recent_second = repo
            .enqueue_eh_download(
                -100,
                101,
                "tok",
                "Recent second",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let recent_newer = repo
            .enqueue_eh_download(
                -100,
                102,
                "tok",
                "Recent newer",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let cutoff_first = repo
            .enqueue_eh_download(
                -100,
                200,
                "tok",
                "Cutoff first",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let cutoff_second = repo
            .enqueue_eh_download(
                -100,
                201,
                "tok",
                "Cutoff second",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let old = repo
            .enqueue_eh_download(
                -100,
                300,
                "tok",
                "Old",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let future_retry = repo
            .enqueue_eh_download(
                -100,
                400,
                "tok",
                "Future retry",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let recent_first_job = job_for_delivery(&repo, &recent_first).await;
        let recent_second_job = job_for_delivery(&repo, &recent_second).await;
        let recent_newer_job = job_for_delivery(&repo, &recent_newer).await;
        let cutoff_first_job = job_for_delivery(&repo, &cutoff_first).await;
        let cutoff_second_job = job_for_delivery(&repo, &cutoff_second).await;
        let old_job = job_for_delivery(&repo, &old).await;
        let future_retry_job = job_for_delivery(&repo, &future_retry).await;

        set_eh_job_claim_fields(
            &repo,
            recent_first_job.id,
            anchor - Duration::minutes(90),
            None,
        )
        .await;
        set_eh_job_claim_fields(
            &repo,
            recent_second_job.id,
            anchor - Duration::minutes(90),
            None,
        )
        .await;
        set_eh_job_claim_fields(
            &repo,
            recent_newer_job.id,
            anchor - Duration::minutes(30),
            None,
        )
        .await;
        set_eh_job_claim_fields(
            &repo,
            cutoff_first_job.id,
            anchor - Duration::hours(2),
            None,
        )
        .await;
        set_eh_job_claim_fields(
            &repo,
            cutoff_second_job.id,
            anchor - Duration::hours(2),
            None,
        )
        .await;
        set_eh_job_claim_fields(&repo, old_job.id, anchor - Duration::hours(3), None).await;
        set_eh_job_claim_fields(
            &repo,
            future_retry_job.id,
            anchor - Duration::hours(4),
            Some(anchor + Duration::minutes(1)),
        )
        .await;

        assert!(recent_first_job.id < recent_second_job.id);
        assert!(cutoff_first_job.id < cutoff_second_job.id);
        for expected_gid in [100, 101, 102, 201, 200, 300] {
            let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
            assert_eq!(claimed.gid, expected_gid);
            assert_eq!(claimed.status, JOB_STATUS_DOWNLOADING);
        }

        assert!(repo.get_next_eh_job_for_download().await.unwrap().is_none());
        let deferred = job_for_delivery(&repo, &future_retry).await;
        assert_eq!(deferred.status, JOB_STATUS_PENDING);
        assert_eq!(deferred.next_retry_at, Some(anchor + Duration::minutes(1)));
    }

    #[tokio::test]
    async fn test_background_download_lifecycle_success_retry_and_stale_reset() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(
                -100,
                45,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.job_id.unwrap());
        let main_generation = claimed.started_at;
        let handed_off = repo
            .schedule_eh_job_background_download(claimed.id, JOB_STATUS_DOWNLOADING, "slow")
            .await
            .unwrap();
        assert_eq!(handed_off.started_at, main_generation);

        let bg_claim = repo
            .get_next_eh_job_for_background_download()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bg_claim.id, model.job_id.unwrap());
        assert_eq!(
            bg_claim.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_RUNNING)
        );
        assert!(bg_claim.started_at > main_generation);
        let background_generation = bg_claim.started_at;

        let (retry, permanent) = repo
            .schedule_eh_job_background_retry(
                bg_claim.id,
                bg_claim.started_at.unwrap(),
                "still slow",
                6,
            )
            .await
            .unwrap();
        assert!(!permanent);
        assert_eq!(retry.status, STATUS_PENDING);
        assert_eq!(retry.background_download_attempt_count, 1);
        assert_eq!(
            retry.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_PENDING)
        );
        assert!(retry.background_download_next_retry_at.is_some());
        assert_eq!(retry.started_at, background_generation);

        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStatus,
                Expr::value(Some(BACKGROUND_STATUS_RUNNING.to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStartedAt,
                Expr::value(Some(
                    Local::now().naive_local() - chrono::Duration::seconds(7200),
                )),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(model.job_id.unwrap()))
            .exec(&repo.db)
            .await
            .unwrap();
        let reset = repo.reset_stale_eh_shared_work(3600, 3600).await.unwrap();
        assert_eq!(reset.backgrounds, 1);
        let reset_row = eh_gallery_jobs::Entity::find_by_id(model.job_id.unwrap())
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reset_row.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_PENDING)
        );
        assert!(reset_row.background_download_started_at.is_none());
        assert_eq!(reset_row.started_at, background_generation);

        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadNextRetryAt,
                Expr::value(Some(
                    Local::now().naive_local() - chrono::Duration::seconds(1),
                )),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(model.job_id.unwrap()))
            .exec(&repo.db)
            .await
            .unwrap();

        let bg_claim = repo
            .get_next_eh_job_for_background_download()
            .await
            .unwrap()
            .unwrap();
        assert!(bg_claim.started_at > background_generation);
        let done = repo
            .mark_eh_job_background_downloaded(
                bg_claim.id,
                bg_claim.started_at.unwrap(),
                1234,
                "/tmp/bg.zip",
                0,
            )
            .await
            .unwrap();
        assert_eq!(done.status, STATUS_DOWNLOADED);
        assert_eq!(done.file_size, 1234);
        assert_eq!(done.zip_path.as_deref(), Some("/tmp/bg.zip"));
        assert!(done.background_download_status.is_none());
        assert!(done.background_download_error.is_none());
        assert_eq!(done.started_at, bg_claim.started_at);
    }

    #[tokio::test]
    async fn test_release_background_downloads_to_main_queue_clears_pending_background_state() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(
                -100,
                46,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.schedule_eh_job_background_download(claimed.id, JOB_STATUS_DOWNLOADING, "slow")
            .await
            .unwrap();

        let released = repo
            .release_eh_job_background_downloads_to_main_queue()
            .await
            .unwrap();
        assert_eq!(released, 1);
        let row = eh_gallery_jobs::Entity::find_by_id(model.job_id.unwrap())
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_PENDING);
        assert!(row.background_download_status.is_none());
        assert_eq!(row.background_download_attempt_count, 0);

        let next = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(next.id, model.job_id.unwrap());
    }

    #[tokio::test]
    async fn test_cancel_subscription_queue_entries_retires_background_job() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_subscription_download(
                -100,
                123,
                47,
                "tok",
                "Title",
                false,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.schedule_eh_job_background_download(claimed.id, JOB_STATUS_DOWNLOADING, "slow")
            .await
            .unwrap();

        repo.cancel_eh_subscription_queue_entries(123, true)
            .await
            .unwrap();
        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_CANCELED);
        let job = job_for_delivery(&repo, &model).await;
        assert_eq!(job.status, JOB_STATUS_RETIRED);
    }

    #[tokio::test]
    async fn test_reenqueue_terminal_row_clears_background_state() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let generation = NaiveDate::from_ymd_opt(2026, 7, 22)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let model = repo
            .enqueue_eh_download(
                -100,
                48,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::Status,
                Expr::value(crate::db::repo::eh_gallery_jobs::JOB_STATUS_FAILED),
            )
            .col_expr(
                eh_gallery_jobs::Column::StartedAt,
                Expr::value(Some(generation)),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStatus,
                Expr::value(Some(BACKGROUND_STATUS_PENDING.to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadAttemptCount,
                Expr::value(5),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(model.job_id.unwrap()))
            .exec(&repo.db)
            .await
            .unwrap();

        let reenqueued = repo
            .enqueue_eh_download(
                -100,
                48,
                "tok2",
                "Title 2",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        assert_eq!(reenqueued.status, STATUS_WAITING);
        let reactivated_job = job_for_delivery(&repo, &reenqueued).await;
        assert_eq!(reactivated_job.status, JOB_STATUS_PENDING);
        assert!(reactivated_job.started_at.is_none());
        assert!(reactivated_job.background_download_status.is_none());
        assert_eq!(reactivated_job.background_download_attempt_count, 0);
    }

    #[tokio::test]
    async fn test_background_completion_settles_canceled_race_state_for_cleanup() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(
                -100,
                49,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.schedule_eh_job_background_download(claimed.id, JOB_STATUS_DOWNLOADING, "slow")
            .await
            .unwrap();
        let bg_claim = repo
            .get_next_eh_job_for_background_download()
            .await
            .unwrap()
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_CANCELED))
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
            .await
            .unwrap();
        repo.evaluate_eh_job_liveness(bg_claim.id, true)
            .await
            .unwrap();

        let settled = repo
            .mark_eh_job_background_downloaded(
                bg_claim.id,
                bg_claim.started_at.unwrap(),
                10,
                "/tmp/bg.zip",
                0,
            )
            .await
            .unwrap();
        assert_eq!(settled.status, JOB_STATUS_RETIRED);
        assert_eq!(settled.cleanup_status, CLEANUP_STATUS_PENDING);

        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_CANCELED);
        assert_eq!(
            job_for_delivery(&repo, &model).await.status,
            JOB_STATUS_RETIRED
        );
    }

    #[tokio::test]
    async fn test_background_retry_permanent_failure_clears_background_state() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(
                -100,
                50,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.schedule_eh_job_background_download(claimed.id, JOB_STATUS_DOWNLOADING, "slow")
            .await
            .unwrap();
        let bg_claim = repo
            .get_next_eh_job_for_background_download()
            .await
            .unwrap()
            .unwrap();

        let (failed, permanent) = repo
            .schedule_eh_job_background_retry(
                bg_claim.id,
                bg_claim.started_at.unwrap(),
                "exhausted",
                1,
            )
            .await
            .unwrap();
        assert!(permanent);
        assert_eq!(failed.status, STATUS_FAILED);
        assert_eq!(failed.error.as_deref(), Some("exhausted"));
        assert!(failed.background_download_status.is_none());
        assert!(failed.background_download_started_at.is_none());
        assert!(failed.background_download_next_retry_at.is_none());
        assert!(failed.background_download_error.is_none());
        assert_eq!(failed.background_download_attempt_count, 0);

        let row = eh_gallery_jobs::Entity::find_by_id(model.job_id.unwrap())
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.background_download_attempt_count, 0);
        let delivery = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivery.status, STATUS_FAILED);
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
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        assert_eq!(model.chat_id, -100123);
        assert_eq!(model.gid, 123456);
        assert_eq!(model.status, STATUS_WAITING);
        assert_eq!(model.source, SOURCE_SUBSCRIPTION);

        let next = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(next.id, model.job_id.unwrap());
        assert_eq!(next.status, JOB_STATUS_DOWNLOADING);
        assert!(next.started_at.is_some());

        let none = repo.get_next_eh_job_for_download().await.unwrap();
        assert!(none.is_none());
    }

    #[tokio::test]
    async fn test_mark_delivery_done() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        let model = repo
            .enqueue_eh_download(
                -100,
                1,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.job_id.unwrap());
        repo.mark_eh_job_downloaded(
            claimed.id,
            claimed.started_at.unwrap(),
            50000,
            "/tmp/1.zip",
            0,
        )
        .await
        .unwrap();

        let publish_claim = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(publish_claim.delivery.id, model.id);
        repo.mark_eh_archive_delivery_sent(model.id).await.unwrap();
        let done = repo
            .mark_eh_delivery_done(model.id, model.job_id.unwrap(), true)
            .await
            .unwrap();

        assert_eq!(done.status, STATUS_DONE);
        assert!(done.completed_at.is_some());
        assert!(done.error.is_none());
    }

    #[tokio::test]
    async fn delivery_done_rolls_back_when_liveness_settlement_fails() {
        use crate::db::repo::eh_gallery_jobs::EH_JOB_LIVENESS_UPDATE_FAILURE;
        use std::sync::{atomic::AtomicBool, Arc};

        let repo = tests_helpers::setup_test_db().await.unwrap();
        let (delivery, job) = seed_publishing_archive_delivery(&repo, 901).await;
        repo.mark_eh_archive_delivery_sent(delivery.id)
            .await
            .unwrap();

        let error = EH_JOB_LIVENESS_UPDATE_FAILURE
            .scope(
                Arc::new(AtomicBool::new(true)),
                repo.mark_eh_delivery_done(delivery.id, job.id, true),
            )
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("injected shared EH job liveness update failure"));

        let rolled_back_delivery = Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let rolled_back_job = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rolled_back_delivery.status, STATUS_PUBLISHING);
        assert_eq!(rolled_back_job.status, JOB_STATUS_DOWNLOADED);
        assert_eq!(rolled_back_job.cleanup_status, CLEANUP_STATUS_NONE);

        let done = repo
            .mark_eh_delivery_done(delivery.id, job.id, true)
            .await
            .unwrap();
        let settled_job = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.status, STATUS_DONE);
        assert_eq!(settled_job.status, JOB_STATUS_RETIRED);
        assert_eq!(settled_job.cleanup_status, CLEANUP_STATUS_PENDING);
    }

    #[tokio::test]
    async fn terminal_delivery_retry_rolls_back_with_liveness_settlement() {
        use crate::db::repo::eh_gallery_jobs::EH_JOB_LIVENESS_UPDATE_FAILURE;
        use std::sync::{atomic::AtomicBool, Arc};

        let repo = tests_helpers::setup_test_db().await.unwrap();
        let (delivery, job) = seed_publishing_archive_delivery(&repo, 902).await;

        let error = EH_JOB_LIVENESS_UPDATE_FAILURE
            .scope(
                Arc::new(AtomicBool::new(true)),
                repo.schedule_eh_delivery_retry(delivery.id, "telegram failed", 0, true),
            )
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("injected shared EH job liveness update failure"));

        let rolled_back_delivery = Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let rolled_back_job = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rolled_back_delivery.status, STATUS_PUBLISHING);
        assert_eq!(rolled_back_delivery.retry_count, 0);
        assert!(rolled_back_delivery.error.is_none());
        assert_eq!(rolled_back_job.cleanup_status, CLEANUP_STATUS_NONE);

        let (failed, terminal) = repo
            .schedule_eh_delivery_retry(delivery.id, "telegram failed", 0, true)
            .await
            .unwrap();
        let settled_job = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert!(terminal);
        assert_eq!(failed.status, STATUS_FAILED);
        assert_eq!(settled_job.status, JOB_STATUS_RETIRED);
        assert_eq!(settled_job.cleanup_status, CLEANUP_STATUS_PENDING);
    }

    #[tokio::test]
    async fn implicit_dead_subscription_cancel_preserves_pending_rewrite_without_cleanup_claim() {
        use crate::db::repo::eh_gallery_jobs::{
            TELEGRAPH_REWRITE_STATUS_PENDING, TELEGRAPH_STATUS_NOT_REQUIRED,
        };

        let repo = tests_helpers::setup_test_db().await.unwrap();
        repo.upsert_chat(-100, "private".to_string(), None, true, Default::default())
            .await
            .unwrap();
        let task = repo
            .get_or_create_task(TaskType::Ehentai, "eh:atomic-cancel".to_string(), None)
            .await
            .unwrap();
        let subscription = repo
            .upsert_eh_subscription(-100, task.id, TagFilter::default(), None)
            .await
            .unwrap();
        let delivery = repo
            .enqueue_eh_subscription_download(
                -100,
                subscription.id,
                903,
                "atomic-cancel",
                "Atomic cancel",
                true,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            1,
            "/tmp/atomic-cancel.zip",
            0,
        )
        .await
        .unwrap();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteData,
                Expr::value(Some("{\"pages\":[]}".to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteStatus,
                Expr::value(Some(TELEGRAPH_REWRITE_STATUS_PENDING.to_string())),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(download.id))
            .exec(repo.db())
            .await
            .unwrap();
        subscriptions::Entity::delete_by_id(subscription.id)
            .exec(repo.db())
            .await
            .unwrap();

        assert!(!repo
            .eh_download_is_active(delivery.id, STATUS_WAITING, true)
            .await
            .unwrap());
        let canceled = Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let settled_job = eh_gallery_jobs::Entity::find_by_id(download.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(canceled.status, STATUS_CANCELED);
        assert!(!canceled.telegraph);
        assert!(canceled.subscription_ids.is_none());
        assert!(!settled_job.telegraph_required);
        assert_eq!(settled_job.telegraph_status, TELEGRAPH_STATUS_NOT_REQUIRED);
        assert_eq!(
            settled_job.telegraph_rewrite_status.as_deref(),
            Some(TELEGRAPH_REWRITE_STATUS_PENDING)
        );
        assert!(settled_job.telegraph_rewrite_data.is_some());
        assert_eq!(settled_job.cleanup_status, CLEANUP_STATUS_NONE);
    }

    #[tokio::test]
    async fn startup_reconciliation_repairs_consumerless_crash_state_idempotently() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let (delivery, job) = seed_publishing_archive_delivery(&repo, 904).await;
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_DONE))
            .filter(Column::Id.eq(delivery.id))
            .exec(repo.db())
            .await
            .unwrap();

        assert_eq!(
            repo.reconcile_eh_shared_job_liveness(true).await.unwrap(),
            1
        );
        let first = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.status, JOB_STATUS_RETIRED);
        assert_eq!(first.cleanup_status, CLEANUP_STATUS_PENDING);

        assert_eq!(
            repo.reconcile_eh_shared_job_liveness(true).await.unwrap(),
            1
        );
        let second = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second, first);
    }

    #[tokio::test]
    async fn startup_reconciliation_preserves_active_claims_and_dirty_cleanup_generation() {
        use crate::db::repo::eh_gallery_jobs::{
            CLEANUP_STATUS_RUNNING, TELEGRAPH_REWRITE_STATUS_REWRITING, TELEGRAPH_STATUS_UPLOADING,
        };

        let repo = tests_helpers::setup_test_db().await.unwrap();

        let (_active_delivery, active_delivery_job) =
            seed_publishing_archive_delivery(&repo, 905).await;

        let active_download = repo
            .enqueue_eh_download(
                -200,
                906,
                "active-download",
                "Active download",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let active_download_job = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(active_download.job_id, Some(active_download_job.id));

        let active_upload = repo
            .enqueue_eh_download(
                -300,
                907,
                "active-upload",
                "Active upload",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let upload_download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(active_upload.job_id, Some(upload_download.id));
        repo.mark_eh_job_downloaded(
            upload_download.id,
            upload_download.started_at.unwrap(),
            1,
            "/tmp/active-upload.zip",
            0,
        )
        .await
        .unwrap();
        let active_upload_job = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        assert_eq!(
            active_upload_job.telegraph_status,
            TELEGRAPH_STATUS_UPLOADING
        );

        let active_rewrite = repo
            .enqueue_eh_download(
                -400,
                908,
                "active-rewrite",
                "Active rewrite",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let active_rewrite_job_id = active_rewrite.job_id.unwrap();
        let rewrite_started_at = chrono::Local::now().naive_local();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::Status,
                Expr::value(JOB_STATUS_DOWNLOADED),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteData,
                Expr::value(Some("{\"pages\":[]}".to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteStatus,
                Expr::value(Some(TELEGRAPH_REWRITE_STATUS_REWRITING.to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteStartedAt,
                Expr::value(Some(rewrite_started_at)),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(active_rewrite_job_id))
            .exec(repo.db())
            .await
            .unwrap();

        let dirty_cleanup = repo
            .enqueue_eh_download(
                -500,
                909,
                "dirty-cleanup",
                "Dirty cleanup",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let dirty_download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(dirty_cleanup.job_id, Some(dirty_download.id));
        repo.mark_eh_job_downloaded(
            dirty_download.id,
            dirty_download.started_at.unwrap(),
            1,
            "/tmp/dirty-cleanup.zip",
            0,
        )
        .await
        .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_DONE))
            .filter(Column::Id.eq(dirty_cleanup.id))
            .exec(repo.db())
            .await
            .unwrap();
        let cleanup_started_at = chrono::Local::now().naive_local();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::CleanupStatus,
                Expr::value(CLEANUP_STATUS_RUNNING),
            )
            .col_expr(
                eh_gallery_jobs::Column::CleanupStartedAt,
                Expr::value(Some(cleanup_started_at)),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(dirty_download.id))
            .exec(repo.db())
            .await
            .unwrap();

        let active_delivery_before = eh_gallery_jobs::Entity::find_by_id(active_delivery_job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let active_download_before = eh_gallery_jobs::Entity::find_by_id(active_download_job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let active_upload_before = eh_gallery_jobs::Entity::find_by_id(active_upload_job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let active_rewrite_before = eh_gallery_jobs::Entity::find_by_id(active_rewrite_job_id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            repo.reconcile_eh_shared_job_liveness(true).await.unwrap(),
            5
        );

        assert_eq!(
            eh_gallery_jobs::Entity::find_by_id(active_delivery_job.id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap(),
            active_delivery_before
        );
        assert_eq!(
            eh_gallery_jobs::Entity::find_by_id(active_download_job.id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap(),
            active_download_before
        );
        assert_eq!(
            eh_gallery_jobs::Entity::find_by_id(active_upload_job.id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap(),
            active_upload_before
        );
        assert_eq!(
            eh_gallery_jobs::Entity::find_by_id(active_rewrite_job_id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap(),
            active_rewrite_before
        );
        let dirty_after = eh_gallery_jobs::Entity::find_by_id(dirty_download.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(dirty_after.cleanup_status, CLEANUP_STATUS_RUNNING);
        assert_eq!(dirty_after.cleanup_started_at, Some(cleanup_started_at));
        assert_eq!(dirty_after.background_download_status.as_deref(), None);
    }

    #[tokio::test]
    async fn test_mark_failed() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        let model = repo
            .enqueue_eh_download(
                -100,
                1,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
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

        // Each successful job generation appends one immutable completion row.
        let first_delivery = repo
            .enqueue_eh_download(
                -100,
                1,
                "tok1",
                "T1",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let first = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(first.id, first_delivery.job_id.unwrap());
        repo.mark_eh_job_downloaded(first.id, first.started_at.unwrap(), 10000, "/tmp/1.zip", 0)
            .await
            .unwrap();

        let second_delivery = repo
            .enqueue_eh_download(
                -100,
                2,
                "tok2",
                "T2",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let second = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(second.id, second_delivery.job_id.unwrap());
        repo.mark_eh_job_downloaded(
            second.id,
            second.started_at.unwrap(),
            20000,
            "/tmp/2.zip",
            0,
        )
        .await
        .unwrap();

        let bytes = repo.get_eh_downloaded_bytes_in_window(24).await.unwrap();
        assert_eq!(bytes, 30000);
    }

    #[tokio::test]
    async fn test_reset_stale_downloads() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        let delivery = repo
            .enqueue_eh_download(
                -100,
                1,
                "tok",
                "T",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let first_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();

        let reset_count = repo.reset_stale_eh_shared_work(3600, 3600).await.unwrap();
        assert_eq!(reset_count.downloads, 1);
        let reset = job_for_delivery(&repo, &delivery).await;
        assert_eq!(reset.status, JOB_STATUS_PENDING);
        assert_eq!(reset.started_at, first_claim.started_at);

        let next = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(next.id, delivery.job_id.unwrap());
        assert_eq!(next.status, JOB_STATUS_DOWNLOADING);
        assert!(next.started_at > first_claim.started_at);
    }

    #[tokio::test]
    async fn test_main_claim_generation_survives_retry_handoff_and_completion() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let entry = repo
            .enqueue_eh_download(
                -100,
                69,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        let first_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        let (retry, permanent) = repo
            .schedule_eh_job_download_retry(
                first_claim.id,
                first_claim.started_at.unwrap(),
                "temporary",
                5,
            )
            .await
            .unwrap();
        assert!(!permanent);
        assert_eq!(retry.started_at, first_claim.started_at);

        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(entry.job_id.unwrap()))
            .exec(&repo.db)
            .await
            .unwrap();
        let second_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(
            second_claim.started_at,
            first_claim
                .started_at
                .map(|generation| generation + Duration::seconds(1))
        );

        let handed_off = repo
            .schedule_eh_job_background_download(
                entry.job_id.unwrap(),
                JOB_STATUS_DOWNLOADING,
                "slow",
            )
            .await
            .unwrap();
        assert_eq!(handed_off.started_at, second_claim.started_at);
        assert_eq!(
            repo.release_eh_job_background_downloads_to_main_queue()
                .await
                .unwrap(),
            1
        );
        let released = eh_gallery_jobs::Entity::find_by_id(entry.job_id.unwrap())
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(released.started_at, second_claim.started_at);

        let third_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(
            third_claim.started_at,
            second_claim
                .started_at
                .map(|generation| generation + Duration::seconds(1))
        );
        let downloaded = repo
            .mark_eh_job_downloaded(
                entry.job_id.unwrap(),
                third_claim.started_at.unwrap(),
                1024,
                "/tmp/69.zip",
                0,
            )
            .await
            .unwrap();
        assert_eq!(downloaded.started_at, third_claim.started_at);
    }

    #[tokio::test]
    async fn test_count_pending() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        repo.enqueue_eh_download(
            -100,
            1,
            "tok1",
            "T1",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
        )
        .await
        .unwrap();
        repo.enqueue_eh_download(
            -100,
            2,
            "tok2",
            "T2",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
        )
        .await
        .unwrap();

        let count = repo.count_pending_eh_downloads().await.unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn test_queue_schema_has_publish_marker_columns() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let entry = repo
            .enqueue_eh_download(
                -100,
                42,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        assert!(entry.archive_sent_at.is_none());
        assert!(entry.telegraph_sent_at.is_none());
    }

    /// Permanent retry settles the unbound claimed job without changing the
    /// replacement delivery that was re-enqueued after the claim.
    #[tokio::test]
    async fn test_schedule_permanent_retry_settles_unbound_job_without_failing_replacement() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let delivery = repo
            .enqueue_eh_download(
                -100,
                60,
                "tok",
                "Title",
                false,
                SOURCE_SUBSCRIPTION,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        let claimed_started_at = claimed.started_at.unwrap();
        assert_eq!(claimed.id, delivery.job_id.unwrap());
        assert_eq!(claimed.status, JOB_STATUS_DOWNLOADING);

        // A changed token rebinds the delivery to a fresh pending canonical job.
        let reenq = repo
            .enqueue_eh_download(
                -100,
                60,
                "new",
                "New",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        assert_eq!(reenq.id, delivery.id);
        assert_eq!(reenq.status, STATUS_WAITING);
        let rebound_job = job_for_delivery(&repo, &reenq).await;
        assert_ne!(rebound_job.id, claimed.id);
        assert_eq!(rebound_job.status, JOB_STATUS_PENDING);

        let (failed, permanent) = repo
            .schedule_eh_job_download_retry(claimed.id, claimed_started_at, "stale error", 0)
            .await
            .unwrap();
        assert!(permanent);
        assert_eq!(failed.id, claimed.id);
        assert_eq!(
            failed.status,
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_FAILED
        );

        assert_eq!(
            job_for_delivery(&repo, &reenq).await.status,
            JOB_STATUS_PENDING
        );
        assert_eq!(completion_count_for_job(&repo, claimed.id).await, 0);
    }

    /// A policy decision made by a main worker settles its unbound claimed job
    /// without overwriting the replacement delivery's job.
    #[tokio::test]
    async fn test_main_archive_policy_failure_settles_unbound_job_without_failing_replacement() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let delivery = repo
            .enqueue_eh_download(
                -100,
                62,
                "tok",
                "Title",
                false,
                SOURCE_SUBSCRIPTION,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, delivery.job_id.unwrap());
        assert_eq!(claimed.status, JOB_STATUS_DOWNLOADING);

        let reenqueued = repo
            .enqueue_eh_download(
                -100,
                62,
                "new",
                "New",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        assert_eq!(reenqueued.id, delivery.id);
        assert_eq!(reenqueued.status, STATUS_WAITING);
        let rebound_job = job_for_delivery(&repo, &reenqueued).await;
        assert_ne!(rebound_job.id, claimed.id);
        assert_eq!(rebound_job.status, JOB_STATUS_PENDING);

        let failed = repo
            .fail_eh_job_for_archive_policy(&claimed, "policy reject")
            .await
            .unwrap();
        assert_eq!(failed.id, claimed.id);
        assert_eq!(
            failed.status,
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_FAILED
        );

        assert_eq!(
            job_for_delivery(&repo, &reenqueued).await.status,
            JOB_STATUS_PENDING
        );
        assert_eq!(completion_count_for_job(&repo, claimed.id).await, 0);
    }

    /// A policy decision made by a background worker settles its claim after
    /// cancellation without changing the canceled delivery.
    #[tokio::test]
    async fn test_background_archive_policy_failure_settles_canceled_claim() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_subscription_download(
                -100,
                123,
                63,
                "tok",
                "Title",
                false,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.schedule_eh_job_background_download(claimed.id, JOB_STATUS_DOWNLOADING, "slow")
            .await
            .unwrap();
        let background_claim = repo
            .get_next_eh_job_for_background_download()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            background_claim.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_RUNNING)
        );

        repo.cancel_eh_subscription_queue_entries(123, true)
            .await
            .unwrap();

        let failed = repo
            .fail_eh_job_background_download_for_archive_policy(&background_claim, "policy reject")
            .await
            .unwrap();
        assert_eq!(failed.id, background_claim.id);
        assert_eq!(
            failed.status,
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_FAILED
        );

        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_CANCELED);
        assert_ne!(row.status, STATUS_FAILED);
    }

    #[tokio::test]
    async fn test_archive_policy_failure_rejects_missing_claim_timestamp() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let entry = repo
            .enqueue_eh_download(
                -100,
                66,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        let job = job_for_delivery(&repo, &entry).await;
        let main_err = repo
            .fail_eh_job_for_archive_policy(&job, "policy reject")
            .await
            .expect_err("main policy failure requires a claim timestamp");
        assert!(main_err
            .to_string()
            .contains("missing download claim started_at"));

        let background_err = repo
            .fail_eh_job_background_download_for_archive_policy(&job, "policy reject")
            .await
            .expect_err("background policy failure requires a claim timestamp");
        assert!(background_err
            .to_string()
            .contains("missing claim started_at"));
    }

    /// A main policy transition must be bound to the original worker claim, not
    /// merely to a status that a newer worker can claim again.
    #[tokio::test]
    async fn test_main_archive_policy_aba_does_not_fail_reclaimed_row() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let delivery = repo
            .enqueue_eh_download(
                -100,
                64,
                "tok",
                "Title",
                false,
                SOURCE_SUBSCRIPTION,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        let first_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(first_claim.id, delivery.job_id.unwrap());
        assert_eq!(first_claim.status, JOB_STATUS_DOWNLOADING);
        assert!(first_claim.started_at.is_some());

        repo.defer_eh_job_download(first_claim.id, 0).await.unwrap();
        let second_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(second_claim.status, JOB_STATUS_DOWNLOADING);
        assert_eq!(
            second_claim.started_at,
            first_claim
                .started_at
                .map(|generation| generation + Duration::seconds(1))
        );

        let err = repo
            .fail_eh_job_for_archive_policy(&first_claim, "policy reject")
            .await
            .expect_err("stale main claim must not fail the newer claim");
        assert!(err.to_string().contains("claim changed concurrently"));

        let job = job_for_delivery(&repo, &delivery).await;
        assert_eq!(job.status, JOB_STATUS_DOWNLOADING);
        assert_eq!(job.started_at, second_claim.started_at);
        assert_ne!(
            job.status,
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_FAILED
        );
        assert_eq!(completion_count_for_job(&repo, job.id).await, 0);
    }

    /// A background policy transition settles its original unbound job without
    /// changing the distinct job claimed after cancellation and re-enqueue.
    #[tokio::test]
    async fn test_background_archive_policy_settles_unbound_job_without_failing_reclaimed_row() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let _model = repo
            .enqueue_eh_subscription_download(
                -100,
                124,
                65,
                "tok",
                "Title",
                false,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        let first_main_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.schedule_eh_job_background_download(
            first_main_claim.id,
            JOB_STATUS_DOWNLOADING,
            "slow",
        )
        .await
        .unwrap();
        let first_background_claim = repo
            .get_next_eh_job_for_background_download()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            first_background_claim.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_RUNNING)
        );
        assert!(first_background_claim
            .background_download_started_at
            .is_some());
        assert!(first_background_claim.started_at.is_some());

        repo.cancel_eh_subscription_queue_entries(124, true)
            .await
            .unwrap();
        let reenqueued = repo
            .enqueue_eh_download(
                -100,
                65,
                "new",
                "New",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        assert_eq!(reenqueued.status, STATUS_WAITING);
        let replacement_job = job_for_delivery(&repo, &reenqueued).await;
        assert_ne!(replacement_job.id, first_background_claim.id);
        let second_main_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(second_main_claim.id, replacement_job.id);
        repo.schedule_eh_job_background_download(
            second_main_claim.id,
            JOB_STATUS_DOWNLOADING,
            "slow",
        )
        .await
        .unwrap();
        let second_background_claim = repo
            .get_next_eh_job_for_background_download()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            second_background_claim
                .background_download_status
                .as_deref(),
            Some(BACKGROUND_STATUS_RUNNING)
        );

        let failed = repo
            .fail_eh_job_background_download_for_archive_policy(
                &first_background_claim,
                "policy reject",
            )
            .await
            .unwrap();
        assert_eq!(failed.id, first_background_claim.id);
        assert_eq!(
            failed.status,
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_FAILED
        );

        let row = job_for_delivery(&repo, &reenqueued).await;
        assert_eq!(row.status, STATUS_PENDING);
        assert_eq!(
            row.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_RUNNING)
        );
        assert_eq!(
            row.background_download_started_at,
            second_background_claim.background_download_started_at
        );
        assert_eq!(row.started_at, second_background_claim.started_at);
        assert_ne!(row.status, STATUS_FAILED);
    }

    #[tokio::test]
    async fn test_main_download_claim_refetch_detects_lost_claim() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let delivery = repo
            .enqueue_eh_download(
                -100,
                68,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        repo.db
            .execute(Statement::from_string(
                DbBackend::Sqlite,
                "CREATE TRIGGER release_eh_job_claim AFTER UPDATE OF status ON eh_gallery_jobs \
                 WHEN NEW.status = 'downloading' BEGIN \
                      UPDATE eh_gallery_jobs SET status = 'pending' WHERE id = NEW.id; \
                  END;",
            ))
            .await
            .unwrap();

        assert!(
            repo.get_next_eh_job_for_download().await.unwrap().is_none(),
            "a claim lost before refetch must not be returned to its original worker"
        );
        let job = job_for_delivery(&repo, &delivery).await;
        assert_eq!(job.status, JOB_STATUS_PENDING);
        assert!(job.started_at.is_some());
    }

    #[tokio::test]
    async fn test_stale_upload_retry_does_not_overwrite_publishing_row() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let delivery = repo
            .enqueue_eh_download(
                -100,
                61,
                "tok",
                "Title",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        let download_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(download_claim.id, delivery.job_id.unwrap());
        assert_eq!(download_claim.status, JOB_STATUS_DOWNLOADING);
        repo.mark_eh_job_downloaded(
            download_claim.id,
            download_claim.started_at.unwrap(),
            5000,
            "/tmp/61.zip",
            0,
        )
        .await
        .unwrap();

        let upload_claim = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        assert_eq!(upload_claim.id, delivery.job_id.unwrap());
        let upload_started_at = upload_claim.started_at.unwrap();
        repo.mark_eh_job_telegraph_ready(
            upload_claim.id,
            upload_started_at,
            "https://telegra.ph/61",
            None,
            true,
        )
        .await
        .unwrap();

        let publish_claim = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(publish_claim.delivery.id, delivery.id);
        assert_eq!(publish_claim.delivery.status, STATUS_PUBLISHING);

        let stale_upload_retry = repo
            .record_eh_job_upload_failure(
                upload_claim.id,
                upload_started_at,
                "stale upload failure",
                3,
                true,
            )
            .await
            .unwrap();
        assert!(
            matches!(
                stale_upload_retry,
                crate::db::repo::eh_gallery_jobs::EhJobUploadFailureOutcome::Stale
            ),
            "stale shared upload retry must not affect a delivery that is already publishing"
        );

        let row = Entity::find_by_id(delivery.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_PUBLISHING);
        assert_eq!(row.retry_count, 0);
        assert_eq!(
            job_for_delivery(&repo, &delivery).await.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_READY
        );
    }

    #[tokio::test]
    async fn test_enqueue_preserves_a_markerless_crashed_publishing_claim() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_subscription_download(
                -100,
                123,
                65,
                "tok",
                "Title",
                false,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.job_id.unwrap());
        repo.mark_eh_job_downloaded(
            claimed.id,
            claimed.started_at.unwrap(),
            5000,
            "/tmp/65.zip",
            0,
        )
        .await
        .unwrap();

        let publish_claim = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(publish_claim.delivery.id, model.id);

        // A new token creates a different shared job, but a crash-residual
        // publishing claim must remain bound to its original wave for startup
        // recovery. Enqueue may merge ownership, never replace the claim.
        let merged = repo
            .enqueue_eh_subscription_download(
                -100,
                456,
                65,
                "newtok",
                "NewTitle",
                true,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        assert_eq!(merged.id, model.id);
        assert_eq!(merged.status, STATUS_PUBLISHING);
        assert!(
            merged.telegraph,
            "owner merge must preserve telegraph demand"
        );
        assert_eq!(merged.subscription_ids.as_deref(), Some("123,456"));
        assert_eq!(merged.telegraph_subscription_ids.as_deref(), Some("456"));
        assert_eq!(merged.token, "tok");
        assert_eq!(merged.title, "Title");
        assert_eq!(merged.job_id, model.job_id);
        let claimed_job = job_for_delivery(&repo, &merged).await;
        assert_eq!(claimed_job.status, JOB_STATUS_DOWNLOADED);
        assert!(claimed_job.telegraph_required);
        assert_eq!(claimed_job.title, "Title");
        let unbound_requested_job = eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::Gid.eq(65_i64))
            .filter(eh_gallery_jobs::Column::Token.eq("newtok"))
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(unbound_requested_job.status, JOB_STATUS_RETIRED);
        assert!(
            repo.get_next_eh_job_for_download().await.unwrap().is_none(),
            "a requested job left unbound by a publishing claim must not remain claimable"
        );
    }

    /// When an insert conflicts on (chat_id, gid) unique constraint, the
    /// insert-error path re-selects and calls merge_eh_download.  Verify that
    /// merge correctly handles a row inserted directly into the DB (simulating
    /// a concurrent insert that won the race).
    #[tokio::test]
    async fn test_enqueue_insert_error_reselect_merge_helper() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        // Simulate a concurrent caller that inserted the row first.
        let now = chrono::Local::now().naive_local();
        let conflict = eh_download_queue::ActiveModel {
            chat_id: Set(-100i64),
            gid: Set(70i64),
            token: Set("other".to_string()),
            title: Set("Other".to_string()),
            telegraph: Set(false),
            source: Set(SOURCE_SUBSCRIPTION.to_string()),
            status: Set(STATUS_PENDING.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            ..Default::default()
        };
        conflict.insert(&repo.db).await.unwrap();

        // Now enqueue the "real" request — SELECT finds the directly-inserted
        // row and merges via merge_eh_download.
        let merged = repo
            .enqueue_eh_download(
                -100,
                70,
                "tok2",
                "Title2",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();

        assert_eq!(merged.chat_id, -100);
        assert_eq!(merged.gid, 70);
        // Merge should have applied the new values and OR'd telegraph + source upgrade.
        assert_eq!(merged.token, "tok2");
        assert_eq!(merged.title, "Title2");
        assert!(merged.telegraph, "telegraph should be OR-merged to true");
        assert_eq!(
            merged.source, SOURCE_DIRECT,
            "source should be upgraded to direct"
        );

        // No duplicate rows
        let all: Vec<_> = Entity::find()
            .filter(Column::ChatId.eq(-100))
            .filter(Column::Gid.eq(70))
            .all(&repo.db)
            .await
            .unwrap();
        assert_eq!(all.len(), 1);
    }

    /// When a stale subscription merge races with a direct-source upgrade,
    /// the CAS source guard prevents the stale snapshot from overwriting
    /// the direct upgrade.  The retry loop re-reads and recomputes so the
    /// final source stays `SOURCE_DIRECT`.
    #[tokio::test]
    async fn test_merge_source_guard_preserves_direct_upgrade() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        // Insert a row with SOURCE_SUBSCRIPTION, status=waiting.
        let model = repo
            .enqueue_eh_download(
                -100,
                80,
                "tok",
                "Title",
                false,
                SOURCE_SUBSCRIPTION,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        assert_eq!(model.source, SOURCE_SUBSCRIPTION);
        assert_eq!(model.status, STATUS_WAITING);

        // Snapshot A: the old subscription row
        let snap_a = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snap_a.source, SOURCE_SUBSCRIPTION);

        // Apply a direct upgrade via enqueue (full reset path)
        let upgraded = repo
            .enqueue_eh_download(
                -100,
                80,
                "direct_tok",
                "Direct Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        assert_eq!(upgraded.id, model.id);
        assert_eq!(upgraded.source, SOURCE_DIRECT);

        // Snapshot B: the upgraded delivery is still waiting.
        let snap_b = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snap_b.source, SOURCE_DIRECT);

        // Apply stale subscription merge from snapshot A.
        // The CAS guard must detect that source changed from SUBSCRIPTION to
        // DIRECT, fail the update, re-read, and recompute -> source stays DIRECT.
        let merged = repo
            .merge_eh_download(
                snap_a,
                "stale_tok",
                "Stale Title",
                false,
                SOURCE_SUBSCRIPTION,
                None,
            )
            .await
            .unwrap();
        assert_eq!(merged.id, model.id);
        assert_eq!(
            merged.source, SOURCE_DIRECT,
            "stale subscription merge must not overwrite direct upgrade"
        );
        // Token may be updated by the stale merge (normal in-place update after
        // the CAS retry re-reads the row with the correct source).  The key
        // invariant is that source stays SOURCE_DIRECT.
        assert_eq!(merged.token, "stale_tok");
    }

    /// After fallback, stale Telegraph URL and sent markers must be cleared
    /// so publish workers do not send stale Telegraph links.
    #[tokio::test]
    async fn test_fallback_clears_stale_telegraph_state() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        // Construct a STATUS_UPLOADING row with stale Telegraph state
        let now = chrono::Local::now().naive_local();
        let active = eh_download_queue::ActiveModel {
            chat_id: Set(-100i64),
            gid: Set(90i64),
            token: Set("tok".to_string()),
            title: Set("Title".to_string()),
            telegraph: Set(true),
            source: Set(SOURCE_DIRECT.to_string()),
            status: Set(STATUS_UPLOADING.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(2),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(Some(now)),
            zip_path: Set(Some("/tmp/90.zip".to_string())),
            telegraph_url: Set(Some("https://telegra.ph/stale".to_string())),
            archive_sent_at: Set(Some(now)),
            telegraph_sent_at: Set(Some(now)),
            next_retry_at: Set(Some(now)),
            ..Default::default()
        };
        let model = active.insert(&repo.db).await.unwrap();

        // Perform fallback
        let result = repo
            .fallback_eh_upload_to_archive(model.id, "permanent upload failure")
            .await
            .unwrap();

        // Assert: status downgraded to downloaded, telegraph cleared
        assert_eq!(result.status, STATUS_DOWNLOADED);
        assert!(!result.telegraph);

        // Assert: stale Telegraph state cleared
        assert!(
            result.telegraph_url.is_none(),
            "telegraph_url must be cleared after fallback"
        );
        assert!(
            result.archive_sent_at.is_none(),
            "archive_sent_at must be cleared after fallback"
        );
        assert!(
            result.telegraph_sent_at.is_none(),
            "telegraph_sent_at must be cleared after fallback"
        );

        // Assert: retry state reset
        assert_eq!(result.retry_count, 0);
        assert!(
            result.next_retry_at.is_none(),
            "next_retry_at must be cleared after fallback"
        );

        // Assert: error recorded
        assert_eq!(result.error.as_deref(), Some("permanent upload failure"));

        // Assert: ZIP path preserved
        assert_eq!(result.zip_path.as_deref(), Some("/tmp/90.zip"));
    }

    #[tokio::test]
    async fn test_disable_telegraph_without_token_downgrades_unuploaded_downloaded_rows() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(
                -100,
                91,
                "tok",
                "Title",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.job_id.unwrap());
        repo.mark_eh_job_downloaded(
            claimed.id,
            claimed.started_at.unwrap(),
            5000,
            "/tmp/91.zip",
            0,
        )
        .await
        .unwrap();

        let changed = repo
            .disable_eh_telegraph_for_unuploaded_jobs()
            .await
            .unwrap();
        assert_eq!(changed, 1);

        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_WAITING);
        assert!(!row.telegraph);
        assert!(row.telegraph_url.is_none());

        let job = eh_gallery_jobs::Entity::find_by_id(model.job_id.unwrap())
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            job.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_NOT_REQUIRED
        );
        assert!(!job.telegraph_required);
    }

    #[tokio::test]
    async fn test_disable_telegraph_without_token_preserves_uploaded_rows_with_url() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(
                -100,
                92,
                "tok",
                "Title",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.job_id.unwrap());
        repo.mark_eh_job_downloaded(
            claimed.id,
            claimed.started_at.unwrap(),
            5000,
            "/tmp/92.zip",
            0,
        )
        .await
        .unwrap();
        let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        assert_eq!(upload.id, model.job_id.unwrap());
        repo.mark_eh_job_telegraph_ready(
            upload.id,
            upload.started_at.unwrap(),
            "https://telegra.ph/92",
            None,
            true,
        )
        .await
        .unwrap();

        let changed = repo
            .disable_eh_telegraph_for_unuploaded_jobs()
            .await
            .unwrap();
        assert_eq!(changed, 0);

        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert!(row.telegraph);
        assert_eq!(row.status, STATUS_WAITING);
        assert!(row.telegraph_url.is_none());
        let job = eh_gallery_jobs::Entity::find_by_id(model.job_id.unwrap())
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.telegraph_status, TELEGRAPH_STATUS_READY);
        assert_eq!(job.telegraph_url.as_deref(), Some("https://telegra.ph/92"));
    }

    #[tokio::test]
    async fn test_disable_telegraph_without_token_clears_terminal_stale_flag() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(
                -100,
                93,
                "tok",
                "Title",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_FAILED))
            .col_expr(Column::Error, Expr::value(Some("old failure".to_string())))
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
            .await
            .unwrap();
        let canceled_model = repo
            .enqueue_eh_download(
                -100,
                94,
                "tok",
                "Title",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_CANCELED))
            .filter(Column::Id.eq(canceled_model.id))
            .exec(&repo.db)
            .await
            .unwrap();
        let done_with_url = repo
            .enqueue_eh_download(
                -100,
                95,
                "tok",
                "Title",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_DONE))
            .col_expr(
                Column::TelegraphUrl,
                Expr::value(Some("https://telegra.ph/old".to_string())),
            )
            .filter(Column::Id.eq(done_with_url.id))
            .exec(&repo.db)
            .await
            .unwrap();

        let changed = repo
            .disable_eh_telegraph_for_unuploaded_jobs()
            .await
            .unwrap();
        assert_eq!(changed, 0);

        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_FAILED);
        assert!(
            row.telegraph,
            "terminal delivery history is not startup work"
        );
        let canceled = Entity::find_by_id(canceled_model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(canceled.status, STATUS_CANCELED);
        assert!(canceled.telegraph);
        let done = Entity::find_by_id(done_with_url.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.status, STATUS_DONE);
        assert!(done.telegraph);
        assert_eq!(
            done.telegraph_url.as_deref(),
            Some("https://telegra.ph/old")
        );
    }

    #[tokio::test]
    async fn shared_enqueue_is_atomic_across_job_and_delivery_unique_constraints() {
        let repo = std::sync::Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let variant = EhGalleryVariant::archive("980x");

        let (first, second, same_chat_upgrade) = tokio::join!(
            repo.enqueue_eh_download(
                -100,
                700,
                "token",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
            ),
            repo.enqueue_eh_download(
                -200,
                700,
                "token",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
            ),
            repo.enqueue_eh_download(-100, 700, "token", "Gallery", true, SOURCE_DIRECT, &variant,),
        );
        let first = first.unwrap();
        let second = second.unwrap();
        let same_chat_upgrade = same_chat_upgrade.unwrap();

        assert_eq!(first.job_id, second.job_id);
        assert_eq!(first.id, same_chat_upgrade.id);
        assert!(same_chat_upgrade.telegraph);
        assert_eq!(
            eh_gallery_jobs::Entity::find()
                .filter(eh_gallery_jobs::Column::Gid.eq(700))
                .count(repo.db())
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            Entity::find()
                .filter(Column::Gid.eq(700))
                .count(repo.db())
                .await
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn enqueue_isolates_variants_and_rebinds_direct_upgrade_before_markers() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let subscription = repo
            .enqueue_eh_subscription_download(
                -100,
                12,
                701,
                "token",
                "Subscription title",
                false,
                &EhGalleryVariant::archive("980x"),
            )
            .await
            .unwrap();
        let subscription_job_id = subscription.job_id.unwrap();

        let direct = repo
            .enqueue_eh_download(
                -100,
                701,
                "token",
                "Direct title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("original"),
            )
            .await
            .unwrap();
        let direct_job_id = direct.job_id.unwrap();
        assert_ne!(direct_job_id, subscription_job_id);
        assert_eq!(direct.source, SOURCE_DIRECT);
        assert_eq!(
            eh_gallery_jobs::Entity::find_by_id(subscription_job_id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap()
                .status,
            JOB_STATUS_RETIRED
        );

        Entity::update_many()
            .col_expr(
                Column::ArchiveSentAt,
                Expr::value(Some(Local::now().naive_local())),
            )
            .filter(Column::Id.eq(direct.id))
            .exec(repo.db())
            .await
            .unwrap();
        let marker_bound = repo
            .enqueue_eh_download(
                -100,
                701,
                "token",
                "Later title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("780x"),
            )
            .await
            .unwrap();
        assert_eq!(marker_bound.job_id, Some(direct_job_id));

        let orphan = eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::Gid.eq(701))
            .filter(eh_gallery_jobs::Column::Resolution.eq("780x"))
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(orphan.status, JOB_STATUS_RETIRED);
    }

    #[tokio::test]
    async fn enqueue_binds_dirty_retired_job_without_reactivating_or_clearing_artifacts() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("980x");
        let initial = repo
            .enqueue_eh_download(
                -100,
                702,
                "token",
                "Initial title",
                false,
                SOURCE_DIRECT,
                &variant,
            )
            .await
            .unwrap();
        let job_id = initial.job_id.unwrap();
        Entity::update_many()
            .col_expr(Column::JobId, Expr::value(None::<i32>))
            .col_expr(Column::Status, Expr::value(STATUS_CANCELED))
            .filter(Column::Id.eq(initial.id))
            .exec(repo.db())
            .await
            .unwrap();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::Status,
                Expr::value(JOB_STATUS_RETIRED),
            )
            .col_expr(
                eh_gallery_jobs::Column::CleanupStatus,
                Expr::value(CLEANUP_STATUS_FAILED),
            )
            .col_expr(
                eh_gallery_jobs::Column::ZipPath,
                Expr::value(Some("cache/702.zip".to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::CleanupError,
                Expr::value(Some("disk unavailable".to_string())),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .exec(repo.db())
            .await
            .unwrap();

        let bound = repo
            .enqueue_eh_download(
                -200,
                702,
                "token",
                "Later title",
                false,
                SOURCE_DIRECT,
                &variant,
            )
            .await
            .unwrap();
        assert_eq!(bound.job_id, Some(job_id));
        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.status, JOB_STATUS_RETIRED);
        assert_eq!(job.cleanup_status, CLEANUP_STATUS_FAILED);
        assert_eq!(job.zip_path.as_deref(), Some("cache/702.zip"));
        assert_eq!(job.cleanup_error.as_deref(), Some("disk unavailable"));
    }

    #[tokio::test]
    async fn publish_claim_readback_failure_rolls_back_the_delivery_cas() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let delivery = repo
            .enqueue_eh_download(
                -100,
                703,
                "token",
                "Claim rollback",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("980x"),
            )
            .await
            .unwrap();
        repo.db
            .execute(Statement::from_string(
                DbBackend::Sqlite,
                format!(
                    "CREATE TRIGGER delete_claimed_delivery AFTER UPDATE OF status ON eh_download_queue \
                     WHEN NEW.id = {} AND NEW.status = 'publishing' BEGIN \
                         DELETE FROM eh_download_queue WHERE id = NEW.id; \
                     END;",
                    delivery.id
                ),
            ))
            .await
            .unwrap();

        let error = repo
            .get_next_eh_delivery_for_publish(false)
            .await
            .expect_err("a missing readback must fail the claim transaction");
        assert!(error
            .to_string()
            .contains("Shared EH delivery changed before publish claim readback"));

        let delivery = Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .expect("the failed readback must not leave a deleted or publishing delivery");
        assert_eq!(delivery.status, STATUS_WAITING);
        assert!(delivery.started_at.is_none());
    }
}
