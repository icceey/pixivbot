mod cancel;
mod orphan_cleanup;
mod publish;
#[cfg(test)]
mod tests;

use super::Repo;
use crate::db::entities::{eh_download_queue, eh_gallery_jobs};
use crate::db::repo::eh_gallery_jobs::{
    eh_gallery_job_artifact_path, legacy_eh_gallery_job_artifact_path, CLEANUP_STATUS_FAILED,
    CLEANUP_STATUS_NONE, CLEANUP_STATUS_PENDING, CLEANUP_STATUS_RUNNING, JOB_STATUS_DOWNLOADED,
    JOB_STATUS_DOWNLOADING, JOB_STATUS_PENDING, JOB_STATUS_RETIRED, TELEGRAPH_STATUS_PENDING,
    TELEGRAPH_STATUS_READY, TELEGRAPH_STATUS_UPLOADING,
};
use crate::db::repo::eh_gallery_push_ledger::{record_eh_push_in_txn, EhPushSurface};
use anyhow::{Context, Result};
use chrono::{Local, Timelike};
use eh_client::ArchiveArtifacts;
use media_publish::ImageUploader;
use sea_orm::prelude::DateTime;
use sea_orm::sea_query::{Expr, Query, SimpleExpr};
use sea_orm::{
    ColumnTrait, Condition, DatabaseTransaction, EntityTrait, Order, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, Set, TransactionTrait,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use tokio::sync::OwnedMutexGuard;
use tracing::warn;

use crate::db::entities::subscriptions;

/// How many times a whole transaction is retried when SQLite reports
/// lock contention that a fresh read snapshot could resolve.
///
/// WAL mode lets readers proceed while writers commit, but a deferred
/// read→write transaction whose snapshot was invalidated by a concurrent
/// commit fails with `SQLITE_BUSY` (5/517) regardless of `busy_timeout`:
/// waiting cannot refresh a stale snapshot. Rolling back and re-running the
/// transaction acquires a fresh snapshot, so bounded retries absorb the
/// contention instead of failing the scheduler tick.
pub(super) const SQLITE_TRANSACTION_ATTEMPTS: usize = 3;

/// Lock-contention errors that a full transaction retry can resolve. The
/// CAS filters inside each transaction keep retries safe: a row changed by
/// the contender is either claimed under its new generation or skipped.
pub(super) fn is_retryable_sqlite_lock_error(error: &anyhow::Error) -> bool {
    let message = error
        .chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    message.contains("database is locked") || message.contains("database is busy")
}

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
const PUBLISH_MAINTENANCE_BATCH_SIZE: u64 = 16;

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

pub(super) fn claim_generation_filter(previous: Option<DateTime>) -> sea_orm::Condition {
    match previous {
        Some(generation) => {
            sea_orm::Condition::all().add(eh_download_queue::Column::StartedAt.eq(generation))
        }
        None => sea_orm::Condition::all().add(eh_download_queue::Column::StartedAt.is_null()),
    }
}

fn eh_job_claim_generation_filter(previous: Option<DateTime>) -> sea_orm::Condition {
    match previous {
        Some(generation) => {
            sea_orm::Condition::all().add(eh_gallery_jobs::Column::StartedAt.eq(generation))
        }
        None => sea_orm::Condition::all().add(eh_gallery_jobs::Column::StartedAt.is_null()),
    }
}

fn optional_eh_delivery_datetime_filter(
    column: eh_download_queue::Column,
    value: Option<DateTime>,
) -> SimpleExpr {
    match value {
        Some(value) => column.eq(value),
        None => column.is_null(),
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

pub(crate) fn eh_pending_telegraph_delivery_filter() -> SimpleExpr {
    Condition::all()
        .add(eh_download_queue::Column::Telegraph.eq(true))
        .add(eh_download_queue::Column::TelegraphSentAt.is_null())
        .into()
}

fn eh_delivery_is_ready_for_publish(
    delivery: &eh_download_queue::Model,
    job: &eh_gallery_jobs::Model,
    send_archive: bool,
) -> bool {
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
    if archive_required && job.cleanup_status != CLEANUP_STATUS_NONE {
        return false;
    }

    let archive_ready = job.status == JOB_STATUS_DOWNLOADED && job.zip_path.is_some();
    (archive_required && archive_ready)
        || (telegraph_required && telegraph_ready)
        || (!archive_required && !telegraph_required)
}

/// SQL equivalent of `eh_delivery_is_ready_for_publish`, scoped to delivery
/// rows joined with their shared jobs. This makes the claim selector read only
/// a publishable row rather than materializing due-but-blocked deliveries.
fn eh_delivery_ready_for_publish_filter(send_archive: bool) -> Condition {
    let archive_pending = Condition::all()
        .add(Expr::value(send_archive))
        .add(eh_download_queue::Column::ArchiveSentAt.is_null());
    let telegraph_pending = eh_pending_telegraph_delivery_filter();
    let telegraph_ready = Condition::all()
        .add(telegraph_pending.clone())
        .add(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_READY))
        .add(eh_gallery_jobs::Column::TelegraphUrl.is_not_null());
    let no_pending_surfaces = Condition::all()
        .add(archive_pending.clone().not())
        .add(telegraph_pending.clone().not());
    let archive_ready = Condition::all()
        .add(archive_pending.clone())
        .add(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADED))
        .add(eh_gallery_jobs::Column::ZipPath.is_not_null());
    Condition::all()
        .add(
            Condition::any()
                .add(telegraph_pending.not())
                .add(telegraph_ready.clone()),
        )
        .add(
            Condition::any()
                .add(telegraph_ready)
                .add(archive_ready)
                .add(no_pending_surfaces),
        )
        .add(
            Condition::any()
                .add(archive_pending.not())
                .add(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE)),
        )
}

fn eh_chat_has_no_publishing_delivery_filter() -> SimpleExpr {
    eh_download_queue::Column::ChatId.not_in_subquery(
        Query::select()
            .column(eh_download_queue::Column::ChatId)
            .from(eh_download_queue::Entity)
            .and_where(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
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
    #[cfg(test)]
    pub async fn count_pending_eh_downloads(&self) -> Result<u64> {
        eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.is_in([STATUS_WAITING, STATUS_PENDING]))
            .count(&self.db)
            .await
            .context("Failed to count pending eh downloads")
    }

    /// EH retry delay: two minutes doubled per retry, capped at 24 hours.
    pub fn backoff_delay_secs(retry_count: i32) -> i64 {
        120_i64
            .saturating_mul(2_i64.saturating_pow(retry_count.saturating_sub(1).max(0) as u32))
            .min(24 * 60 * 60)
    }
}
