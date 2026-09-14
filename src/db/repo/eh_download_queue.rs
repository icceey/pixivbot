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
use eh_client::{ArchiveArtifacts, ImageUploader};
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
const SQLITE_TRANSACTION_ATTEMPTS: usize = 3;

/// Lock-contention errors that a full transaction retry can resolve. The
/// CAS filters inside each transaction keep retries safe: a row changed by
/// the contender is either claimed under its new generation or skipped.
fn is_retryable_sqlite_lock_error(error: &anyhow::Error) -> bool {
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
const NO_CONFIGURED_EH_DELIVERY_PUBLISH_SURFACE_ERROR: &str =
    "No configured EH delivery publish surface";
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

fn claim_generation_filter(previous: Option<DateTime>) -> sea_orm::Condition {
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

#[cfg(test)]
tokio::task_local! {
    static EH_PUBLISH_CANDIDATE_GATE: Option<std::sync::Arc<EhPublishCandidateGate>>;
}

/// Test-only double-fence between the publish candidate SELECT and the
/// claiming UPDATE. The first suspended transaction blocks on `entered`
/// (candidate selected, snapshot taken) and then on `release`, letting the
/// test commit an interfering write between the two fences so the deferred
/// write upgrade deterministically hits SQLITE_BUSY_SNAPSHOT (517). The gate
/// fires once; retry attempts pass through so the fix can be observed.
#[cfg(test)]
pub(crate) struct EhPublishCandidateGate {
    entered: tokio::sync::Barrier,
    release: tokio::sync::Barrier,
    armed: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl EhPublishCandidateGate {
    pub(crate) fn new() -> Self {
        Self {
            entered: tokio::sync::Barrier::new(2),
            release: tokio::sync::Barrier::new(2),
            armed: std::sync::atomic::AtomicBool::new(true),
        }
    }
}

#[cfg(test)]
async fn await_eh_publish_candidate_gate() {
    let gate = EH_PUBLISH_CANDIDATE_GATE
        .try_with(|gate| gate.clone())
        .ok()
        .flatten();
    let Some(gate) = gate else {
        return;
    };
    if gate.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
        gate.entered.wait().await;
        gate.release.wait().await;
    }
}
fn eh_delivery_is_ready_for_publish(
    delivery: &eh_download_queue::Model,
    job: &eh_gallery_jobs::Model,
    send_archive: bool,
) -> bool {
    let archive_required = send_archive && delivery.archive_sent_at.is_none();
    let telegraph_required = delivery.telegraph && delivery.telegraph_sent_at.is_none();
    let has_requested_surface = send_archive
        || delivery.telegraph
        || delivery.archive_sent_at.is_some()
        || delivery.telegraph_sent_at.is_some();
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
        || (!archive_required && !telegraph_required && has_requested_surface)
}

/// SQL equivalent of `eh_delivery_is_ready_for_publish`, scoped to delivery
/// rows joined with their shared jobs. This makes the claim selector read only
/// a publishable row rather than materializing due-but-blocked deliveries.
fn eh_delivery_ready_for_publish_filter(send_archive: bool) -> Condition {
    let telegraph_ready = Condition::all()
        .add(eh_download_queue::Column::Telegraph.eq(true))
        .add(eh_download_queue::Column::TelegraphSentAt.is_null())
        .add(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_READY))
        .add(eh_gallery_jobs::Column::TelegraphUrl.is_not_null());

    let settled_requested_surfaces = if send_archive {
        Condition::all()
            .add(eh_download_queue::Column::ArchiveSentAt.is_not_null())
            .add(
                Condition::any()
                    .add(eh_download_queue::Column::Telegraph.eq(false))
                    .add(eh_download_queue::Column::TelegraphSentAt.is_not_null()),
            )
    } else {
        Condition::all()
            .add(
                Condition::any()
                    .add(eh_download_queue::Column::Telegraph.eq(false))
                    .add(eh_download_queue::Column::TelegraphSentAt.is_not_null()),
            )
            .add(
                Condition::any()
                    .add(eh_download_queue::Column::Telegraph.eq(true))
                    .add(eh_download_queue::Column::ArchiveSentAt.is_not_null())
                    .add(eh_download_queue::Column::TelegraphSentAt.is_not_null()),
            )
    };

    let telegraph_not_required_or_ready = Condition::any()
        .add(eh_download_queue::Column::Telegraph.eq(false))
        .add(eh_download_queue::Column::TelegraphSentAt.is_not_null())
        .add(telegraph_ready.clone());
    let mut ready = Condition::any()
        .add(telegraph_ready)
        .add(settled_requested_surfaces);
    if send_archive {
        ready = ready.add(
            Condition::all()
                .add(eh_download_queue::Column::ArchiveSentAt.is_null())
                .add(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADED))
                .add(eh_gallery_jobs::Column::ZipPath.is_not_null()),
        );
    }
    let ready = Condition::all()
        .add(telegraph_not_required_or_ready)
        .add(ready);
    if send_archive {
        ready.add(
            Condition::any()
                .add(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
                .add(eh_download_queue::Column::ArchiveSentAt.is_not_null()),
        )
    } else {
        ready
    }
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

    /// Delete an EH subscription and cancel/prune its deliveries while holding
    /// that subscription's chat lock.
    pub async fn delete_eh_subscription_and_cancel_queue(
        &self,
        subscription_id: i32,
        send_archive: bool,
    ) -> Result<()> {
        for _ in 0..SQLITE_TRANSACTION_ATTEMPTS {
            match self
                .delete_eh_subscription_and_cancel_queue_once(subscription_id, send_archive)
                .await
            {
                Ok(()) => return Ok(()),
                Err(error) => {
                    if !is_retryable_sqlite_lock_error(&error) {
                        return Err(error);
                    }
                }
            }
        }
        self.delete_eh_subscription_and_cancel_queue_once(subscription_id, send_archive)
            .await
    }

    async fn delete_eh_subscription_and_cancel_queue_once(
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
        for _ in 0..SQLITE_TRANSACTION_ATTEMPTS {
            match self
                .cancel_eh_subscription_queue_entries_under_chat_locks_once(
                    subscription_id,
                    send_archive,
                )
                .await
            {
                Ok(changed) => return Ok(changed),
                Err(error) => {
                    if !is_retryable_sqlite_lock_error(&error) {
                        return Err(error);
                    }
                }
            }
        }
        self.cancel_eh_subscription_queue_entries_under_chat_locks_once(
            subscription_id,
            send_archive,
        )
        .await
    }

    async fn cancel_eh_subscription_queue_entries_under_chat_locks_once(
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
                .set(eh_download_queue::ActiveModel {
                    subscription_ids: Set(remaining_ids),
                    telegraph_subscription_ids: Set(remaining_telegraph_ids),
                    status: Set(new_status.to_string()),
                    telegraph: Set(telegraph_still_required),
                    ..Default::default()
                })
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
        for _ in 0..SQLITE_TRANSACTION_ATTEMPTS {
            match self
                .eh_download_has_live_owner_or_cancel_once(id, expected_status, send_archive)
                .await
            {
                Ok(active) => return Ok(active),
                Err(error) => {
                    if !is_retryable_sqlite_lock_error(&error) {
                        return Err(error);
                    }
                }
            }
        }
        self.eh_download_has_live_owner_or_cancel_once(id, expected_status, send_archive)
            .await
    }

    async fn eh_download_has_live_owner_or_cancel_once(
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
                    .set(eh_download_queue::ActiveModel {
    status: Set(STATUS_CANCELED.to_string()),
    telegraph: Set(false),
    subscription_ids: Set(None),
    telegraph_subscription_ids: Set(None),
    telegraph_url: Set(None),
    archive_sent_at: Set(None),
    telegraph_sent_at: Set(None),
    telegraph_rewrite_data: Set(None),
    telegraph_rewrite_status: Set(None),
    telegraph_rewrite_after: Set(None),
    telegraph_rewrite_started_at: Set(None),
    telegraph_rewrite_next_retry_at: Set(None),
    telegraph_rewrite_retry_count: Set(0),
    telegraph_rewrite_error: Set(None),
    telegraph_rewritten_at: Set(None),
    ..Default::default()
})
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

    /// Reopen a cleaned rewrite-retained job only when this waiting delivery
    /// still needs its archive. The source claim is intentionally deferred to
    /// the policy-aware download selector after this transaction commits.
    async fn recover_eh_job_for_missing_archive_in_txn(
        &self,
        txn: &DatabaseTransaction,
        delivery: &eh_download_queue::Model,
        job: &eh_gallery_jobs::Model,
        send_archive: bool,
    ) -> Result<bool> {
        if !send_archive
            || delivery.archive_sent_at.is_some()
            || job.status != JOB_STATUS_DOWNLOADED
            || job.zip_path.is_some()
            || job.cleanup_status != CLEANUP_STATUS_NONE
            || job.background_download_status.is_some()
            || job.telegraph_status == TELEGRAPH_STATUS_UPLOADING
        {
            return Ok(false);
        }

        let delivery_still_waiting_for_archive = Expr::exists(
            Query::select()
                .expr(Expr::value(1))
                .from(eh_download_queue::Entity)
                .and_where(eh_download_queue::Column::Id.eq(delivery.id))
                .and_where(eh_download_queue::Column::JobId.eq(job.id))
                .and_where(eh_download_queue::Column::Status.eq(STATUS_WAITING))
                .and_where(eh_download_queue::Column::ArchiveSentAt.is_null())
                .and_where(optional_eh_delivery_datetime_filter(
                    eh_download_queue::Column::StartedAt,
                    delivery.started_at,
                ))
                .and_where(optional_eh_delivery_datetime_filter(
                    eh_download_queue::Column::NextRetryAt,
                    delivery.next_retry_at,
                ))
                .to_owned(),
        );
        let recovered = eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                status: Set(JOB_STATUS_PENDING.to_string()),
                error: Set(None),
                retry_count: Set(0_i32),
                next_retry_at: Set(None),
                completed_at: Set(None),
                background_download_status: Set(None),
                background_download_started_at: Set(None),
                background_download_next_retry_at: Set(None),
                background_download_attempt_count: Set(0_i32),
                background_download_error: Set(None),
                ..Default::default()
            })
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADED))
            .filter(eh_gallery_jobs::Column::ZipPath.is_null())
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(eh_gallery_jobs::Column::BackgroundDownloadStatus.is_null())
            .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(&job.telegraph_status))
            .filter(eh_gallery_jobs::Column::TelegraphRequired.eq(job.telegraph_required))
            .filter(eh_job_claim_generation_filter(job.started_at))
            .filter(delivery_still_waiting_for_archive)
            .exec(txn)
            .await
            .context("Failed to recover shared EH job with a missing archive")?;
        Ok(recovered.rows_affected == 1)
    }

    /// Claim the next due shared-gallery delivery that has at least one ready
    /// publish surface. Legacy rows without a `job_id` are deliberately never
    /// claimed by this runtime lane.
    pub async fn get_next_eh_delivery_for_publish(
        &self,
        send_archive: bool,
    ) -> Result<Option<EhDeliveryClaim>> {
        for _ in 0..SQLITE_TRANSACTION_ATTEMPTS {
            match self
                .get_next_eh_delivery_for_publish_once(send_archive)
                .await
            {
                Ok(outcome) => return Ok(outcome),
                Err(error) => {
                    if !is_retryable_sqlite_lock_error(&error) {
                        return Err(error);
                    }
                }
            }
        }
        self.get_next_eh_delivery_for_publish_once(send_archive)
            .await
    }

    async fn get_next_eh_delivery_for_publish_once(
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
            let mut committed_state_transition = false;
            if !send_archive {
                let no_surface_candidates = eh_download_queue::Entity::find()
                    .find_also_related(eh_gallery_jobs::Entity)
                    .filter(eh_download_queue::Column::Status.eq(STATUS_WAITING))
                    .filter(eh_download_queue::Column::JobId.is_not_null())
                    .filter(eh_download_queue::Column::Telegraph.eq(false))
                    .filter(eh_download_queue::Column::ArchiveSentAt.is_null())
                    .filter(eh_download_queue::Column::TelegraphSentAt.is_null())
                    .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
                    .filter(
                        eh_download_queue::Column::NextRetryAt
                            .is_null()
                            .or(eh_download_queue::Column::NextRetryAt.lte(now)),
                    )
                    .order_by(eh_download_queue::Column::CreatedAt, Order::Asc)
                    .order_by(eh_download_queue::Column::Id, Order::Asc)
                    .limit(PUBLISH_MAINTENANCE_BATCH_SIZE)
                    .all(&txn)
                    .await
                    .context("Failed to fetch surface-less shared EH deliveries for publish")?;
                for (delivery, job) in no_surface_candidates {
                    let Some(job) = job else {
                        continue;
                    };
                    let failed = eh_download_queue::Entity::update_many()
                        .set(eh_download_queue::ActiveModel {
                            status: Set(STATUS_FAILED.to_string()),
                            error: Set(Some(
                                NO_CONFIGURED_EH_DELIVERY_PUBLISH_SURFACE_ERROR.to_string(),
                            )),
                            completed_at: Set(Some(now)),
                            next_retry_at: Set(None),
                            ..Default::default()
                        })
                        .filter(eh_download_queue::Column::Id.eq(delivery.id))
                        .filter(eh_download_queue::Column::JobId.eq(job.id))
                        .filter(eh_download_queue::Column::Status.eq(STATUS_WAITING))
                        .filter(eh_download_queue::Column::Telegraph.eq(false))
                        .filter(eh_download_queue::Column::ArchiveSentAt.is_null())
                        .filter(eh_download_queue::Column::TelegraphSentAt.is_null())
                        .filter(eh_job_cleanup_is_none_filter(job.id))
                        .filter(claim_generation_filter(delivery.started_at))
                        .filter(
                            eh_download_queue::Column::NextRetryAt
                                .is_null()
                                .or(eh_download_queue::Column::NextRetryAt.lte(now)),
                        )
                        .exec(&txn)
                        .await
                        .context("Failed to fail shared EH delivery without a publish surface")?;
                    if failed.rows_affected == 1 {
                        self.recompute_eh_job_telegraph_requirement_in_txn(&txn, job.id)
                            .await?;
                        self.evaluate_eh_job_liveness_in_txn(&txn, job.id, send_archive)
                            .await?;
                        committed_state_transition = true;
                    }
                }
            }

            if send_archive {
                let missing_archive_candidates = eh_download_queue::Entity::find()
                    .find_also_related(eh_gallery_jobs::Entity)
                    .filter(eh_download_queue::Column::Status.eq(STATUS_WAITING))
                    .filter(eh_download_queue::Column::JobId.is_not_null())
                    .filter(eh_download_queue::Column::ArchiveSentAt.is_null())
                    .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADED))
                    .filter(eh_gallery_jobs::Column::ZipPath.is_null())
                    .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
                    .filter(eh_gallery_jobs::Column::BackgroundDownloadStatus.is_null())
                    .filter(eh_gallery_jobs::Column::TelegraphStatus.ne(TELEGRAPH_STATUS_UPLOADING))
                    .filter(
                        eh_download_queue::Column::NextRetryAt
                            .is_null()
                            .or(eh_download_queue::Column::NextRetryAt.lte(now)),
                    )
                    .order_by(eh_download_queue::Column::CreatedAt, Order::Asc)
                    .order_by(eh_download_queue::Column::Id, Order::Asc)
                    .limit(PUBLISH_MAINTENANCE_BATCH_SIZE)
                    .all(&txn)
                    .await
                    .context("Failed to fetch missing-archive shared EH deliveries for publish")?;
                for (delivery, job) in missing_archive_candidates {
                    let Some(job) = job else {
                        continue;
                    };
                    if self
                        .recover_eh_job_for_missing_archive_in_txn(
                            &txn,
                            &delivery,
                            &job,
                            send_archive,
                        )
                        .await?
                    {
                        committed_state_transition = true;
                    }
                }
            }

            let Some((delivery, Some(job))) = eh_download_queue::Entity::find()
                .find_also_related(eh_gallery_jobs::Entity)
                .filter(eh_download_queue::Column::Status.eq(STATUS_WAITING))
                .filter(eh_download_queue::Column::JobId.is_not_null())
                .filter(eh_chat_has_no_publishing_delivery_filter())
                .filter(
                    eh_download_queue::Column::NextRetryAt
                        .is_null()
                        .or(eh_download_queue::Column::NextRetryAt.lte(now)),
                )
                .filter(eh_delivery_ready_for_publish_filter(send_archive))
                .order_by(eh_download_queue::Column::CreatedAt, Order::Asc)
                .order_by(eh_download_queue::Column::Id, Order::Asc)
                .one(&txn)
                .await
                .context("Failed to fetch next ready shared EH delivery for publish")?
            else {
                return Ok((None, committed_state_transition));
            };
            #[cfg(test)]
            await_eh_publish_candidate_gate().await;
            debug_assert!(eh_delivery_is_ready_for_publish(
                &delivery,
                &job,
                send_archive
            ));

            let generation = next_claim_generation(now, delivery.started_at)?;
            let claimed = eh_download_queue::Entity::update_many()
                .set(eh_download_queue::ActiveModel {
                    status: Set(STATUS_PUBLISHING.to_string()),
                    started_at: Set(Some(generation)),
                    next_retry_at: Set(None),
                    ..Default::default()
                })
                .filter(eh_download_queue::Column::Id.eq(delivery.id))
                .filter(eh_download_queue::Column::JobId.eq(job.id))
                .filter(eh_download_queue::Column::Status.eq(STATUS_WAITING))
                .filter(claim_generation_filter(delivery.started_at))
                .filter(eh_chat_has_no_publishing_delivery_filter())
                .filter(
                    eh_download_queue::Column::NextRetryAt
                        .is_null()
                        .or(eh_download_queue::Column::NextRetryAt.lte(now)),
                )
                .exec(&txn)
                .await
                .context("Failed to atomically claim shared EH delivery for publish")?;
            if claimed.rows_affected == 0 {
                return Ok((None, committed_state_transition));
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
                .one(&txn)
                .await
                .context("Failed to reread shared EH job for delivery claim")?
                .context("Shared EH job changed before delivery claim readback")?;
            Ok((
                Some(EhDeliveryClaim {
                    delivery: claimed_delivery,
                    job: claimed_job,
                }),
                committed_state_transition,
            ))
        }
        .await;

        match result {
            Ok((Some(claim), _)) => {
                txn.commit()
                    .await
                    .context("Failed to commit shared EH delivery publish claim transaction")?;
                Ok(Some(claim))
            }
            Ok((None, true)) => txn
                .commit()
                .await
                .context("Failed to commit shared EH delivery state transition transaction")
                .map(|()| None),
            Ok((None, false)) => {
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
        send_archive: bool,
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
        if !eh_delivery_is_ready_for_publish(&delivery, &job, send_archive) {
            eh_download_queue::Entity::update_many()
                .set(eh_download_queue::ActiveModel {
                    status: Set(STATUS_WAITING.to_string()),
                    next_retry_at: Set(None),
                    ..Default::default()
                })
                .filter(eh_download_queue::Column::Id.eq(delivery.id))
                .filter(eh_download_queue::Column::JobId.eq(job.id))
                .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
                .exec(&self.db)
                .await
                .context("Failed to release unready shared EH publishing delivery")?;
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
            .set(eh_download_queue::ActiveModel {
                status: Set(STATUS_WAITING.to_string()),
                next_retry_at: Set(Some(retry_at)),
                ..Default::default()
            })
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
        for _ in 0..SQLITE_TRANSACTION_ATTEMPTS {
            match self
                .schedule_eh_delivery_retry_once(delivery_id, error, max_retry_count, send_archive)
                .await
            {
                Ok(outcome) => return Ok(outcome),
                Err(err) => {
                    if !is_retryable_sqlite_lock_error(&err) {
                        return Err(err);
                    }
                }
            }
        }
        self.schedule_eh_delivery_retry_once(delivery_id, error, max_retry_count, send_archive)
            .await
    }

    async fn schedule_eh_delivery_retry_once(
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
                .set(eh_download_queue::ActiveModel {
                    status: Set((if terminal {
                        STATUS_FAILED
                    } else {
                        STATUS_WAITING
                    })
                    .to_string()),
                    error: Set(Some(error.to_string())),
                    retry_count: Set(retry_count),
                    ..Default::default()
                })
                .filter(eh_download_queue::Column::Id.eq(delivery_id))
                .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
                .filter(job_id_filter(delivery.job_id));
            if terminal {
                update = update.set(eh_download_queue::ActiveModel {
                    completed_at: Set(Some(now)),
                    next_retry_at: Set(None),
                    ..Default::default()
                });
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
        for _ in 0..SQLITE_TRANSACTION_ATTEMPTS {
            match self.mark_eh_archive_delivery_sent_once(delivery_id).await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    if !is_retryable_sqlite_lock_error(&error) {
                        return Err(error);
                    }
                }
            }
        }
        self.mark_eh_archive_delivery_sent_once(delivery_id).await
    }

    async fn mark_eh_archive_delivery_sent_once(&self, delivery_id: i32) -> Result<()> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH archive sent-marker transaction")?;
        let result: Result<()> = async {
            let delivery = eh_download_queue::Entity::find_by_id(delivery_id)
                .one(&txn)
                .await
                .context("Failed to read shared EH delivery for archive sent marker")?
                .context("Shared EH delivery disappeared before archive sent marker")?;
            let sent_at = Local::now().naive_local();
            let marked = eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::ArchiveSentAt,
                    Expr::value(Some(sent_at)),
                )
                .filter(eh_download_queue::Column::Id.eq(delivery_id))
                .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
                .filter(eh_download_queue::Column::ArchiveSentAt.is_null())
                .exec(&txn)
                .await
                .context("Failed to mark shared EH archive delivery sent")?;
            if marked.rows_affected != 1 {
                anyhow::bail!(
                    "Cannot mark archive sent for shared EH delivery {}: publishing claim changed",
                    delivery_id
                );
            }
            record_eh_push_in_txn(
                &txn,
                delivery.chat_id,
                delivery.gid,
                EhPushSurface::Archive,
                sent_at,
            )
            .await
        }
        .await;
        match result {
            Ok(()) => txn
                .commit()
                .await
                .context("Failed to commit shared EH archive sent-marker transaction"),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back shared EH archive sent-marker transaction")?;
                Err(error)
            }
        }
    }

    /// Finish only this delivery after all of its enabled/requested surfaces
    /// have durable sent markers.
    pub async fn mark_eh_delivery_done(
        &self,
        delivery_id: i32,
        expected_job_id: i32,
        send_archive: bool,
    ) -> Result<eh_download_queue::Model> {
        for _ in 0..SQLITE_TRANSACTION_ATTEMPTS {
            match self
                .mark_eh_delivery_done_once(delivery_id, expected_job_id, send_archive)
                .await
            {
                Ok(delivery) => return Ok(delivery),
                Err(error) => {
                    if !is_retryable_sqlite_lock_error(&error) {
                        return Err(error);
                    }
                }
            }
        }
        self.mark_eh_delivery_done_once(delivery_id, expected_job_id, send_archive)
            .await
    }

    async fn mark_eh_delivery_done_once(
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
                .set(eh_download_queue::ActiveModel {
                    status: Set(STATUS_DONE.to_string()),
                    completed_at: Set(Some(Local::now().naive_local())),
                    next_retry_at: Set(None),
                    error: Set(None),
                    ..Default::default()
                })
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

    /// Delete cache artifact families which have no registered shared-job owner.
    ///
    /// Delivery compatibility columns are deliberately excluded from the
    /// keep-set: only a shared job owns its deterministic artifact family.
    /// Families carrying remote multipart state are Abort-gated before every
    /// local deletion and remain intact when that gate cannot be satisfied.
    /// Registered families whose path was lost are restored to durable cleanup
    /// ownership when liveness confirms that no consumer needs them.
    pub async fn cleanup_eh_cache_orphans(
        &self,
        cache_dir: &std::path::Path,
        abort_uploader: Option<&dyn ImageUploader>,
        send_archive: bool,
    ) -> Result<()> {
        if !cache_dir.exists() {
            return Ok(());
        }

        // Snapshot files before reading ownership: files created by a newly
        // enqueued job after this scan must never become deletion candidates.
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
        if artifact_families.is_empty() {
            return Ok(());
        }

        let mut owned_final_zips: HashSet<std::path::PathBuf> = HashSet::new();
        let mut job_artifacts: HashMap<_, _> = eh_gallery_jobs::Entity::find()
            .filter(
                eh_gallery_jobs::Column::Status
                    .ne(JOB_STATUS_RETIRED)
                    .or(eh_gallery_jobs::Column::CleanupStatus.ne(CLEANUP_STATUS_NONE))
                    .or(eh_gallery_jobs::Column::ZipPath.is_not_null())
                    .or(eh_gallery_jobs::Column::LegacyArtifactHandoff.is_not_null()),
            )
            .all(&self.db)
            .await
            .context("Failed to fetch shared EH jobs for cache cleanup")?
            .into_iter()
            .map(|job| (job.id, job))
            .collect();

        // Both current and legacy artifact names start with the gallery ID.
        // Only inspect clean retired jobs for galleries actually in the cache;
        // their deterministic paths still need protection against reactivation.
        let cached_gids: Vec<i64> = artifact_families
            .keys()
            .filter_map(|path| path.file_name()?.to_str()?.split_once('_')?.0.parse().ok())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        for gids in cached_gids.chunks(500) {
            // A retired job may reactivate after the first query. Look up all
            // matching jobs here so no owner can fall between status filters.
            job_artifacts.extend(
                eh_gallery_jobs::Entity::find()
                    .filter(eh_gallery_jobs::Column::Gid.is_in(gids.iter().copied()))
                    .all(&self.db)
                    .await
                    .context("Failed to fetch EH jobs with cached artifacts")?
                    .into_iter()
                    .map(|job| (job.id, job)),
            );
        }
        for job in job_artifacts.into_values() {
            if let Some(zip_path) = job.zip_path.as_deref() {
                owned_final_zips.insert(std::path::PathBuf::from(zip_path));
            }
            let job_zip = eh_gallery_job_artifact_path(cache_dir, &job);
            if job.zip_path.is_none()
                && job.cleanup_status == CLEANUP_STATUS_NONE
                && job.status != JOB_STATUS_DOWNLOADING
                && job.telegraph_status != TELEGRAPH_STATUS_UPLOADING
                && job.background_download_status.as_deref() != Some(BACKGROUND_STATUS_RUNNING)
                && job.legacy_artifact_handoff.is_none()
                && artifact_families.contains_key(&job_zip)
            {
                // Missing-ZIP recovery and cached-result reuse can clear the
                // path while leaving resume state. Use the existing liveness
                // policy to restore cleanup ownership atomically, without
                // taking over a newer attempt or changing active resume state.
                let txn = self
                    .db
                    .begin()
                    .await
                    .context("Failed to begin shared EH artifact ownership recovery")?;
                let restored = eh_gallery_jobs::Entity::update_many()
                    .set(eh_gallery_jobs::ActiveModel {
                        zip_path: Set(Some(job_zip.to_string_lossy().into_owned())),
                        ..Default::default()
                    })
                    .filter(eh_gallery_jobs::Column::Id.eq(job.id))
                    .filter(eh_gallery_jobs::Column::Status.eq(&job.status))
                    .filter(eh_gallery_jobs::Column::SourceGeneration.eq(job.source_generation))
                    .filter(eh_gallery_jobs::Column::ZipPath.is_null())
                    .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
                    .filter(eh_job_claim_generation_filter(job.started_at))
                    .filter(job.cleanup_started_at.map_or_else(
                        || eh_gallery_jobs::Column::CleanupStartedAt.is_null(),
                        |generation| eh_gallery_jobs::Column::CleanupStartedAt.eq(generation),
                    ))
                    .exec(&txn)
                    .await
                    .context("Failed to restore shared EH artifact cleanup ownership")?;
                if restored.rows_affected == 1
                    && self
                        .evaluate_eh_job_liveness_in_txn(&txn, job.id, send_archive)
                        .await?
                        .remove_archive_family
                {
                    txn.commit()
                        .await
                        .context("Failed to commit shared EH artifact ownership recovery")?;
                } else {
                    txn.rollback()
                        .await
                        .context("Failed to roll back shared EH artifact ownership recovery")?;
                }
            }
            // Even a retired job can be reactivated while Abort awaits the
            // network. Its source files belong to durable job cleanup instead.
            owned_final_zips.insert(job_zip);
            if job.legacy_artifact_handoff.is_some() {
                owned_final_zips.insert(legacy_eh_gallery_job_artifact_path(cache_dir, &job));
            }
        }

        let mut abort_failures = BTreeMap::<String, usize>::new();
        let mut families_without_abort_uploader = 0;
        for (final_zip, artifacts) in artifact_families {
            let result = if !owned_final_zips.contains(&final_zip) {
                if artifacts.uploads_dir().exists() {
                    let Some(uploader) = abort_uploader else {
                        families_without_abort_uploader += 1;
                        continue;
                    };
                    if let Err(error) = uploader.abort_upload_state(artifacts.uploads_dir()).await {
                        *abort_failures.entry(error.to_string()).or_default() += 1;
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
        if families_without_abort_uploader > 0 {
            warn!(
                families = families_without_abort_uploader,
                "Preserving EH orphan upload state because no S3/ipfS3 abort uploader is configured"
            );
        }
        if !abort_failures.is_empty() {
            warn!(
                failures = ?abort_failures,
                "Failed to abort EH orphan upload state; preserving local archive families"
            );
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
    use crate::db::repo::eh_gallery_jobs::EhDownloadQueue::{Background, Main};
    use crate::db::repo::eh_gallery_jobs::{
        EhGalleryVariant, CLEANUP_STATUS_FAILED, CLEANUP_STATUS_PENDING, JOB_STATUS_DOWNLOADED,
        JOB_STATUS_DOWNLOADING, JOB_STATUS_PENDING, JOB_STATUS_RETIRED,
        TELEGRAPH_STATUS_NOT_REQUIRED,
    };
    use crate::db::repo::tests_helpers;
    use crate::db::types::{TagFilter, TaskType};
    use chrono::{Duration, NaiveDate};
    use sea_orm::{sea_query::Expr, ActiveModelTrait, ConnectionTrait, DbBackend, Set, Statement};

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

        async fn abort_upload_state(
            &self,
            uploads_dir: &std::path::Path,
        ) -> Result<(), eh_client::UploadStateError> {
            self.aborts
                .lock()
                .unwrap()
                .push((uploads_dir.to_path_buf(), uploads_dir.exists()));
            if self.fail_abort {
                return Err(eh_client::UploadStateError::AbortHttp(503));
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        repo.mark_eh_job_downloaded(
            Main,
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
            .set(eh_gallery_jobs::ActiveModel {
                created_at: Set(created_at),
                next_retry_at: Set(next_retry_at),
                ..Default::default()
            })
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

    /// Deferred read→write transactions on a WAL database deterministically
    /// fail with SQLITE_BUSY_SNAPSHOT (517) when another connection commits
    /// between the candidate SELECT and the claiming UPDATE. The publish
    /// claim must absorb that error and retry the whole transaction.
    #[tokio::test]
    async fn publish_claim_retries_after_busy_snapshot_from_concurrent_commit() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, outside) = setup_wal_file_db(&dir).await;

        // Seed one ready archive delivery through the normal enqueue path.
        let delivery = repo
            .enqueue_eh_download(
                -100,
                90_001,
                "busy-snapshot",
                "Busy Snapshot",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        repo.mark_eh_job_downloaded(
            Main,
            download.id,
            download.started_at.unwrap(),
            1,
            "/tmp/busy-snapshot.zip",
            0,
        )
        .await
        .unwrap();

        let gate = std::sync::Arc::new(super::EhPublishCandidateGate::new());
        let claim_task = {
            let repo = Repo::new(repo.db().clone());
            let gate = gate.clone();
            tokio::spawn(async move {
                EH_PUBLISH_CANDIDATE_GATE
                    .scope(Some(gate), repo.get_next_eh_delivery_for_publish(true))
                    .await
            })
        };

        // Fence 1: the candidate SELECT completed; the claim transaction is
        // suspended before the claiming UPDATE with its WAL read snapshot.
        gate.entered.wait().await;
        // Interfering commit on the row the candidate scan read.
        sea_orm::sqlx::query("UPDATE eh_download_queue SET error = 'interfered' WHERE id = ?")
            .bind(delivery.id)
            .execute(&outside)
            .await
            .unwrap();
        // Fence 2: release the claim transaction; its UPDATE now upgrades a
        // stale WAL snapshot and hits SQLITE_BUSY_SNAPSHOT instead of waiting.
        gate.release.wait().await;

        let claim = claim_task
            .await
            .unwrap()
            .expect("claim must survive a busy snapshot and retry")
            .expect("claim should succeed after retry");
        assert_eq!(claim.delivery.id, delivery.id);
        assert_eq!(claim.delivery.status, STATUS_PUBLISHING);
    }

    async fn setup_wal_file_db(dir: &tempfile::TempDir) -> (Repo, sea_orm::sqlx::SqlitePool) {
        let path = dir.path().join("wal-contention.db");
        let url = format!("sqlite:{}?mode=rwc", path.display());
        let repo = {
            let db = crate::db::establish_connection(&url).await.unwrap();
            db.execute_unprepared("PRAGMA foreign_keys = ON")
                .await
                .unwrap();
            tests_helpers::create_schema(&db).await.unwrap();
            let mode: String = sea_orm::sqlx::query_scalar("PRAGMA journal_mode")
                .fetch_one(db.get_sqlite_connection_pool())
                .await
                .unwrap();
            assert_eq!(mode.to_ascii_lowercase(), "wal");
            Repo::new(db)
        };
        let outside = {
            use sea_orm::sqlx::ConnectOptions as _;
            use url::Url;
            let url: Url = url.parse().unwrap();
            let options = sea_orm::sqlx::sqlite::SqliteConnectOptions::from_url(&url)
                .unwrap()
                .busy_timeout(std::time::Duration::from_millis(200));
            sea_orm::sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(options)
                .await
                .unwrap()
        };
        (repo, outside)
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
    async fn cancel_one_shared_delivery_preserves_remaining_job_ownership() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let canceled = repo
            .enqueue_eh_subscription_download(
                -100, 123, 700, "tok", "Title", true, &variant, None, true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let sibling = repo
            .enqueue_eh_subscription_download(
                -200, 456, 700, "tok", "Title", false, &variant, None, true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(canceled.job_id, sibling.job_id);

        let job = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        repo.mark_eh_job_downloaded(
            Main,
            job.id,
            job.started_at.unwrap(),
            1,
            "/tmp/shared-cancel.zip",
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
        assert_eq!(job.cleanup_status, CLEANUP_STATUS_NONE);
        assert_eq!(job.zip_path.as_deref(), Some("/tmp/shared-cancel.zip"));
    }

    #[tokio::test]
    async fn cancellation_before_and_after_upload_claim_obeys_aggregate_boundary() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");

        let before = repo
            .enqueue_eh_subscription_download(
                -100, 123, 701, "before", "Before", true, &variant, None, true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let before_job = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        repo.mark_eh_job_downloaded(
            Main,
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

        assert!(!before_job.telegraph_required);
        assert!(repo.get_next_eh_job_for_upload().await.unwrap().is_none());
        let after = repo
            .enqueue_eh_subscription_download(
                -200, 456, 702, "after", "After", true, &variant, None, true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let after_job = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        repo.mark_eh_job_downloaded(
            Main,
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
    async fn liveness_schedules_cleanup_while_preserving_pending_rewrite_payload() {
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let job = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        repo.mark_eh_job_downloaded(
            Main,
            job.id,
            job.started_at.unwrap(),
            1,
            "/tmp/rewrite.zip",
            0,
        )
        .await
        .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_CANCELED))
            .filter(Column::Id.eq(delivery.id))
            .exec(repo.db())
            .await
            .unwrap();
        eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                telegraph_rewrite_data: Set(Some("payload".to_string())),
                telegraph_rewrite_status: Set(Some(
                    crate::db::repo::eh_gallery_jobs::TELEGRAPH_REWRITE_STATUS_PENDING.to_string(),
                )),
                ..Default::default()
            })
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
        assert!(decision.remove_archive_family);
        assert!(decision.preserve_rewrite_payload);
        assert_ne!(job.status, JOB_STATUS_RETIRED);
        assert_eq!(job.cleanup_status, CLEANUP_STATUS_PENDING);
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(sub_row.source, SOURCE_SUBSCRIPTION);
        assert_eq!(sub_row.subscription_ids.as_deref(), Some("123"));
        let other_sub_row = repo
            .enqueue_eh_subscription_download(
                -100,
                456,
                41,
                "tok",
                "Title",
                false,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let direct_row = repo
            .enqueue_eh_download(
                -100,
                42,
                "tok",
                "Title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert!(direct_row.subscription_ids.is_none());
        let done_row = repo
            .enqueue_eh_subscription_download(
                -100,
                123,
                43,
                "tok",
                "Title",
                false,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
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
        assert_eq!(
            Entity::find_by_id(other_sub_row.id)
                .one(&repo.db)
                .await
                .unwrap()
                .unwrap()
                .status,
            other_sub_row.status
        );
        assert_eq!(
            Entity::find_by_id(direct_row.id)
                .one(&repo.db)
                .await
                .unwrap()
                .unwrap()
                .status,
            direct_row.status
        );
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let merged = repo
            .enqueue_eh_subscription_download(
                -100,
                456,
                44,
                "tok2",
                "Title 2",
                false,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let merged = repo
            .enqueue_eh_subscription_download(
                -100,
                456,
                52,
                "tok2",
                "Title 2",
                false,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
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
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
        let stale = repo
            .enqueue_eh_subscription_download(
                -100,
                456,
                54,
                "tok2",
                "Title 2",
                false,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(stale.subscription_ids.as_deref(), Some("123,456"));
        assert_eq!(stale.telegraph_subscription_ids, None);

        Entity::update_many()
            .set(eh_download_queue::ActiveModel {
                telegraph: Set(true),
                telegraph_subscription_ids: Set(Some("456".to_string())),
                ..Default::default()
            })
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        Entity::update_many()
            .set(eh_download_queue::ActiveModel {
                status: Set(STATUS_DONE.to_string()),
                telegraph_url: Set(Some("https://telegra.ph/old".to_string())),
                ..Default::default()
            })
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
                "tok",
                "New",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(reenqueued.status, STATUS_WAITING);
        assert!(!reenqueued.telegraph);
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
                Column::SubscriptionIds,
                Expr::value(Some(format!("123,{}", live_sub.id))),
            )
            .filter(Column::Id.eq(row.id))
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        let claimed = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(rebound.id, delivery.id);
        assert_eq!(rebound.status, STATUS_WAITING);
        let replacement_job = job_for_delivery(&repo, &rebound).await;
        assert_ne!(replacement_job.id, claimed.id);
        assert_eq!(replacement_job.status, JOB_STATUS_PENDING);

        // The claimed worker may finish after its last consumer rebounded. Its
        // real completion is retained for the owned family and scheduled for
        // cleanup; it must never alter the replacement job.
        let settled = repo
            .mark_eh_job_downloaded(Main, claimed.id, claimed_started_at, 9999, "/tmp/40.zip", 0)
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        let claimed = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, model.job_id.unwrap());
        repo.mark_eh_job_downloaded(
            Main,
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
            .set(eh_download_queue::ActiveModel {
                status: Set(STATUS_WAITING.to_string()),
                telegraph: Set(true),
                next_retry_at: Set(None),
                ..Default::default()
            })
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
            .await
            .unwrap();
        eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                telegraph_required: Set(true),
                telegraph_status: Set(
                    crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_PENDING.to_string()
                ),
                ..Default::default()
            })
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
    async fn test_publish_selector_fails_delivery_without_configured_surface() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let delivery = repo
            .enqueue_eh_download(
                -100,
                912,
                "tok",
                "No surface",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        assert!(
            repo.get_next_eh_delivery_for_publish(false)
                .await
                .unwrap()
                .is_none(),
            "a consumerless delivery must not be claimed for publishing"
        );

        let failed = Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed.status, STATUS_FAILED);
        assert!(failed.completed_at.is_some());
        assert_eq!(
            failed.error.as_deref(),
            Some("No configured EH delivery publish surface")
        );
        assert_ne!(failed.status, STATUS_DONE);
        assert!(
            repo.claim_eh_download_job(Main, true)
                .await
                .unwrap()
                .is_none(),
            "a consumerless job must not remain download-claimable"
        );
        let job = job_for_delivery(&repo, &failed).await;
        assert_eq!(job.status, JOB_STATUS_RETIRED);
    }

    #[tokio::test]
    async fn test_publish_selector_claims_marker_complete_requested_surface() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let delivery = repo
            .enqueue_eh_download(
                -100,
                913,
                "tok",
                "Marker complete",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        repo.mark_eh_job_downloaded(
            Main,
            download.id,
            download.started_at.unwrap(),
            5000,
            "/tmp/913.zip",
            0,
        )
        .await
        .unwrap();
        let claim = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        repo.mark_eh_archive_delivery_sent(claim.delivery.id)
            .await
            .unwrap();
        Entity::update_many()
            .set(eh_download_queue::ActiveModel {
                status: Set(STATUS_WAITING.to_string()),
                next_retry_at: Set(None),
                ..Default::default()
            })
            .filter(Column::Id.eq(delivery.id))
            .exec(repo.db())
            .await
            .unwrap();

        let marker_complete = repo
            .get_next_eh_delivery_for_publish(false)
            .await
            .unwrap()
            .expect("a requested surface with a sent marker must be claimable to finish");
        assert_eq!(marker_complete.delivery.id, delivery.id);
        let done = repo
            .mark_eh_delivery_done(marker_complete.delivery.id, marker_complete.job.id, false)
            .await
            .unwrap();
        assert_eq!(done.status, STATUS_DONE);
    }

    #[tokio::test]
    async fn publish_selector_reaches_ready_rows_behind_blocked_backlog() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        for gid in 10_000..10_064 {
            repo.enqueue_eh_download(
                -gid,
                gid,
                "blocked",
                "Blocked",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        }

        let first_ready = repo
            .enqueue_eh_download(
                -20_000,
                20_000,
                "first-ready",
                "First ready",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                status: Set(JOB_STATUS_DOWNLOADED.to_string()),
                zip_path: Set(Some("/tmp/first-ready.zip".to_string())),
                ..Default::default()
            })
            .filter(eh_gallery_jobs::Column::Id.eq(first_ready.job_id.unwrap()))
            .exec(repo.db())
            .await
            .unwrap();

        let second_ready = repo
            .enqueue_eh_download(
                -20_001,
                20_001,
                "second-ready",
                "Second ready",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                status: Set(JOB_STATUS_DOWNLOADED.to_string()),
                zip_path: Set(Some("/tmp/second-ready.zip".to_string())),
                ..Default::default()
            })
            .filter(eh_gallery_jobs::Column::Id.eq(second_ready.job_id.unwrap()))
            .exec(repo.db())
            .await
            .unwrap();

        let first_claim = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first_claim.delivery.id, first_ready.id);
        repo.defer_eh_delivery_publish(first_claim.delivery.id, 60)
            .await
            .unwrap();

        let second_claim = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second_claim.delivery.id, second_ready.id);
    }

    #[tokio::test]
    async fn test_publish_selector_keeps_one_in_flight_delivery_per_chat() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let mut deliveries = Vec::new();
        for (chat_id, gid) in [(-100, 21_000), (-100, 21_001), (-200, 21_002)] {
            let delivery = repo
                .enqueue_eh_download(
                    chat_id,
                    gid,
                    "token",
                    "Ready",
                    false,
                    SOURCE_DIRECT,
                    &EhGalleryVariant::archive("1280x"),
                    None,
                    true,
                )
                .await
                .unwrap()
                .expect("delivery should be enqueued");
            eh_gallery_jobs::Entity::update_many()
                .set(eh_gallery_jobs::ActiveModel {
                    status: Set(JOB_STATUS_DOWNLOADED.to_string()),
                    zip_path: Set(Some(format!("/tmp/{gid}.zip"))),
                    ..Default::default()
                })
                .filter(eh_gallery_jobs::Column::Id.eq(delivery.job_id.unwrap()))
                .exec(repo.db())
                .await
                .unwrap();
            deliveries.push(delivery);
        }

        let first = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.delivery.id, deliveries[0].id);

        let second = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            second.delivery.id, deliveries[2].id,
            "another chat must receive the next publish slot"
        );

        repo.defer_eh_delivery_publish(first.delivery.id, 60)
            .await
            .unwrap();
        let third = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            third.delivery.id, deliveries[1].id,
            "the chat may claim its next delivery after the prior claim is released"
        );
    }

    #[tokio::test]
    async fn test_ready_telegraph_can_publish_after_cleanup_failure_without_archive() {
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        repo.mark_eh_job_downloaded(
            Main,
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
            None,
            false,
        )
        .await
        .unwrap();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::CleanupStatus,
                Expr::value(CLEANUP_STATUS_FAILED),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(download.id))
            .exec(repo.db())
            .await
            .unwrap();

        assert!(
            repo.get_next_eh_delivery_for_publish(true)
                .await
                .unwrap()
                .is_none(),
            "failed cleanup must still block a delivery that needs the archive"
        );

        let claim = repo
            .get_next_eh_delivery_for_publish(false)
            .await
            .unwrap()
            .expect("ready Telegraph must not depend on archive cleanup");
        assert_eq!(claim.delivery.id, delivery.id);
        assert!(
            repo.get_eh_delivery_publish_claim(delivery.id, false)
                .await
                .unwrap()
                .is_some(),
            "the chat-locked reread must preserve the Telegraph-only claim"
        );
    }

    #[tokio::test]
    async fn enqueue_merges_bound_and_unbound_deliveries_without_duplicates() {
        for bound in [true, false] {
            let repo = tests_helpers::setup_test_db().await.unwrap();
            let first = if bound {
                repo.enqueue_eh_download(
                    -100,
                    70,
                    "old",
                    "Old",
                    false,
                    SOURCE_SUBSCRIPTION,
                    &EhGalleryVariant::archive("1280x"),
                    None,
                    true,
                )
                .await
                .unwrap()
                .expect("delivery should be enqueued")
            } else {
                eh_download_queue::ActiveModel {
                    chat_id: Set(-100),
                    gid: Set(70),
                    token: Set("old".to_string()),
                    title: Set("Old".to_string()),
                    telegraph: Set(false),
                    source: Set(SOURCE_SUBSCRIPTION.to_string()),
                    status: Set(STATUS_PENDING.to_string()),
                    file_size: Set(0),
                    error: Set(None),
                    retry_count: Set(0),
                    created_at: Set(Local::now().naive_local()),
                    started_at: Set(None),
                    completed_at: Set(None),
                    ..Default::default()
                }
                .insert(repo.db())
                .await
                .unwrap()
            };
            let merged = repo
                .enqueue_eh_download(
                    -100,
                    70,
                    "new",
                    "New",
                    true,
                    SOURCE_DIRECT,
                    &EhGalleryVariant::archive("1280x"),
                    None,
                    true,
                )
                .await
                .unwrap()
                .expect("delivery should be enqueued");
            assert_eq!(merged.id, first.id);
            assert_eq!(merged.chat_id, -100);
            assert_eq!(merged.gid, 70);
            assert!(merged.telegraph);
            assert_eq!(merged.source, SOURCE_DIRECT);
            assert_eq!(merged.token, "new");
            assert_eq!(merged.title, "New");
            assert_eq!(job_for_delivery(&repo, &merged).await.token, "new");
            assert_eq!(
                Entity::find()
                    .filter(Column::ChatId.eq(-100))
                    .filter(Column::Gid.eq(70))
                    .count(repo.db())
                    .await
                    .unwrap(),
                1
            );
        }
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
                    None,
                    true,
                )
                .await
                .unwrap()
                .expect("delivery should be enqueued");
            let job = repo
                .claim_eh_download_job(Main, true)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(job.id, delivery.job_id.unwrap());
            repo.mark_eh_job_downloaded(
                Main,
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let claimed = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, model.job_id.unwrap());
        repo.mark_eh_job_downloaded(
            Main,
            claimed.id,
            claimed.started_at.unwrap(),
            5000,
            active_zip.to_str().unwrap(),
            0,
        )
        .await
        .unwrap();

        let uploader = RecordingAbortUploader::default();
        repo.cleanup_eh_cache_orphans(cache_dir, Some(&uploader), true)
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let claimed = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
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
        repo.cleanup_eh_cache_orphans(cache_dir, Some(&uploader), true)
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

        let retry = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        std::fs::write(artifacts.final_zip(), b"zip").unwrap();
        repo.mark_eh_job_downloaded(
            Main,
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

        repo.cleanup_eh_cache_orphans(cache_dir, Some(&uploader), true)
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

        repo.cleanup_eh_cache_orphans(cache_dir, None, true)
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

        repo.cleanup_eh_cache_orphans(cache_dir, Some(&uploader), true)
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
                "tok/en?",
                "First",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x/unsafe"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let second = repo
            .enqueue_eh_download(
                -200,
                89,
                "tok/en?",
                "Second",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("original"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let first_job = job_for_delivery(&repo, &first).await;
        let second_job = job_for_delivery(&repo, &second).await;
        let first_artifacts =
            ArchiveArtifacts::new(eh_gallery_job_artifact_path(cache_dir, &first_job));
        let second_artifacts =
            ArchiveArtifacts::new(eh_gallery_job_artifact_path(cache_dir, &second_job));
        assert_ne!(first_artifacts.final_zip(), second_artifacts.final_zip());
        for artifacts in [&first_artifacts, &second_artifacts] {
            assert_eq!(artifacts.final_zip().parent(), Some(cache_dir));
            std::fs::write(artifacts.final_zip(), b"zip").unwrap();
            std::fs::create_dir_all(artifacts.uploads_dir()).unwrap();
            std::fs::write(artifacts.uploads_dir().join("archive.json"), b"state").unwrap();
        }
        // An older job for this same gallery/variant must have its own artifact family.
        let mut orphan_job = first_job.clone();
        orphan_job.id += 1000;
        let orphan = ArchiveArtifacts::new(eh_gallery_job_artifact_path(cache_dir, &orphan_job));
        std::fs::write(orphan.final_zip(), b"zip").unwrap();
        std::fs::create_dir_all(orphan.uploads_dir()).unwrap();
        std::fs::write(orphan.uploads_dir().join("archive.json"), b"state").unwrap();
        let uploader = RecordingAbortUploader::default();

        repo.cleanup_eh_cache_orphans(cache_dir, Some(&uploader), true)
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        let claimed = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        repo.mark_eh_job_downloaded(
            Main,
            claimed.id,
            claimed.started_at.unwrap(),
            5000,
            "/tmp/20.zip",
            0,
        )
        .await
        .unwrap();
        eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                telegraph_url: Set(Some("https://telegra.ph/20".to_string())),
                telegraph_status: Set(
                    crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_READY.to_string()
                ),
                telegraph_required: Set(true),
                ..Default::default()
            })
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
                "tok",
                "New",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(rebound.job_id, model.job_id);
        assert!(rebound.archive_sent_at.is_some());
        assert!(rebound.telegraph_sent_at.is_some());
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let claimed = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, delivery.job_id.unwrap());

        // Defer with a long delay — the item should not be picked up
        repo.defer_eh_job_download(claimed.id, 3600).await.unwrap();

        let next = repo.claim_eh_download_job(Main, true).await.unwrap();
        assert!(
            next.is_none(),
            "deferred item should not be claimable before delay expires"
        );

        let reloaded = job_for_delivery(&repo, &delivery).await;
        assert_eq!(reloaded.status, JOB_STATUS_PENDING);
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let fast = repo
            .enqueue_eh_download(
                -100,
                41,
                "fast",
                "Fast",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        let claimed = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
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

        let next = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let recent_second = repo
            .enqueue_eh_download(
                -100,
                101,
                "tok",
                "Recent second",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let recent_newer = repo
            .enqueue_eh_download(
                -100,
                102,
                "tok",
                "Recent newer",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let cutoff_first = repo
            .enqueue_eh_download(
                -100,
                200,
                "tok",
                "Cutoff first",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let cutoff_second = repo
            .enqueue_eh_download(
                -100,
                201,
                "tok",
                "Cutoff second",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let old = repo
            .enqueue_eh_download(
                -100,
                300,
                "tok",
                "Old",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let future_retry = repo
            .enqueue_eh_download(
                -100,
                400,
                "tok",
                "Future retry",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
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
            let claimed = repo
                .claim_eh_download_job(Main, true)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(claimed.gid, expected_gid);
            assert_eq!(claimed.status, JOB_STATUS_DOWNLOADING);
        }

        assert!(repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .is_none());
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        let claimed = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, model.job_id.unwrap());
        let main_generation = claimed.started_at;
        let handed_off = repo
            .schedule_eh_job_background_download(claimed.id, JOB_STATUS_DOWNLOADING, "slow")
            .await
            .unwrap();
        assert_eq!(handed_off.started_at, main_generation);

        let bg_claim = repo
            .claim_eh_download_job(Background, true)
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
            .schedule_eh_job_download_retry(
                Background,
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
            .set(eh_gallery_jobs::ActiveModel {
                background_download_status: Set(Some(BACKGROUND_STATUS_RUNNING.to_string())),
                background_download_started_at: Set(Some(
                    Local::now().naive_local() - chrono::Duration::seconds(7200),
                )),
                ..Default::default()
            })
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
            .claim_eh_download_job(Background, true)
            .await
            .unwrap()
            .unwrap();
        assert!(bg_claim.started_at > background_generation);
        let done = repo
            .mark_eh_job_downloaded(
                Background,
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        let claimed = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                status: Set(crate::db::repo::eh_gallery_jobs::JOB_STATUS_FAILED.to_string()),
                started_at: Set(Some(generation)),
                background_download_status: Set(Some(BACKGROUND_STATUS_PENDING.to_string())),
                background_download_attempt_count: Set(5),
                ..Default::default()
            })
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(reenqueued.status, STATUS_WAITING);
        let reactivated_job = job_for_delivery(&repo, &reenqueued).await;
        assert_eq!(reactivated_job.status, JOB_STATUS_PENDING);
        assert!(reactivated_job.started_at.is_none());
        assert!(reactivated_job.background_download_status.is_none());
        assert_eq!(reactivated_job.background_download_attempt_count, 0);
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        let claimed = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        repo.schedule_eh_job_background_download(claimed.id, JOB_STATUS_DOWNLOADING, "slow")
            .await
            .unwrap();
        let bg_claim = repo
            .claim_eh_download_job(Background, true)
            .await
            .unwrap()
            .unwrap();

        let (failed, permanent) = repo
            .schedule_eh_job_download_retry(
                Background,
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
    async fn implicit_dead_subscription_cancel_schedules_cleanup_without_retiring_rewrite() {
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        repo.mark_eh_job_downloaded(
            Main,
            download.id,
            download.started_at.unwrap(),
            1,
            "/tmp/atomic-cancel.zip",
            0,
        )
        .await
        .unwrap();
        eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                telegraph_rewrite_data: Set(Some("{\"pages\":[]}".to_string())),
                telegraph_rewrite_status: Set(Some(TELEGRAPH_REWRITE_STATUS_PENDING.to_string())),
                ..Default::default()
            })
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
        assert_ne!(settled_job.status, JOB_STATUS_RETIRED);
        assert_eq!(settled_job.cleanup_status, CLEANUP_STATUS_PENDING);
        assert_eq!(
            settled_job.zip_path.as_deref(),
            Some("/tmp/atomic-cancel.zip")
        );
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let active_download_job = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let upload_download = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(active_upload.job_id, Some(upload_download.id));
        repo.mark_eh_job_downloaded(
            Main,
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let active_rewrite_job_id = active_rewrite.job_id.unwrap();
        let rewrite_started_at = chrono::Local::now().naive_local();
        eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                status: Set(JOB_STATUS_DOWNLOADED.to_string()),
                telegraph_rewrite_data: Set(Some("{\"pages\":[]}".to_string())),
                telegraph_rewrite_status: Set(Some(TELEGRAPH_REWRITE_STATUS_REWRITING.to_string())),
                telegraph_rewrite_started_at: Set(Some(rewrite_started_at)),
                ..Default::default()
            })
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let dirty_download = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(dirty_cleanup.job_id, Some(dirty_download.id));
        repo.mark_eh_job_downloaded(
            Main,
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
            .set(eh_gallery_jobs::ActiveModel {
                cleanup_status: Set(CLEANUP_STATUS_RUNNING.to_string()),
                cleanup_started_at: Set(Some(cleanup_started_at)),
                ..Default::default()
            })
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        let first_claim = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        let (retry, permanent) = repo
            .schedule_eh_job_download_retry(
                Main,
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
        let second_claim = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
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
        assert_eq!(released.status, JOB_STATUS_PENDING);
        assert!(released.background_download_status.is_none());
        assert_eq!(released.background_download_attempt_count, 0);

        let third_claim = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            third_claim.started_at,
            second_claim
                .started_at
                .map(|generation| generation + Duration::seconds(1))
        );
        let downloaded = repo
            .mark_eh_job_downloaded(
                Main,
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
    async fn terminal_download_failure_settles_unbound_job_without_failing_replacement() {
        for permanent_retry in [true, false] {
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
                    None,
                    true,
                )
                .await
                .unwrap()
                .expect("delivery should be enqueued");

            let claimed = repo
                .claim_eh_download_job(Main, true)
                .await
                .unwrap()
                .unwrap();
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
                    None,
                    true,
                )
                .await
                .unwrap()
                .expect("delivery should be enqueued");
            assert_eq!(reenqueued.id, delivery.id);
            assert_eq!(reenqueued.status, STATUS_WAITING);
            let rebound_job = job_for_delivery(&repo, &reenqueued).await;
            assert_ne!(rebound_job.id, claimed.id);
            assert_eq!(rebound_job.status, JOB_STATUS_PENDING);

            let failed = if permanent_retry {
                let (failed, permanent) = repo
                    .schedule_eh_job_download_retry(
                        Main,
                        claimed.id,
                        claimed.started_at.unwrap(),
                        "stale error",
                        0,
                    )
                    .await
                    .unwrap();
                assert!(permanent);
                failed
            } else {
                repo.fail_eh_job_for_archive_policy(Main, &claimed, "policy reject")
                    .await
                    .unwrap()
            };
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        let claimed = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        repo.schedule_eh_job_background_download(claimed.id, JOB_STATUS_DOWNLOADING, "slow")
            .await
            .unwrap();
        let background_claim = repo
            .claim_eh_download_job(Background, true)
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
            .fail_eh_job_for_archive_policy(Background, &background_claim, "policy reject")
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        let first_claim = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first_claim.id, delivery.job_id.unwrap());
        assert_eq!(first_claim.status, JOB_STATUS_DOWNLOADING);
        assert!(first_claim.started_at.is_some());

        repo.defer_eh_job_download(first_claim.id, 0).await.unwrap();
        let second_claim = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second_claim.status, JOB_STATUS_DOWNLOADING);
        assert_eq!(
            second_claim.started_at,
            first_claim
                .started_at
                .map(|generation| generation + Duration::seconds(1))
        );

        let err = repo
            .fail_eh_job_for_archive_policy(Main, &first_claim, "policy reject")
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        let first_main_claim = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        repo.schedule_eh_job_background_download(
            first_main_claim.id,
            JOB_STATUS_DOWNLOADING,
            "slow",
        )
        .await
        .unwrap();
        let first_background_claim = repo
            .claim_eh_download_job(Background, true)
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(reenqueued.status, STATUS_WAITING);
        let replacement_job = job_for_delivery(&repo, &reenqueued).await;
        assert_ne!(replacement_job.id, first_background_claim.id);
        let second_main_claim = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second_main_claim.id, replacement_job.id);
        repo.schedule_eh_job_background_download(
            second_main_claim.id,
            JOB_STATUS_DOWNLOADING,
            "slow",
        )
        .await
        .unwrap();
        let second_background_claim = repo
            .claim_eh_download_job(Background, true)
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
            .fail_eh_job_for_archive_policy(Background, &first_background_claim, "policy reject")
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
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
            repo.claim_eh_download_job(Main, true)
                .await
                .unwrap()
                .is_none(),
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        let download_claim = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(download_claim.id, delivery.job_id.unwrap());
        assert_eq!(download_claim.status, JOB_STATUS_DOWNLOADING);
        repo.mark_eh_job_downloaded(
            Main,
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
    async fn test_enqueue_preserves_a_same_job_markerless_publishing_claim() {
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        let claimed = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, model.job_id.unwrap());
        repo.mark_eh_job_downloaded(
            Main,
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

        // A same-job request merges owner demand, but must leave the live
        // publishing generation and binding untouched.
        let merged = repo
            .enqueue_eh_download(
                -100,
                65,
                "tok",
                "NewTitle",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        assert_eq!(merged.id, model.id);
        assert_eq!(merged.status, STATUS_PUBLISHING);
        assert!(
            merged.telegraph,
            "owner merge must preserve telegraph demand"
        );
        assert_eq!(merged.source, SOURCE_DIRECT);
        assert_eq!(merged.subscription_ids, None);
        assert_eq!(merged.telegraph_subscription_ids, None);
        assert_eq!(merged.token, "tok");
        assert_eq!(merged.title, "Title");
        assert_eq!(merged.job_id, model.job_id);
        assert_eq!(merged.started_at, publish_claim.delivery.started_at);
        assert!(merged.archive_sent_at.is_none());
        assert!(merged.telegraph_sent_at.is_none());
        let claimed_job = job_for_delivery(&repo, &merged).await;
        assert_eq!(claimed_job.status, JOB_STATUS_DOWNLOADED);
        assert!(claimed_job.telegraph_required);
    }

    #[tokio::test]
    async fn test_direct_enqueue_restarts_a_marker_bearing_publishing_delivery_wave() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(
                -100,
                66,
                "tok",
                "Published title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let claimed = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        repo.mark_eh_job_downloaded(
            Main,
            claimed.id,
            claimed.started_at.unwrap(),
            5000,
            "/tmp/66.zip",
            0,
        )
        .await
        .unwrap();
        let _publish_claim = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        repo.mark_eh_archive_delivery_sent(model.id).await.unwrap();

        let merged = repo
            .enqueue_eh_download(
                -100,
                66,
                "tok",
                "Requested original",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("original"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        assert_eq!(merged.status, STATUS_WAITING);
        assert_ne!(merged.job_id, model.job_id);
        assert!(merged.started_at.is_none());
        assert!(merged.archive_sent_at.is_none());
        assert!(merged.telegraph_sent_at.is_none());
        assert!(merged.telegraph);
        assert_eq!(merged.token, "tok");
        assert_eq!(merged.title, "Requested original");
        assert_eq!(merged.retry_count, 0);
        assert!(merged.completed_at.is_none());
        assert!(merged.next_retry_at.is_none());
        let requested_job = eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::Gid.eq(66_i64))
            .filter(eh_gallery_jobs::Column::Resolution.eq("original"))
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requested_job.id, merged.job_id.unwrap());
        assert_eq!(requested_job.status, JOB_STATUS_PENDING);
        assert_eq!(
            eh_gallery_jobs::Entity::find_by_id(model.job_id.unwrap())
                .one(repo.db())
                .await
                .unwrap()
                .unwrap()
                .status,
            JOB_STATUS_RETIRED
        );
        assert_eq!(
            repo.claim_eh_download_job(Main, true)
                .await
                .unwrap()
                .unwrap()
                .id,
            requested_job.id
        );
    }

    #[tokio::test]
    async fn disable_telegraph_without_token_downgrades_only_unuploaded_jobs() {
        for phase in [
            JOB_STATUS_PENDING,
            JOB_STATUS_DOWNLOADED,
            TELEGRAPH_STATUS_READY,
        ] {
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
                    None,
                    true,
                )
                .await
                .unwrap()
                .expect("delivery should be enqueued");
            let initial_job = job_for_delivery(&repo, &model).await;
            assert_eq!(initial_job.status, JOB_STATUS_PENDING);
            assert_eq!(initial_job.telegraph_status, TELEGRAPH_STATUS_NOT_REQUIRED);
            if phase != JOB_STATUS_PENDING {
                let download = repo
                    .claim_eh_download_job(Main, true)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(download.id, initial_job.id);
                repo.mark_eh_job_downloaded(
                    Main,
                    download.id,
                    download.started_at.unwrap(),
                    5000,
                    "/tmp/91.zip",
                    0,
                )
                .await
                .unwrap();
            }
            let uploaded = phase == TELEGRAPH_STATUS_READY;
            if uploaded {
                let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
                assert_eq!(upload.id, initial_job.id);
                repo.mark_eh_job_telegraph_ready(
                    upload.id,
                    upload.started_at.unwrap(),
                    "https://example.com/gallery",
                    None,
                    None,
                    true,
                )
                .await
                .unwrap();
            }
            assert_eq!(
                repo.disable_eh_telegraph_for_unuploaded_jobs()
                    .await
                    .unwrap(),
                u64::from(!uploaded),
                "{phase}"
            );
            let delivery = Entity::find_by_id(model.id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(delivery.status, STATUS_WAITING);
            assert_eq!(delivery.telegraph, uploaded, "{phase}");
            assert!(delivery.telegraph_url.is_none());
            assert!(delivery.telegraph_subscription_ids.is_none());
            let job = job_for_delivery(&repo, &delivery).await;
            if uploaded {
                assert_eq!(job.telegraph_status, TELEGRAPH_STATUS_READY);
                assert_eq!(
                    job.telegraph_url.as_deref(),
                    Some("https://example.com/gallery")
                );
            } else {
                assert!(!job.telegraph_required);
                assert_eq!(job.telegraph_status, TELEGRAPH_STATUS_NOT_REQUIRED);
            }
            if phase == JOB_STATUS_PENDING {
                let download = repo
                    .claim_eh_download_job(Main, true)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(download.id, job.id);
                repo.mark_eh_job_downloaded(
                    Main,
                    download.id,
                    download.started_at.unwrap(),
                    5000,
                    "/tmp/91.zip",
                    0,
                )
                .await
                .unwrap();
                assert!(
                    repo.get_next_eh_job_for_upload().await.unwrap().is_none(),
                    "downgraded work must not become upload-claimable after download"
                );
            }
        }
    }

    #[tokio::test]
    async fn disable_telegraph_without_token_preserves_terminal_delivery_history() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let mut history = Vec::new();
        for (gid, status, error, url) in [
            (93, STATUS_FAILED, Some("old failure"), None),
            (94, STATUS_CANCELED, None, None),
            (95, STATUS_DONE, None, Some("https://example.com/old")),
        ] {
            let model = repo
                .enqueue_eh_download(
                    -100,
                    gid,
                    "tok",
                    "Title",
                    true,
                    SOURCE_DIRECT,
                    &EhGalleryVariant::archive("1280x"),
                    None,
                    true,
                )
                .await
                .unwrap()
                .expect("delivery should be enqueued");
            Entity::update_many()
                .set(eh_download_queue::ActiveModel {
                    status: Set(status.to_string()),
                    error: Set(error.map(str::to_string)),
                    telegraph_url: Set(url.map(str::to_string)),
                    ..Default::default()
                })
                .filter(Column::Id.eq(model.id))
                .exec(repo.db())
                .await
                .unwrap();
            history.push((model.id, status, error, url));
        }
        assert_eq!(
            repo.disable_eh_telegraph_for_unuploaded_jobs()
                .await
                .unwrap(),
            0
        );
        for (id, status, error, url) in history {
            let row = Entity::find_by_id(id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.status, status);
            assert!(
                row.telegraph,
                "terminal delivery history is not startup work: {status}"
            );
            assert_eq!(row.error.as_deref(), error);
            assert_eq!(row.telegraph_url.as_deref(), url);
        }
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
                None,
                true,
            ),
            repo.enqueue_eh_download(
                -200,
                700,
                "token",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            ),
            repo.enqueue_eh_download(
                -100,
                700,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true
            ),
        );
        let first = first.unwrap().expect("delivery should be enqueued");
        let second = second.unwrap().expect("delivery should be enqueued");
        let same_chat_upgrade = same_chat_upgrade
            .unwrap()
            .expect("delivery should be enqueued");

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
    async fn enqueue_isolates_variants_and_rebinds_direct_upgrade_before_and_after_markers() {
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_ne!(marker_bound.job_id, Some(direct_job_id));
        assert!(marker_bound.archive_sent_at.is_none());
        assert!(marker_bound.telegraph_sent_at.is_none());

        let requested = eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::Gid.eq(701))
            .filter(eh_gallery_jobs::Column::Resolution.eq("780x"))
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requested.id, marker_bound.job_id.unwrap());
        assert_eq!(requested.status, JOB_STATUS_PENDING);
        assert_eq!(
            eh_gallery_jobs::Entity::find_by_id(direct_job_id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap()
                .status,
            JOB_STATUS_RETIRED
        );
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let job_id = initial.job_id.unwrap();
        Entity::update_many()
            .set(eh_download_queue::ActiveModel {
                job_id: Set(None),
                status: Set(STATUS_CANCELED.to_string()),
                ..Default::default()
            })
            .filter(Column::Id.eq(initial.id))
            .exec(repo.db())
            .await
            .unwrap();
        eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                status: Set(JOB_STATUS_RETIRED.to_string()),
                cleanup_status: Set(CLEANUP_STATUS_FAILED.to_string()),
                zip_path: Set(Some("cache/702.zip".to_string())),
                cleanup_error: Set(Some("disk unavailable".to_string())),
                ..Default::default()
            })
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
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
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        repo.mark_eh_job_downloaded(
            Main,
            download.id,
            download.started_at.unwrap(),
            5000,
            "/tmp/703.zip",
            0,
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
            .get_next_eh_delivery_for_publish(true)
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
