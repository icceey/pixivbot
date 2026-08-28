use super::Repo;
use crate::config::EhentaiConfig;
use crate::db::entities::{eh_download_queue, eh_gallery_jobs, eh_gallery_push_ledger};
use crate::db::repo::eh_download_queue::{
    merge_subscription_ids, merge_telegraph_subscription_ids, EH_CHAT_LOCKS, SOURCE_DIRECT,
    SOURCE_SUBSCRIPTION,
};
use crate::db::repo::eh_gallery_push_ledger::{record_eh_push_in_txn, EhPushSurface};
use crate::db::repo::eh_gallery_results::{
    find_eh_gallery_result_in_txn, upsert_eh_gallery_result_in_txn,
};
use anyhow::{Context, Result};
use chrono::{Local, Timelike};
use sea_orm::prelude::DateTime;
use sea_orm::sea_query::{Expr, Query, SimpleExpr};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseTransaction, EntityTrait, Order, QueryFilter,
    QueryOrder, QueryTrait, Set, TransactionTrait,
};

pub const DOWNLOAD_MODE_ARCHIVE: &str = "archive";
pub const DOWNLOAD_MODE_IMAGES: &str = "images";
pub const DOWNLOAD_MODE_LEGACY: &str = "legacy";

pub const JOB_STATUS_PENDING: &str = "pending";
pub const JOB_STATUS_DOWNLOADING: &str = "downloading";
pub const JOB_STATUS_DOWNLOADED: &str = "downloaded";
pub const JOB_STATUS_FAILED: &str = "failed";
pub const JOB_STATUS_RETIRED: &str = "retired";

pub const TELEGRAPH_STATUS_NOT_REQUIRED: &str = "not_required";
pub const TELEGRAPH_STATUS_PENDING: &str = "pending";
pub const TELEGRAPH_STATUS_UPLOADING: &str = "uploading";
pub const TELEGRAPH_STATUS_READY: &str = "ready";
pub const TELEGRAPH_STATUS_FAILED: &str = "failed";

pub const BACKGROUND_STATUS_PENDING: &str = "pending";
pub const BACKGROUND_STATUS_RUNNING: &str = "running";

pub const TELEGRAPH_REWRITE_STATUS_PENDING: &str = "pending";
pub const TELEGRAPH_REWRITE_STATUS_REWRITING: &str = "rewriting";
pub const TELEGRAPH_REWRITE_STATUS_FAILED: &str = "failed";

pub const CLEANUP_STATUS_NONE: &str = "none";
pub const CLEANUP_STATUS_PENDING: &str = "pending";
pub const CLEANUP_STATUS_RUNNING: &str = "running";
pub const CLEANUP_STATUS_FAILED: &str = "failed";

pub const DELIVERY_STATUS_WAITING: &str = "waiting";
pub const DELIVERY_STATUS_PUBLISHING: &str = "publishing";
pub const DELIVERY_STATUS_DONE: &str = "done";
pub const DELIVERY_STATUS_FAILED: &str = "failed";
pub const DELIVERY_STATUS_CANCELED: &str = "canceled";

const MAX_ENQUEUE_TRANSACTION_ATTEMPTS: usize = 3;
const MAIN_DOWNLOAD_RECENT_WINDOW_HOURS: i64 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EhGalleryVariant {
    pub download_mode: String,
    pub resolution: String,
}

impl EhGalleryVariant {
    pub fn archive(resolution: impl Into<String>) -> Self {
        Self {
            download_mode: DOWNLOAD_MODE_ARCHIVE.to_string(),
            resolution: resolution.into(),
        }
    }

    pub fn images() -> Self {
        Self {
            download_mode: DOWNLOAD_MODE_IMAGES.to_string(),
            resolution: String::new(),
        }
    }

    pub fn for_request(is_logged_in: bool, source: &str, config: &EhentaiConfig) -> Self {
        if !is_logged_in {
            return Self::images();
        }

        if source == SOURCE_DIRECT {
            Self::archive(config.download_resolution.clone())
        } else {
            Self::archive(config.subscription_resolution.clone())
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn try_apply_cached_eh_result_in_txn(
    txn: &DatabaseTransaction,
    job_id: i32,
    send_archive: bool,
) -> Result<bool> {
    let Some(job) = eh_gallery_jobs::Entity::find_by_id(job_id)
        .one(txn)
        .await
        .context("Failed to select shared EH gallery job for cached-result reuse")?
    else {
        return Ok(false);
    };
    let Some(fingerprint) = job.source_fingerprint.as_deref() else {
        return Ok(false);
    };
    let variant = EhGalleryVariant {
        download_mode: job.download_mode.clone(),
        resolution: job.resolution.clone(),
    };
    let Some(cached_result) =
        find_eh_gallery_result_in_txn(txn, job.gid, &job.token, &variant).await?
    else {
        return Ok(false);
    };
    if cached_result.source_fingerprint != fingerprint
        || job.cleanup_status != CLEANUP_STATUS_NONE
        || job.telegraph_status == TELEGRAPH_STATUS_UPLOADING
    {
        return Ok(false);
    }

    let has_archive_demand =
        send_archive && has_active_eh_archive_delivery_in_txn(txn, job.id).await?;
    let make_zipless_ready = !has_archive_demand
        && job.status == JOB_STATUS_PENDING
        && job.background_download_status.is_none();
    let now = Local::now().naive_local();
    let mut update = eh_gallery_jobs::Entity::update_many()
        .col_expr(
            eh_gallery_jobs::Column::TelegraphStatus,
            Expr::value(TELEGRAPH_STATUS_READY),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphUrl,
            Expr::value(Some(cached_result.telegraph_url)),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRewriteData,
            Expr::value(cached_result.telegraph_rewrite_data),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRewriteStatus,
            Expr::value(None::<String>),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRewriteAfter,
            Expr::value(None::<DateTime>),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRewriteStartedAt,
            Expr::value(None::<DateTime>),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt,
            Expr::value(None::<DateTime>),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRewriteRetryCount,
            Expr::value(0_i32),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRewriteError,
            Expr::value(None::<String>),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRewrittenAt,
            Expr::value(None::<DateTime>),
        )
        .filter(eh_gallery_jobs::Column::Id.eq(job.id))
        .filter(eh_gallery_jobs::Column::Status.eq(&job.status))
        .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(&job.telegraph_status))
        .filter(eh_gallery_jobs::Column::SourceFingerprint.eq(fingerprint))
        .filter(eh_gallery_jobs::Column::CleanupStatus.eq(&job.cleanup_status))
        .filter(job_claim_generation_filter(job.started_at))
        .filter(cleanup_claim_generation_filter(job.cleanup_started_at))
        .filter(optional_job_string_filter(
            eh_gallery_jobs::Column::BackgroundDownloadStatus,
            job.background_download_status.as_deref(),
        ))
        .filter(optional_job_datetime_filter(
            eh_gallery_jobs::Column::BackgroundDownloadStartedAt,
            job.background_download_started_at,
        ))
        .filter(optional_job_string_filter(
            eh_gallery_jobs::Column::TelegraphRewriteData,
            job.telegraph_rewrite_data.as_deref(),
        ))
        .filter(optional_job_string_filter(
            eh_gallery_jobs::Column::TelegraphRewriteStatus,
            job.telegraph_rewrite_status.as_deref(),
        ))
        .filter(optional_job_datetime_filter(
            eh_gallery_jobs::Column::TelegraphRewriteAfter,
            job.telegraph_rewrite_after,
        ))
        .filter(optional_job_datetime_filter(
            eh_gallery_jobs::Column::TelegraphRewriteStartedAt,
            job.telegraph_rewrite_started_at,
        ))
        .filter(optional_job_datetime_filter(
            eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt,
            job.telegraph_rewrite_next_retry_at,
        ))
        .filter(
            eh_gallery_jobs::Column::TelegraphRewriteRetryCount
                .eq(job.telegraph_rewrite_retry_count),
        )
        .filter(optional_job_string_filter(
            eh_gallery_jobs::Column::TelegraphRewriteError,
            job.telegraph_rewrite_error.as_deref(),
        ))
        .filter(optional_job_datetime_filter(
            eh_gallery_jobs::Column::TelegraphRewrittenAt,
            job.telegraph_rewritten_at,
        ));
    if make_zipless_ready {
        update = update
            .col_expr(
                eh_gallery_jobs::Column::Status,
                Expr::value(JOB_STATUS_DOWNLOADED),
            )
            .col_expr(
                eh_gallery_jobs::Column::ZipPath,
                Expr::value(None::<String>),
            )
            .col_expr(eh_gallery_jobs::Column::FileSize, Expr::value(0_i64))
            .col_expr(eh_gallery_jobs::Column::GpCost, Expr::value(0_i64))
            .col_expr(eh_gallery_jobs::Column::CompletedAt, Expr::value(Some(now)));
    }
    let updated = update
        .exec(txn)
        .await
        .context("Failed to apply cached EH gallery result")?;
    Ok(updated.rows_affected == 1)
}

pub(crate) fn eh_gallery_job_artifact_filename(job: &eh_gallery_jobs::Model) -> String {
    let resolution = if job.resolution.is_empty() {
        "none"
    } else {
        &job.resolution
    };
    format!(
        "{}_{}_j{}_{}_{}.zip",
        sanitize_artifact_component(&job.gid.to_string()),
        sanitize_artifact_component(&job.token),
        sanitize_artifact_component(&job.id.to_string()),
        sanitize_artifact_component(&job.download_mode),
        sanitize_artifact_component(resolution),
    )
}

pub(crate) fn eh_gallery_job_artifact_path(
    cache_dir: &std::path::Path,
    job: &eh_gallery_jobs::Model,
) -> std::path::PathBuf {
    cache_dir.join(eh_gallery_job_artifact_filename(job))
}

fn sanitize_artifact_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EhJobCleanupDecision {
    pub job_id: i32,
    pub zip_path: Option<String>,
    pub retire: bool,
    pub remove_archive_family: bool,
    pub preserve_rewrite_payload: bool,
}

/// The durable result of a claimed shared-artifact cleanup generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EhCleanupFinalizeOutcome {
    /// A delivery arrived while the retired generation was being cleaned; it
    /// now owns a fresh pending download generation.
    ReactivatedPending,
    /// Active deliveries arrived during cleanup but require neither an archive
    /// nor a new Telegraph page, so the shared reusable result remains ready
    /// for publishing without another source claim.
    FinalizedWithoutSourceWork,
    /// No consumer remains and all rewrite work is terminal.
    CleanRetired,
    /// The archive family was removed, but delayed Telegraph rewrite state is
    /// still owned by the job.
    RetainedForRewrite,
    /// The cleanup generation was replaced before its executor finalized.
    Stale,
}

/// Counts returned by the single startup reset boundary.  Each transition is
/// idempotent, so a second call without new crashes returns the default value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EhStaleResetCounts {
    pub downloads: u64,
    pub uploads: u64,
    pub backgrounds: u64,
    pub rewrites: u64,
    pub cleanups: u64,
    pub deliveries: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EhFailedTelegraphDelivery {
    pub delivery_id: i32,
    pub chat_id: i64,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EhJobUploadFailureOutcome {
    RetryScheduled(eh_gallery_jobs::Model),
    Stale,
    Terminal {
        job: eh_gallery_jobs::Model,
        deliveries: Vec<EhFailedTelegraphDelivery>,
    },
}

/// The durable result of a missing shared-ZIP reset attempted by a publish
/// worker. Exactly one racing delivery observes `Reset` or `Exhausted`; every
/// other caller sees `Stale` and must leave the shared state alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EhMissingZipResetOutcome {
    /// The shared job was put back into the pending redownload queue with
    /// backoff and an incremented retry count.
    Reset,
    /// The retry limit was reached: the job failed terminally, its active
    /// deliveries failed once, and no further download is scheduled.
    Exhausted,
    /// The `(downloaded, expected generation, expected path)` CAS did not
    /// match, so this caller lost the race and changed nothing.
    Stale,
}

struct EhEnqueueRequest<'a> {
    chat_id: i64,
    gid: i64,
    token: &'a str,
    title: &'a str,
    fingerprint: Option<String>,
    telegraph: bool,
    send_archive: bool,
    source: &'a str,
    subscription_id: Option<i32>,
    variant: &'a EhGalleryVariant,
}

struct EhGalleryJobResolution {
    job: eh_gallery_jobs::Model,
    started_new_generation: bool,
}

struct EhDeliveryResolution {
    old_job_id: Option<i32>,
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct EhEnqueueChatLockHook {
    pub(crate) waiting: std::sync::Arc<tokio::sync::Notify>,
    pub(crate) acquired: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(test)]
tokio::task_local! {
    pub(crate) static EH_ENQUEUE_CHAT_LOCK_HOOK: EhEnqueueChatLockHook;
}

// Test-only failpoint for the durable liveness write. Keeping this task-local
// prevents parallel tests from observing another test's injected failure.
#[cfg(test)]
tokio::task_local! {
    pub(crate) static EH_JOB_LIVENESS_UPDATE_FAILURE: std::sync::Arc<std::sync::atomic::AtomicBool>;
}

#[cfg(test)]
fn maybe_fail_eh_job_liveness_update() -> Result<()> {
    if EH_JOB_LIVENESS_UPDATE_FAILURE
        .try_with(|failure| failure.swap(false, std::sync::atomic::Ordering::SeqCst))
        .unwrap_or(false)
    {
        anyhow::bail!("injected shared EH job liveness update failure");
    }
    Ok(())
}

#[cfg(not(test))]
fn maybe_fail_eh_job_liveness_update() -> Result<()> {
    Ok(())
}

impl Repo {
    /// Enqueue a direct EH request and atomically bind its delivery to the
    /// canonical shared-gallery job for the requested variant.
    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_eh_download(
        &self,
        chat_id: i64,
        gid: i64,
        token: &str,
        title: &str,
        telegraph: bool,
        source: &str,
        variant: &EhGalleryVariant,
        fingerprint: Option<&str>,
        send_archive: bool,
    ) -> Result<Option<eh_download_queue::Model>> {
        self.enqueue_eh_download_request(EhEnqueueRequest {
            chat_id,
            gid,
            token,
            title,
            fingerprint: fingerprint.map(str::to_owned),
            telegraph,
            send_archive,
            source,
            subscription_id: None,
            variant,
        })
        .await
    }

    /// Enqueue a scheduler-created delivery and retain subscription ownership
    /// while binding it to the requested shared-gallery job variant.
    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_eh_subscription_download(
        &self,
        chat_id: i64,
        subscription_id: i32,
        gid: i64,
        token: &str,
        title: &str,
        telegraph: bool,
        variant: &EhGalleryVariant,
        fingerprint: Option<&str>,
        send_archive: bool,
    ) -> Result<Option<eh_download_queue::Model>> {
        self.enqueue_eh_download_request(EhEnqueueRequest {
            chat_id,
            gid,
            token,
            title,
            fingerprint: fingerprint.map(str::to_owned),
            telegraph,
            send_archive,
            source: SOURCE_SUBSCRIPTION,
            subscription_id: Some(subscription_id),
            variant,
        })
        .await
    }

    async fn enqueue_eh_download_request(
        &self,
        req: EhEnqueueRequest<'_>,
    ) -> Result<Option<eh_download_queue::Model>> {
        // Keep enqueue in the same per-chat critical section as cancellation
        // and publish. In particular, a new request cannot rebind a delivery
        // after a publisher has read it but before it commits its markers.
        #[cfg(test)]
        let test_lock_hook = EH_ENQUEUE_CHAT_LOCK_HOOK.try_with(|hook| hook.clone()).ok();
        #[cfg(test)]
        if let Some(hook) = &test_lock_hook {
            hook.waiting.notify_one();
        }
        let _chat_guard = EH_CHAT_LOCKS.lock_chat(req.chat_id).await;
        #[cfg(test)]
        if let Some(hook) = &test_lock_hook {
            hook.acquired
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        for attempt in 0..MAX_ENQUEUE_TRANSACTION_ATTEMPTS {
            let txn = match self.db.begin().await {
                Ok(txn) => txn,
                Err(error) => {
                    if is_retryable_enqueue_db_error(&error)
                        && attempt + 1 < MAX_ENQUEUE_TRANSACTION_ATTEMPTS
                    {
                        continue;
                    }
                    return Err(error).context("Failed to begin shared EH enqueue transaction");
                }
            };
            match self.enqueue_eh_download_in_txn(&txn, &req).await {
                Ok(Some(delivery_id)) => match txn.commit().await {
                    Ok(()) => {
                        return eh_download_queue::Entity::find_by_id(delivery_id)
                            .one(&self.db)
                            .await
                            .context("Failed to reread shared EH delivery after enqueue")?
                            .context("Shared EH delivery disappeared after enqueue commit")
                            .map(Some);
                    }
                    Err(error) => {
                        if is_retryable_enqueue_db_error(&error)
                            && attempt + 1 < MAX_ENQUEUE_TRANSACTION_ATTEMPTS
                        {
                            continue;
                        }
                        return Err(error)
                            .context("Failed to commit shared EH enqueue transaction");
                    }
                },
                Ok(None) => match txn.commit().await {
                    Ok(()) => return Ok(None),
                    Err(error) => {
                        if is_retryable_enqueue_db_error(&error)
                            && attempt + 1 < MAX_ENQUEUE_TRANSACTION_ATTEMPTS
                        {
                            continue;
                        }
                        return Err(error)
                            .context("Failed to commit shared EH enqueue transaction");
                    }
                },
                Err(error) => {
                    let should_retry = is_retryable_enqueue_error(&error);
                    let rollback = txn.rollback().await;
                    if let Err(rollback_error) = rollback {
                        return Err(rollback_error)
                            .context("Failed to roll back shared EH enqueue transaction");
                    }
                    if should_retry && attempt + 1 < MAX_ENQUEUE_TRANSACTION_ATTEMPTS {
                        continue;
                    }
                    return Err(error).context("Failed to enqueue shared EH gallery delivery");
                }
            }
        }

        unreachable!("shared EH enqueue retry loop always returns")
    }

    async fn enqueue_eh_download_in_txn(
        &self,
        txn: &DatabaseTransaction,
        req: &EhEnqueueRequest<'_>,
    ) -> Result<Option<i32>> {
        let push_ledger = if req.source == SOURCE_SUBSCRIPTION {
            eh_gallery_push_ledger::Entity::find()
                .filter(eh_gallery_push_ledger::Column::ChatId.eq(req.chat_id))
                .filter(eh_gallery_push_ledger::Column::Gid.eq(req.gid))
                .one(txn)
                .await
                .context("Failed to select EH subscription push ledger")?
        } else {
            None
        };
        let premarked_archive_sent_at = if req.source == SOURCE_SUBSCRIPTION && req.send_archive {
            push_ledger
                .as_ref()
                .and_then(|ledger| ledger.archive_sent_at)
        } else {
            None
        };
        let premarked_telegraph_sent_at = if req.source == SOURCE_SUBSCRIPTION && req.telegraph {
            push_ledger
                .as_ref()
                .and_then(|ledger| ledger.telegraph_sent_at)
        } else {
            None
        };
        if (!req.send_archive || premarked_archive_sent_at.is_some())
            && (!req.telegraph || premarked_telegraph_sent_at.is_some())
        {
            return Ok(None);
        }

        let EhGalleryJobResolution {
            job,
            started_new_generation,
        } = self.get_or_create_eh_gallery_job_in_txn(txn, req).await?;
        let EhDeliveryResolution { old_job_id } = self
            .upsert_eh_delivery_in_txn(
                txn,
                req,
                job.id,
                premarked_archive_sent_at,
                premarked_telegraph_sent_at,
            )
            .await?;

        if req.telegraph && premarked_telegraph_sent_at.is_none() && started_new_generation {
            try_apply_cached_eh_result_in_txn(txn, job.id, req.send_archive).await?;
        }

        self.recompute_eh_job_telegraph_requirement_with_policy_in_txn(
            txn,
            job.id,
            req.send_archive,
        )
        .await?;
        if let Some(old_job_id) = old_job_id.filter(|old_job_id| *old_job_id != job.id) {
            self.recompute_eh_job_telegraph_requirement_with_policy_in_txn(
                txn,
                old_job_id,
                req.send_archive,
            )
            .await?;
            retire_consumerless_eh_job_in_txn(txn, old_job_id).await?;
        }

        // A marker-bearing delivery may remain on its previous job.  Retire a
        // just-created/reused target job if it ended this transaction without a
        // consumer rather than leaving a claimable orphan behind.
        retire_consumerless_eh_job_in_txn(txn, job.id).await?;

        eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::ChatId.eq(req.chat_id))
            .filter(eh_download_queue::Column::Gid.eq(req.gid))
            .one(txn)
            .await
            .context("Failed to reread shared EH delivery in transaction")?
            .map(|delivery| delivery.id)
            .context("Shared EH delivery disappeared in enqueue transaction")
            .map(Some)
    }

    async fn get_or_create_eh_gallery_job_in_txn(
        &self,
        txn: &DatabaseTransaction,
        req: &EhEnqueueRequest<'_>,
    ) -> Result<EhGalleryJobResolution> {
        anyhow::ensure!(
            matches!(
                req.variant.download_mode.as_str(),
                DOWNLOAD_MODE_ARCHIVE | DOWNLOAD_MODE_IMAGES | DOWNLOAD_MODE_LEGACY
            ),
            "Unsupported shared EH gallery download mode '{}'",
            req.variant.download_mode
        );
        let existing = eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::Gid.eq(req.gid))
            .filter(eh_gallery_jobs::Column::Token.eq(req.token))
            .filter(eh_gallery_jobs::Column::DownloadMode.eq(&req.variant.download_mode))
            .filter(eh_gallery_jobs::Column::Resolution.eq(&req.variant.resolution))
            .filter(optional_job_string_filter(
                eh_gallery_jobs::Column::SourceFingerprint,
                req.fingerprint.as_deref(),
            ))
            .one(txn)
            .await
            .context("Failed to select shared EH gallery job")?;

        let (mut job, mut started_new_generation) = if let Some(job) = existing {
            (job, false)
        } else {
            (
                eh_gallery_jobs::ActiveModel {
                    gid: Set(req.gid),
                    token: Set(req.token.to_string()),
                    download_mode: Set(req.variant.download_mode.clone()),
                    resolution: Set(req.variant.resolution.clone()),
                    source_fingerprint: Set(req.fingerprint.clone()),
                    title: Set(req.title.to_string()),
                    status: Set(JOB_STATUS_PENDING.to_string()),
                    telegraph_status: Set(TELEGRAPH_STATUS_NOT_REQUIRED.to_string()),
                    telegraph_required: Set(false),
                    cleanup_status: Set(CLEANUP_STATUS_NONE.to_string()),
                    created_at: Set(Local::now().naive_local()),
                    ..Default::default()
                }
                .insert(txn)
                .await
                .context("Failed to insert shared EH gallery job")?,
                true,
            )
        };

        let has_active_delivery = has_active_eh_delivery_in_txn(txn, job.id).await?;
        let clean_for_reactivation = job.cleanup_status == CLEANUP_STATUS_NONE;
        let should_reactivate = clean_for_reactivation
            && !has_active_delivery
            && matches!(job.status.as_str(), JOB_STATUS_RETIRED | JOB_STATUS_FAILED);
        let should_update_title = !req.title.is_empty() && job.title != req.title;

        if should_reactivate {
            reset_eh_gallery_job_generation_in_txn(
                txn,
                job.id,
                req.title,
                req.fingerprint.as_deref(),
            )
            .await?;
            job = eh_gallery_jobs::Entity::find_by_id(job.id)
                .one(txn)
                .await
                .context("Failed to reread reactivated shared EH gallery job")?
                .context("Reactivated shared EH gallery job disappeared")?;
            started_new_generation = true;
        } else if should_update_title {
            eh_gallery_jobs::Entity::update_many()
                .col_expr(
                    eh_gallery_jobs::Column::Title,
                    Expr::value(req.title.to_string()),
                )
                .filter(eh_gallery_jobs::Column::Id.eq(job.id))
                .exec(txn)
                .await
                .context("Failed to update shared EH gallery job title")?;
            job.title = req.title.to_string();
        }

        Ok(EhGalleryJobResolution {
            job,
            started_new_generation,
        })
    }

    async fn upsert_eh_delivery_in_txn(
        &self,
        txn: &DatabaseTransaction,
        req: &EhEnqueueRequest<'_>,
        requested_job_id: i32,
        premarked_archive_sent_at: Option<DateTime>,
        premarked_telegraph_sent_at: Option<DateTime>,
    ) -> Result<EhDeliveryResolution> {
        let existing = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::ChatId.eq(req.chat_id))
            .filter(eh_download_queue::Column::Gid.eq(req.gid))
            .one(txn)
            .await
            .context("Failed to select shared EH delivery")?;

        let Some(existing) = existing else {
            eh_download_queue::ActiveModel {
                job_id: Set(Some(requested_job_id)),
                chat_id: Set(req.chat_id),
                gid: Set(req.gid),
                token: Set(req.token.to_string()),
                title: Set(req.title.to_string()),
                telegraph: Set(req.telegraph),
                source: Set(req.source.to_string()),
                subscription_ids: Set(req.subscription_id.map(|id| id.to_string())),
                telegraph_subscription_ids: Set(if req.telegraph {
                    req.subscription_id.map(|id| id.to_string())
                } else {
                    None
                }),
                status: Set(DELIVERY_STATUS_WAITING.to_string()),
                created_at: Set(Local::now().naive_local()),
                archive_sent_at: Set(premarked_archive_sent_at),
                telegraph_sent_at: Set(premarked_telegraph_sent_at),
                ..Default::default()
            }
            .insert(txn)
            .await
            .context("Failed to insert shared EH delivery")?;
            return Ok(EhDeliveryResolution { old_job_id: None });
        };

        let terminal = is_terminal_delivery_status(&existing.status);
        let (
            merged_source,
            merged_subscription_ids,
            merged_telegraph_subscription_ids,
            merged_telegraph,
        ) = if terminal {
            // A terminal record is history, not an owner of the next
            // delivery wave. Start that wave from the request alone.
            (
                req.source,
                req.subscription_id.map(|id| id.to_string()),
                if req.telegraph {
                    req.subscription_id.map(|id| id.to_string())
                } else {
                    None
                },
                req.telegraph,
            )
        } else {
            let merged_source = if existing.source == SOURCE_DIRECT || req.source == SOURCE_DIRECT {
                SOURCE_DIRECT
            } else {
                SOURCE_SUBSCRIPTION
            };
            let merged_subscription_ids = if merged_source == SOURCE_DIRECT {
                None
            } else {
                merge_subscription_ids(existing.subscription_ids.as_deref(), req.subscription_id)
            };
            let merged_telegraph_subscription_ids = if merged_source == SOURCE_DIRECT {
                None
            } else {
                merge_telegraph_subscription_ids(
                    existing.telegraph_subscription_ids.as_deref(),
                    req.subscription_id,
                    req.telegraph,
                )
            };
            let merged_telegraph = if merged_source == SOURCE_SUBSCRIPTION {
                merged_telegraph_subscription_ids.is_some()
            } else {
                existing.telegraph || req.telegraph
            };
            (
                merged_source,
                merged_subscription_ids,
                merged_telegraph_subscription_ids,
                merged_telegraph,
            )
        };
        let marker_bearing =
            existing.archive_sent_at.is_some() || existing.telegraph_sent_at.is_some();
        let direct_marker_bearing_rebind = !terminal
            && req.source == SOURCE_DIRECT
            && existing.job_id != Some(requested_job_id)
            && marker_bearing;
        let subscription_fingerprint_generation_rebind = if !terminal
            && req.source == SOURCE_SUBSCRIPTION
            && existing.job_id != Some(requested_job_id)
        {
            if let Some(existing_job_id) = existing.job_id {
                let existing_job = eh_gallery_jobs::Entity::find_by_id(existing_job_id)
                    .one(txn)
                    .await
                    .context("Failed to select existing shared EH subscription delivery job")?
                    .context("Shared EH subscription delivery referenced a missing job")?;
                let requested_job = eh_gallery_jobs::Entity::find_by_id(requested_job_id)
                    .one(txn)
                    .await
                    .context("Failed to select requested shared EH subscription delivery job")?
                    .context("Requested shared EH subscription delivery job disappeared")?;
                existing_job.gid == requested_job.gid
                    && existing_job.token == requested_job.token
                    && existing_job.download_mode == requested_job.download_mode
                    && existing_job.resolution == requested_job.resolution
                    && existing_job.source_fingerprint != requested_job.source_fingerprint
            } else {
                false
            }
        } else {
            false
        };

        if existing.status == DELIVERY_STATUS_PUBLISHING
            && (existing.job_id == Some(requested_job_id)
                || (marker_bearing
                    && !direct_marker_bearing_rebind
                    && !subscription_fingerprint_generation_rebind))
        {
            // A publisher already bound to the requested job, or one with a
            // persisted send marker, owns this delivery's binding and local
            // state. Merge only owner demand; never replace its job, status,
            // metadata, sent markers, or generation fields.
            let updated = eh_download_queue::Entity::update_many()
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
                    Expr::value(merged_subscription_ids),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphSubscriptionIds,
                    Expr::value(merged_telegraph_subscription_ids),
                )
                .filter(eh_download_queue::Column::Id.eq(existing.id))
                .filter(eh_download_queue::Column::Status.eq(DELIVERY_STATUS_PUBLISHING))
                .filter(eh_download_queue::Column::Telegraph.eq(existing.telegraph))
                .filter(eh_download_queue::Column::Source.eq(&existing.source))
                .filter(optional_string_filter(
                    eh_download_queue::Column::SubscriptionIds,
                    existing.subscription_ids.as_deref(),
                ))
                .filter(optional_string_filter(
                    eh_download_queue::Column::TelegraphSubscriptionIds,
                    existing.telegraph_subscription_ids.as_deref(),
                ))
                .exec(txn)
                .await
                .context("Failed to merge shared EH publishing delivery owner")?;
            if updated.rows_affected != 1 {
                anyhow::bail!("Shared EH delivery changed concurrently during enqueue")
            }
            return Ok(EhDeliveryResolution {
                old_job_id: existing.job_id,
            });
        }

        if existing.status == DELIVERY_STATUS_PUBLISHING
            && !marker_bearing
            && !subscription_fingerprint_generation_rebind
        {
            // `enqueue_eh_download_request` and `process_claimed` share the
            // keyed chat lock. A markerless publishing claim therefore has not
            // entered the send critical section yet, so a different requested
            // job may replace the stale binding.
            let updated = eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::JobId,
                    Expr::value(Some(requested_job_id)),
                )
                .col_expr(
                    eh_download_queue::Column::Token,
                    Expr::value(req.token.to_string()),
                )
                .col_expr(
                    eh_download_queue::Column::Title,
                    Expr::value(req.title.to_string()),
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
                    Expr::value(merged_subscription_ids),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphSubscriptionIds,
                    Expr::value(merged_telegraph_subscription_ids),
                )
                .col_expr(
                    eh_download_queue::Column::Status,
                    Expr::value(DELIVERY_STATUS_WAITING),
                )
                .col_expr(eh_download_queue::Column::FileSize, Expr::value(0_i64))
                .col_expr(eh_download_queue::Column::GpCost, Expr::value(0_i64))
                .col_expr(
                    eh_download_queue::Column::Error,
                    Expr::value(None::<String>),
                )
                .col_expr(eh_download_queue::Column::RetryCount, Expr::value(0_i32))
                .col_expr(
                    eh_download_queue::Column::StartedAt,
                    Expr::value(None::<DateTime>),
                )
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
                .filter(eh_download_queue::Column::Id.eq(existing.id))
                .filter(eh_download_queue::Column::Status.eq(DELIVERY_STATUS_PUBLISHING))
                .filter(optional_i32_filter(
                    eh_download_queue::Column::JobId,
                    existing.job_id,
                ))
                .filter(eh_download_queue::Column::Telegraph.eq(existing.telegraph))
                .filter(eh_download_queue::Column::Source.eq(&existing.source))
                .filter(optional_string_filter(
                    eh_download_queue::Column::SubscriptionIds,
                    existing.subscription_ids.as_deref(),
                ))
                .filter(optional_string_filter(
                    eh_download_queue::Column::TelegraphSubscriptionIds,
                    existing.telegraph_subscription_ids.as_deref(),
                ))
                .filter(eh_download_queue::Column::ArchiveSentAt.is_null())
                .filter(eh_download_queue::Column::TelegraphSentAt.is_null())
                .exec(txn)
                .await
                .context("Failed to rebind markerless shared EH publishing delivery")?;
            if updated.rows_affected != 1 {
                anyhow::bail!("Shared EH delivery changed concurrently during enqueue")
            }
            return Ok(EhDeliveryResolution {
                old_job_id: existing.job_id,
            });
        }

        if terminal || direct_marker_bearing_rebind || subscription_fingerprint_generation_rebind {
            // Keep `created_at` stable: delivery ordering remains attached to
            // the chat/gid record while every wave-local field starts clean.
            let reset_telegraph = if subscription_fingerprint_generation_rebind {
                merged_telegraph
            } else {
                req.telegraph
            };
            let reset_source = if subscription_fingerprint_generation_rebind {
                merged_source
            } else {
                req.source
            };
            let reset_subscription_ids = if subscription_fingerprint_generation_rebind {
                merged_subscription_ids.clone()
            } else {
                req.subscription_id.map(|id| id.to_string())
            };
            let reset_telegraph_subscription_ids = if subscription_fingerprint_generation_rebind {
                merged_telegraph_subscription_ids.clone()
            } else if req.telegraph {
                req.subscription_id.map(|id| id.to_string())
            } else {
                None
            };
            let reset_archive_sent_at = if subscription_fingerprint_generation_rebind {
                existing.archive_sent_at
            } else {
                premarked_archive_sent_at
            };
            let reset_telegraph_sent_at = if subscription_fingerprint_generation_rebind {
                existing.telegraph_sent_at
            } else {
                premarked_telegraph_sent_at
            };
            let mut update = eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::JobId,
                    Expr::value(Some(requested_job_id)),
                )
                .col_expr(
                    eh_download_queue::Column::Token,
                    Expr::value(req.token.to_string()),
                )
                .col_expr(
                    eh_download_queue::Column::Title,
                    Expr::value(req.title.to_string()),
                )
                .col_expr(
                    eh_download_queue::Column::Telegraph,
                    Expr::value(reset_telegraph),
                )
                .col_expr(
                    eh_download_queue::Column::Source,
                    Expr::value(reset_source.to_string()),
                )
                .col_expr(
                    eh_download_queue::Column::SubscriptionIds,
                    Expr::value(reset_subscription_ids),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphSubscriptionIds,
                    Expr::value(reset_telegraph_subscription_ids),
                )
                .col_expr(
                    eh_download_queue::Column::Status,
                    Expr::value(DELIVERY_STATUS_WAITING),
                )
                .col_expr(eh_download_queue::Column::FileSize, Expr::value(0_i64))
                .col_expr(eh_download_queue::Column::GpCost, Expr::value(0_i64))
                .col_expr(
                    eh_download_queue::Column::Error,
                    Expr::value(None::<String>),
                )
                .col_expr(eh_download_queue::Column::RetryCount, Expr::value(0_i32))
                .col_expr(
                    eh_download_queue::Column::StartedAt,
                    Expr::value(None::<DateTime>),
                )
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
                    Expr::value(reset_archive_sent_at),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphSentAt,
                    Expr::value(reset_telegraph_sent_at),
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
                    Expr::value(0_i32),
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
                    Expr::value(0_i32),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteError,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewrittenAt,
                    Expr::value(None::<DateTime>),
                )
                .filter(eh_download_queue::Column::Id.eq(existing.id))
                .filter(eh_download_queue::Column::Status.eq(&existing.status))
                .filter(optional_i32_filter(
                    eh_download_queue::Column::JobId,
                    existing.job_id,
                ));
            if !terminal {
                update = update
                    .filter(eh_download_queue::Column::Telegraph.eq(existing.telegraph))
                    .filter(eh_download_queue::Column::Source.eq(&existing.source))
                    .filter(optional_string_filter(
                        eh_download_queue::Column::SubscriptionIds,
                        existing.subscription_ids.as_deref(),
                    ))
                    .filter(optional_string_filter(
                        eh_download_queue::Column::TelegraphSubscriptionIds,
                        existing.telegraph_subscription_ids.as_deref(),
                    ))
                    .filter(optional_datetime_filter(
                        eh_download_queue::Column::ArchiveSentAt,
                        existing.archive_sent_at,
                    ))
                    .filter(optional_datetime_filter(
                        eh_download_queue::Column::TelegraphSentAt,
                        existing.telegraph_sent_at,
                    ));
            }
            let updated = update
                .exec(txn)
                .await
                .context("Failed to reset shared EH delivery wave")?;
            if updated.rows_affected != 1 {
                anyhow::bail!("Shared EH delivery changed concurrently during enqueue")
            }
            return Ok(EhDeliveryResolution {
                old_job_id: existing.job_id,
            });
        }

        let target_job_id = if marker_bearing {
            existing.job_id
        } else {
            Some(requested_job_id)
        };
        let target_status = if marker_bearing {
            existing.status.clone()
        } else {
            DELIVERY_STATUS_WAITING.to_string()
        };

        let mut update = eh_download_queue::Entity::update_many()
            .col_expr(eh_download_queue::Column::JobId, Expr::value(target_job_id))
            .col_expr(
                eh_download_queue::Column::Token,
                Expr::value(req.token.to_string()),
            )
            .col_expr(
                eh_download_queue::Column::Title,
                Expr::value(req.title.to_string()),
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
                Expr::value(merged_subscription_ids),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphSubscriptionIds,
                Expr::value(merged_telegraph_subscription_ids),
            )
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(target_status),
            )
            .filter(eh_download_queue::Column::Id.eq(existing.id))
            .filter(eh_download_queue::Column::Status.eq(&existing.status))
            .filter(eh_download_queue::Column::Telegraph.eq(existing.telegraph))
            .filter(eh_download_queue::Column::Source.eq(&existing.source))
            .filter(optional_i32_filter(
                eh_download_queue::Column::JobId,
                existing.job_id,
            ));
        update = update.filter(optional_string_filter(
            eh_download_queue::Column::SubscriptionIds,
            existing.subscription_ids.as_deref(),
        ));
        update = update.filter(optional_string_filter(
            eh_download_queue::Column::TelegraphSubscriptionIds,
            existing.telegraph_subscription_ids.as_deref(),
        ));
        update = update.filter(optional_datetime_filter(
            eh_download_queue::Column::ArchiveSentAt,
            existing.archive_sent_at,
        ));
        update = update.filter(optional_datetime_filter(
            eh_download_queue::Column::TelegraphSentAt,
            existing.telegraph_sent_at,
        ));

        let updated = update
            .exec(txn)
            .await
            .context("Failed to merge shared EH delivery")?;
        if updated.rows_affected != 1 {
            anyhow::bail!("Shared EH delivery changed concurrently during enqueue")
        }
        Ok(EhDeliveryResolution {
            old_job_id: existing.job_id,
        })
    }

    pub(crate) async fn recompute_eh_job_telegraph_requirement_in_txn(
        &self,
        txn: &DatabaseTransaction,
        job_id: i32,
    ) -> Result<()> {
        self.recompute_eh_job_telegraph_requirement_with_policy_in_txn(txn, job_id, true)
            .await
    }

    async fn recompute_eh_job_telegraph_requirement_with_policy_in_txn(
        &self,
        txn: &DatabaseTransaction,
        job_id: i32,
        send_archive: bool,
    ) -> Result<()> {
        let deliveries = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::JobId.eq(job_id))
            .all(txn)
            .await
            .context("Failed to select shared EH job deliveries for Telegraph requirement")?;
        let telegraph_required = deliveries.iter().any(|delivery| {
            delivery.telegraph
                && delivery.telegraph_sent_at.is_none()
                && is_active_delivery_status(&delivery.status)
        });
        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(txn)
            .await
            .context("Failed to select shared EH job for Telegraph requirement")?
            .context("Shared EH job disappeared while recomputing Telegraph requirement")?;
        let should_try_cached_result = telegraph_required
            && !job.telegraph_required
            && job.telegraph_status == TELEGRAPH_STATUS_NOT_REQUIRED;
        if should_try_cached_result
            && try_apply_cached_eh_result_in_txn(txn, job_id, send_archive).await?
        {
            let updated = eh_gallery_jobs::Entity::update_many()
                .col_expr(
                    eh_gallery_jobs::Column::TelegraphRequired,
                    Expr::value(true),
                )
                .filter(eh_gallery_jobs::Column::Id.eq(job_id))
                .filter(eh_gallery_jobs::Column::TelegraphRequired.eq(false))
                .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_READY))
                .exec(txn)
                .await
                .context("Failed to record cached shared EH Telegraph demand")?;
            if updated.rows_affected != 1 {
                anyhow::bail!(
                    "Shared EH job {} changed concurrently while applying cached Telegraph result",
                    job_id
                );
            }
            return Ok(());
        }
        let start_fresh_upload_wave = telegraph_required
            && job.status == JOB_STATUS_DOWNLOADED
            && job.telegraph_status == TELEGRAPH_STATUS_FAILED;
        let telegraph_status = if telegraph_required {
            match job.telegraph_status.as_str() {
                TELEGRAPH_STATUS_NOT_REQUIRED | TELEGRAPH_STATUS_FAILED
                    if job.status == JOB_STATUS_DOWNLOADED =>
                {
                    TELEGRAPH_STATUS_PENDING
                }
                TELEGRAPH_STATUS_PENDING | TELEGRAPH_STATUS_UPLOADING | TELEGRAPH_STATUS_READY => {
                    &job.telegraph_status
                }
                _ => &job.telegraph_status,
            }
        } else if job.telegraph_status == TELEGRAPH_STATUS_PENDING {
            TELEGRAPH_STATUS_NOT_REQUIRED
        } else {
            &job.telegraph_status
        };

        let mut update = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRequired,
                Expr::value(telegraph_required),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphStatus,
                Expr::value(telegraph_status.to_string()),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id));
        if start_fresh_upload_wave {
            update = update
                .col_expr(
                    eh_gallery_jobs::Column::TelegraphUrl,
                    Expr::value(None::<String>),
                )
                .col_expr(eh_gallery_jobs::Column::Error, Expr::value(None::<String>))
                .col_expr(eh_gallery_jobs::Column::RetryCount, Expr::value(0_i32))
                .col_expr(
                    eh_gallery_jobs::Column::NextRetryAt,
                    Expr::value(None::<DateTime>),
                );
        }
        update
            .exec(txn)
            .await
            .context("Failed to update shared EH job Telegraph requirement")?;
        Ok(())
    }
}

impl Repo {
    /// Claim the next normal shared-gallery download job. Recent work remains
    /// FIFO while older work remains LIFO, matching the pre-sharing queue lane.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn get_next_eh_job_for_download(&self) -> Result<Option<eh_gallery_jobs::Model>> {
        self.get_next_eh_job_for_download_with_policy(true).await
    }

    /// Claim the next normal shared-gallery download job with the configured
    /// archive publish policy applied to every bound delivery.
    pub async fn get_next_eh_job_for_download_with_policy(
        &self,
        send_archive: bool,
    ) -> Result<Option<eh_gallery_jobs::Model>> {
        self.get_next_eh_job_for_download_at(Local::now().naive_local(), send_archive)
            .await
    }

    async fn get_next_eh_job_for_download_at(
        &self,
        now: DateTime,
        send_archive: bool,
    ) -> Result<Option<eh_gallery_jobs::Model>> {
        let cutoff = now - chrono::Duration::hours(MAIN_DOWNLOAD_RECENT_WINDOW_HOURS);
        let is_recent = Expr::col(eh_gallery_jobs::Column::CreatedAt).gt(cutoff);
        let recent_priority: SimpleExpr = Expr::case(is_recent.clone(), 0).finally(1).into();
        let recent_created_at: SimpleExpr = Expr::case(
            is_recent.clone(),
            Expr::col(eh_gallery_jobs::Column::CreatedAt),
        )
        .finally(Expr::value(None::<DateTime>))
        .into();
        let recent_id: SimpleExpr =
            Expr::case(is_recent.clone(), Expr::col(eh_gallery_jobs::Column::Id))
                .finally(Expr::value(None::<i32>))
                .into();
        let old_created_at: SimpleExpr =
            Expr::case(is_recent.clone(), Expr::value(None::<DateTime>))
                .finally(Expr::col(eh_gallery_jobs::Column::CreatedAt))
                .into();
        let old_id: SimpleExpr = Expr::case(is_recent, Expr::value(None::<i32>))
            .finally(Expr::col(eh_gallery_jobs::Column::Id))
            .into();
        let mut query = eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_PENDING))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(eh_gallery_jobs::Column::BackgroundDownloadStatus.is_null())
            .filter(eh_job_has_configured_source_work_candidate_filter(
                send_archive,
            ))
            .filter(
                eh_gallery_jobs::Column::NextRetryAt
                    .is_null()
                    .or(eh_gallery_jobs::Column::NextRetryAt.lte(now)),
            );
        QueryTrait::query(&mut query)
            .order_by_expr(recent_priority, Order::Asc)
            .order_by_expr(recent_created_at, Order::Asc)
            .order_by_expr(recent_id, Order::Asc)
            .order_by_expr(old_created_at, Order::Desc)
            .order_by_expr(old_id, Order::Desc);
        let Some(job) = query
            .one(&self.db)
            .await
            .context("Failed to fetch next shared EH gallery job for download")?
        else {
            return Ok(None);
        };

        self.claim_eh_job_download_from_snapshot_at(&job, now, send_archive)
            .await
    }

    async fn claim_eh_job_download_from_snapshot_at(
        &self,
        job: &eh_gallery_jobs::Model,
        now: DateTime,
        send_archive: bool,
    ) -> Result<Option<eh_gallery_jobs::Model>> {
        let generation = next_job_claim_generation(now, job.started_at)?;
        let result = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::Status,
                Expr::value(JOB_STATUS_DOWNLOADING),
            )
            .col_expr(eh_gallery_jobs::Column::StartedAt, Expr::value(generation))
            .col_expr(
                eh_gallery_jobs::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_PENDING))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(eh_gallery_jobs::Column::BackgroundDownloadStatus.is_null())
            .filter(eh_job_has_configured_source_work_filter(
                job.id,
                send_archive,
            ))
            .filter(job_claim_generation_filter(job.started_at))
            .filter(
                sea_orm::Condition::any()
                    .add(eh_gallery_jobs::Column::NextRetryAt.is_null())
                    .add(eh_gallery_jobs::Column::NextRetryAt.lte(now)),
            )
            .exec(&self.db)
            .await
            .context("Failed to atomically claim shared EH gallery job")?;
        if result.rows_affected == 0 {
            return Ok(None);
        }

        eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADING))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(eh_gallery_jobs::Column::BackgroundDownloadStatus.is_null())
            .filter(eh_gallery_jobs::Column::StartedAt.eq(generation))
            .one(&self.db)
            .await
            .context("Failed to reread shared EH gallery download claim")
    }

    /// Persist the deterministic archive family's durable owner before a
    /// download worker can make any filesystem or provider-side change. The
    /// claim generation and lane state make this a no-op for a stale worker.
    pub async fn persist_eh_job_archive_artifact_ownership(
        &self,
        job_id: i32,
        expected_started_at: DateTime,
        zip_path: &str,
        background_claim: bool,
    ) -> Result<bool> {
        let mut update = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::ZipPath,
                Expr::value(Some(zip_path.to_string())),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at))
            .filter(
                eh_gallery_jobs::Column::ZipPath
                    .is_null()
                    .or(eh_gallery_jobs::Column::ZipPath.eq(zip_path)),
            );
        if background_claim {
            update = update
                .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_PENDING))
                .filter(
                    eh_gallery_jobs::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING),
                );
        } else {
            update = update
                .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADING))
                .filter(eh_gallery_jobs::Column::BackgroundDownloadStatus.is_null());
        }
        let persisted = update
            .exec(&self.db)
            .await
            .context("Failed to persist shared EH archive artifact ownership")?;
        Ok(persisted.rows_affected == 1)
    }

    /// Claim one due shared-artifact cleanup generation.  A cleanup lease is
    /// independent from download/upload leases so a delayed stale executor
    /// cannot reactivate or retire a newer generation.
    pub async fn get_next_eh_job_for_cleanup(&self) -> Result<Option<eh_gallery_jobs::Model>> {
        let now = Local::now().naive_local();
        let mut query = eh_gallery_jobs::Entity::find().filter(
            sea_orm::Condition::any()
                .add(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_PENDING))
                .add(
                    sea_orm::Condition::all()
                        .add(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_FAILED))
                        .add(
                            eh_gallery_jobs::Column::CleanupNextRetryAt
                                .is_null()
                                .or(eh_gallery_jobs::Column::CleanupNextRetryAt.lte(now)),
                        ),
                ),
        );
        query = query
            .filter(eh_gallery_jobs::Column::Status.ne(JOB_STATUS_DOWNLOADING))
            .filter(eh_gallery_jobs::Column::TelegraphStatus.ne(TELEGRAPH_STATUS_UPLOADING))
            .filter(
                eh_gallery_jobs::Column::BackgroundDownloadStatus
                    .is_null()
                    .or(eh_gallery_jobs::Column::BackgroundDownloadStatus
                        .ne(BACKGROUND_STATUS_RUNNING)),
            );
        let pending_first: SimpleExpr = Expr::case(
            eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_PENDING),
            0,
        )
        .finally(1)
        .into();
        QueryTrait::query(&mut query)
            .order_by_expr(pending_first, Order::Asc)
            .order_by(eh_gallery_jobs::Column::CleanupNextRetryAt, Order::Asc)
            .order_by(eh_gallery_jobs::Column::Id, Order::Asc);
        let Some(job) = query
            .one(&self.db)
            .await
            .context("Failed to fetch due shared EH artifact cleanup")?
        else {
            return Ok(None);
        };

        let generation = next_job_claim_generation(now, job.cleanup_started_at)?;
        let mut claim = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::CleanupStatus,
                Expr::value(CLEANUP_STATUS_RUNNING),
            )
            .col_expr(
                eh_gallery_jobs::Column::CleanupStartedAt,
                Expr::value(Some(generation)),
            )
            .col_expr(
                eh_gallery_jobs::Column::CleanupNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(&job.cleanup_status))
            .filter(cleanup_claim_generation_filter(job.cleanup_started_at))
            .filter(eh_gallery_jobs::Column::Status.ne(JOB_STATUS_DOWNLOADING))
            .filter(eh_gallery_jobs::Column::TelegraphStatus.ne(TELEGRAPH_STATUS_UPLOADING))
            .filter(
                eh_gallery_jobs::Column::BackgroundDownloadStatus
                    .is_null()
                    .or(eh_gallery_jobs::Column::BackgroundDownloadStatus
                        .ne(BACKGROUND_STATUS_RUNNING)),
            );
        if job.cleanup_status == CLEANUP_STATUS_FAILED {
            claim = claim.filter(
                eh_gallery_jobs::Column::CleanupNextRetryAt
                    .is_null()
                    .or(eh_gallery_jobs::Column::CleanupNextRetryAt.lte(now)),
            );
        }
        let claimed = claim
            .exec(&self.db)
            .await
            .context("Failed to atomically claim shared EH artifact cleanup")?;
        if claimed.rows_affected == 0 {
            return Ok(None);
        }

        eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_RUNNING))
            .filter(eh_gallery_jobs::Column::CleanupStartedAt.eq(generation))
            .one(&self.db)
            .await
            .context("Failed to reread shared EH artifact cleanup claim")
    }

    /// Keep every local artifact after an Abort or local-removal failure.  A
    /// false result means an old executor no longer owns this cleanup lease.
    pub async fn record_eh_job_cleanup_failure(
        &self,
        job_id: i32,
        expected_cleanup_started_at: DateTime,
        error: &str,
        retry_delay_secs: i64,
    ) -> Result<bool> {
        let retry_at = Local::now()
            .naive_local()
            .checked_add_signed(chrono::Duration::seconds(retry_delay_secs))
            .context("Shared EH cleanup retry deadline overflow")?;
        let failed = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::CleanupStatus,
                Expr::value(CLEANUP_STATUS_FAILED),
            )
            .col_expr(
                eh_gallery_jobs::Column::CleanupError,
                Expr::value(Some(error.to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::CleanupNextRetryAt,
                Expr::value(Some(retry_at)),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_RUNNING))
            .filter(eh_gallery_jobs::Column::CleanupStartedAt.eq(expected_cleanup_started_at))
            .exec(&self.db)
            .await
            .context("Failed to record shared EH artifact cleanup failure")?;
        Ok(failed.rows_affected == 1)
    }

    /// Finalize a locally removed artifact family only for its matching cleanup
    /// generation.  The transaction rechecks delivery ownership immediately
    /// before changing job state, preventing stale local work from waking a
    /// consumer that has since canceled.
    pub async fn finalize_eh_job_cleanup(
        &self,
        job_id: i32,
        expected_cleanup_started_at: DateTime,
        send_archive: bool,
    ) -> Result<Option<EhCleanupFinalizeOutcome>> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH artifact cleanup finalization")?;
        let result: Result<Option<EhCleanupFinalizeOutcome>> = async {
            let Some(job) = eh_gallery_jobs::Entity::find_by_id(job_id)
                .one(&txn)
                .await
                .context("Failed to select shared EH job for cleanup finalization")?
            else {
                return Ok(None);
            };
            if job.cleanup_status != CLEANUP_STATUS_RUNNING
                || job.cleanup_started_at != Some(expected_cleanup_started_at)
            {
                return Ok(None);
            }

            let deliveries = eh_download_queue::Entity::find()
                .filter(eh_download_queue::Column::JobId.eq(job_id))
                .all(&txn)
                .await
                .context("Failed to select shared EH deliveries for cleanup finalization")?;
            let has_active_delivery = deliveries
                .iter()
                .any(|delivery| is_active_delivery_status(&delivery.status));
            let rewrite_in_progress = matches!(
                job.telegraph_rewrite_status.as_deref(),
                Some(TELEGRAPH_REWRITE_STATUS_PENDING | TELEGRAPH_REWRITE_STATUS_REWRITING)
            );
            let has_usable_ready_telegraph =
                job.telegraph_status == TELEGRAPH_STATUS_READY && job.telegraph_url.is_some();
            let archive_source_required = send_archive
                && deliveries.iter().any(|delivery| {
                    is_active_delivery_status(&delivery.status)
                        && delivery.archive_sent_at.is_none()
                });
            let telegraph_source_required = !has_usable_ready_telegraph
                && deliveries.iter().any(|delivery| {
                    is_active_delivery_status(&delivery.status)
                        && delivery.telegraph
                        && delivery.telegraph_sent_at.is_none()
                });
            let source_work_required = archive_source_required || telegraph_source_required;
            let mut update = eh_gallery_jobs::Entity::update_many()
                .col_expr(
                    eh_gallery_jobs::Column::CleanupStatus,
                    Expr::value(CLEANUP_STATUS_NONE),
                )
                .col_expr(
                    eh_gallery_jobs::Column::CleanupError,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_gallery_jobs::Column::CleanupNextRetryAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_gallery_jobs::Column::ZipPath,
                    Expr::value(None::<String>),
                )
                .col_expr(eh_gallery_jobs::Column::FileSize, Expr::value(0_i64))
                .col_expr(eh_gallery_jobs::Column::GpCost, Expr::value(0_i64))
                .filter(eh_gallery_jobs::Column::Id.eq(job_id))
                .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_RUNNING))
                .filter(eh_gallery_jobs::Column::CleanupStartedAt.eq(expected_cleanup_started_at));

            let outcome = if has_active_delivery && source_work_required {
                // A delivery bound after this consumerless cleanup generation
                // was claimed and still needs source work. The ZIP is already
                // gone, so make that work claimable again without disturbing a
                // ready Telegraph result or any rewrite lease that may have
                // appeared meanwhile.
                update = update
                    .col_expr(
                        eh_gallery_jobs::Column::Status,
                        Expr::value(JOB_STATUS_PENDING),
                    )
                    .col_expr(eh_gallery_jobs::Column::Error, Expr::value(None::<String>))
                    .col_expr(eh_gallery_jobs::Column::RetryCount, Expr::value(0_i32))
                    .col_expr(
                        eh_gallery_jobs::Column::NextRetryAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::CompletedAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::BackgroundDownloadStatus,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::BackgroundDownloadStartedAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::BackgroundDownloadNextRetryAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::BackgroundDownloadAttemptCount,
                        Expr::value(0_i32),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::BackgroundDownloadError,
                        Expr::value(None::<String>),
                    )
                    .filter(no_active_eh_delivery_filter(job_id).not());
                if !has_usable_ready_telegraph {
                    update = update
                        .col_expr(
                            eh_gallery_jobs::Column::TelegraphStatus,
                            Expr::value(TELEGRAPH_STATUS_NOT_REQUIRED),
                        )
                        .col_expr(
                            eh_gallery_jobs::Column::TelegraphUrl,
                            Expr::value(None::<String>),
                        )
                        .col_expr(
                            eh_gallery_jobs::Column::TelegraphRewriteData,
                            Expr::value(None::<String>),
                        )
                        .col_expr(
                            eh_gallery_jobs::Column::TelegraphRewriteStatus,
                            Expr::value(None::<String>),
                        )
                        .col_expr(
                            eh_gallery_jobs::Column::TelegraphRewriteAfter,
                            Expr::value(None::<DateTime>),
                        )
                        .col_expr(
                            eh_gallery_jobs::Column::TelegraphRewriteStartedAt,
                            Expr::value(None::<DateTime>),
                        )
                        .col_expr(
                            eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt,
                            Expr::value(None::<DateTime>),
                        )
                        .col_expr(
                            eh_gallery_jobs::Column::TelegraphRewriteRetryCount,
                            Expr::value(0_i32),
                        )
                        .col_expr(
                            eh_gallery_jobs::Column::TelegraphRewriteError,
                            Expr::value(None::<String>),
                        )
                        .col_expr(
                            eh_gallery_jobs::Column::TelegraphRewrittenAt,
                            Expr::value(None::<DateTime>),
                        );
                }
                EhCleanupFinalizeOutcome::ReactivatedPending
            } else if has_active_delivery {
                // The archive is disabled and each active Telegraph delivery
                // can reuse the ready page, or no configured publish surface
                // remains. Finalize cleanup without making a source selector
                // claim this job; the publish transaction handles no-surface
                // deliveries as its existing terminal transition.
                update = update
                    .col_expr(
                        eh_gallery_jobs::Column::Status,
                        Expr::value(JOB_STATUS_DOWNLOADED),
                    )
                    .col_expr(eh_gallery_jobs::Column::Error, Expr::value(None::<String>))
                    .col_expr(eh_gallery_jobs::Column::RetryCount, Expr::value(0_i32))
                    .col_expr(
                        eh_gallery_jobs::Column::NextRetryAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::CompletedAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::BackgroundDownloadStatus,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::BackgroundDownloadStartedAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::BackgroundDownloadNextRetryAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::BackgroundDownloadAttemptCount,
                        Expr::value(0_i32),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::BackgroundDownloadError,
                        Expr::value(None::<String>),
                    )
                    .filter(no_active_eh_delivery_filter(job_id).not());
                EhCleanupFinalizeOutcome::FinalizedWithoutSourceWork
            } else if rewrite_in_progress {
                update = update.filter(no_active_eh_delivery_filter(job_id));
                EhCleanupFinalizeOutcome::RetainedForRewrite
            } else {
                update = update
                    .col_expr(
                        eh_gallery_jobs::Column::Status,
                        Expr::value(JOB_STATUS_RETIRED),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::TelegraphStatus,
                        Expr::value(TELEGRAPH_STATUS_NOT_REQUIRED),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::TelegraphRequired,
                        Expr::value(false),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::TelegraphUrl,
                        Expr::value(None::<String>),
                    )
                    .col_expr(eh_gallery_jobs::Column::Error, Expr::value(None::<String>))
                    .col_expr(eh_gallery_jobs::Column::RetryCount, Expr::value(0_i32))
                    .col_expr(
                        eh_gallery_jobs::Column::NextRetryAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::CompletedAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::BackgroundDownloadStatus,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::BackgroundDownloadStartedAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::BackgroundDownloadNextRetryAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::BackgroundDownloadAttemptCount,
                        Expr::value(0_i32),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::BackgroundDownloadError,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::TelegraphRewriteData,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::TelegraphRewriteStatus,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::TelegraphRewriteAfter,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::TelegraphRewriteStartedAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::TelegraphRewriteRetryCount,
                        Expr::value(0_i32),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::TelegraphRewriteError,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::TelegraphRewrittenAt,
                        Expr::value(None::<DateTime>),
                    )
                    .filter(no_active_eh_delivery_filter(job_id));
                EhCleanupFinalizeOutcome::CleanRetired
            };
            let finalized = update
                .exec(&txn)
                .await
                .context("Failed to finalize shared EH artifact cleanup")?;
            Ok((finalized.rows_affected == 1).then_some(outcome))
        }
        .await;
        match result {
            Ok(outcome) => txn
                .commit()
                .await
                .context("Failed to commit shared EH artifact cleanup finalization")
                .map(|()| outcome),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back shared EH artifact cleanup finalization")?;
                Err(error)
            }
        }
    }

    pub async fn eh_job_has_active_deliveries(&self, job_id: i32) -> Result<bool> {
        let deliveries = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::JobId.eq(job_id))
            .all(&self.db)
            .await
            .context("Failed to select shared EH job deliveries")?;
        Ok(deliveries
            .iter()
            .any(|delivery| is_active_delivery_status(&delivery.status)))
    }

    /// Claim one downloaded shared-gallery job for its single Telegraph upload.
    pub async fn get_next_eh_job_for_upload(&self) -> Result<Option<eh_gallery_jobs::Model>> {
        let now = Local::now().naive_local();
        let job = eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADED))
            .filter(eh_gallery_jobs::Column::TelegraphRequired.eq(true))
            .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_PENDING))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(
                eh_gallery_jobs::Column::NextRetryAt
                    .is_null()
                    .or(eh_gallery_jobs::Column::NextRetryAt.lte(now)),
            )
            .order_by(eh_gallery_jobs::Column::CreatedAt, Order::Asc)
            .order_by(eh_gallery_jobs::Column::Id, Order::Asc)
            .one(&self.db)
            .await
            .context("Failed to fetch next shared EH gallery job for upload")?;
        let Some(job) = job else {
            return Ok(None);
        };

        let generation = next_job_claim_generation(now, job.started_at)?;
        let claimed = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphStatus,
                Expr::value(TELEGRAPH_STATUS_UPLOADING),
            )
            .col_expr(eh_gallery_jobs::Column::StartedAt, Expr::value(generation))
            .col_expr(
                eh_gallery_jobs::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADED))
            .filter(eh_gallery_jobs::Column::TelegraphRequired.eq(true))
            .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_PENDING))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(job_claim_generation_filter(job.started_at))
            .filter(
                sea_orm::Condition::any()
                    .add(eh_gallery_jobs::Column::NextRetryAt.is_null())
                    .add(eh_gallery_jobs::Column::NextRetryAt.lte(now)),
            )
            .exec(&self.db)
            .await
            .context("Failed to atomically claim shared EH gallery upload")?;
        if claimed.rows_affected == 0 {
            return Ok(None);
        }

        // Do not re-check telegraph_required here. Cancellation after the CAS is
        // allowed to remove the final consumer, but it must not revoke this
        // generation or delete its multipart state while the upload is running.
        eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_UPLOADING))
            .filter(eh_gallery_jobs::Column::StartedAt.eq(generation))
            .one(&self.db)
            .await
            .context("Failed to reread shared EH gallery upload claim")
    }

    /// Persist the single Telegraph page and optional delayed-rewrite payload
    /// owned by a shared gallery job.
    pub async fn mark_eh_job_telegraph_ready(
        &self,
        job_id: i32,
        expected_started_at: DateTime,
        telegraph_url: &str,
        rewrite_data_json: Option<&str>,
        media_cids: Option<&str>,
        send_archive: bool,
    ) -> Result<eh_gallery_jobs::Model> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH Telegraph completion transaction")?;
        let result: Result<eh_gallery_jobs::Model> = async {
            let updated = eh_gallery_jobs::Entity::update_many()
                .col_expr(
                    eh_gallery_jobs::Column::TelegraphStatus,
                    Expr::value(TELEGRAPH_STATUS_READY),
                )
                .col_expr(
                    eh_gallery_jobs::Column::TelegraphUrl,
                    Expr::value(Some(telegraph_url.to_string())),
                )
                .col_expr(eh_gallery_jobs::Column::Error, Expr::value(None::<String>))
                .col_expr(
                    eh_gallery_jobs::Column::NextRetryAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_gallery_jobs::Column::TelegraphRewriteData,
                    Expr::value(rewrite_data_json.map(str::to_string)),
                )
                .col_expr(
                    eh_gallery_jobs::Column::TelegraphRewriteStatus,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_gallery_jobs::Column::TelegraphRewriteAfter,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_gallery_jobs::Column::TelegraphRewriteStartedAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_gallery_jobs::Column::TelegraphRewriteRetryCount,
                    Expr::value(0_i32),
                )
                .col_expr(
                    eh_gallery_jobs::Column::TelegraphRewriteError,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_gallery_jobs::Column::TelegraphRewrittenAt,
                    Expr::value(None::<DateTime>),
                )
                .filter(eh_gallery_jobs::Column::Id.eq(job_id))
                .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_UPLOADING))
                .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at))
                .exec(&txn)
                .await
                .context("Failed to mark shared EH gallery Telegraph page ready")?;
            if updated.rows_affected != 1 {
                anyhow::bail!(
                    "Cannot mark shared EH gallery job {} Telegraph-ready: upload claim changed concurrently",
                    job_id
                );
            }

            let completed_job = eh_gallery_jobs::Entity::find_by_id(job_id)
                .one(&txn)
                .await
                .context("Failed to read shared EH gallery job for result caching")?
                .context("Shared EH gallery job disappeared after Telegraph completion")?;
            if let (Some(fingerprint), Some(media_cids)) =
                (completed_job.source_fingerprint.as_deref(), media_cids)
            {
                let variant = EhGalleryVariant {
                    download_mode: completed_job.download_mode.clone(),
                    resolution: completed_job.resolution.clone(),
                };
                upsert_eh_gallery_result_in_txn(
                    &txn,
                    completed_job.gid,
                    &completed_job.token,
                    &variant,
                    fingerprint,
                    telegraph_url,
                    rewrite_data_json,
                    Some(media_cids),
                )
                .await?;
            }

            self.recompute_eh_job_telegraph_requirement_with_policy_in_txn(
                &txn,
                job_id,
                send_archive,
            )
                .await?;
            self.evaluate_eh_job_liveness_in_txn(&txn, job_id, send_archive)
                .await?;
            eh_gallery_jobs::Entity::find_by_id(job_id)
                .one(&txn)
                .await
                .context("Failed to reread settled shared EH gallery Telegraph completion")?
                .context("Shared EH gallery job disappeared after Telegraph completion")
        }
        .await;
        match result {
            Ok(job) => txn
                .commit()
                .await
                .context("Failed to commit shared EH Telegraph completion transaction")
                .map(|()| job),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back shared EH Telegraph completion transaction")?;
                Err(error)
            }
        }
    }

    /// Persist one delivery's sent marker and schedule the shared job's delayed
    /// Telegraph rewrite exactly once.
    pub async fn mark_eh_telegraph_delivery_sent(
        &self,
        delivery_id: i32,
        job_id: i32,
        rewrite_delay_secs: Option<i64>,
    ) -> Result<()> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin EH Telegraph sent-marker transaction")?;
        let result: Result<()> = async {
            let delivery = eh_download_queue::Entity::find()
                .filter(eh_download_queue::Column::Id.eq(delivery_id))
                .filter(eh_download_queue::Column::JobId.eq(job_id))
                .filter(eh_download_queue::Column::Telegraph.eq(true))
                .filter(eh_download_queue::Column::Status.eq(DELIVERY_STATUS_PUBLISHING))
                .one(&txn)
                .await
                .context("Failed to read shared EH Telegraph delivery for sent marker")?
                .context("Shared EH Telegraph delivery disappeared before sent marker")?;
            let now = Local::now().naive_local();
            let marked = eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::TelegraphSentAt,
                    Expr::value(Some(now)),
                )
                .filter(eh_download_queue::Column::Id.eq(delivery_id))
                .filter(eh_download_queue::Column::JobId.eq(job_id))
                .filter(eh_download_queue::Column::Telegraph.eq(true))
                .filter(eh_download_queue::Column::Status.eq(DELIVERY_STATUS_PUBLISHING))
                .filter(eh_download_queue::Column::TelegraphSentAt.is_null())
                .exec(&txn)
                .await
                .context("Failed to mark shared EH Telegraph delivery sent")?;
            let (sent_at, chat_id, gid) = if marked.rows_affected == 1 {
                (now, delivery.chat_id, delivery.gid)
            } else {
                let already_marked = eh_download_queue::Entity::find()
                    .filter(eh_download_queue::Column::Id.eq(delivery_id))
                    .filter(eh_download_queue::Column::JobId.eq(job_id))
                    .filter(eh_download_queue::Column::Telegraph.eq(true))
                    .filter(eh_download_queue::Column::Status.eq(DELIVERY_STATUS_PUBLISHING))
                    .filter(eh_download_queue::Column::TelegraphSentAt.is_not_null())
                    .one(&txn)
                    .await
                    .context("Failed to verify shared EH Telegraph sent marker")?;
                let Some(already_marked) = already_marked else {
                    anyhow::bail!(
                        "Cannot mark Telegraph sent for EH delivery {} on job {}: publishing claim changed",
                        delivery_id,
                        job_id
                    );
                };
                (
                    already_marked.telegraph_sent_at.context(
                        "Shared EH Telegraph sent marker disappeared during verification",
                    )?,
                    already_marked.chat_id,
                    already_marked.gid,
                )
            };
            record_eh_push_in_txn(
                &txn,
                chat_id,
                gid,
                EhPushSurface::Telegraph,
                sent_at,
            )
            .await?;

            if let Some(delay_secs) = rewrite_delay_secs {
                eh_gallery_jobs::Entity::update_many()
                    .col_expr(
                        eh_gallery_jobs::Column::TelegraphRewriteStatus,
                        Expr::value(Some(TELEGRAPH_REWRITE_STATUS_PENDING.to_string())),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::TelegraphRewriteAfter,
                        Expr::value(Some(
                            now.checked_add_signed(chrono::Duration::seconds(delay_secs))
                                .context("EH Telegraph rewrite deadline overflow")?,
                        )),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::TelegraphRewriteRetryCount,
                        Expr::value(0_i32),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::TelegraphRewriteError,
                        Expr::value(None::<String>),
                    )
                    .filter(eh_gallery_jobs::Column::Id.eq(job_id))
                    .filter(eh_gallery_jobs::Column::TelegraphRewriteData.is_not_null())
                    .filter(eh_gallery_jobs::Column::TelegraphRewriteStatus.is_null())
                    .filter(eh_gallery_jobs::Column::TelegraphRewriteAfter.is_null())
                    .filter(eh_gallery_jobs::Column::TelegraphRewriteStartedAt.is_null())
                    .filter(eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt.is_null())
                    .filter(eh_gallery_jobs::Column::TelegraphRewrittenAt.is_null())
                    .exec(&txn)
                    .await
                    .context("Failed to schedule shared EH Telegraph rewrite")?;
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => txn
                .commit()
                .await
                .context("Failed to commit EH Telegraph sent-marker transaction"),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back EH Telegraph sent-marker transaction")?;
                Err(error)
            }
        }
    }

    /// Claim one due shared Telegraph rewrite. The claim generation advances
    /// monotonically even after retry or stale-claim recovery.
    pub async fn get_next_eh_job_for_telegraph_rewrite(
        &self,
    ) -> Result<Option<eh_gallery_jobs::Model>> {
        let now = Local::now().naive_local();
        let job = eh_gallery_jobs::Entity::find()
            .filter(
                eh_gallery_jobs::Column::TelegraphRewriteStatus
                    .eq(TELEGRAPH_REWRITE_STATUS_PENDING),
            )
            .filter(eh_gallery_jobs::Column::TelegraphRewriteData.is_not_null())
            .filter(eh_gallery_jobs::Column::TelegraphRewrittenAt.is_null())
            .filter(
                sea_orm::Condition::any()
                    .add(eh_gallery_jobs::Column::TelegraphRewriteAfter.is_null())
                    .add(eh_gallery_jobs::Column::TelegraphRewriteAfter.lte(now)),
            )
            .filter(
                sea_orm::Condition::any()
                    .add(eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt.is_null())
                    .add(eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt.lte(now)),
            )
            .order_by(eh_gallery_jobs::Column::TelegraphRewriteAfter, Order::Asc)
            .one(&self.db)
            .await
            .context("Failed to fetch next shared EH Telegraph rewrite")?;
        let Some(job) = job else {
            return Ok(None);
        };

        let generation = next_job_claim_generation(now, job.telegraph_rewrite_started_at)?;
        let mut update = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteStatus,
                Expr::value(Some(TELEGRAPH_REWRITE_STATUS_REWRITING.to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteStartedAt,
                Expr::value(Some(generation)),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .filter(
                eh_gallery_jobs::Column::TelegraphRewriteStatus
                    .eq(TELEGRAPH_REWRITE_STATUS_PENDING),
            )
            .filter(eh_gallery_jobs::Column::TelegraphRewriteData.is_not_null())
            .filter(eh_gallery_jobs::Column::TelegraphRewrittenAt.is_null())
            .filter(
                sea_orm::Condition::any()
                    .add(eh_gallery_jobs::Column::TelegraphRewriteAfter.is_null())
                    .add(eh_gallery_jobs::Column::TelegraphRewriteAfter.lte(now)),
            )
            .filter(
                sea_orm::Condition::any()
                    .add(eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt.is_null())
                    .add(eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt.lte(now)),
            );
        update = update.filter(optional_job_datetime_filter(
            eh_gallery_jobs::Column::TelegraphRewriteStartedAt,
            job.telegraph_rewrite_started_at,
        ));
        let claimed = update
            .exec(&self.db)
            .await
            .context("Failed to atomically claim shared EH Telegraph rewrite")?;
        if claimed.rows_affected == 0 {
            return Ok(None);
        }

        eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .filter(
                eh_gallery_jobs::Column::TelegraphRewriteStatus
                    .eq(TELEGRAPH_REWRITE_STATUS_REWRITING),
            )
            .filter(eh_gallery_jobs::Column::TelegraphRewriteStartedAt.eq(generation))
            .one(&self.db)
            .await
            .context("Failed to reread shared EH Telegraph rewrite claim")
    }

    /// Complete a shared Telegraph rewrite only when the caller still owns the
    /// claimed generation. Payload removal is deferred to liveness evaluation.
    pub async fn mark_eh_job_telegraph_rewritten(
        &self,
        job_id: i32,
        expected_started_at: DateTime,
    ) -> Result<bool> {
        let now = Local::now().naive_local();
        let updated = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteAfter,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteRetryCount,
                Expr::value(0_i32),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteError,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewrittenAt,
                Expr::value(Some(now)),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(
                eh_gallery_jobs::Column::TelegraphRewriteStatus
                    .eq(TELEGRAPH_REWRITE_STATUS_REWRITING),
            )
            .filter(eh_gallery_jobs::Column::TelegraphRewriteStartedAt.eq(expected_started_at))
            .exec(&self.db)
            .await
            .context("Failed to complete shared EH Telegraph rewrite")?;
        Ok(updated.rows_affected == 1)
    }

    /// Retry a claimed shared Telegraph rewrite. Returns true only when the
    /// matching generation becomes terminally failed; retries and stale calls
    /// both return false and retain the rewrite payload.
    pub async fn schedule_eh_job_telegraph_rewrite_retry(
        &self,
        job_id: i32,
        expected_started_at: DateTime,
        error: &str,
        max_retry_count: u8,
    ) -> Result<bool> {
        let Some(job) = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&self.db)
            .await
            .context("Failed to fetch shared EH Telegraph rewrite for retry")?
        else {
            return Ok(false);
        };
        if job.telegraph_rewrite_status.as_deref() != Some(TELEGRAPH_REWRITE_STATUS_REWRITING)
            || job.telegraph_rewrite_started_at != Some(expected_started_at)
            || job.telegraph_rewrite_data.is_none()
            || job.telegraph_rewritten_at.is_some()
        {
            return Ok(false);
        }
        let retry_count = job
            .telegraph_rewrite_retry_count
            .checked_add(1)
            .context("Shared EH Telegraph rewrite retry count overflow")?;
        let terminal = retry_count > i32::from(max_retry_count);
        let now = Local::now().naive_local();

        let mut update = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteStatus,
                Expr::value(Some(
                    if terminal {
                        TELEGRAPH_REWRITE_STATUS_FAILED
                    } else {
                        TELEGRAPH_REWRITE_STATUS_PENDING
                    }
                    .to_string(),
                )),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt,
                Expr::value(if terminal {
                    None
                } else {
                    Some(
                        now.checked_add_signed(chrono::Duration::seconds(
                            Self::backoff_delay_secs(retry_count),
                        ))
                        .context("Shared EH Telegraph rewrite retry deadline overflow")?,
                    )
                }),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteRetryCount,
                Expr::value(retry_count),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteError,
                Expr::value(Some(error.to_string())),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(
                eh_gallery_jobs::Column::TelegraphRewriteStatus
                    .eq(TELEGRAPH_REWRITE_STATUS_REWRITING),
            )
            .filter(eh_gallery_jobs::Column::TelegraphRewriteData.is_not_null())
            .filter(eh_gallery_jobs::Column::TelegraphRewriteStartedAt.eq(expected_started_at));
        if terminal {
            update = update.filter(eh_gallery_jobs::Column::TelegraphRewrittenAt.is_null());
        }
        let updated = update
            .exec(&self.db)
            .await
            .context("Failed to persist shared EH Telegraph rewrite retry")?;
        Ok(terminal && updated.rows_affected == 1)
    }

    /// Record one failed shared Telegraph upload generation. Terminal failure
    /// returns only deliveries newly transitioned by this transaction, so a
    /// repeated or stale caller cannot fan out duplicate notifications.
    pub async fn record_eh_job_upload_failure(
        &self,
        job_id: i32,
        expected_started_at: DateTime,
        error: &str,
        max_retry_count: u8,
        send_archive: bool,
    ) -> Result<EhJobUploadFailureOutcome> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH gallery upload failure transaction")?;
        let Some(job) = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&txn)
            .await
            .context("Failed to select shared EH gallery job for upload failure")?
        else {
            txn.rollback().await?;
            return Ok(EhJobUploadFailureOutcome::Stale);
        };
        if job.telegraph_status != TELEGRAPH_STATUS_UPLOADING
            || job.started_at != Some(expected_started_at)
        {
            txn.rollback().await?;
            return Ok(EhJobUploadFailureOutcome::Stale);
        }

        let retry_count = job
            .retry_count
            .checked_add(1)
            .context("Shared EH gallery upload retry count overflow")?;
        if retry_count <= i32::from(max_retry_count) {
            let retry_at = Local::now().naive_local()
                + chrono::Duration::seconds(Self::backoff_delay_secs(retry_count));
            let updated = eh_gallery_jobs::Entity::update_many()
                .col_expr(
                    eh_gallery_jobs::Column::TelegraphStatus,
                    Expr::value(TELEGRAPH_STATUS_PENDING),
                )
                .col_expr(
                    eh_gallery_jobs::Column::Error,
                    Expr::value(Some(error.to_string())),
                )
                .col_expr(
                    eh_gallery_jobs::Column::RetryCount,
                    Expr::value(retry_count),
                )
                .col_expr(
                    eh_gallery_jobs::Column::NextRetryAt,
                    Expr::value(Some(retry_at)),
                )
                .filter(eh_gallery_jobs::Column::Id.eq(job_id))
                .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_UPLOADING))
                .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at))
                .exec(&txn)
                .await
                .context("Failed to schedule shared EH gallery upload retry")?;
            if updated.rows_affected != 1 {
                txn.rollback().await?;
                return Ok(EhJobUploadFailureOutcome::Stale);
            }
            self.recompute_eh_job_telegraph_requirement_with_policy_in_txn(
                &txn,
                job_id,
                send_archive,
            )
            .await?;
            self.evaluate_eh_job_liveness_in_txn(&txn, job_id, send_archive)
                .await?;
            let job = eh_gallery_jobs::Entity::find_by_id(job_id)
                .one(&txn)
                .await
                .context("Failed to reread shared EH gallery upload retry")?
                .context("Shared EH gallery job disappeared after upload retry")?;
            txn.commit()
                .await
                .context("Failed to commit shared EH gallery upload retry")?;
            return Ok(EhJobUploadFailureOutcome::RetryScheduled(job));
        }

        let failed = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphStatus,
                Expr::value(TELEGRAPH_STATUS_FAILED),
            )
            .col_expr(
                eh_gallery_jobs::Column::Error,
                Expr::value(Some(error.to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::RetryCount,
                Expr::value(retry_count),
            )
            .col_expr(
                eh_gallery_jobs::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_UPLOADING))
            .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at))
            .exec(&txn)
            .await
            .context("Failed to mark shared EH gallery upload terminally failed")?;
        if failed.rows_affected != 1 {
            txn.rollback().await?;
            return Ok(EhJobUploadFailureOutcome::Stale);
        }

        let candidates = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::JobId.eq(job_id))
            .filter(eh_download_queue::Column::Telegraph.eq(true))
            .filter(eh_download_queue::Column::TelegraphSentAt.is_null())
            .filter(
                eh_download_queue::Column::Status
                    .is_in([DELIVERY_STATUS_WAITING, DELIVERY_STATUS_PUBLISHING]),
            )
            .order_by(eh_download_queue::Column::Id, Order::Asc)
            .all(&txn)
            .await
            .context("Failed to select Telegraph deliveries for terminal upload failure")?;
        let now = Local::now().naive_local();
        let mut deliveries = Vec::with_capacity(candidates.len());
        for delivery in candidates {
            let mut transition = eh_download_queue::Entity::update_many()
                .filter(eh_download_queue::Column::Id.eq(delivery.id))
                .filter(eh_download_queue::Column::JobId.eq(job_id))
                .filter(eh_download_queue::Column::Telegraph.eq(true))
                .filter(eh_download_queue::Column::TelegraphSentAt.is_null())
                .filter(eh_download_queue::Column::Status.eq(&delivery.status));
            if send_archive {
                transition = transition
                    .col_expr(eh_download_queue::Column::Telegraph, Expr::value(false))
                    .col_expr(
                        eh_download_queue::Column::TelegraphSubscriptionIds,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::Status,
                        Expr::value(if delivery.archive_sent_at.is_some() {
                            DELIVERY_STATUS_DONE
                        } else {
                            DELIVERY_STATUS_WAITING
                        }),
                    )
                    .col_expr(
                        eh_download_queue::Column::NextRetryAt,
                        Expr::value(None::<DateTime>),
                    );
                if delivery.archive_sent_at.is_some() {
                    transition = transition.col_expr(
                        eh_download_queue::Column::CompletedAt,
                        Expr::value(Some(now)),
                    );
                } else {
                    transition = transition
                        .col_expr(
                            eh_download_queue::Column::StartedAt,
                            Expr::value(None::<DateTime>),
                        )
                        .col_expr(
                            eh_download_queue::Column::CompletedAt,
                            Expr::value(None::<DateTime>),
                        );
                }
            } else {
                transition = transition
                    .col_expr(
                        eh_download_queue::Column::Status,
                        Expr::value(DELIVERY_STATUS_FAILED),
                    )
                    .col_expr(
                        eh_download_queue::Column::CompletedAt,
                        Expr::value(Some(now)),
                    )
                    .col_expr(
                        eh_download_queue::Column::NextRetryAt,
                        Expr::value(None::<DateTime>),
                    );
            }
            let transitioned = transition
                .filter(optional_datetime_filter(
                    eh_download_queue::Column::ArchiveSentAt,
                    delivery.archive_sent_at,
                ))
                .exec(&txn)
                .await
                .context(
                    "Failed to transition one Telegraph delivery after terminal upload failure",
                )?;
            if transitioned.rows_affected == 1 {
                deliveries.push(EhFailedTelegraphDelivery {
                    delivery_id: delivery.id,
                    chat_id: delivery.chat_id,
                    title: delivery.title,
                });
            }
        }
        self.recompute_eh_job_telegraph_requirement_with_policy_in_txn(&txn, job_id, send_archive)
            .await?;
        self.evaluate_eh_job_liveness_in_txn(&txn, job_id, send_archive)
            .await?;

        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&txn)
            .await
            .context("Failed to reread terminally failed shared EH gallery upload")?
            .context("Shared EH gallery job disappeared after terminal upload failure")?;
        txn.commit()
            .await
            .context("Failed to commit terminal shared EH gallery upload failure")?;
        Ok(EhJobUploadFailureOutcome::Terminal { job, deliveries })
    }

    /// Release a claimed upload when no active destination can currently
    /// receive Telegraph. This is deliberately not a provider retry: it keeps
    /// the upload generation, retry budget, error, and artifact ownership.
    pub async fn defer_eh_job_upload(
        &self,
        job_id: i32,
        expected_started_at: DateTime,
        delay_secs: i64,
        send_archive: bool,
    ) -> Result<bool> {
        let retry_at = Local::now()
            .naive_local()
            .checked_add_signed(chrono::Duration::seconds(delay_secs.max(1)))
            .context("Shared EH gallery upload defer deadline overflow")?;
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH gallery upload defer transaction")?;
        let result: Result<bool> = async {
            let deferred = eh_gallery_jobs::Entity::update_many()
                .col_expr(
                    eh_gallery_jobs::Column::TelegraphStatus,
                    Expr::value(TELEGRAPH_STATUS_PENDING),
                )
                .col_expr(
                    eh_gallery_jobs::Column::NextRetryAt,
                    Expr::value(Some(retry_at)),
                )
                .filter(eh_gallery_jobs::Column::Id.eq(job_id))
                .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_UPLOADING))
                .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at))
                .exec(&txn)
                .await
                .context("Failed to defer shared EH gallery upload")?;
            if deferred.rows_affected != 1 {
                return Ok(false);
            }

            self.recompute_eh_job_telegraph_requirement_with_policy_in_txn(
                &txn,
                job_id,
                send_archive,
            )
            .await?;
            self.evaluate_eh_job_liveness_in_txn(&txn, job_id, send_archive)
                .await?;
            Ok(true)
        }
        .await;
        match result {
            Ok(deferred) => txn
                .commit()
                .await
                .context("Failed to commit shared EH gallery upload defer transaction")
                .map(|()| deferred),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back shared EH gallery upload defer transaction")?;
                Err(error)
            }
        }
    }

    /// Recover every shared-work lease at the one startup boundary.  Lease
    /// generations remain persisted so a subsequent claim always advances past
    /// a crashed worker; payloads, paths, and sent markers are deliberately
    /// left intact.
    pub async fn reset_stale_eh_shared_work(
        &self,
        background_stale_sec: u64,
        rewrite_stale_sec: i64,
    ) -> Result<EhStaleResetCounts> {
        let background_stale_sec = i64::try_from(background_stale_sec)
            .context("Shared EH background stale timeout exceeds supported range")?;
        let now = Local::now().naive_local();
        let background_cutoff = now
            .checked_sub_signed(chrono::Duration::seconds(background_stale_sec))
            .context("Shared EH background stale cutoff overflow")?;
        let rewrite_cutoff = now
            .checked_sub_signed(chrono::Duration::seconds(rewrite_stale_sec))
            .context("Shared EH rewrite stale cutoff overflow")?;
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH stale-work reset transaction")?;
        let result: Result<EhStaleResetCounts> = async {
            let downloads = eh_gallery_jobs::Entity::update_many()
                .col_expr(
                    eh_gallery_jobs::Column::Status,
                    Expr::value(JOB_STATUS_PENDING),
                )
                .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADING))
                .exec(&txn)
                .await
                .context("Failed to reset stale shared EH download claim")?;
            let stale_uploads = eh_gallery_jobs::Entity::find()
                .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_UPLOADING))
                .all(&txn)
                .await
                .context("Failed to select stale shared EH Telegraph upload claims")?;
            let mut uploads = 0u64;
            for stale_upload in stale_uploads {
                let telegraph_required =
                    has_active_eh_telegraph_delivery_in_txn(&txn, stale_upload.id).await?;
                let reset = eh_gallery_jobs::Entity::update_many()
                    .col_expr(
                        eh_gallery_jobs::Column::TelegraphStatus,
                        Expr::value(if telegraph_required {
                            TELEGRAPH_STATUS_PENDING
                        } else {
                            TELEGRAPH_STATUS_NOT_REQUIRED
                        }),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::TelegraphRequired,
                        Expr::value(telegraph_required),
                    )
                    .filter(eh_gallery_jobs::Column::Id.eq(stale_upload.id))
                    .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_UPLOADING))
                    .exec(&txn)
                    .await
                    .context("Failed to reset stale shared EH Telegraph upload claim")?;
                if reset.rows_affected != 1 {
                    continue;
                }
                uploads += 1;
                self.recompute_eh_job_telegraph_requirement_with_policy_in_txn(
                    &txn,
                    stale_upload.id,
                    true,
                )
                .await?;
                // Startup has no runtime archive policy, so retain any active
                // archive consumer conservatively. A consumerless stale upload
                // nevertheless becomes a durable job-owned cleanup generation.
                self.evaluate_eh_job_liveness_in_txn(&txn, stale_upload.id, true)
                    .await?;
            }
            let backgrounds = eh_gallery_jobs::Entity::update_many()
                .col_expr(
                    eh_gallery_jobs::Column::BackgroundDownloadStatus,
                    Expr::value(Some(BACKGROUND_STATUS_PENDING.to_string())),
                )
                .col_expr(
                    eh_gallery_jobs::Column::BackgroundDownloadStartedAt,
                    Expr::value(None::<DateTime>),
                )
                .filter(
                    eh_gallery_jobs::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING),
                )
                .filter(
                    eh_gallery_jobs::Column::BackgroundDownloadStartedAt
                        .is_null()
                        .or(eh_gallery_jobs::Column::BackgroundDownloadStartedAt
                            .lte(background_cutoff)),
                )
                .exec(&txn)
                .await
                .context("Failed to reset stale shared EH background claim")?;
            let rewrites = eh_gallery_jobs::Entity::update_many()
                .col_expr(
                    eh_gallery_jobs::Column::TelegraphRewriteStatus,
                    Expr::value(Some(TELEGRAPH_REWRITE_STATUS_PENDING.to_string())),
                )
                .filter(
                    eh_gallery_jobs::Column::TelegraphRewriteStatus
                        .eq(TELEGRAPH_REWRITE_STATUS_REWRITING),
                )
                .filter(eh_gallery_jobs::Column::TelegraphRewriteData.is_not_null())
                .filter(eh_gallery_jobs::Column::TelegraphRewrittenAt.is_null())
                .filter(
                    eh_gallery_jobs::Column::TelegraphRewriteStartedAt
                        .is_null()
                        .or(eh_gallery_jobs::Column::TelegraphRewriteStartedAt.lte(rewrite_cutoff)),
                )
                .exec(&txn)
                .await
                .context("Failed to reset stale shared EH Telegraph rewrite claim")?;
            let cleanups = eh_gallery_jobs::Entity::update_many()
                .col_expr(
                    eh_gallery_jobs::Column::CleanupStatus,
                    Expr::value(CLEANUP_STATUS_PENDING),
                )
                .col_expr(
                    eh_gallery_jobs::Column::CleanupNextRetryAt,
                    Expr::value(None::<DateTime>),
                )
                .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_RUNNING))
                .exec(&txn)
                .await
                .context("Failed to reset stale shared EH cleanup claim")?;
            let deliveries = eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::Status,
                    Expr::value(DELIVERY_STATUS_WAITING),
                )
                .filter(eh_download_queue::Column::Status.eq(DELIVERY_STATUS_PUBLISHING))
                .exec(&txn)
                .await
                .context("Failed to reset stale shared EH delivery publish claim")?;
            Ok(EhStaleResetCounts {
                downloads: downloads.rows_affected,
                uploads,
                backgrounds: backgrounds.rows_affected,
                rewrites: rewrites.rows_affected,
                cleanups: cleanups.rows_affected,
                deliveries: deliveries.rows_affected,
            })
        }
        .await;
        match result {
            Ok(counts) => txn
                .commit()
                .await
                .context("Failed to commit shared EH stale-work reset transaction")
                .map(|()| counts),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back shared EH stale-work reset transaction")?;
                Err(error)
            }
        }
    }

    /// Reconcile durable shared-job aggregates and liveness at startup without
    /// taking, resetting, or releasing any runtime claim. This is deliberately
    /// database-only: cleanup executors remain responsible for provider Abort
    /// and filesystem removal after this transaction commits.
    pub async fn reconcile_eh_shared_job_liveness(&self, send_archive: bool) -> Result<u64> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH job liveness reconciliation transaction")?;
        let result: Result<u64> = async {
            // A full scan is intentionally conservative. It covers historical
            // consumerless jobs that predate the current delivery/job atomic
            // transitions, including rows retained only for artifacts, rewrite
            // payload, or durable cleanup state.
            let jobs = eh_gallery_jobs::Entity::find()
                .order_by(eh_gallery_jobs::Column::Id, Order::Asc)
                .all(&txn)
                .await
                .context("Failed to select shared EH jobs for liveness reconciliation")?;
            for job in &jobs {
                self.recompute_eh_job_telegraph_requirement_with_policy_in_txn(
                    &txn,
                    job.id,
                    send_archive,
                )
                .await?;
                self.evaluate_eh_job_liveness_in_txn(&txn, job.id, send_archive)
                    .await?;
            }
            Ok(jobs.len() as u64)
        }
        .await;
        match result {
            Ok(reconciled) => txn
                .commit()
                .await
                .context("Failed to commit shared EH job liveness reconciliation transaction")
                .map(|()| reconciled),
            Err(error) => {
                txn.rollback().await.context(
                    "Failed to roll back shared EH job liveness reconciliation transaction",
                )?;
                Err(error)
            }
        }
    }

    /// When no Telegraph client exists, stop only job-level uploads that have
    /// not claimed an upload generation.  Ready links and all rewrite payloads
    /// are intentionally left untouched.
    pub async fn disable_eh_telegraph_for_unuploaded_jobs(&self) -> Result<u64> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH Telegraph disable transaction")?;
        let result: Result<u64> = async {
            let jobs = eh_gallery_jobs::Entity::find()
                .filter(eh_gallery_jobs::Column::TelegraphRequired.eq(true))
                .filter(
                    eh_gallery_jobs::Column::TelegraphStatus
                        .is_in([TELEGRAPH_STATUS_NOT_REQUIRED, TELEGRAPH_STATUS_PENDING]),
                )
                .order_by(eh_gallery_jobs::Column::Id, Order::Asc)
                .all(&txn)
                .await
                .context("Failed to select unclaimed shared EH Telegraph jobs")?;
            let mut changed = 0;
            for job in jobs {
                let deliveries = eh_download_queue::Entity::update_many()
                    .col_expr(eh_download_queue::Column::Telegraph, Expr::value(false))
                    .col_expr(
                        eh_download_queue::Column::TelegraphSubscriptionIds,
                        Expr::value(None::<String>),
                    )
                    .filter(eh_download_queue::Column::JobId.eq(job.id))
                    .filter(eh_download_queue::Column::Telegraph.eq(true))
                    .filter(eh_download_queue::Column::TelegraphSentAt.is_null())
                    .filter(
                        eh_download_queue::Column::Status
                            .is_in([DELIVERY_STATUS_WAITING, DELIVERY_STATUS_PUBLISHING]),
                    )
                    .exec(&txn)
                    .await
                    .context("Failed to remove unclaimed shared EH Telegraph delivery intent")?;
                if deliveries.rows_affected == 0 {
                    continue;
                }
                self.recompute_eh_job_telegraph_requirement_with_policy_in_txn(&txn, job.id, true)
                    .await?;
                changed += 1;
            }
            Ok(changed)
        }
        .await;
        match result {
            Ok(changed) => txn
                .commit()
                .await
                .context("Failed to commit shared EH Telegraph disable transaction")
                .map(|()| changed),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back shared EH Telegraph disable transaction")?;
                Err(error)
            }
        }
    }

    /// Hand a normal shared-gallery download claim to the background worker.
    /// The normal selector excludes every job with a background state, so the
    /// state transition is also the ownership handoff.
    pub async fn schedule_eh_job_background_download(
        &self,
        job_id: i32,
        expected_status: &str,
        error: &str,
    ) -> Result<eh_gallery_jobs::Model> {
        let now = Local::now().naive_local();
        let result = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::Status,
                Expr::value(JOB_STATUS_PENDING),
            )
            .col_expr(
                eh_gallery_jobs::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_gallery_jobs::Column::Error,
                Expr::value(Some(error.to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStatus,
                Expr::value(Some(BACKGROUND_STATUS_PENDING.to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadNextRetryAt,
                Expr::value(Some(now)),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadAttemptCount,
                Expr::value(0_i32),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadError,
                Expr::value(None::<String>),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(eh_gallery_jobs::Column::Status.eq(expected_status))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(eh_gallery_jobs::Column::BackgroundDownloadStatus.is_null())
            .exec(&self.db)
            .await
            .context("Failed to schedule shared EH gallery background download")?;
        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot schedule shared EH gallery job {} for background download: expected status '{}' without a background claim",
                job_id,
                expected_status
            );
        }

        eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&self.db)
            .await
            .context("Failed to reread shared EH gallery job after background handoff")?
            .context("Shared EH gallery job disappeared after background handoff")
    }

    /// Claim the next background-owned shared-gallery job. Its ordering matches
    /// the normal lane: recent jobs are FIFO and older jobs are LIFO.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn get_next_eh_job_for_background_download(
        &self,
    ) -> Result<Option<eh_gallery_jobs::Model>> {
        self.get_next_eh_job_for_background_download_with_policy(true)
            .await
    }

    /// Claim the next background-owned shared-gallery download job with the
    /// configured archive publish policy applied to every bound delivery.
    pub async fn get_next_eh_job_for_background_download_with_policy(
        &self,
        send_archive: bool,
    ) -> Result<Option<eh_gallery_jobs::Model>> {
        self.get_next_eh_job_for_background_download_at(Local::now().naive_local(), send_archive)
            .await
    }

    async fn get_next_eh_job_for_background_download_at(
        &self,
        now: DateTime,
        send_archive: bool,
    ) -> Result<Option<eh_gallery_jobs::Model>> {
        let cutoff = now - chrono::Duration::hours(MAIN_DOWNLOAD_RECENT_WINDOW_HOURS);
        let is_recent = Expr::col(eh_gallery_jobs::Column::CreatedAt).gt(cutoff);
        let recent_priority: SimpleExpr = Expr::case(is_recent.clone(), 0).finally(1).into();
        let recent_created_at: SimpleExpr = Expr::case(
            is_recent.clone(),
            Expr::col(eh_gallery_jobs::Column::CreatedAt),
        )
        .finally(Expr::value(None::<DateTime>))
        .into();
        let recent_id: SimpleExpr =
            Expr::case(is_recent.clone(), Expr::col(eh_gallery_jobs::Column::Id))
                .finally(Expr::value(None::<i32>))
                .into();
        let old_created_at: SimpleExpr =
            Expr::case(is_recent.clone(), Expr::value(None::<DateTime>))
                .finally(Expr::col(eh_gallery_jobs::Column::CreatedAt))
                .into();
        let old_id: SimpleExpr = Expr::case(is_recent, Expr::value(None::<i32>))
            .finally(Expr::col(eh_gallery_jobs::Column::Id))
            .into();
        let mut query = eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_PENDING))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(eh_gallery_jobs::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_PENDING))
            .filter(eh_job_has_configured_source_work_candidate_filter(
                send_archive,
            ))
            .filter(
                eh_gallery_jobs::Column::BackgroundDownloadNextRetryAt
                    .is_null()
                    .or(eh_gallery_jobs::Column::BackgroundDownloadNextRetryAt.lte(now)),
            );
        QueryTrait::query(&mut query)
            .order_by_expr(recent_priority, Order::Asc)
            .order_by_expr(recent_created_at, Order::Asc)
            .order_by_expr(recent_id, Order::Asc)
            .order_by_expr(old_created_at, Order::Desc)
            .order_by_expr(old_id, Order::Desc);
        let Some(job) = query
            .one(&self.db)
            .await
            .context("Failed to fetch next shared EH gallery background download")?
        else {
            return Ok(None);
        };

        self.claim_eh_job_background_download_from_snapshot_at(&job, now, send_archive)
            .await
    }

    async fn claim_eh_job_background_download_from_snapshot_at(
        &self,
        job: &eh_gallery_jobs::Model,
        now: DateTime,
        send_archive: bool,
    ) -> Result<Option<eh_gallery_jobs::Model>> {
        let generation = next_job_claim_generation(now, job.started_at)?;
        let lease_started_at = next_job_claim_generation(now, None)?;
        let result = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStatus,
                Expr::value(Some(BACKGROUND_STATUS_RUNNING.to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStartedAt,
                Expr::value(Some(lease_started_at)),
            )
            .col_expr(eh_gallery_jobs::Column::StartedAt, Expr::value(generation))
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_PENDING))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(eh_gallery_jobs::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_PENDING))
            .filter(eh_job_has_configured_source_work_filter(
                job.id,
                send_archive,
            ))
            .filter(job_claim_generation_filter(job.started_at))
            .filter(
                sea_orm::Condition::any()
                    .add(eh_gallery_jobs::Column::BackgroundDownloadNextRetryAt.is_null())
                    .add(eh_gallery_jobs::Column::BackgroundDownloadNextRetryAt.lte(now)),
            )
            .exec(&self.db)
            .await
            .context("Failed to atomically claim shared EH gallery background download")?;
        if result.rows_affected == 0 {
            return Ok(None);
        }

        eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_PENDING))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(eh_gallery_jobs::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING))
            .filter(eh_gallery_jobs::Column::StartedAt.eq(generation))
            .filter(eh_gallery_jobs::Column::BackgroundDownloadStartedAt.eq(lease_started_at))
            .one(&self.db)
            .await
            .context("Failed to reread shared EH gallery background claim")
    }

    /// Return a background-owned job to its own retry queue without consuming a
    /// retry attempt. Used by rate-limit and availability deferrals.
    pub async fn defer_eh_job_background_download(
        &self,
        job_id: i32,
        delay_secs: i64,
        reason: &str,
    ) -> Result<eh_gallery_jobs::Model> {
        let now = Local::now().naive_local();
        let result = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStatus,
                Expr::value(Some(BACKGROUND_STATUS_PENDING.to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadNextRetryAt,
                Expr::value(Some(now + chrono::Duration::seconds(delay_secs))),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadError,
                Expr::value(Some(reason.to_string())),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_PENDING))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(eh_gallery_jobs::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING))
            .exec(&self.db)
            .await
            .context("Failed to defer shared EH gallery background download")?;
        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot defer shared EH gallery background download {}: claim changed concurrently",
                job_id
            );
        }

        eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&self.db)
            .await
            .context("Failed to reread shared EH gallery job after background defer")?
            .context("Shared EH gallery job disappeared after background defer")
    }

    /// Complete a background download and append its immutable completion row in
    /// the same transaction. A stale or canceled claim commits neither change.
    pub async fn mark_eh_job_background_downloaded(
        &self,
        job_id: i32,
        expected_started_at: DateTime,
        file_size: i64,
        zip_path: &str,
        gp_cost: i64,
    ) -> Result<eh_gallery_jobs::Model> {
        anyhow::ensure!(
            file_size >= 0,
            "Shared EH gallery background download file size must be non-negative"
        );
        let now = Local::now().naive_local();
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH gallery background completion transaction")?;
        let telegraph_status: SimpleExpr = Expr::case(
            sea_orm::Condition::all()
                .add(eh_gallery_jobs::Column::TelegraphRequired.eq(true))
                .add(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_NOT_REQUIRED)),
            Expr::value(TELEGRAPH_STATUS_PENDING),
        )
        .finally(Expr::col(eh_gallery_jobs::Column::TelegraphStatus))
        .into();
        let updated = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::Status,
                Expr::value(JOB_STATUS_DOWNLOADED),
            )
            .col_expr(eh_gallery_jobs::Column::FileSize, Expr::value(file_size))
            .col_expr(eh_gallery_jobs::Column::GpCost, Expr::value(gp_cost))
            .col_expr(
                eh_gallery_jobs::Column::ZipPath,
                Expr::value(Some(zip_path.to_string())),
            )
            .col_expr(eh_gallery_jobs::Column::CompletedAt, Expr::value(now))
            .col_expr(eh_gallery_jobs::Column::Error, Expr::value(None::<String>))
            .col_expr(
                eh_gallery_jobs::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(eh_gallery_jobs::Column::TelegraphStatus, telegraph_status)
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadAttemptCount,
                Expr::value(0_i32),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadError,
                Expr::value(None::<String>),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_PENDING))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(eh_gallery_jobs::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING))
            .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at))
            .exec(&txn)
            .await
            .context("Failed to mark shared EH gallery background download complete")?;
        if updated.rows_affected != 1 {
            txn.rollback().await?;
            anyhow::bail!(
                "Cannot mark shared EH gallery job {} background downloaded: claim changed concurrently",
                job_id
            );
        }
        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&txn)
            .await
            .context(
                "Failed to reread shared EH gallery job before background completion ledger append",
            )?
            .context(
                "Shared EH gallery job disappeared before background completion ledger append",
            )?;
        crate::db::repo::eh_download_completions::append_eh_download_completion_in_txn(
            &txn, job.id, job.gid, file_size, now,
        )
        .await?;
        self.evaluate_eh_job_liveness_in_txn(&txn, job_id, true)
            .await?;
        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&txn)
            .await
            .context("Failed to reread settled shared EH gallery background completion")?
            .context("Shared EH gallery job disappeared after background completion settlement")?;
        txn.commit()
            .await
            .context("Failed to commit shared EH gallery background completion transaction")?;
        Ok(job)
    }

    /// Schedule the next retry for a failed background claim, or atomically
    /// fail all active deliveries when the background retry budget is exhausted.
    pub async fn schedule_eh_job_background_retry(
        &self,
        job_id: i32,
        expected_started_at: DateTime,
        error: &str,
        max_attempts: u8,
    ) -> Result<(eh_gallery_jobs::Model, bool)> {
        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&self.db)
            .await
            .context("Failed to fetch shared EH gallery job for background retry")?
            .context("Shared EH gallery job disappeared before background retry")?;
        let attempt_count = job
            .background_download_attempt_count
            .checked_add(1)
            .context("Shared EH gallery background attempt count overflow")?;
        if attempt_count >= i32::from(max_attempts) {
            return Ok((
                self.fail_eh_job_background_claim(job_id, expected_started_at, error, 0)
                    .await?,
                true,
            ));
        }

        let delay = Self::backoff_delay_secs(attempt_count);
        let now = Local::now().naive_local();
        let result = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStatus,
                Expr::value(Some(BACKGROUND_STATUS_PENDING.to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadNextRetryAt,
                Expr::value(Some(now + chrono::Duration::seconds(delay))),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadError,
                Expr::value(Some(error.to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadAttemptCount,
                Expr::value(attempt_count),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_PENDING))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(eh_gallery_jobs::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING))
            .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at))
            .exec(&self.db)
            .await
            .context("Failed to schedule shared EH gallery background retry")?;
        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot retry shared EH gallery background job {}: claim changed concurrently",
                job_id
            );
        }
        let updated = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&self.db)
            .await
            .context("Failed to reread shared EH gallery job after background retry")?
            .context("Shared EH gallery job disappeared after background retry")?;
        Ok((updated, false))
    }

    pub async fn fail_eh_job_background_download_for_archive_policy(
        &self,
        job: &eh_gallery_jobs::Model,
        error: &str,
    ) -> Result<eh_gallery_jobs::Model> {
        let started_at = job.started_at.context(
            "Cannot fail shared EH gallery background download for archive policy: missing claim started_at",
        )?;
        self.fail_eh_job_background_claim(job.id, started_at, error, 0)
            .await
    }

    async fn fail_eh_job_background_claim(
        &self,
        job_id: i32,
        expected_started_at: DateTime,
        error: &str,
        attempt_count: i32,
    ) -> Result<eh_gallery_jobs::Model> {
        let now = Local::now().naive_local();
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH gallery background failure transaction")?;
        let updated = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::Status,
                Expr::value(JOB_STATUS_FAILED),
            )
            .col_expr(
                eh_gallery_jobs::Column::Error,
                Expr::value(Some(error.to_string())),
            )
            .col_expr(eh_gallery_jobs::Column::CompletedAt, Expr::value(now))
            .col_expr(
                eh_gallery_jobs::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_gallery_jobs::Column::CleanupStatus,
                cleanup_pending_when_zip_owned_expr(),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadAttemptCount,
                Expr::value(attempt_count),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadError,
                Expr::value(None::<String>),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_PENDING))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(eh_gallery_jobs::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING))
            .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at))
            .exec(&txn)
            .await
            .context("Failed to fail shared EH gallery background job")?;
        if updated.rows_affected != 1 {
            txn.rollback().await?;
            anyhow::bail!(
                "Cannot fail shared EH gallery background job {}: claim changed concurrently",
                job_id
            );
        }
        fail_active_eh_job_deliveries_in_txn(&txn, job_id).await?;
        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&txn)
            .await
            .context("Failed to reread failed shared EH gallery background job")?
            .context("Shared EH gallery job disappeared after background failure")?;
        txn.commit()
            .await
            .context("Failed to commit shared EH gallery background failure transaction")?;
        Ok(job)
    }

    pub async fn release_eh_job_background_downloads_to_main_queue(&self) -> Result<u64> {
        let result = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadAttemptCount,
                Expr::value(0_i32),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadError,
                Expr::value(None::<String>),
            )
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_PENDING))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(eh_gallery_jobs::Column::BackgroundDownloadStatus.is_not_null())
            .exec(&self.db)
            .await
            .context("Failed to release shared EH gallery background downloads to main queue")?;
        Ok(result.rows_affected)
    }

    /// Reset one downloaded shared job when a publish worker discovers that its
    /// persisted ZIP has disappeared. The `(downloaded, expected generation,
    /// expected path)` CAS is the generation boundary: exactly one racing
    /// delivery advances the shared retry count, while all other callers
    /// observe `Stale` and leave it alone.
    ///
    /// Ready Telegraph state and rewrite state deliberately survive. They are
    /// shared work that remains valid even though the archive must be fetched
    /// again for delivery.
    ///
    /// Once the incremented retry count exceeds `max_retry_count`, the reset
    /// becomes terminal in the same transaction: the job fails with a stable
    /// error, its active deliveries fail once, and no further download is
    /// scheduled. The persisted ZIP path is kept so durable cleanup still owns
    /// any leftover `.part/.parts/.uploads` family members.
    pub async fn reset_eh_job_for_missing_zip(
        &self,
        job_id: i32,
        expected_started_at: DateTime,
        expected_zip_path: &str,
        max_retry_count: u8,
    ) -> Result<EhMissingZipResetOutcome> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin missing shared EH ZIP reset transaction")?;
        let result: Result<EhMissingZipResetOutcome> = async {
            let Some(job) = eh_gallery_jobs::Entity::find_by_id(job_id)
                .one(&txn)
                .await
                .context("Failed to fetch shared EH job for missing ZIP reset")?
            else {
                return Ok(EhMissingZipResetOutcome::Stale);
            };
            if job.status != JOB_STATUS_DOWNLOADED
                || job.started_at != Some(expected_started_at)
                || job.zip_path.as_deref() != Some(expected_zip_path)
                || job.telegraph_status == TELEGRAPH_STATUS_UPLOADING
            {
                return Ok(EhMissingZipResetOutcome::Stale);
            }
            let retry_count = job
                .retry_count
                .checked_add(1)
                .context("Shared EH missing ZIP retry count overflow")?;
            let now = Local::now().naive_local();
            if retry_count > i32::from(max_retry_count) {
                let exhausted = eh_gallery_jobs::Entity::update_many()
                    .col_expr(
                        eh_gallery_jobs::Column::Status,
                        Expr::value(JOB_STATUS_FAILED),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::Error,
                        Expr::value(Some("cached EH ZIP is missing".to_string())),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::RetryCount,
                        Expr::value(retry_count),
                    )
                    .col_expr(eh_gallery_jobs::Column::CompletedAt, Expr::value(now))
                    .col_expr(
                        eh_gallery_jobs::Column::NextRetryAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_gallery_jobs::Column::CleanupStatus,
                        cleanup_pending_when_zip_owned_expr(),
                    )
                    .filter(eh_gallery_jobs::Column::Id.eq(job_id))
                    .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADED))
                    .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at))
                    .filter(eh_gallery_jobs::Column::ZipPath.eq(expected_zip_path))
                    .filter(eh_gallery_jobs::Column::TelegraphStatus.ne(TELEGRAPH_STATUS_UPLOADING))
                    .exec(&txn)
                    .await
                    .context("Failed to fail shared EH job after missing ZIP retries")?;
                if exhausted.rows_affected == 0 {
                    return Ok(EhMissingZipResetOutcome::Stale);
                }
                // Terminal download failures match the ordinary claim-failure
                // path: active deliveries fail once, without erasing a
                // successfully sent surface marker.
                fail_active_eh_job_deliveries_in_txn(&txn, job_id).await?;
                return Ok(EhMissingZipResetOutcome::Exhausted);
            }
            let retry_at = now
                .checked_add_signed(chrono::Duration::seconds(Self::backoff_delay_secs(
                    retry_count,
                )))
                .context("Shared EH missing ZIP retry deadline overflow")?;
            let reset = eh_gallery_jobs::Entity::update_many()
                .col_expr(
                    eh_gallery_jobs::Column::Status,
                    Expr::value(JOB_STATUS_PENDING),
                )
                .col_expr(
                    eh_gallery_jobs::Column::ZipPath,
                    Expr::value(None::<String>),
                )
                .col_expr(eh_gallery_jobs::Column::FileSize, Expr::value(0_i64))
                .col_expr(eh_gallery_jobs::Column::GpCost, Expr::value(0_i64))
                .col_expr(
                    eh_gallery_jobs::Column::CompletedAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_gallery_jobs::Column::NextRetryAt,
                    Expr::value(Some(retry_at)),
                )
                .col_expr(
                    eh_gallery_jobs::Column::Error,
                    Expr::value(Some("cached EH ZIP is missing".to_string())),
                )
                .col_expr(
                    eh_gallery_jobs::Column::RetryCount,
                    Expr::value(retry_count),
                )
                .filter(eh_gallery_jobs::Column::Id.eq(job_id))
                .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADED))
                .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at))
                .filter(eh_gallery_jobs::Column::ZipPath.eq(expected_zip_path))
                .filter(eh_gallery_jobs::Column::TelegraphStatus.ne(TELEGRAPH_STATUS_UPLOADING))
                .exec(&txn)
                .await
                .context("Failed to reset shared EH job for missing ZIP")?;
            if reset.rows_affected == 0 {
                return Ok(EhMissingZipResetOutcome::Stale);
            }

            // Other archive consumers may already have claimed their delivery.
            // Put only marker-less archive work back into waiting; never erase a
            // successfully sent surface marker.
            eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::Status,
                    Expr::value(DELIVERY_STATUS_WAITING),
                )
                .col_expr(
                    eh_download_queue::Column::NextRetryAt,
                    Expr::value(None::<DateTime>),
                )
                .filter(eh_download_queue::Column::JobId.eq(job_id))
                .filter(eh_download_queue::Column::Status.eq(DELIVERY_STATUS_PUBLISHING))
                .filter(eh_download_queue::Column::ArchiveSentAt.is_null())
                .exec(&txn)
                .await
                .context("Failed to release shared EH archive deliveries after missing ZIP")?;
            Ok(EhMissingZipResetOutcome::Reset)
        }
        .await;
        match result {
            Ok(reset) => txn
                .commit()
                .await
                .context("Failed to commit missing shared EH ZIP reset transaction")
                .map(|()| reset),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back missing shared EH ZIP reset transaction")?;
                Err(error)
            }
        }
    }

    pub async fn get_active_eh_job_deliveries(
        &self,
        job_id: i32,
    ) -> Result<Vec<eh_download_queue::Model>> {
        let deliveries = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::JobId.eq(job_id))
            .all(&self.db)
            .await
            .context("Failed to select active shared EH job deliveries")?;
        Ok(deliveries
            .into_iter()
            .filter(|delivery| is_active_delivery_status(&delivery.status))
            .collect())
    }

    /// Persist the cleanup/retirement decision for a shared-gallery job.
    ///
    /// This method deliberately performs no filesystem work. Active rewrite
    /// payload is preserved; terminal rewrite payload may be discarded while
    /// cleanup execution remains a later, separately claimed maintenance step.
    #[allow(dead_code)] // Task 6/9 workers call this public liveness boundary.
    pub async fn evaluate_eh_job_liveness(
        &self,
        job_id: i32,
        send_archive: bool,
    ) -> Result<EhJobCleanupDecision> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH job liveness transaction")?;
        let result = self
            .evaluate_eh_job_liveness_in_txn(&txn, job_id, send_archive)
            .await;
        match result {
            Ok(decision) => txn
                .commit()
                .await
                .context("Failed to commit shared EH job liveness transaction")
                .map(|()| decision),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back shared EH job liveness transaction")?;
                Err(error)
            }
        }
    }

    pub(crate) async fn evaluate_eh_job_liveness_in_txn(
        &self,
        txn: &DatabaseTransaction,
        job_id: i32,
        send_archive: bool,
    ) -> Result<EhJobCleanupDecision> {
        const MAX_RETRIES: usize = 3;

        for attempt in 0..MAX_RETRIES {
            let job = eh_gallery_jobs::Entity::find_by_id(job_id)
                .one(txn)
                .await
                .context("Failed to select shared EH job for liveness evaluation")?
                .context("Shared EH job disappeared during liveness evaluation")?;
            let deliveries = eh_download_queue::Entity::find()
                .filter(eh_download_queue::Column::JobId.eq(job_id))
                .all(txn)
                .await
                .context("Failed to select shared EH deliveries for liveness evaluation")?;

            let has_active_delivery = deliveries
                .iter()
                .any(|delivery| is_active_delivery_status(&delivery.status));
            let rewrite_in_progress = matches!(
                job.telegraph_rewrite_status.as_deref(),
                Some(TELEGRAPH_REWRITE_STATUS_PENDING | TELEGRAPH_REWRITE_STATUS_REWRITING)
            );
            let rewrite_is_terminal = job.telegraph_rewritten_at.is_some()
                || job.telegraph_rewrite_status.as_deref() == Some(TELEGRAPH_REWRITE_STATUS_FAILED);
            let clear_rewrite_payload =
                rewrite_is_terminal && !rewrite_in_progress && job.telegraph_rewrite_data.is_some();
            let archive_still_needed = send_archive
                && deliveries.iter().any(|delivery| {
                    is_active_delivery_status(&delivery.status)
                        && delivery.archive_sent_at.is_none()
                });
            let upload_in_flight = job.telegraph_status == TELEGRAPH_STATUS_UPLOADING;
            let upload_still_needs_zip = upload_in_flight
                || (job.telegraph_status == TELEGRAPH_STATUS_PENDING && job.telegraph_required);
            let download_in_flight = job.status == JOB_STATUS_DOWNLOADING
                || job.background_download_status.as_deref() == Some(BACKGROUND_STATUS_RUNNING);
            let remove_archive_family = job.zip_path.is_some()
                && !archive_still_needed
                && !upload_still_needs_zip
                && !download_in_flight;
            let retire = !has_active_delivery
                && !rewrite_in_progress
                && !download_in_flight
                && !upload_in_flight;
            let cleanup_is_dirty = matches!(
                job.cleanup_status.as_str(),
                CLEANUP_STATUS_PENDING | CLEANUP_STATUS_RUNNING | CLEANUP_STATUS_FAILED
            );
            let cleanup_status = if remove_archive_family && !cleanup_is_dirty {
                CLEANUP_STATUS_PENDING
            } else {
                &job.cleanup_status
            };
            let status = if retire {
                JOB_STATUS_RETIRED
            } else {
                &job.status
            };
            let decision = EhJobCleanupDecision {
                job_id,
                zip_path: job.zip_path.clone(),
                retire,
                remove_archive_family,
                preserve_rewrite_payload: rewrite_in_progress,
            };

            if status == job.status
                && cleanup_status == job.cleanup_status
                && !clear_rewrite_payload
            {
                return Ok(decision);
            }

            let mut update = eh_gallery_jobs::Entity::update_many()
                .col_expr(
                    eh_gallery_jobs::Column::Status,
                    Expr::value(status.to_string()),
                )
                .col_expr(
                    eh_gallery_jobs::Column::CleanupStatus,
                    Expr::value(cleanup_status.to_string()),
                )
                .filter(eh_gallery_jobs::Column::Id.eq(job_id))
                .filter(eh_gallery_jobs::Column::Status.eq(&job.status))
                .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(&job.telegraph_status))
                .filter(eh_gallery_jobs::Column::CleanupStatus.eq(&job.cleanup_status));
            if clear_rewrite_payload {
                update = update.col_expr(
                    eh_gallery_jobs::Column::TelegraphRewriteData,
                    Expr::value(None::<String>),
                );
            }
            update = update.filter(optional_job_datetime_filter(
                eh_gallery_jobs::Column::StartedAt,
                job.started_at,
            ));
            update = update.filter(optional_job_string_filter(
                eh_gallery_jobs::Column::ZipPath,
                job.zip_path.as_deref(),
            ));
            update = update.filter(optional_job_string_filter(
                eh_gallery_jobs::Column::TelegraphRewriteStatus,
                job.telegraph_rewrite_status.as_deref(),
            ));
            update = update.filter(optional_job_string_filter(
                eh_gallery_jobs::Column::TelegraphRewriteData,
                job.telegraph_rewrite_data.as_deref(),
            ));
            update = update.filter(optional_job_datetime_filter(
                eh_gallery_jobs::Column::TelegraphRewrittenAt,
                job.telegraph_rewritten_at,
            ));
            if retire {
                update = update.filter(no_active_eh_delivery_filter(job_id));
            }

            maybe_fail_eh_job_liveness_update()?;
            let result = update
                .exec(txn)
                .await
                .context("Failed to persist shared EH job liveness decision")?;
            if result.rows_affected == 1 {
                return Ok(decision);
            }
            if attempt + 1 == MAX_RETRIES {
                anyhow::bail!(
                    "Shared EH job {} changed too frequently during liveness evaluation",
                    job_id
                );
            }
        }

        unreachable!("liveness retry loop always returns")
    }

    pub async fn defer_eh_job_download(&self, job_id: i32, delay_secs: i64) -> Result<()> {
        let result = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::Status,
                Expr::value(JOB_STATUS_PENDING),
            )
            .col_expr(
                eh_gallery_jobs::Column::NextRetryAt,
                Expr::value(Local::now().naive_local() + chrono::Duration::seconds(delay_secs)),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADING))
            .exec(&self.db)
            .await
            .context("Failed to defer shared EH gallery download")?;
        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot defer shared EH gallery job {}: expected downloading claim",
                job_id
            );
        }
        Ok(())
    }

    pub async fn schedule_eh_job_download_retry(
        &self,
        job_id: i32,
        expected_started_at: DateTime,
        error: &str,
        max_retry_count: u8,
    ) -> Result<(eh_gallery_jobs::Model, bool)> {
        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&self.db)
            .await
            .context("Failed to fetch shared EH gallery job for retry")?
            .context("Shared EH gallery job disappeared before retry")?;
        let retry_count = job
            .retry_count
            .checked_add(1)
            .context("Shared EH gallery job retry count overflow")?;
        if retry_count > i32::from(max_retry_count) {
            return Ok((
                self.fail_eh_job_download_claim(job_id, expected_started_at, error, retry_count)
                    .await?,
                true,
            ));
        }

        let delay = Self::backoff_delay_secs(retry_count);
        let result = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::Status,
                Expr::value(JOB_STATUS_PENDING),
            )
            .col_expr(
                eh_gallery_jobs::Column::NextRetryAt,
                Expr::value(Local::now().naive_local() + chrono::Duration::seconds(delay)),
            )
            .col_expr(
                eh_gallery_jobs::Column::Error,
                Expr::value(Some(error.to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::RetryCount,
                Expr::value(retry_count),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADING))
            .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at))
            .exec(&self.db)
            .await
            .context("Failed to schedule shared EH gallery download retry")?;
        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot retry shared EH gallery job {}: claim changed concurrently",
                job_id
            );
        }
        let updated = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&self.db)
            .await
            .context("Failed to reread shared EH gallery job after retry")?
            .context("Shared EH gallery job disappeared after retry")?;
        Ok((updated, false))
    }

    pub async fn fail_eh_job_for_archive_policy(
        &self,
        job: &eh_gallery_jobs::Model,
        error: &str,
    ) -> Result<eh_gallery_jobs::Model> {
        let started_at = job
            .started_at
            .context("Cannot fail shared EH gallery job for archive policy: missing download claim started_at")?;
        self.fail_eh_job_download_claim(job.id, started_at, error, job.retry_count)
            .await
    }

    pub async fn mark_eh_job_downloaded(
        &self,
        job_id: i32,
        expected_started_at: DateTime,
        file_size: i64,
        zip_path: &str,
        gp_cost: i64,
    ) -> Result<eh_gallery_jobs::Model> {
        anyhow::ensure!(
            file_size >= 0,
            "Shared EH gallery download file size must be non-negative"
        );
        let now = Local::now().naive_local();
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH gallery completion transaction")?;
        let telegraph_status: SimpleExpr = Expr::case(
            sea_orm::Condition::all()
                .add(eh_gallery_jobs::Column::TelegraphRequired.eq(true))
                .add(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_NOT_REQUIRED)),
            Expr::value(TELEGRAPH_STATUS_PENDING),
        )
        .finally(Expr::col(eh_gallery_jobs::Column::TelegraphStatus))
        .into();
        let updated = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::Status,
                Expr::value(JOB_STATUS_DOWNLOADED),
            )
            .col_expr(eh_gallery_jobs::Column::FileSize, Expr::value(file_size))
            .col_expr(eh_gallery_jobs::Column::GpCost, Expr::value(gp_cost))
            .col_expr(
                eh_gallery_jobs::Column::ZipPath,
                Expr::value(Some(zip_path.to_string())),
            )
            .col_expr(eh_gallery_jobs::Column::CompletedAt, Expr::value(now))
            .col_expr(eh_gallery_jobs::Column::Error, Expr::value(None::<String>))
            .col_expr(
                eh_gallery_jobs::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(eh_gallery_jobs::Column::TelegraphStatus, telegraph_status)
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADING))
            .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at))
            .exec(&txn)
            .await
            .context("Failed to mark shared EH gallery job downloaded")?;
        if updated.rows_affected != 1 {
            txn.rollback().await?;
            anyhow::bail!(
                "Cannot mark shared EH gallery job {} downloaded: claim changed concurrently",
                job_id
            );
        }
        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&txn)
            .await
            .context("Failed to reread shared EH gallery job before completion ledger append")?
            .context("Shared EH gallery job disappeared before completion ledger append")?;
        crate::db::repo::eh_download_completions::append_eh_download_completion_in_txn(
            &txn, job.id, job.gid, file_size, now,
        )
        .await?;
        self.evaluate_eh_job_liveness_in_txn(&txn, job_id, true)
            .await?;
        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&txn)
            .await
            .context("Failed to reread settled shared EH gallery completion")?
            .context("Shared EH gallery job disappeared after completion settlement")?;
        txn.commit()
            .await
            .context("Failed to commit shared EH gallery completion transaction")?;
        Ok(job)
    }

    pub(crate) async fn retire_eh_job_without_active_deliveries(
        &self,
        job: &eh_gallery_jobs::Model,
    ) -> Result<bool> {
        let started_at = job
            .started_at
            .context("Cannot retire shared EH gallery job: missing download claim started_at")?;
        if self.eh_job_has_active_deliveries(job.id).await? {
            return Ok(false);
        }
        let background_claimed = job.status == JOB_STATUS_PENDING
            && job.background_download_status.as_deref() == Some(BACKGROUND_STATUS_RUNNING);
        let status: SimpleExpr = Expr::case(
            sea_orm::Condition::any()
                .add(
                    eh_gallery_jobs::Column::TelegraphRewriteStatus
                        .eq(TELEGRAPH_REWRITE_STATUS_PENDING),
                )
                .add(
                    eh_gallery_jobs::Column::TelegraphRewriteStatus
                        .eq(TELEGRAPH_REWRITE_STATUS_REWRITING),
                ),
            Expr::value(JOB_STATUS_DOWNLOADED),
        )
        .finally(Expr::value(JOB_STATUS_RETIRED))
        .into();
        let mut update = eh_gallery_jobs::Entity::update_many()
            .col_expr(eh_gallery_jobs::Column::Status, status)
            .col_expr(
                eh_gallery_jobs::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_gallery_jobs::Column::CleanupStatus,
                cleanup_pending_when_zip_owned_expr(),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .filter(eh_gallery_jobs::Column::StartedAt.eq(started_at))
            .filter(no_active_eh_delivery_filter(job.id));
        if background_claimed {
            update = update
                .col_expr(
                    eh_gallery_jobs::Column::BackgroundDownloadStatus,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_gallery_jobs::Column::BackgroundDownloadStartedAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_gallery_jobs::Column::BackgroundDownloadNextRetryAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_gallery_jobs::Column::BackgroundDownloadAttemptCount,
                    Expr::value(0_i32),
                )
                .col_expr(
                    eh_gallery_jobs::Column::BackgroundDownloadError,
                    Expr::value(None::<String>),
                )
                .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_PENDING))
                .filter(
                    eh_gallery_jobs::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING),
                );
        } else {
            update = update
                .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADING))
                .filter(eh_gallery_jobs::Column::BackgroundDownloadStatus.is_null());
        }
        let result = update
            .exec(&self.db)
            .await
            .context("Failed to retire consumerless shared EH gallery job")?;
        Ok(result.rows_affected == 1)
    }

    async fn fail_eh_job_download_claim(
        &self,
        job_id: i32,
        expected_started_at: DateTime,
        error: &str,
        retry_count: i32,
    ) -> Result<eh_gallery_jobs::Model> {
        let now = Local::now().naive_local();
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH gallery failure transaction")?;
        let updated = eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::Status,
                Expr::value(JOB_STATUS_FAILED),
            )
            .col_expr(
                eh_gallery_jobs::Column::Error,
                Expr::value(Some(error.to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::RetryCount,
                Expr::value(retry_count),
            )
            .col_expr(eh_gallery_jobs::Column::CompletedAt, Expr::value(now))
            .col_expr(
                eh_gallery_jobs::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_gallery_jobs::Column::CleanupStatus,
                cleanup_pending_when_zip_owned_expr(),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADING))
            .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at))
            .exec(&txn)
            .await
            .context("Failed to fail shared EH gallery job")?;
        if updated.rows_affected != 1 {
            txn.rollback().await?;
            anyhow::bail!(
                "Cannot fail shared EH gallery job {}: claim changed concurrently",
                job_id
            );
        }
        fail_active_eh_job_deliveries_in_txn(&txn, job_id).await?;
        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&txn)
            .await
            .context("Failed to reread failed shared EH gallery job")?
            .context("Shared EH gallery job disappeared after failure")?;
        txn.commit()
            .await
            .context("Failed to commit shared EH gallery failure transaction")?;
        Ok(job)
    }
}

/// Transaction-local liveness evaluation used by enqueue/rebind paths.
///
/// Like the public evaluator, this only records durable cleanup intent and may
/// clear terminal rewrite payload; it never removes filesystem artifacts.
pub(crate) async fn retire_consumerless_eh_job_in_txn(
    txn: &DatabaseTransaction,
    job_id: i32,
) -> Result<EhJobCleanupDecision> {
    let job = eh_gallery_jobs::Entity::find_by_id(job_id)
        .one(txn)
        .await
        .context("Failed to select shared EH job for retirement")?
        .context("Shared EH job disappeared before retirement")?;
    let has_active_delivery = has_active_eh_delivery_in_txn(txn, job_id).await?;
    let preserve_rewrite_payload = matches!(
        job.telegraph_rewrite_status.as_deref(),
        Some(TELEGRAPH_REWRITE_STATUS_PENDING | TELEGRAPH_REWRITE_STATUS_REWRITING)
    );
    let clear_rewrite_payload = !preserve_rewrite_payload
        && job.telegraph_rewrite_data.is_some()
        && (job.telegraph_rewritten_at.is_some()
            || job.telegraph_rewrite_status.as_deref() == Some(TELEGRAPH_REWRITE_STATUS_FAILED));
    if let Some(background_status) = job.background_download_status.as_deref() {
        debug_assert!(matches!(
            background_status,
            BACKGROUND_STATUS_PENDING | BACKGROUND_STATUS_RUNNING
        ));
    }
    if let Some(rewrite_status) = job.telegraph_rewrite_status.as_deref() {
        debug_assert!(matches!(
            rewrite_status,
            TELEGRAPH_REWRITE_STATUS_PENDING
                | TELEGRAPH_REWRITE_STATUS_REWRITING
                | TELEGRAPH_REWRITE_STATUS_FAILED
        ));
    }
    if has_active_delivery {
        if clear_rewrite_payload {
            eh_gallery_jobs::Entity::update_many()
                .col_expr(
                    eh_gallery_jobs::Column::TelegraphRewriteData,
                    Expr::value(None::<String>),
                )
                .filter(eh_gallery_jobs::Column::Id.eq(job_id))
                .filter(optional_job_string_filter(
                    eh_gallery_jobs::Column::TelegraphRewriteStatus,
                    job.telegraph_rewrite_status.as_deref(),
                ))
                .filter(optional_job_string_filter(
                    eh_gallery_jobs::Column::TelegraphRewriteData,
                    job.telegraph_rewrite_data.as_deref(),
                ))
                .filter(optional_job_datetime_filter(
                    eh_gallery_jobs::Column::TelegraphRewrittenAt,
                    job.telegraph_rewritten_at,
                ))
                .exec(txn)
                .await
                .context("Failed to clear terminal shared EH rewrite payload")?;
        }
        return Ok(EhJobCleanupDecision {
            job_id,
            zip_path: job.zip_path,
            retire: false,
            remove_archive_family: false,
            preserve_rewrite_payload,
        });
    }

    let upload_in_flight = job.telegraph_status == TELEGRAPH_STATUS_UPLOADING;
    let download_in_flight = job.status == JOB_STATUS_DOWNLOADING
        || job.background_download_status.as_deref() == Some(BACKGROUND_STATUS_RUNNING);
    if download_in_flight || upload_in_flight {
        return Ok(EhJobCleanupDecision {
            job_id,
            zip_path: job.zip_path,
            retire: false,
            remove_archive_family: false,
            preserve_rewrite_payload,
        });
    }

    let upload_still_needs_zip = job.telegraph_status == TELEGRAPH_STATUS_UPLOADING
        || (job.telegraph_status == TELEGRAPH_STATUS_PENDING && job.telegraph_required);
    let remove_archive_family = job.zip_path.is_some() && !upload_still_needs_zip;
    let cleanup_is_dirty = matches!(
        job.cleanup_status.as_str(),
        CLEANUP_STATUS_PENDING | CLEANUP_STATUS_RUNNING | CLEANUP_STATUS_FAILED
    );
    let cleanup_status = if remove_archive_family && !cleanup_is_dirty {
        CLEANUP_STATUS_PENDING
    } else {
        &job.cleanup_status
    };
    let rewrite_in_progress = preserve_rewrite_payload;
    let retire = !rewrite_in_progress;
    let status = if retire {
        JOB_STATUS_RETIRED
    } else {
        &job.status
    };
    let mut update = eh_gallery_jobs::Entity::update_many()
        .col_expr(
            eh_gallery_jobs::Column::Status,
            Expr::value(status.to_string()),
        )
        .col_expr(
            eh_gallery_jobs::Column::CleanupStatus,
            Expr::value(cleanup_status.to_string()),
        )
        .filter(eh_gallery_jobs::Column::Id.eq(job_id));
    if clear_rewrite_payload {
        update = update.col_expr(
            eh_gallery_jobs::Column::TelegraphRewriteData,
            Expr::value(None::<String>),
        );
    }
    update
        .exec(txn)
        .await
        .context("Failed to retire consumerless shared EH job")?;
    Ok(EhJobCleanupDecision {
        job_id,
        zip_path: job.zip_path,
        retire,
        remove_archive_family,
        preserve_rewrite_payload,
    })
}

async fn has_active_eh_delivery_in_txn(txn: &DatabaseTransaction, job_id: i32) -> Result<bool> {
    let deliveries = eh_download_queue::Entity::find()
        .filter(eh_download_queue::Column::JobId.eq(job_id))
        .all(txn)
        .await
        .context("Failed to select shared EH job deliveries")?;
    Ok(deliveries
        .iter()
        .any(|delivery| is_active_delivery_status(&delivery.status)))
}

async fn has_active_eh_archive_delivery_in_txn(
    txn: &DatabaseTransaction,
    job_id: i32,
) -> Result<bool> {
    Ok(eh_download_queue::Entity::find()
        .filter(eh_download_queue::Column::JobId.eq(job_id))
        .filter(eh_download_queue::Column::ArchiveSentAt.is_null())
        .filter(eh_active_delivery_status_filter())
        .one(txn)
        .await
        .context("Failed to select active shared EH archive deliveries")?
        .is_some())
}

async fn has_active_eh_telegraph_delivery_in_txn(
    txn: &DatabaseTransaction,
    job_id: i32,
) -> Result<bool> {
    let deliveries = eh_download_queue::Entity::find()
        .filter(eh_download_queue::Column::JobId.eq(job_id))
        .filter(eh_download_queue::Column::Telegraph.eq(true))
        .filter(eh_download_queue::Column::TelegraphSentAt.is_null())
        .all(txn)
        .await
        .context("Failed to select shared EH Telegraph deliveries")?;
    Ok(deliveries
        .iter()
        .any(|delivery| is_active_delivery_status(&delivery.status)))
}

fn no_active_eh_delivery_filter(job_id: i32) -> SimpleExpr {
    Expr::exists(
        Query::select()
            .expr(Expr::value(1))
            .from(eh_download_queue::Entity)
            .and_where(eh_download_queue::Column::JobId.eq(job_id))
            .and_where(eh_download_queue::Column::Status.is_in([
                DELIVERY_STATUS_WAITING,
                "pending",
                "downloading",
                "downloaded",
                "uploading",
                "uploaded",
                DELIVERY_STATUS_PUBLISHING,
            ]))
            .to_owned(),
    )
    .not()
}

fn eh_job_has_configured_source_work_filter(job_id: i32, send_archive: bool) -> SimpleExpr {
    Expr::exists(
        Query::select()
            .expr(Expr::value(1))
            .from(eh_download_queue::Entity)
            .and_where(eh_download_queue::Column::JobId.eq(job_id))
            .and_where(eh_active_delivery_status_filter())
            .and_where(eh_configured_source_surface_filter(send_archive))
            .to_owned(),
    )
}

fn eh_job_has_configured_source_work_candidate_filter(send_archive: bool) -> SimpleExpr {
    eh_gallery_jobs::Column::Id.in_subquery(
        Query::select()
            .column(eh_download_queue::Column::JobId)
            .from(eh_download_queue::Entity)
            .and_where(eh_active_delivery_status_filter())
            .and_where(eh_configured_source_surface_filter(send_archive))
            .to_owned(),
    )
}

fn eh_active_delivery_status_filter() -> SimpleExpr {
    eh_download_queue::Column::Status.is_in([
        DELIVERY_STATUS_WAITING,
        "pending",
        "downloading",
        "downloaded",
        "uploading",
        "uploaded",
        DELIVERY_STATUS_PUBLISHING,
    ])
}

fn eh_configured_source_surface_filter(send_archive: bool) -> SimpleExpr {
    let mut source_surface = sea_orm::Condition::any();
    if send_archive {
        source_surface = source_surface.add(eh_download_queue::Column::ArchiveSentAt.is_null());
    }
    source_surface = source_surface.add(
        sea_orm::Condition::all()
            .add(eh_download_queue::Column::Telegraph.eq(true))
            .add(eh_download_queue::Column::TelegraphSentAt.is_null()),
    );
    source_surface.into()
}

fn cleanup_pending_when_zip_owned_expr() -> SimpleExpr {
    Expr::case(
        sea_orm::Condition::all()
            .add(eh_gallery_jobs::Column::ZipPath.is_not_null())
            .add(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE)),
        Expr::value(CLEANUP_STATUS_PENDING),
    )
    .finally(Expr::col(eh_gallery_jobs::Column::CleanupStatus))
    .into()
}

fn optional_job_string_filter(column: eh_gallery_jobs::Column, value: Option<&str>) -> SimpleExpr {
    match value {
        Some(value) => column.eq(value),
        None => column.is_null(),
    }
}

fn optional_job_datetime_filter(
    column: eh_gallery_jobs::Column,
    value: Option<DateTime>,
) -> SimpleExpr {
    match value {
        Some(value) => column.eq(value),
        None => column.is_null(),
    }
}

async fn fail_active_eh_job_deliveries_in_txn(
    txn: &DatabaseTransaction,
    job_id: i32,
) -> Result<()> {
    eh_download_queue::Entity::update_many()
        .col_expr(
            eh_download_queue::Column::Status,
            Expr::value(DELIVERY_STATUS_FAILED),
        )
        .filter(eh_download_queue::Column::JobId.eq(job_id))
        .filter(eh_download_queue::Column::Status.is_in([
            DELIVERY_STATUS_WAITING,
            DELIVERY_STATUS_PUBLISHING,
            "pending",
            "downloading",
            "downloaded",
            "uploading",
            "uploaded",
        ]))
        .exec(txn)
        .await
        .context("Failed to fail active shared EH job deliveries")?;
    Ok(())
}

fn next_job_claim_generation(now: DateTime, previous: Option<DateTime>) -> Result<DateTime> {
    let now_second = now
        .with_nanosecond(0)
        .context("Cannot normalize shared EH gallery claim generation timestamp")?;
    let Some(previous) = previous else {
        return Ok(now_second);
    };
    let previous_second = previous
        .with_nanosecond(0)
        .context("Cannot normalize previous shared EH gallery claim generation timestamp")?;
    let following_generation = previous_second
        .checked_add_signed(chrono::Duration::seconds(1))
        .context("Shared EH gallery claim generation timestamp overflow")?;
    Ok(now_second.max(following_generation))
}

fn job_claim_generation_filter(previous: Option<DateTime>) -> sea_orm::Condition {
    match previous {
        Some(generation) => {
            sea_orm::Condition::all().add(eh_gallery_jobs::Column::StartedAt.eq(generation))
        }
        None => sea_orm::Condition::all().add(eh_gallery_jobs::Column::StartedAt.is_null()),
    }
}

fn cleanup_claim_generation_filter(previous: Option<DateTime>) -> sea_orm::Condition {
    match previous {
        Some(generation) => {
            sea_orm::Condition::all().add(eh_gallery_jobs::Column::CleanupStartedAt.eq(generation))
        }
        None => sea_orm::Condition::all().add(eh_gallery_jobs::Column::CleanupStartedAt.is_null()),
    }
}

fn is_terminal_delivery_status(status: &str) -> bool {
    matches!(
        status,
        DELIVERY_STATUS_DONE | DELIVERY_STATUS_FAILED | DELIVERY_STATUS_CANCELED
    )
}

fn is_active_delivery_status(status: &str) -> bool {
    if is_terminal_delivery_status(status) {
        return false;
    }
    matches!(
        status,
        DELIVERY_STATUS_WAITING
            | "pending"
            | "downloading"
            | "downloaded"
            | "uploading"
            | "uploaded"
            | DELIVERY_STATUS_PUBLISHING
    )
}

async fn reset_eh_gallery_job_generation_in_txn(
    txn: &DatabaseTransaction,
    job_id: i32,
    title: &str,
    fingerprint: Option<&str>,
) -> Result<()> {
    let mut update = eh_gallery_jobs::Entity::update_many()
        .col_expr(
            eh_gallery_jobs::Column::Status,
            Expr::value(JOB_STATUS_PENDING),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphStatus,
            Expr::value(TELEGRAPH_STATUS_NOT_REQUIRED),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRequired,
            Expr::value(false),
        )
        .col_expr(eh_gallery_jobs::Column::FileSize, Expr::value(0_i64))
        .col_expr(eh_gallery_jobs::Column::GpCost, Expr::value(0_i64))
        .col_expr(
            eh_gallery_jobs::Column::ZipPath,
            Expr::value(None::<String>),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphUrl,
            Expr::value(None::<String>),
        )
        .col_expr(eh_gallery_jobs::Column::Error, Expr::value(None::<String>))
        .col_expr(eh_gallery_jobs::Column::RetryCount, Expr::value(0_i32))
        .col_expr(
            eh_gallery_jobs::Column::NextRetryAt,
            Expr::value(None::<chrono::NaiveDateTime>),
        )
        .col_expr(
            eh_gallery_jobs::Column::CompletedAt,
            Expr::value(None::<chrono::NaiveDateTime>),
        )
        .col_expr(
            eh_gallery_jobs::Column::BackgroundDownloadStatus,
            Expr::value(None::<String>),
        )
        .col_expr(
            eh_gallery_jobs::Column::BackgroundDownloadStartedAt,
            Expr::value(None::<chrono::NaiveDateTime>),
        )
        .col_expr(
            eh_gallery_jobs::Column::BackgroundDownloadNextRetryAt,
            Expr::value(None::<chrono::NaiveDateTime>),
        )
        .col_expr(
            eh_gallery_jobs::Column::BackgroundDownloadAttemptCount,
            Expr::value(0_i32),
        )
        .col_expr(
            eh_gallery_jobs::Column::BackgroundDownloadError,
            Expr::value(None::<String>),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRewriteData,
            Expr::value(None::<String>),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRewriteStatus,
            Expr::value(None::<String>),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRewriteAfter,
            Expr::value(None::<chrono::NaiveDateTime>),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRewriteStartedAt,
            Expr::value(None::<chrono::NaiveDateTime>),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt,
            Expr::value(None::<chrono::NaiveDateTime>),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRewriteRetryCount,
            Expr::value(0_i32),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRewriteError,
            Expr::value(None::<String>),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRewrittenAt,
            Expr::value(None::<chrono::NaiveDateTime>),
        )
        .col_expr(
            eh_gallery_jobs::Column::CleanupError,
            Expr::value(None::<String>),
        )
        .col_expr(
            eh_gallery_jobs::Column::CleanupNextRetryAt,
            Expr::value(None::<chrono::NaiveDateTime>),
        )
        .filter(eh_gallery_jobs::Column::Id.eq(job_id))
        .filter(optional_job_string_filter(
            eh_gallery_jobs::Column::SourceFingerprint,
            fingerprint,
        ));
    if !title.is_empty() {
        update = update.col_expr(
            eh_gallery_jobs::Column::Title,
            Expr::value(title.to_string()),
        );
    }
    let result = update
        .exec(txn)
        .await
        .context("Failed to reactivate shared EH gallery job")?;
    anyhow::ensure!(
        result.rows_affected == 1,
        "Shared EH job {job_id} changed identity while reactivating its generation"
    );
    Ok(())
}

fn optional_i32_filter(
    column: eh_download_queue::Column,
    value: Option<i32>,
) -> sea_orm::sea_query::SimpleExpr {
    match value {
        Some(value) => column.eq(value),
        None => column.is_null(),
    }
}

fn optional_string_filter(
    column: eh_download_queue::Column,
    value: Option<&str>,
) -> sea_orm::sea_query::SimpleExpr {
    match value {
        Some(value) => column.eq(value),
        None => column.is_null(),
    }
}

fn optional_datetime_filter(
    column: eh_download_queue::Column,
    value: Option<chrono::NaiveDateTime>,
) -> sea_orm::sea_query::SimpleExpr {
    match value {
        Some(value) => column.eq(value),
        None => column.is_null(),
    }
}

fn is_retryable_enqueue_error(error: &anyhow::Error) -> bool {
    let message = error
        .chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    message.contains("unique constraint")
        || message.contains("duplicate key")
        || message.contains("database is locked")
        || message.contains("database is busy")
        || message.contains("serialization")
        || message.contains("changed concurrently")
}

fn is_retryable_enqueue_db_error(error: &sea_orm::DbErr) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("unique constraint")
        || message.contains("duplicate key")
        || message.contains("database is locked")
        || message.contains("database is busy")
        || message.contains("serialization")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::entities::{eh_download_completions, eh_gallery_push_ledger};
    use crate::db::repo::eh_gallery_results::{
        find_eh_gallery_result_in_txn, upsert_eh_gallery_result_in_txn,
    };
    use crate::db::repo::tests_helpers;
    use sea_orm::{
        sea_query::Expr, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, TransactionTrait,
    };

    async fn seed_cached_result(
        repo: &Repo,
        gid: i64,
        token: &str,
        variant: &EhGalleryVariant,
        fingerprint: &str,
        telegraph_url: &str,
        rewrite_data: Option<&str>,
    ) {
        let txn = repo.db().begin().await.unwrap();
        upsert_eh_gallery_result_in_txn(
            &txn,
            gid,
            token,
            variant,
            fingerprint,
            telegraph_url,
            rewrite_data,
            Some("[{\"name\":\"001.jpg\",\"cid\":\"bafk-cached\"}]"),
        )
        .await
        .unwrap();
        txn.commit().await.unwrap();
    }

    async fn load_eh_job(repo: &Repo, job_id: i32) -> eh_gallery_jobs::Model {
        eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap()
    }

    async fn seed_push_ledger(
        repo: &Repo,
        chat_id: i64,
        gid: i64,
        archive_sent_at: Option<DateTime>,
        telegraph_sent_at: Option<DateTime>,
    ) -> eh_gallery_push_ledger::Model {
        eh_gallery_push_ledger::ActiveModel {
            chat_id: Set(chat_id),
            gid: Set(gid),
            archive_sent_at: Set(archive_sent_at),
            telegraph_sent_at: Set(telegraph_sent_at),
            updated_at: Set(Local::now().naive_local()),
            ..Default::default()
        }
        .insert(repo.db())
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn subscription_enqueue_skips_fully_delivered_gallery() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let archive_sent_at = Local::now().naive_local();
        let telegraph_sent_at = archive_sent_at + chrono::Duration::seconds(1);
        seed_push_ledger(
            &repo,
            -100,
            8801,
            Some(archive_sent_at),
            Some(telegraph_sent_at),
        )
        .await;

        let delivery = repo
            .enqueue_eh_subscription_download(
                -100,
                101,
                8801,
                "token",
                "Delivered Gallery",
                true,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap();

        assert!(delivery.is_none());
        assert_eq!(
            eh_download_queue::Entity::find()
                .count(repo.db())
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            eh_gallery_jobs::Entity::find()
                .count(repo.db())
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn subscription_enqueue_pre_marks_archive_only_surface() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let archive_sent_at = Local::now().naive_local();
        seed_push_ledger(&repo, -100, 8802, Some(archive_sent_at), None).await;

        let delivery = repo
            .enqueue_eh_subscription_download(
                -100,
                101,
                8802,
                "token",
                "Archive delivered Gallery",
                true,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("a missing Telegraph surface must create a delivery wave");
        assert_eq!(delivery.archive_sent_at, Some(archive_sent_at));
        assert!(delivery.telegraph_sent_at.is_none());

        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::Status,
                Expr::value(JOB_STATUS_DOWNLOADED),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphStatus,
                Expr::value(TELEGRAPH_STATUS_READY),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphUrl,
                Expr::value(Some("https://telegra.ph/8802".to_string())),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(delivery.job_id.unwrap()))
            .exec(repo.db())
            .await
            .unwrap();
        let claimed = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .expect("only the missing Telegraph surface should be claimable");
        assert_eq!(claimed.delivery.id, delivery.id);
        assert_eq!(claimed.delivery.archive_sent_at, Some(archive_sent_at));
        assert!(claimed.delivery.telegraph_sent_at.is_none());
        repo.mark_eh_telegraph_delivery_sent(claimed.delivery.id, claimed.job.id, None)
            .await
            .unwrap();
        let ledger = eh_gallery_push_ledger::Entity::find()
            .filter(eh_gallery_push_ledger::Column::ChatId.eq(-100))
            .filter(eh_gallery_push_ledger::Column::Gid.eq(8802))
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ledger.archive_sent_at, Some(archive_sent_at));
        assert!(ledger.telegraph_sent_at.is_some());
    }

    #[tokio::test]
    async fn subscription_enqueue_pre_marks_telegraph_only_surface() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let telegraph_sent_at = Local::now().naive_local();
        seed_push_ledger(&repo, -100, 8803, None, Some(telegraph_sent_at)).await;

        let delivery = repo
            .enqueue_eh_subscription_download(
                -100,
                101,
                8803,
                "token",
                "Telegraph delivered Gallery",
                true,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("a missing archive surface must create a delivery wave");
        assert!(delivery.archive_sent_at.is_none());
        assert_eq!(delivery.telegraph_sent_at, Some(telegraph_sent_at));
        let job_id = delivery.job_id.unwrap();
        assert!(!load_eh_job(&repo, job_id).await.telegraph_required);
        assert!(repo.get_next_eh_job_for_upload().await.unwrap().is_none());

        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::Status,
                Expr::value(JOB_STATUS_DOWNLOADED),
            )
            .col_expr(
                eh_gallery_jobs::Column::ZipPath,
                Expr::value(Some("shared-8803.zip".to_string())),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .exec(repo.db())
            .await
            .unwrap();
        let claimed = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .expect("only the missing archive surface should be claimable");
        assert_eq!(claimed.delivery.id, delivery.id);
        assert!(claimed.delivery.archive_sent_at.is_none());
        assert_eq!(claimed.delivery.telegraph_sent_at, Some(telegraph_sent_at));
        repo.mark_eh_archive_delivery_sent(claimed.delivery.id)
            .await
            .unwrap();
        let ledger = eh_gallery_push_ledger::Entity::find()
            .filter(eh_gallery_push_ledger::Column::ChatId.eq(-100))
            .filter(eh_gallery_push_ledger::Column::Gid.eq(8803))
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert!(ledger.archive_sent_at.is_some());
        assert_eq!(ledger.telegraph_sent_at, Some(telegraph_sent_at));
    }

    #[tokio::test]
    async fn direct_enqueue_bypasses_ledger_dedup() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let sent_at = Local::now().naive_local();
        seed_push_ledger(&repo, -100, 8804, Some(sent_at), Some(sent_at)).await;

        let delivery = repo
            .enqueue_eh_download(
                -100,
                8804,
                "token",
                "Direct Gallery",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("direct enqueue must not be suppressed by the subscription ledger");
        assert!(delivery.archive_sent_at.is_none());
        assert!(delivery.telegraph_sent_at.is_none());
        assert_eq!(delivery.source, SOURCE_DIRECT);
    }

    #[tokio::test]
    async fn active_wave_merges_without_premark() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let first = repo
            .enqueue_eh_subscription_download(
                -100,
                101,
                8805,
                "token",
                "Active Gallery",
                true,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("first subscription enqueue must create a delivery");
        let archive_sent_at = Local::now().naive_local();
        seed_push_ledger(&repo, -100, 8805, Some(archive_sent_at), None).await;

        let merged = repo
            .enqueue_eh_subscription_download(
                -100,
                202,
                8805,
                "token",
                "Active Gallery",
                true,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("a partially ledgered active wave must merge its owner");
        assert_eq!(merged.id, first.id);
        assert!(merged.archive_sent_at.is_none());
        assert!(merged.telegraph_sent_at.is_none());
        assert_eq!(merged.subscription_ids.as_deref(), Some("101,202"));
    }

    async fn seed_rewrite_ready_job(
        repo: &Repo,
        gid: i64,
    ) -> (eh_gallery_jobs::Model, eh_download_queue::Model) {
        let delivery = repo
            .enqueue_eh_download(
                -100,
                gid,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let job_id = delivery.job_id.unwrap();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::Status,
                Expr::value(JOB_STATUS_DOWNLOADED),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphStatus,
                Expr::value(TELEGRAPH_STATUS_READY),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphUrl,
                Expr::value(Some("https://telegra.ph/Gallery".to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteData,
                Expr::value(Some("{\"pages\":[]}".to_string())),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .exec(repo.db())
            .await
            .unwrap();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_PUBLISHING),
            )
            .filter(eh_download_queue::Column::Id.eq(delivery.id))
            .exec(repo.db())
            .await
            .unwrap();
        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let delivery = eh_download_queue::Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        (job, delivery)
    }

    async fn set_active_telegraph_rewrite(
        repo: &Repo,
        job_id: i32,
        rewrite_status: &str,
    ) -> eh_gallery_jobs::Model {
        let now = Local::now().naive_local();
        let rewrite_after = now + chrono::Duration::seconds(10);
        let retry_at = now + chrono::Duration::seconds(20);
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphStatus,
                Expr::value(TELEGRAPH_STATUS_READY),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphUrl,
                Expr::value(Some("https://telegra.ph/rewrite".to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteData,
                Expr::value(Some("{\"pages\":[]}".to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteStatus,
                Expr::value(Some(rewrite_status.to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteAfter,
                Expr::value(Some(rewrite_after)),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteStartedAt,
                Expr::value(Some(now)),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt,
                Expr::value(Some(retry_at)),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteRetryCount,
                Expr::value(2_i32),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteError,
                Expr::value(Some("temporary rewrite error".to_string())),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .exec(repo.db())
            .await
            .unwrap();
        eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap()
    }

    fn assert_telegraph_rewrite_state_preserved(
        expected: &eh_gallery_jobs::Model,
        actual: &eh_gallery_jobs::Model,
    ) {
        assert_eq!(actual.telegraph_status, expected.telegraph_status);
        assert_eq!(actual.telegraph_url, expected.telegraph_url);
        assert_eq!(
            actual.telegraph_rewrite_data,
            expected.telegraph_rewrite_data
        );
        assert_eq!(
            actual.telegraph_rewrite_status,
            expected.telegraph_rewrite_status
        );
        assert_eq!(
            actual.telegraph_rewrite_after,
            expected.telegraph_rewrite_after
        );
        assert_eq!(
            actual.telegraph_rewrite_started_at,
            expected.telegraph_rewrite_started_at
        );
        assert_eq!(
            actual.telegraph_rewrite_next_retry_at,
            expected.telegraph_rewrite_next_retry_at
        );
        assert_eq!(
            actual.telegraph_rewrite_retry_count,
            expected.telegraph_rewrite_retry_count
        );
        assert_eq!(
            actual.telegraph_rewrite_error,
            expected.telegraph_rewrite_error
        );
        assert_eq!(
            actual.telegraph_rewritten_at,
            expected.telegraph_rewritten_at
        );
    }

    #[tokio::test]
    async fn new_wave_telegraph_only_enqueue_reuses_cached_result() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        seed_cached_result(
            &repo,
            8101,
            "token",
            &variant,
            "fingerprint",
            "https://telegra.ph/cached-only",
            Some("{\"pages\":[]}"),
        )
        .await;

        let delivery = repo
            .enqueue_eh_download(
                -8101,
                8101,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                Some("fingerprint"),
                false,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let job = load_eh_job(&repo, delivery.job_id.unwrap()).await;

        assert_eq!(job.status, JOB_STATUS_DOWNLOADED);
        assert_eq!(job.telegraph_status, TELEGRAPH_STATUS_READY);
        assert_eq!(
            job.telegraph_url.as_deref(),
            Some("https://telegra.ph/cached-only")
        );
        assert!(job.zip_path.is_none());
        assert!(repo
            .get_next_eh_job_for_download_with_policy(false)
            .await
            .unwrap()
            .is_none());
        assert!(repo
            .get_next_eh_job_for_background_download_with_policy(false)
            .await
            .unwrap()
            .is_none());
        assert!(repo.get_next_eh_job_for_upload().await.unwrap().is_none());
        assert_eq!(
            repo.get_next_eh_delivery_for_publish(false)
                .await
                .unwrap()
                .unwrap()
                .delivery
                .id,
            delivery.id
        );
    }

    #[tokio::test]
    async fn new_wave_with_archive_demand_reuses_telegraph_only() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        seed_cached_result(
            &repo,
            8102,
            "token",
            &variant,
            "fingerprint",
            "https://telegra.ph/cached-archive",
            None,
        )
        .await;

        let delivery = repo
            .enqueue_eh_download(
                -8102,
                8102,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                Some("fingerprint"),
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let pending = load_eh_job(&repo, delivery.job_id.unwrap()).await;
        assert_eq!(pending.status, JOB_STATUS_PENDING);
        assert_eq!(pending.telegraph_status, TELEGRAPH_STATUS_READY);

        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(download.id, pending.id);
        let completed = repo
            .mark_eh_job_downloaded(
                download.id,
                download.started_at.unwrap(),
                123,
                "cached-archive.zip",
                0,
            )
            .await
            .unwrap();
        assert_eq!(completed.telegraph_status, TELEGRAPH_STATUS_READY);
        assert_eq!(
            completed.telegraph_url.as_deref(),
            Some("https://telegra.ph/cached-archive")
        );
        assert!(repo.get_next_eh_job_for_upload().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn fingerprint_mismatch_falls_back_to_full_flow() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        seed_cached_result(
            &repo,
            8103,
            "token",
            &variant,
            "old-fingerprint",
            "https://telegra.ph/stale",
            None,
        )
        .await;

        let delivery = repo
            .enqueue_eh_download(
                -8103,
                8103,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                Some("new-fingerprint"),
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let pending = load_eh_job(&repo, delivery.job_id.unwrap()).await;
        assert_eq!(pending.status, JOB_STATUS_PENDING);
        assert_eq!(pending.telegraph_status, TELEGRAPH_STATUS_NOT_REQUIRED);

        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        let downloaded = repo
            .mark_eh_job_downloaded(
                download.id,
                download.started_at.unwrap(),
                123,
                "fresh.zip",
                0,
            )
            .await
            .unwrap();
        assert_eq!(downloaded.telegraph_status, TELEGRAPH_STATUS_PENDING);
        let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        repo.mark_eh_job_telegraph_ready(
            upload.id,
            upload.started_at.unwrap(),
            "https://telegra.ph/fresh",
            None,
            Some("[{\"name\":\"001.jpg\",\"cid\":\"bafk-fresh\"}]"),
            true,
        )
        .await
        .unwrap();

        let txn = repo.db().begin().await.unwrap();
        let cached = find_eh_gallery_result_in_txn(&txn, 8103, "token", &variant)
            .await
            .unwrap()
            .unwrap();
        txn.commit().await.unwrap();
        assert_eq!(cached.source_fingerprint, "new-fingerprint");
        assert_eq!(cached.telegraph_url, "https://telegra.ph/fresh");
    }

    #[tokio::test]
    async fn different_source_fingerprints_create_distinct_active_jobs() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let first = repo
            .enqueue_eh_download(
                -18103,
                18103,
                "token",
                "Generation A",
                false,
                SOURCE_DIRECT,
                &variant,
                Some("fingerprint-a"),
                true,
            )
            .await
            .unwrap()
            .expect("first generation should be enqueued");
        let first_job = load_eh_job(&repo, first.job_id.unwrap()).await;

        let second = repo
            .enqueue_eh_download(
                -28103,
                18103,
                "token",
                "Generation B",
                false,
                SOURCE_DIRECT,
                &variant,
                Some("fingerprint-b"),
                true,
            )
            .await
            .unwrap()
            .expect("second generation should be enqueued");
        let second_job = load_eh_job(&repo, second.job_id.unwrap()).await;

        assert_ne!(second_job.id, first_job.id);
        assert_eq!(
            first_job.source_fingerprint.as_deref(),
            Some("fingerprint-a")
        );
        assert_eq!(
            second_job.source_fingerprint.as_deref(),
            Some("fingerprint-b")
        );
        assert_eq!(load_eh_job(&repo, first_job.id).await, first_job);
    }

    #[tokio::test]
    async fn same_fingerprint_reuses_a_job_but_null_is_an_isolated_bucket() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let known = repo
            .enqueue_eh_download(
                -18104,
                18104,
                "token",
                "Known generation",
                false,
                SOURCE_DIRECT,
                &variant,
                Some("fingerprint-a"),
                true,
            )
            .await
            .unwrap()
            .expect("known generation should be enqueued");
        let repeated_known = repo
            .enqueue_eh_download(
                -28104,
                18104,
                "token",
                "Updated known title",
                false,
                SOURCE_DIRECT,
                &variant,
                Some("fingerprint-a"),
                true,
            )
            .await
            .unwrap()
            .expect("same known generation should be enqueued");
        assert_eq!(repeated_known.job_id, known.job_id);

        let unknown = repo
            .enqueue_eh_download(
                -38104,
                18104,
                "token",
                "Unknown generation",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("unknown generation should be enqueued");
        let repeated_unknown = repo
            .enqueue_eh_download(
                -48104,
                18104,
                "token",
                "Updated unknown title",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("repeated unknown generation should be enqueued");
        assert_eq!(repeated_unknown.job_id, unknown.job_id);
        assert_ne!(unknown.job_id, known.job_id);

        let unknown_before_new_known = load_eh_job(&repo, unknown.job_id.unwrap()).await;
        let new_known = repo
            .enqueue_eh_download(
                -58104,
                18104,
                "token",
                "New known generation",
                false,
                SOURCE_DIRECT,
                &variant,
                Some("fingerprint-b"),
                true,
            )
            .await
            .unwrap()
            .expect("new known generation should be enqueued");
        assert_ne!(new_known.job_id, unknown.job_id);
        assert_eq!(
            load_eh_job(&repo, unknown.job_id.unwrap()).await,
            unknown_before_new_known
        );
    }

    #[tokio::test]
    async fn new_fingerprint_preserves_ready_job_and_existing_result() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let first = repo
            .enqueue_eh_download(
                -18105,
                18105,
                "token",
                "Generation A",
                true,
                SOURCE_DIRECT,
                &variant,
                Some("fingerprint-a"),
                true,
            )
            .await
            .unwrap()
            .expect("first generation should be enqueued");
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            123,
            "generation-a.zip",
            0,
        )
        .await
        .unwrap();
        let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        repo.mark_eh_job_telegraph_ready(
            upload.id,
            upload.started_at.unwrap(),
            "https://telegra.ph/generation-a",
            None,
            Some("[{\"name\":\"001.jpg\",\"cid\":\"bafk-a\"}]"),
            true,
        )
        .await
        .unwrap();
        let ready_a = load_eh_job(&repo, first.job_id.unwrap()).await;

        let second = repo
            .enqueue_eh_download(
                -28105,
                18105,
                "token",
                "Generation B",
                true,
                SOURCE_DIRECT,
                &variant,
                Some("fingerprint-b"),
                true,
            )
            .await
            .unwrap()
            .expect("second generation should be enqueued");
        let pending_b = load_eh_job(&repo, second.job_id.unwrap()).await;
        assert_ne!(pending_b.id, ready_a.id);
        assert_eq!(
            pending_b.source_fingerprint.as_deref(),
            Some("fingerprint-b")
        );
        assert_eq!(pending_b.status, JOB_STATUS_PENDING);
        assert!(pending_b.zip_path.is_none());
        assert_eq!(load_eh_job(&repo, ready_a.id).await, ready_a);

        let txn = repo.db().begin().await.unwrap();
        let result = find_eh_gallery_result_in_txn(&txn, 18105, "token", &variant)
            .await
            .unwrap()
            .expect("first upload result should remain cached");
        txn.commit().await.unwrap();
        assert_eq!(result.source_fingerprint, "fingerprint-a");
        assert_eq!(result.telegraph_url, "https://telegra.ph/generation-a");
    }

    #[tokio::test]
    async fn marker_bearing_subscription_fingerprint_rebind_moves_only_unsent_surface() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let first = repo
            .enqueue_eh_subscription_download(
                -18107,
                101,
                18107,
                "token",
                "Old generation",
                true,
                &variant,
                Some("fingerprint-a"),
                true,
            )
            .await
            .unwrap()
            .expect("first subscription generation should be enqueued");
        let old_job = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            old_job.id,
            old_job.started_at.unwrap(),
            123,
            "old-generation.zip",
            0,
        )
        .await
        .unwrap();
        let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        repo.mark_eh_job_telegraph_ready(
            upload.id,
            upload.started_at.unwrap(),
            "https://telegra.ph/old-generation",
            None,
            None,
            true,
        )
        .await
        .unwrap();
        let claimed = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .expect("old generation should be publishable");
        repo.mark_eh_archive_delivery_sent(claimed.delivery.id)
            .await
            .unwrap();
        let marked = eh_download_queue::Entity::find_by_id(first.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let archive_sent_at = marked
            .archive_sent_at
            .expect("archive marker should be persisted before the fingerprint changes");

        let rebound = repo
            .enqueue_eh_subscription_download(
                -18107,
                202,
                18107,
                "token",
                "New generation",
                true,
                &variant,
                Some("fingerprint-b"),
                true,
            )
            .await
            .unwrap()
            .expect("new fingerprint must retain the unsent Telegraph demand");
        let new_job_id = rebound.job_id.expect("rebound delivery should have a job");
        assert_ne!(new_job_id, old_job.id);
        assert_eq!(rebound.id, first.id);
        assert_eq!(rebound.status, DELIVERY_STATUS_WAITING);
        assert_eq!(rebound.archive_sent_at, Some(archive_sent_at));
        assert!(rebound.telegraph_sent_at.is_none());
        assert_eq!(rebound.subscription_ids.as_deref(), Some("101,202"));
        assert_eq!(
            rebound.telegraph_subscription_ids.as_deref(),
            Some("101,202")
        );
        assert!(rebound.zip_path.is_none());
        assert!(rebound.telegraph_url.is_none());

        let new_job = load_eh_job(&repo, new_job_id).await;
        assert_eq!(new_job.source_fingerprint.as_deref(), Some("fingerprint-b"));
        assert_eq!(new_job.status, JOB_STATUS_PENDING);
        assert!(new_job.telegraph_required);
        assert_eq!(new_job.telegraph_status, TELEGRAPH_STATUS_NOT_REQUIRED);
        assert!(new_job.zip_path.is_none());
        assert!(new_job.telegraph_url.is_none());
        assert!(repo
            .get_next_eh_delivery_for_publish(false)
            .await
            .unwrap()
            .is_none());

        let old_after_rebind = load_eh_job(&repo, old_job.id).await;
        assert_eq!(old_after_rebind.status, JOB_STATUS_RETIRED);
        assert_eq!(old_after_rebind.cleanup_status, CLEANUP_STATUS_PENDING);
        assert_eq!(
            repo.get_next_eh_job_for_cleanup()
                .await
                .unwrap()
                .unwrap()
                .id,
            old_job.id
        );
        assert_eq!(
            repo.get_next_eh_job_for_download()
                .await
                .unwrap()
                .unwrap()
                .id,
            new_job_id
        );
    }

    #[tokio::test]
    async fn marker_bearing_subscription_delivery_keeps_legacy_merges_for_same_fingerprint_or_variant_change(
    ) {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let archive_variant = EhGalleryVariant::archive("1280x");
        let _first = repo
            .enqueue_eh_subscription_download(
                -18108,
                101,
                18108,
                "token",
                "Existing generation",
                true,
                &archive_variant,
                Some("fingerprint-a"),
                true,
            )
            .await
            .unwrap()
            .expect("first subscription generation should be enqueued");
        let old_job = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            old_job.id,
            old_job.started_at.unwrap(),
            123,
            "legacy-partial-wave.zip",
            0,
        )
        .await
        .unwrap();
        let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        repo.mark_eh_job_telegraph_ready(
            upload.id,
            upload.started_at.unwrap(),
            "https://telegra.ph/legacy-partial-wave",
            None,
            None,
            true,
        )
        .await
        .unwrap();
        let claim = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .expect("existing generation should be publishable");
        repo.mark_eh_archive_delivery_sent(claim.delivery.id)
            .await
            .unwrap();

        let same_fingerprint = repo
            .enqueue_eh_subscription_download(
                -18108,
                202,
                18108,
                "token",
                "Same fingerprint",
                true,
                &archive_variant,
                Some("fingerprint-a"),
                true,
            )
            .await
            .unwrap()
            .expect("same fingerprint must merge the active partial wave");
        assert_eq!(same_fingerprint.job_id, Some(old_job.id));
        assert_eq!(same_fingerprint.status, DELIVERY_STATUS_PUBLISHING);
        assert!(same_fingerprint.archive_sent_at.is_some());
        assert!(same_fingerprint.telegraph_sent_at.is_none());
        assert_eq!(
            same_fingerprint.subscription_ids.as_deref(),
            Some("101,202")
        );

        let variant_changed = repo
            .enqueue_eh_subscription_download(
                -18108,
                303,
                18108,
                "token",
                "Different resolution",
                true,
                &EhGalleryVariant::archive("original"),
                Some("fingerprint-a"),
                true,
            )
            .await
            .unwrap()
            .expect("different variants retain the existing partial-wave binding");
        assert_eq!(variant_changed.job_id, Some(old_job.id));
        assert_eq!(variant_changed.status, DELIVERY_STATUS_PUBLISHING);
        assert!(variant_changed.archive_sent_at.is_some());
        assert!(variant_changed.telegraph_sent_at.is_none());
        assert_eq!(
            variant_changed.subscription_ids.as_deref(),
            Some("101,202,303")
        );
    }

    #[tokio::test]
    async fn normal_and_background_claims_do_not_complete_new_fingerprint_jobs() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let normal_a = repo
            .enqueue_eh_download(
                -18106,
                18106,
                "token",
                "Normal A",
                false,
                SOURCE_DIRECT,
                &variant,
                Some("fingerprint-normal-a"),
                true,
            )
            .await
            .unwrap()
            .expect("normal generation should be enqueued");
        let normal_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(normal_claim.id, normal_a.job_id.unwrap());
        let normal_b = repo
            .enqueue_eh_download(
                -28106,
                18106,
                "token",
                "Normal B",
                false,
                SOURCE_DIRECT,
                &variant,
                Some("fingerprint-normal-b"),
                true,
            )
            .await
            .unwrap()
            .expect("second normal generation should be enqueued");
        let normal_b_before = load_eh_job(&repo, normal_b.job_id.unwrap()).await;
        assert_ne!(normal_b_before.id, normal_claim.id);
        assert!(normal_b_before.started_at.is_none());
        assert!(normal_b_before.zip_path.is_none());
        let normal_completed = repo
            .mark_eh_job_downloaded(
                normal_claim.id,
                normal_claim.started_at.unwrap(),
                123,
                "normal-a.zip",
                0,
            )
            .await
            .unwrap();
        assert_eq!(normal_completed.id, normal_claim.id);
        assert_eq!(normal_completed.zip_path.as_deref(), Some("normal-a.zip"));
        assert_eq!(
            load_eh_job(&repo, normal_b_before.id).await,
            normal_b_before
        );

        let background_a = repo
            .enqueue_eh_download(
                -38106,
                18106,
                "token",
                "Background A",
                false,
                SOURCE_DIRECT,
                &variant,
                Some("fingerprint-background-a"),
                true,
            )
            .await
            .unwrap()
            .expect("background generation should be enqueued");
        let background_id = background_a.job_id.unwrap();
        repo.schedule_eh_job_background_download(background_id, JOB_STATUS_PENDING, "test handoff")
            .await
            .unwrap();
        let background_claim = repo
            .get_next_eh_job_for_background_download()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(background_claim.id, background_id);
        assert!(background_claim.started_at.is_some());
        assert!(background_claim.background_download_started_at.is_some());
        let background_b = repo
            .enqueue_eh_download(
                -48106,
                18106,
                "token",
                "Background B",
                false,
                SOURCE_DIRECT,
                &variant,
                Some("fingerprint-background-b"),
                true,
            )
            .await
            .unwrap()
            .expect("second background generation should be enqueued");
        let background_b_before = load_eh_job(&repo, background_b.job_id.unwrap()).await;
        assert_ne!(background_b_before.id, background_claim.id);
        assert!(background_b_before.started_at.is_none());
        assert!(background_b_before.zip_path.is_none());
        let background_completed = repo
            .mark_eh_job_background_downloaded(
                background_claim.id,
                background_claim.started_at.unwrap(),
                456,
                "background-a.zip",
                0,
            )
            .await
            .unwrap();
        assert_eq!(background_completed.id, background_claim.id);
        assert_eq!(
            background_completed.zip_path.as_deref(),
            Some("background-a.zip")
        );
        assert_eq!(
            load_eh_job(&repo, background_b_before.id).await,
            background_b_before
        );
    }

    #[tokio::test]
    async fn late_telegraph_demand_applies_cached_result() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let archive = repo
            .enqueue_eh_download(
                -8104,
                8104,
                "token",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                Some("fingerprint"),
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        seed_cached_result(
            &repo,
            8104,
            "token",
            &variant,
            "fingerprint",
            "https://telegra.ph/late",
            None,
        )
        .await;
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            123,
            "late.zip",
            0,
        )
        .await
        .unwrap();

        let late = repo
            .enqueue_eh_download(
                -18104,
                8104,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                Some("fingerprint"),
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(late.job_id, archive.job_id);
        let ready = load_eh_job(&repo, archive.job_id.unwrap()).await;
        assert_eq!(ready.status, JOB_STATUS_DOWNLOADED);
        assert!(ready.telegraph_required);
        assert_eq!(ready.telegraph_status, TELEGRAPH_STATUS_READY);
        assert_eq!(
            ready.telegraph_url.as_deref(),
            Some("https://telegra.ph/late")
        );
        assert!(repo.get_next_eh_job_for_upload().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn late_telegraph_demand_during_archive_download_preserves_claim() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");

        let normal = repo
            .enqueue_eh_download(
                -8105,
                8105,
                "normal",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                Some("fingerprint"),
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        seed_cached_result(
            &repo,
            8105,
            "normal",
            &variant,
            "fingerprint",
            "https://telegra.ph/normal",
            Some("{\"pages\":[]}"),
        )
        .await;
        let normal_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        let normal_generation = normal_claim.started_at.unwrap();
        repo.enqueue_eh_download(
            -18105,
            8105,
            "normal",
            "Gallery",
            true,
            SOURCE_DIRECT,
            &variant,
            Some("fingerprint"),
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
        let normal_ready = load_eh_job(&repo, normal.job_id.unwrap()).await;
        assert_eq!(normal_ready.status, JOB_STATUS_DOWNLOADING);
        assert_eq!(normal_ready.started_at, Some(normal_generation));
        assert!(normal_ready.background_download_status.is_none());
        assert_eq!(normal_ready.telegraph_status, TELEGRAPH_STATUS_READY);
        let normal_complete = repo
            .mark_eh_job_downloaded(normal_ready.id, normal_generation, 123, "normal.zip", 0)
            .await
            .unwrap();
        assert_eq!(normal_complete.telegraph_status, TELEGRAPH_STATUS_READY);
        assert!(repo.get_next_eh_job_for_upload().await.unwrap().is_none());

        let background = repo
            .enqueue_eh_download(
                -8106,
                8106,
                "background",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                Some("fingerprint"),
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        seed_cached_result(
            &repo,
            8106,
            "background",
            &variant,
            "fingerprint",
            "https://telegra.ph/background",
            Some("{\"pages\":[]}"),
        )
        .await;
        let normal_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.schedule_eh_job_background_download(
            normal_claim.id,
            normal_claim.status.as_str(),
            "test handoff",
        )
        .await
        .unwrap();
        let background_claim = repo
            .get_next_eh_job_for_background_download()
            .await
            .unwrap()
            .unwrap();
        let background_generation = background_claim.started_at.unwrap();
        let background_lease = background_claim.background_download_started_at;
        repo.enqueue_eh_download(
            -18106,
            8106,
            "background",
            "Gallery",
            true,
            SOURCE_DIRECT,
            &variant,
            Some("fingerprint"),
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
        let background_ready = load_eh_job(&repo, background.job_id.unwrap()).await;
        assert_eq!(background_ready.status, JOB_STATUS_PENDING);
        assert_eq!(background_ready.started_at, Some(background_generation));
        assert_eq!(
            background_ready.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_RUNNING)
        );
        assert_eq!(
            background_ready.background_download_started_at,
            background_lease
        );
        assert_eq!(background_ready.telegraph_status, TELEGRAPH_STATUS_READY);
        let background_complete = repo
            .mark_eh_job_background_downloaded(
                background_ready.id,
                background_generation,
                456,
                "background.zip",
                0,
            )
            .await
            .unwrap();
        assert_eq!(background_complete.telegraph_status, TELEGRAPH_STATUS_READY);
        assert!(repo.get_next_eh_job_for_upload().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cached_result_unsent_before_retirement_still_rewrites_after_reuse() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let first = repo
            .enqueue_eh_download(
                -8107,
                8107,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                Some("fingerprint"),
                false,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo
            .get_next_eh_job_for_download_with_policy(false)
            .await
            .unwrap()
            .unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            123,
            "retired-before-send.zip",
            0,
        )
        .await
        .unwrap();
        let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_CANCELED),
            )
            .filter(eh_download_queue::Column::Id.eq(first.id))
            .exec(repo.db())
            .await
            .unwrap();
        repo.mark_eh_job_telegraph_ready(
            upload.id,
            upload.started_at.unwrap(),
            "https://telegra.ph/retired-before-send",
            Some("{\"pages\":[]}"),
            Some("[{\"name\":\"001.jpg\",\"cid\":\"bafk-retired\"}]"),
            false,
        )
        .await
        .unwrap();
        let cleanup = repo.get_next_eh_job_for_cleanup().await.unwrap().unwrap();
        assert_eq!(
            repo.finalize_eh_job_cleanup(cleanup.id, cleanup.cleanup_started_at.unwrap(), false)
                .await
                .unwrap(),
            Some(EhCleanupFinalizeOutcome::CleanRetired)
        );

        let reused = repo
            .enqueue_eh_download(
                -18107,
                8107,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                Some("fingerprint"),
                false,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let ready = load_eh_job(&repo, reused.job_id.unwrap()).await;
        assert_eq!(ready.status, JOB_STATUS_DOWNLOADED);
        assert_eq!(ready.telegraph_status, TELEGRAPH_STATUS_READY);
        assert_eq!(
            ready.telegraph_rewrite_data.as_deref(),
            Some("{\"pages\":[]}")
        );
        assert!(ready.telegraph_rewrite_status.is_none());
        assert!(ready.telegraph_rewrite_after.is_none());
        assert!(ready.telegraph_rewrite_started_at.is_none());
        assert!(ready.telegraph_rewrite_next_retry_at.is_none());
        assert_eq!(ready.telegraph_rewrite_retry_count, 0);
        assert!(ready.telegraph_rewrite_error.is_none());
        assert!(ready.telegraph_rewritten_at.is_none());

        let publish = repo
            .get_next_eh_delivery_for_publish(false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(publish.delivery.id, reused.id);
        repo.mark_eh_telegraph_delivery_sent(reused.id, ready.id, Some(0))
            .await
            .unwrap();
        let scheduled = load_eh_job(&repo, ready.id).await;
        assert_eq!(
            scheduled.telegraph_rewrite_status.as_deref(),
            Some(TELEGRAPH_REWRITE_STATUS_PENDING)
        );
        assert!(scheduled.telegraph_rewritten_at.is_none());
    }

    #[tokio::test]
    async fn archive_only_new_wave_does_not_consult_cache() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        seed_cached_result(
            &repo,
            8108,
            "token",
            &variant,
            "fingerprint",
            "https://telegra.ph/unused",
            None,
        )
        .await;

        let delivery = repo
            .enqueue_eh_download(
                -8108,
                8108,
                "token",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                Some("fingerprint"),
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let job = load_eh_job(&repo, delivery.job_id.unwrap()).await;
        assert_eq!(job.status, JOB_STATUS_PENDING);
        assert_eq!(job.telegraph_status, TELEGRAPH_STATUS_NOT_REQUIRED);
        assert!(job.telegraph_url.is_none());
        assert_eq!(
            repo.get_next_eh_job_for_download()
                .await
                .unwrap()
                .unwrap()
                .id,
            job.id
        );
    }

    #[tokio::test]
    async fn late_telegraph_consumer_reuses_download_and_terminal_upload_failure_falls_back_to_archive(
    ) {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let archive_only = repo
            .enqueue_eh_download(
                -100,
                70,
                "token",
                "Shared Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let claimed_download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            claimed_download.id,
            claimed_download.started_at.unwrap(),
            123,
            "shared.zip",
            0,
        )
        .await
        .unwrap();

        let late = repo
            .enqueue_eh_download(
                -200,
                70,
                "token",
                "Shared Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(late.job_id, archive_only.job_id);
        let downloaded = eh_gallery_jobs::Entity::find_by_id(claimed_download.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(downloaded.status, JOB_STATUS_DOWNLOADED);
        assert_eq!(downloaded.telegraph_status, TELEGRAPH_STATUS_PENDING);

        let claimed_upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        let first_started_at = claimed_upload.started_at.unwrap();
        let outcome = repo
            .record_eh_job_upload_failure(
                claimed_upload.id,
                first_started_at,
                "provider secret",
                0,
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            outcome,
            EhJobUploadFailureOutcome::Terminal {
                job: eh_gallery_jobs::Entity::find_by_id(claimed_upload.id)
                    .one(repo.db())
                    .await
                    .unwrap()
                    .unwrap(),
                deliveries: vec![EhFailedTelegraphDelivery {
                    delivery_id: late.id,
                    chat_id: -200,
                    title: "Shared Gallery".to_string(),
                }],
            }
        );

        let archive_only = eh_download_queue::Entity::find_by_id(archive_only.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let late = eh_download_queue::Entity::find_by_id(late.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(archive_only.status, DELIVERY_STATUS_WAITING);
        assert!(!archive_only.telegraph);
        assert_eq!(late.status, DELIVERY_STATUS_WAITING);
        assert!(!late.telegraph);
        assert_eq!(archive_only.error, None);
        assert_eq!(late.error, None);
        assert!(late.archive_sent_at.is_none());
        assert!(late.telegraph_sent_at.is_none());
        assert!(late.started_at.is_none());
        assert!(late.completed_at.is_none());
        assert!(late.next_retry_at.is_none());

        let archive_only_claim = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(archive_only_claim.delivery.id, archive_only.id);
        repo.mark_eh_archive_delivery_sent(archive_only.id)
            .await
            .unwrap();
        repo.mark_eh_delivery_done(archive_only.id, claimed_upload.id, true)
            .await
            .unwrap();
        let late_claim = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(late_claim.delivery.id, late.id);
        assert!(!late_claim.delivery.telegraph);

        assert_eq!(
            repo.record_eh_job_upload_failure(
                claimed_upload.id,
                first_started_at,
                "second provider secret",
                0,
                true,
            )
            .await
            .unwrap(),
            EhJobUploadFailureOutcome::Stale
        );
    }

    #[tokio::test]
    async fn terminal_upload_archive_fallback_retires_subscription_telegraph_owners() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let first = repo
            .enqueue_eh_subscription_download(
                -100, 101, 725, "token", "Gallery", true, &variant, None, true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            123,
            "subscription-fallback.zip",
            0,
        )
        .await
        .unwrap();
        let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        let EhJobUploadFailureOutcome::Terminal { .. } = repo
            .record_eh_job_upload_failure(
                upload.id,
                upload.started_at.unwrap(),
                "terminal provider failure",
                0,
                true,
            )
            .await
            .unwrap()
        else {
            panic!("expected a terminal shared upload failure");
        };

        let fallback = eh_download_queue::Entity::find_by_id(first.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fallback.status, DELIVERY_STATUS_WAITING);
        assert!(!fallback.telegraph);
        assert!(fallback.telegraph_subscription_ids.is_none());
        assert_eq!(fallback.subscription_ids.as_deref(), Some("101"));
        let failed = eh_gallery_jobs::Entity::find_by_id(upload.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert!(!failed.telegraph_required);

        let merged = repo
            .enqueue_eh_subscription_download(
                -100, 202, 725, "token", "Gallery", false, &variant, None, true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(merged.id, first.id);
        assert!(!merged.telegraph);
        assert!(merged.telegraph_subscription_ids.is_none());
        assert_eq!(merged.subscription_ids.as_deref(), Some("101,202"));
        let after_merge = eh_gallery_jobs::Entity::find_by_id(upload.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert!(!after_merge.telegraph_required);
        assert!(repo.get_next_eh_job_for_upload().await.unwrap().is_none());
        assert_eq!(
            repo.get_next_eh_delivery_for_publish(true)
                .await
                .unwrap()
                .unwrap()
                .delivery
                .id,
            first.id
        );
    }

    #[tokio::test]
    async fn terminal_upload_failure_completes_mixed_delivery_after_archive_was_sent() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let delivery = repo
            .enqueue_eh_download(
                -100,
                701,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            123,
            "shared.zip",
            0,
        )
        .await
        .unwrap();
        let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        let archive_sent_at = Local::now().naive_local();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_PUBLISHING),
            )
            .col_expr(
                eh_download_queue::Column::StartedAt,
                Expr::value(Some(archive_sent_at)),
            )
            .col_expr(
                eh_download_queue::Column::ArchiveSentAt,
                Expr::value(Some(archive_sent_at)),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(Some(archive_sent_at + chrono::Duration::minutes(1))),
            )
            .filter(eh_download_queue::Column::Id.eq(delivery.id))
            .exec(repo.db())
            .await
            .unwrap();

        let outcome = repo
            .record_eh_job_upload_failure(
                upload.id,
                upload.started_at.unwrap(),
                "provider secret",
                0,
                true,
            )
            .await
            .unwrap();
        let EhJobUploadFailureOutcome::Terminal { job, deliveries } = outcome else {
            panic!("expected a terminal shared upload failure");
        };
        assert_eq!(
            deliveries,
            vec![EhFailedTelegraphDelivery {
                delivery_id: delivery.id,
                chat_id: -100,
                title: "Gallery".to_string(),
            }]
        );
        assert_eq!(job.telegraph_status, TELEGRAPH_STATUS_FAILED);
        assert_eq!(job.status, JOB_STATUS_RETIRED);
        assert_eq!(job.cleanup_status, CLEANUP_STATUS_PENDING);

        let completed = eh_download_queue::Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(completed.status, DELIVERY_STATUS_DONE);
        assert!(!completed.telegraph);
        assert_eq!(completed.archive_sent_at, Some(archive_sent_at));
        assert!(completed.telegraph_sent_at.is_none());
        assert!(completed.completed_at.is_some());
        assert!(completed.next_retry_at.is_none());
    }

    #[tokio::test]
    async fn job_upload_retry_is_generation_guarded_and_ready_data_stays_shared() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let delivery = repo
            .enqueue_eh_download(
                -100,
                71,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            123,
            "shared.zip",
            0,
        )
        .await
        .unwrap();

        let first = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        let first_started_at = first.started_at.unwrap();
        let retry = repo
            .record_eh_job_upload_failure(first.id, first_started_at, "temporary", 3, true)
            .await
            .unwrap();
        let EhJobUploadFailureOutcome::RetryScheduled(retry) = retry else {
            panic!("expected a scheduled shared upload retry");
        };
        assert_eq!(retry.retry_count, 1);
        assert_eq!(retry.telegraph_status, TELEGRAPH_STATUS_PENDING);
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(first.id))
            .exec(repo.db())
            .await
            .unwrap();

        let second = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        let second_started_at = second.started_at.unwrap();
        assert!(second_started_at > first_started_at);
        assert_eq!(
            repo.record_eh_job_upload_failure(first.id, first_started_at, "stale", 0, true)
                .await
                .unwrap(),
            EhJobUploadFailureOutcome::Stale
        );
        let ready = repo
            .mark_eh_job_telegraph_ready(
                second.id,
                second_started_at,
                "https://telegra.ph/shared",
                Some("{\"pages\":[]}"),
                None,
                true,
            )
            .await
            .unwrap();
        assert_eq!(ready.telegraph_status, TELEGRAPH_STATUS_READY);
        assert_eq!(
            ready.telegraph_url.as_deref(),
            Some("https://telegra.ph/shared")
        );
        assert_eq!(
            ready.telegraph_rewrite_data.as_deref(),
            Some("{\"pages\":[]}")
        );

        let delivery = eh_download_queue::Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivery.status, DELIVERY_STATUS_WAITING);
        assert_eq!(delivery.telegraph_url, None);
        assert_eq!(delivery.telegraph_rewrite_data, None);
    }

    #[tokio::test]
    async fn upload_defer_is_generation_guarded_and_does_not_consume_retry() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let delivery = repo
            .enqueue_eh_download(
                -100,
                72,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            123,
            "shared.zip",
            7,
        )
        .await
        .unwrap();

        let first = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        let first_started_at = first.started_at.unwrap();
        assert!(repo
            .defer_eh_job_upload(first.id, first_started_at, 0, true)
            .await
            .unwrap());
        let deferred = eh_gallery_jobs::Entity::find_by_id(first.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(deferred.telegraph_status, TELEGRAPH_STATUS_PENDING);
        assert_eq!(deferred.started_at, Some(first_started_at));
        assert_eq!(deferred.retry_count, 0);
        assert_eq!(deferred.error, None);
        assert_eq!(deferred.file_size, 123);
        assert_eq!(deferred.gp_cost, 7);
        assert_eq!(deferred.zip_path.as_deref(), Some("shared.zip"));
        assert!(deferred.next_retry_at.is_some());

        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(first.id))
            .exec(repo.db())
            .await
            .unwrap();
        let second = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        let second_started_at = second.started_at.unwrap();
        assert!(second_started_at > first_started_at);

        assert!(!repo
            .defer_eh_job_upload(first.id, first_started_at, 60, true)
            .await
            .unwrap());
        let after_stale_defer = eh_gallery_jobs::Entity::find_by_id(first.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after_stale_defer.telegraph_status,
            TELEGRAPH_STATUS_UPLOADING
        );
        assert_eq!(after_stale_defer.started_at, Some(second_started_at));
        assert_eq!(after_stale_defer.retry_count, 0);
        assert_eq!(after_stale_defer.error, None);
        assert_eq!(after_stale_defer.file_size, 123);
        assert_eq!(after_stale_defer.gp_cost, 7);
        assert_eq!(after_stale_defer.zip_path.as_deref(), Some("shared.zip"));
        assert_eq!(delivery.job_id, Some(first.id));
    }

    #[tokio::test]
    async fn archive_disabled_cleanup_releases_zip_for_active_ready_telegraph_delivery() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let first = repo
            .enqueue_eh_download(
                -100,
                721,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let second = repo
            .enqueue_eh_download(
                -200,
                721,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            123,
            "shared-ready.zip",
            0,
        )
        .await
        .unwrap();
        let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        repo.mark_eh_job_telegraph_ready(
            upload.id,
            upload.started_at.unwrap(),
            "https://telegra.ph/shared-ready",
            Some("{\"pages\":[]}"),
            None,
            false,
        )
        .await
        .unwrap();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_PUBLISHING),
            )
            .filter(eh_download_queue::Column::Id.eq(first.id))
            .exec(repo.db())
            .await
            .unwrap();
        repo.mark_eh_telegraph_delivery_sent(first.id, download.id, Some(0))
            .await
            .unwrap();
        let rewrite = repo
            .get_next_eh_job_for_telegraph_rewrite()
            .await
            .unwrap()
            .unwrap();
        let rewrite_started_at = rewrite.telegraph_rewrite_started_at;
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_DONE),
            )
            .filter(eh_download_queue::Column::Id.eq(first.id))
            .exec(repo.db())
            .await
            .unwrap();

        repo.evaluate_eh_job_liveness(download.id, false)
            .await
            .unwrap();
        let scheduled = eh_gallery_jobs::Entity::find_by_id(download.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(scheduled.cleanup_status, CLEANUP_STATUS_PENDING);
        assert_eq!(scheduled.zip_path.as_deref(), Some("shared-ready.zip"));
        assert_eq!(scheduled.telegraph_status, TELEGRAPH_STATUS_READY);
        assert_eq!(
            scheduled.telegraph_url.as_deref(),
            Some("https://telegra.ph/shared-ready")
        );
        assert_eq!(
            scheduled.telegraph_rewrite_data.as_deref(),
            Some("{\"pages\":[]}")
        );
        assert_eq!(
            scheduled.telegraph_rewrite_status.as_deref(),
            Some(TELEGRAPH_REWRITE_STATUS_REWRITING)
        );
        assert_eq!(scheduled.telegraph_rewrite_started_at, rewrite_started_at);
        let cleanup = repo.get_next_eh_job_for_cleanup().await.unwrap().unwrap();
        assert_eq!(cleanup.id, download.id);
        assert_eq!(
            repo.finalize_eh_job_cleanup(download.id, cleanup.cleanup_started_at.unwrap(), false)
                .await
                .unwrap(),
            Some(EhCleanupFinalizeOutcome::FinalizedWithoutSourceWork)
        );
        let settled = load_eh_job(&repo, download.id).await;
        assert_eq!(settled.status, JOB_STATUS_DOWNLOADED);
        assert_eq!(settled.cleanup_status, CLEANUP_STATUS_NONE);
        assert!(settled.zip_path.is_none());
        assert_eq!(settled.telegraph_status, TELEGRAPH_STATUS_READY);
        assert_eq!(
            settled.telegraph_url.as_deref(),
            Some("https://telegra.ph/shared-ready")
        );
        assert_eq!(
            settled.telegraph_rewrite_data.as_deref(),
            Some("{\"pages\":[]}")
        );
        assert_eq!(
            settled.telegraph_rewrite_status.as_deref(),
            Some(TELEGRAPH_REWRITE_STATUS_REWRITING)
        );
        assert_eq!(settled.telegraph_rewrite_started_at, rewrite_started_at);
        let publish = repo
            .get_next_eh_delivery_for_publish(false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(publish.delivery.id, second.id);
    }

    #[tokio::test]
    async fn active_unsent_archive_surface_retains_zip_when_telegraph_is_ready() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let first = repo
            .enqueue_eh_download(
                -100,
                7210,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            123,
            "active-ready.zip",
            0,
        )
        .await
        .unwrap();
        let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        repo.mark_eh_job_telegraph_ready(
            upload.id,
            upload.started_at.unwrap(),
            "https://telegra.ph/active-ready",
            None,
            None,
            true,
        )
        .await
        .unwrap();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_PUBLISHING),
            )
            .filter(eh_download_queue::Column::Id.eq(first.id))
            .exec(repo.db())
            .await
            .unwrap();

        repo.evaluate_eh_job_liveness(download.id, true)
            .await
            .unwrap();
        let retained = eh_gallery_jobs::Entity::find_by_id(download.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retained.cleanup_status, CLEANUP_STATUS_NONE);
        assert_eq!(retained.zip_path.as_deref(), Some("active-ready.zip"));
        assert!(repo.get_next_eh_job_for_cleanup().await.unwrap().is_none());
        let active = eh_download_queue::Entity::find_by_id(first.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(active.status, DELIVERY_STATUS_PUBLISHING);
        assert!(active.archive_sent_at.is_none());
        assert!(repo.get_next_eh_job_for_download().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn ready_telegraph_delivery_during_cleanup_reuses_page_without_redownload() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let first = repo
            .enqueue_eh_download(
                -100,
                7212,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo
            .get_next_eh_job_for_download_with_policy(false)
            .await
            .unwrap()
            .unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            123,
            "ready-cleanup.zip",
            0,
        )
        .await
        .unwrap();
        let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        repo.mark_eh_job_telegraph_ready(
            upload.id,
            upload.started_at.unwrap(),
            "https://telegra.ph/ready-cleanup",
            Some("{\"pages\":[]}"),
            None,
            false,
        )
        .await
        .unwrap();
        let expected_rewrite =
            set_active_telegraph_rewrite(&repo, download.id, TELEGRAPH_REWRITE_STATUS_PENDING)
                .await;
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_CANCELED),
            )
            .filter(eh_download_queue::Column::Id.eq(first.id))
            .exec(repo.db())
            .await
            .unwrap();
        repo.evaluate_eh_job_liveness(download.id, false)
            .await
            .unwrap();
        let cleanup = repo.get_next_eh_job_for_cleanup().await.unwrap().unwrap();
        let cleanup_generation = cleanup.cleanup_started_at.unwrap();

        let late = repo
            .enqueue_eh_download(
                -200,
                7212,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(late.job_id, Some(download.id));

        assert_eq!(
            repo.finalize_eh_job_cleanup(download.id, cleanup_generation, false)
                .await
                .unwrap(),
            Some(EhCleanupFinalizeOutcome::FinalizedWithoutSourceWork)
        );
        let settled = eh_gallery_jobs::Entity::find_by_id(download.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(settled.cleanup_status, CLEANUP_STATUS_NONE);
        assert!(settled.zip_path.is_none());
        assert_eq!(settled.status, JOB_STATUS_DOWNLOADED);
        assert_telegraph_rewrite_state_preserved(&expected_rewrite, &settled);

        let publish = repo
            .get_next_eh_delivery_for_publish(false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(publish.delivery.id, late.id);
        assert_eq!(publish.job.telegraph_url, expected_rewrite.telegraph_url);
        assert!(repo
            .get_next_eh_job_for_download_with_policy(false)
            .await
            .unwrap()
            .is_none());
        assert!(repo
            .get_next_eh_job_for_background_download_with_policy(false)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn fingerprint_change_during_cleanup_creates_isolated_generation() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let first = repo
            .enqueue_eh_download(
                -100,
                7214,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                Some("old-fingerprint"),
                false,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo
            .get_next_eh_job_for_download_with_policy(false)
            .await
            .unwrap()
            .unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            123,
            "fingerprint-change-cleanup.zip",
            0,
        )
        .await
        .unwrap();
        let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        repo.mark_eh_job_telegraph_ready(
            upload.id,
            upload.started_at.unwrap(),
            "https://telegra.ph/old-fingerprint",
            Some("{\"pages\":[]}"),
            None,
            false,
        )
        .await
        .unwrap();
        seed_cached_result(
            &repo,
            7214,
            "token",
            &variant,
            "old-fingerprint",
            "https://telegra.ph/old-cache",
            Some("{\"pages\":[\"old\"]}"),
        )
        .await;
        set_active_telegraph_rewrite(&repo, download.id, TELEGRAPH_REWRITE_STATUS_PENDING).await;
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_CANCELED),
            )
            .filter(eh_download_queue::Column::Id.eq(first.id))
            .exec(repo.db())
            .await
            .unwrap();
        repo.evaluate_eh_job_liveness(download.id, false)
            .await
            .unwrap();
        let cleanup = repo.get_next_eh_job_for_cleanup().await.unwrap().unwrap();
        let cleanup_generation = cleanup.cleanup_started_at.unwrap();

        let late = repo
            .enqueue_eh_download(
                -200,
                7214,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                Some("new-fingerprint"),
                false,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let new_job_id = late.job_id.expect("new generation should have a job");
        assert_ne!(new_job_id, download.id);
        let new_generation = load_eh_job(&repo, new_job_id).await;
        assert_eq!(
            new_generation.source_fingerprint.as_deref(),
            Some("new-fingerprint")
        );
        assert_eq!(new_generation.cleanup_status, CLEANUP_STATUS_NONE);
        assert_eq!(new_generation.status, JOB_STATUS_PENDING);
        assert!(new_generation.telegraph_required);
        assert_eq!(
            new_generation.telegraph_status,
            TELEGRAPH_STATUS_NOT_REQUIRED
        );
        assert!(new_generation.zip_path.is_none());
        assert!(new_generation.telegraph_url.is_none());
        assert_eq!(load_eh_job(&repo, download.id).await, cleanup);
        assert!(repo
            .get_next_eh_delivery_for_publish(false)
            .await
            .unwrap()
            .is_none());

        assert_eq!(
            repo.finalize_eh_job_cleanup(download.id, cleanup_generation, false)
                .await
                .unwrap(),
            Some(EhCleanupFinalizeOutcome::RetainedForRewrite)
        );
        assert_eq!(load_eh_job(&repo, new_job_id).await, new_generation);

        let txn = repo.db().begin().await.unwrap();
        let cached = find_eh_gallery_result_in_txn(&txn, 7214, "token", &variant)
            .await
            .unwrap()
            .unwrap();
        txn.commit().await.unwrap();
        assert_eq!(cached.source_fingerprint, "old-fingerprint");
        assert_eq!(cached.telegraph_url, "https://telegra.ph/old-cache");

        let fresh_download = repo
            .get_next_eh_job_for_download_with_policy(false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fresh_download.id, new_job_id);
        let downloaded = repo
            .mark_eh_job_downloaded(
                fresh_download.id,
                fresh_download.started_at.unwrap(),
                456,
                "fingerprint-change-fresh.zip",
                0,
            )
            .await
            .unwrap();
        assert_eq!(downloaded.telegraph_status, TELEGRAPH_STATUS_PENDING);
        assert!(downloaded.telegraph_url.is_none());
        let fresh_upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        assert_eq!(fresh_upload.id, new_job_id);
        assert_eq!(fresh_upload.telegraph_status, TELEGRAPH_STATUS_UPLOADING);
    }

    #[tokio::test]
    async fn claimed_cleanup_reactivates_archive_work_without_clearing_ready_telegraph_state() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let first = repo
            .enqueue_eh_download(
                -100,
                7213,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            123,
            "claimed-cleanup.zip",
            0,
        )
        .await
        .unwrap();
        let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        repo.mark_eh_job_telegraph_ready(
            upload.id,
            upload.started_at.unwrap(),
            "https://telegra.ph/claimed-cleanup",
            Some("{\"pages\":[]}"),
            None,
            true,
        )
        .await
        .unwrap();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_CANCELED),
            )
            .filter(eh_download_queue::Column::Id.eq(first.id))
            .exec(repo.db())
            .await
            .unwrap();
        repo.evaluate_eh_job_liveness(download.id, true)
            .await
            .unwrap();
        let cleanup = repo.get_next_eh_job_for_cleanup().await.unwrap().unwrap();
        let cleanup_generation = cleanup.cleanup_started_at.unwrap();

        let rewrite_generation = Local::now().naive_local();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteStatus,
                Expr::value(Some(TELEGRAPH_REWRITE_STATUS_PENDING.to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteAfter,
                Expr::value(Some(rewrite_generation)),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteStartedAt,
                Expr::value(Some(rewrite_generation)),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(download.id))
            .exec(repo.db())
            .await
            .unwrap();
        let rebound = repo
            .enqueue_eh_download(
                -200,
                7213,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(rebound.job_id, Some(download.id));
        assert!(repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .is_none());

        assert_eq!(
            repo.finalize_eh_job_cleanup(download.id, cleanup_generation, true)
                .await
                .unwrap(),
            Some(EhCleanupFinalizeOutcome::ReactivatedPending)
        );
        let settled = eh_gallery_jobs::Entity::find_by_id(download.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(settled.status, JOB_STATUS_PENDING);
        assert_eq!(settled.cleanup_status, CLEANUP_STATUS_NONE);
        assert!(settled.zip_path.is_none());
        assert_eq!(settled.file_size, 0);
        assert_eq!(settled.gp_cost, 0);
        assert_eq!(settled.retry_count, 0);
        assert!(settled.next_retry_at.is_none());
        assert_eq!(settled.telegraph_status, TELEGRAPH_STATUS_READY);
        assert_eq!(
            settled.telegraph_url.as_deref(),
            Some("https://telegra.ph/claimed-cleanup")
        );
        assert_eq!(
            settled.telegraph_rewrite_data.as_deref(),
            Some("{\"pages\":[]}")
        );
        assert_eq!(
            settled.telegraph_rewrite_status.as_deref(),
            Some(TELEGRAPH_REWRITE_STATUS_PENDING)
        );
        assert_eq!(settled.telegraph_rewrite_after, Some(rewrite_generation));
        assert_eq!(
            settled.telegraph_rewrite_started_at,
            Some(rewrite_generation)
        );
        assert_eq!(
            repo.get_next_eh_delivery_for_publish(false)
                .await
                .unwrap()
                .unwrap()
                .delivery
                .id,
            rebound.id
        );
        assert_eq!(
            repo.get_next_eh_job_for_download()
                .await
                .unwrap()
                .unwrap()
                .id,
            download.id
        );
    }

    #[tokio::test]
    async fn archive_disabled_terminal_upload_failure_remains_failed_and_notifies_once() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let first = repo
            .enqueue_eh_download(
                -100,
                7217,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            123,
            "failed-upload-cleanup.zip",
            0,
        )
        .await
        .unwrap();
        let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        let terminal = repo
            .record_eh_job_upload_failure(
                upload.id,
                upload.started_at.unwrap(),
                "terminal provider failure",
                0,
                false,
            )
            .await
            .unwrap();
        let EhJobUploadFailureOutcome::Terminal { deliveries, .. } = terminal else {
            panic!("expected a terminal shared upload failure");
        };
        assert_eq!(
            deliveries,
            vec![EhFailedTelegraphDelivery {
                delivery_id: first.id,
                chat_id: -100,
                title: "Gallery".to_string(),
            }]
        );
        assert_eq!(
            eh_download_queue::Entity::find_by_id(first.id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap()
                .status,
            DELIVERY_STATUS_FAILED
        );
        let cleanup = repo.get_next_eh_job_for_cleanup().await.unwrap().unwrap();
        let cleanup_generation = cleanup.cleanup_started_at.unwrap();

        let late = repo
            .enqueue_eh_download(
                -200,
                7217,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(late.job_id, Some(download.id));
        assert!(repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .is_none());

        assert_eq!(
            repo.finalize_eh_job_cleanup(download.id, cleanup_generation, false)
                .await
                .unwrap(),
            Some(EhCleanupFinalizeOutcome::ReactivatedPending)
        );
        let reactivated = eh_gallery_jobs::Entity::find_by_id(download.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reactivated.status, JOB_STATUS_PENDING);
        assert_eq!(reactivated.cleanup_status, CLEANUP_STATUS_NONE);
        assert!(reactivated.zip_path.is_none());
        assert!(reactivated.telegraph_required);
        assert_eq!(reactivated.telegraph_status, TELEGRAPH_STATUS_NOT_REQUIRED);
        assert!(reactivated.telegraph_url.is_none());
        assert!(reactivated.telegraph_rewrite_data.is_none());
        assert!(reactivated.telegraph_rewrite_status.is_none());
        assert!(reactivated.error.is_none());
        assert_eq!(reactivated.retry_count, 0);
        assert!(reactivated.next_retry_at.is_none());

        let redownload = repo
            .get_next_eh_job_for_download_with_policy(false)
            .await
            .unwrap()
            .unwrap();
        repo.mark_eh_job_downloaded(
            redownload.id,
            redownload.started_at.unwrap(),
            123,
            "failed-upload-retry.zip",
            0,
        )
        .await
        .unwrap();
        let redownloaded = eh_gallery_jobs::Entity::find_by_id(download.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(redownloaded.telegraph_status, TELEGRAPH_STATUS_PENDING);
        assert_eq!(
            repo.get_next_eh_job_for_upload().await.unwrap().unwrap().id,
            download.id
        );
    }

    #[tokio::test]
    async fn no_configured_publish_surface_is_never_claimed_for_source_download() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let no_surface = repo
            .enqueue_eh_download(
                -100,
                7214,
                "token",
                "No surface",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let normal_surface = repo
            .enqueue_eh_download(
                -200,
                7215,
                "token",
                "Telegraph surface",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        let normal = repo
            .get_next_eh_job_for_download_with_policy(false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(normal.id, normal_surface.job_id.unwrap());
        let unclaimed = eh_gallery_jobs::Entity::find_by_id(no_surface.job_id.unwrap())
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(unclaimed.status, JOB_STATUS_PENDING);
        assert!(unclaimed.started_at.is_none());

        assert!(repo
            .get_next_eh_delivery_for_publish(false)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            eh_download_queue::Entity::find_by_id(no_surface.id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap()
                .status,
            DELIVERY_STATUS_FAILED
        );

        let background_no_surface = repo
            .enqueue_eh_download(
                -300,
                7216,
                "token",
                "Background no surface",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let background_surface = repo
            .enqueue_eh_download(
                -400,
                7217,
                "token",
                "Background Telegraph surface",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        repo.schedule_eh_job_background_download(
            background_no_surface.job_id.unwrap(),
            JOB_STATUS_PENDING,
            "handoff",
        )
        .await
        .unwrap();
        repo.schedule_eh_job_background_download(
            background_surface.job_id.unwrap(),
            JOB_STATUS_PENDING,
            "handoff",
        )
        .await
        .unwrap();

        let background = repo
            .get_next_eh_job_for_background_download_with_policy(false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(background.id, background_surface.job_id.unwrap());
        assert_eq!(
            background.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_RUNNING)
        );
        let background_unclaimed =
            eh_gallery_jobs::Entity::find_by_id(background_no_surface.job_id.unwrap())
                .one(repo.db())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            background_unclaimed.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_PENDING)
        );
        assert!(background_unclaimed
            .background_download_started_at
            .is_none());
    }

    #[tokio::test]
    async fn active_telegraph_rewrite_releases_zip_without_retiring() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let (job, delivery) = seed_rewrite_ready_job(&repo, 7214).await;
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::ZipPath,
                Expr::value(Some("rewrite-pending.zip".to_string())),
            )
            .col_expr(eh_gallery_jobs::Column::FileSize, Expr::value(123_i64))
            .col_expr(eh_gallery_jobs::Column::GpCost, Expr::value(4_i64))
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .exec(repo.db())
            .await
            .unwrap();
        repo.mark_eh_archive_sent(delivery.id).await.unwrap();
        repo.mark_eh_telegraph_delivery_sent(delivery.id, job.id, Some(0))
            .await
            .unwrap();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_DONE),
            )
            .filter(eh_download_queue::Column::Id.eq(delivery.id))
            .exec(repo.db())
            .await
            .unwrap();

        repo.evaluate_eh_job_liveness(job.id, true).await.unwrap();
        let pending = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pending.cleanup_status, CLEANUP_STATUS_PENDING);
        assert_eq!(pending.zip_path.as_deref(), Some("rewrite-pending.zip"));
        assert_eq!(
            pending.telegraph_rewrite_status.as_deref(),
            Some(TELEGRAPH_REWRITE_STATUS_PENDING)
        );
        assert_eq!(
            pending.telegraph_rewrite_data.as_deref(),
            Some("{\"pages\":[]}")
        );

        let rewrite = repo
            .get_next_eh_job_for_telegraph_rewrite()
            .await
            .unwrap()
            .unwrap();
        let rewrite_generation = rewrite.telegraph_rewrite_started_at.unwrap();
        let rewrite_after = rewrite.telegraph_rewrite_after;
        let cleanup = repo.get_next_eh_job_for_cleanup().await.unwrap().unwrap();
        assert_eq!(cleanup.id, job.id);
        assert_eq!(
            repo.finalize_eh_job_cleanup(job.id, cleanup.cleanup_started_at.unwrap(), true)
                .await
                .unwrap(),
            Some(EhCleanupFinalizeOutcome::RetainedForRewrite)
        );
        let cleaned = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cleaned.status, JOB_STATUS_DOWNLOADED);
        assert_eq!(cleaned.cleanup_status, CLEANUP_STATUS_NONE);
        assert!(cleaned.zip_path.is_none());
        assert_eq!(cleaned.file_size, 0);
        assert_eq!(cleaned.gp_cost, 0);
        assert_eq!(
            cleaned.telegraph_rewrite_status.as_deref(),
            Some(TELEGRAPH_REWRITE_STATUS_REWRITING)
        );
        assert_eq!(
            cleaned.telegraph_rewrite_data.as_deref(),
            Some("{\"pages\":[]}")
        );
        assert_eq!(cleaned.telegraph_rewrite_after, rewrite_after);
        assert_eq!(
            cleaned.telegraph_rewrite_started_at,
            Some(rewrite_generation)
        );
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::Error,
                Expr::value(Some("stale download error".to_string())),
            )
            .col_expr(eh_gallery_jobs::Column::RetryCount, Expr::value(3_i32))
            .col_expr(
                eh_gallery_jobs::Column::NextRetryAt,
                Expr::value(Some(Local::now().naive_local())),
            )
            .col_expr(
                eh_gallery_jobs::Column::CompletedAt,
                Expr::value(Some(Local::now().naive_local())),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .exec(repo.db())
            .await
            .unwrap();
        let cleaned = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();

        let late_archive = repo
            .enqueue_eh_download(
                -200,
                7214,
                "token",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(late_archive.job_id, Some(job.id));
        assert!(repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .is_none());
        let recovered = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.status, JOB_STATUS_PENDING);
        assert_eq!(recovered.cleanup_status, CLEANUP_STATUS_NONE);
        assert!(recovered.zip_path.is_none());
        assert_eq!(recovered.started_at, cleaned.started_at);
        assert!(recovered.error.is_none());
        assert_eq!(recovered.retry_count, 0);
        assert!(recovered.next_retry_at.is_none());
        assert!(recovered.completed_at.is_none());
        assert!(recovered.background_download_status.is_none());
        assert_telegraph_rewrite_state_preserved(&cleaned, &recovered);
        assert_eq!(
            repo.get_next_eh_job_for_download_with_policy(true)
                .await
                .unwrap()
                .unwrap()
                .id,
            job.id
        );

        assert!(!repo
            .schedule_eh_job_telegraph_rewrite_retry(
                job.id,
                rewrite_generation,
                "temporary rewrite failure",
                1,
            )
            .await
            .unwrap());
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .exec(repo.db())
            .await
            .unwrap();
        let retry = repo
            .get_next_eh_job_for_telegraph_rewrite()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            retry.telegraph_rewrite_data.as_deref(),
            Some("{\"pages\":[]}")
        );
        assert!(retry.telegraph_rewrite_started_at.unwrap() > rewrite_generation);
        assert!(repo
            .schedule_eh_job_telegraph_rewrite_retry(
                job.id,
                retry.telegraph_rewrite_started_at.unwrap(),
                "terminal rewrite failure",
                0,
            )
            .await
            .unwrap());
        let terminal = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            terminal.telegraph_rewrite_status.as_deref(),
            Some(TELEGRAPH_REWRITE_STATUS_FAILED)
        );
        assert_eq!(
            terminal.telegraph_rewrite_data.as_deref(),
            Some("{\"pages\":[]}")
        );
    }

    #[tokio::test]
    async fn terminal_rewrite_before_archive_recovery_does_not_strand_delivery() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let (job, delivery) = seed_rewrite_ready_job(&repo, 7220).await;
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::ZipPath,
                Expr::value(Some("terminal-rewrite.zip".to_string())),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .exec(repo.db())
            .await
            .unwrap();
        repo.mark_eh_archive_sent(delivery.id).await.unwrap();
        repo.mark_eh_telegraph_delivery_sent(delivery.id, job.id, Some(0))
            .await
            .unwrap();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_DONE),
            )
            .filter(eh_download_queue::Column::Id.eq(delivery.id))
            .exec(repo.db())
            .await
            .unwrap();

        repo.evaluate_eh_job_liveness(job.id, true).await.unwrap();
        let rewrite = repo
            .get_next_eh_job_for_telegraph_rewrite()
            .await
            .unwrap()
            .unwrap();
        let cleanup = repo.get_next_eh_job_for_cleanup().await.unwrap().unwrap();
        assert_eq!(
            repo.finalize_eh_job_cleanup(job.id, cleanup.cleanup_started_at.unwrap(), true)
                .await
                .unwrap(),
            Some(EhCleanupFinalizeOutcome::RetainedForRewrite)
        );

        let late_archive = repo
            .enqueue_eh_download(
                -200,
                7220,
                "token",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(late_archive.job_id, Some(job.id));
        assert!(repo
            .schedule_eh_job_telegraph_rewrite_retry(
                job.id,
                rewrite.telegraph_rewrite_started_at.unwrap(),
                "terminal rewrite failure",
                0,
            )
            .await
            .unwrap());
        repo.evaluate_eh_job_liveness(job.id, true).await.unwrap();
        let terminal = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(terminal.status, JOB_STATUS_DOWNLOADED);
        assert_eq!(
            terminal.telegraph_rewrite_status.as_deref(),
            Some(TELEGRAPH_REWRITE_STATUS_FAILED)
        );
        assert_eq!(
            eh_download_queue::Entity::find_by_id(late_archive.id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap()
                .status,
            DELIVERY_STATUS_WAITING
        );

        assert!(repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .is_none());
        let recovered = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.status, JOB_STATUS_PENDING);
        assert_eq!(recovered.started_at, terminal.started_at);
        assert_telegraph_rewrite_state_preserved(&terminal, &recovered);
        assert_eq!(
            repo.get_next_eh_job_for_download_with_policy(true)
                .await
                .unwrap()
                .unwrap()
                .id,
            job.id
        );
    }

    #[tokio::test]
    async fn normal_lost_claim_releases_active_rewrite_without_retiring() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let delivery = repo
            .enqueue_eh_download(
                -100,
                7218,
                "token",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let claim = repo
            .get_next_eh_job_for_download_with_policy(true)
            .await
            .unwrap()
            .unwrap();
        let rewrite =
            set_active_telegraph_rewrite(&repo, claim.id, TELEGRAPH_REWRITE_STATUS_PENDING).await;
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_CANCELED),
            )
            .filter(eh_download_queue::Column::Id.eq(delivery.id))
            .exec(repo.db())
            .await
            .unwrap();

        assert!(repo
            .retire_eh_job_without_active_deliveries(&claim)
            .await
            .unwrap());
        let released = eh_gallery_jobs::Entity::find_by_id(claim.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(released.status, JOB_STATUS_DOWNLOADED);
        assert_eq!(released.cleanup_status, CLEANUP_STATUS_NONE);
        assert_eq!(released.started_at, rewrite.started_at);
        assert_telegraph_rewrite_state_preserved(&rewrite, &released);

        let late = repo
            .enqueue_eh_download(
                -200,
                7218,
                "token",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(late.job_id, Some(claim.id));
        let after_enqueue = eh_gallery_jobs::Entity::find_by_id(claim.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after_enqueue.status, JOB_STATUS_DOWNLOADED);
        assert_telegraph_rewrite_state_preserved(&rewrite, &after_enqueue);
        assert!(!repo
            .retire_eh_job_without_active_deliveries(&claim)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn background_lost_claim_releases_active_rewrite_without_retiring() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let delivery = repo
            .enqueue_eh_download(
                -100,
                7219,
                "token",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let normal = repo
            .get_next_eh_job_for_download_with_policy(true)
            .await
            .unwrap()
            .unwrap();
        repo.schedule_eh_job_background_download(normal.id, &normal.status, "test handoff")
            .await
            .unwrap();
        let claim = repo
            .get_next_eh_job_for_background_download_with_policy(true)
            .await
            .unwrap()
            .unwrap();
        let rewrite =
            set_active_telegraph_rewrite(&repo, claim.id, TELEGRAPH_REWRITE_STATUS_REWRITING).await;
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_CANCELED),
            )
            .filter(eh_download_queue::Column::Id.eq(delivery.id))
            .exec(repo.db())
            .await
            .unwrap();

        assert!(repo
            .retire_eh_job_without_active_deliveries(&claim)
            .await
            .unwrap());
        let released = eh_gallery_jobs::Entity::find_by_id(claim.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(released.status, JOB_STATUS_DOWNLOADED);
        assert!(released.background_download_status.is_none());
        assert!(released.background_download_started_at.is_none());
        assert!(released.background_download_next_retry_at.is_none());
        assert_eq!(released.background_download_attempt_count, 0);
        assert!(released.background_download_error.is_none());
        assert_telegraph_rewrite_state_preserved(&rewrite, &released);

        let late = repo
            .enqueue_eh_download(
                -200,
                7219,
                "token",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(late.job_id, Some(claim.id));
        let after_enqueue = eh_gallery_jobs::Entity::find_by_id(claim.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after_enqueue.status, JOB_STATUS_DOWNLOADED);
        assert_telegraph_rewrite_state_preserved(&rewrite, &after_enqueue);
        assert!(!repo
            .retire_eh_job_without_active_deliveries(&claim)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn upload_success_after_last_cancellation_keeps_family_until_cleanup() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let delivery = repo
            .enqueue_eh_subscription_download(
                -100, 7211, 722, "token", "Gallery", true, &variant, None, true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            123,
            "owned-after-success.zip",
            0,
        )
        .await
        .unwrap();
        let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        repo.cancel_eh_subscription_queue_entries(7211, true)
            .await
            .unwrap();

        let in_flight = eh_gallery_jobs::Entity::find_by_id(download.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(in_flight.telegraph_status, TELEGRAPH_STATUS_UPLOADING);
        assert_eq!(in_flight.cleanup_status, CLEANUP_STATUS_NONE);
        let settled = repo
            .mark_eh_job_telegraph_ready(
                upload.id,
                upload.started_at.unwrap(),
                "https://telegra.ph/owned-after-success",
                None,
                None,
                true,
            )
            .await
            .unwrap();
        assert_eq!(settled.status, JOB_STATUS_RETIRED);
        assert_eq!(settled.cleanup_status, CLEANUP_STATUS_PENDING);
        assert_eq!(settled.zip_path.as_deref(), Some("owned-after-success.zip"));
        assert_eq!(settled.telegraph_status, TELEGRAPH_STATUS_READY);
        assert_eq!(delivery.status, DELIVERY_STATUS_WAITING);
    }

    #[tokio::test]
    async fn retryable_upload_failure_after_last_cancellation_becomes_cleanup() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        repo.enqueue_eh_subscription_download(
            -100, 7212, 723, "token", "Gallery", true, &variant, None, true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            123,
            "owned-after-retry.zip",
            0,
        )
        .await
        .unwrap();
        let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        repo.cancel_eh_subscription_queue_entries(7212, true)
            .await
            .unwrap();

        let outcome = repo
            .record_eh_job_upload_failure(
                upload.id,
                upload.started_at.unwrap(),
                "temporary provider failure",
                3,
                true,
            )
            .await
            .unwrap();
        let EhJobUploadFailureOutcome::RetryScheduled(settled) = outcome else {
            panic!("the upload retry must settle its own current-demand cleanup");
        };
        assert_eq!(settled.status, JOB_STATUS_RETIRED);
        assert_eq!(settled.telegraph_status, TELEGRAPH_STATUS_NOT_REQUIRED);
        assert!(!settled.telegraph_required);
        assert_eq!(settled.cleanup_status, CLEANUP_STATUS_PENDING);
        assert_eq!(settled.zip_path.as_deref(), Some("owned-after-retry.zip"));
    }

    #[tokio::test]
    async fn late_telegraph_demand_restarts_failed_upload_without_redownloading() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let archive_only = repo
            .enqueue_eh_download(
                -100,
                724,
                "token",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let failed_telegraph = repo
            .enqueue_eh_download(
                -200,
                724,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        let downloaded_generation = download.started_at.unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            downloaded_generation,
            123,
            "archive-only-sibling.zip",
            0,
        )
        .await
        .unwrap();
        let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        let failed_upload_generation = upload.started_at.unwrap();
        let outcome = repo
            .record_eh_job_upload_failure(
                upload.id,
                failed_upload_generation,
                "terminal provider failure",
                0,
                true,
            )
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            EhJobUploadFailureOutcome::Terminal { .. }
        ));
        let archive_fallback = eh_download_queue::Entity::find_by_id(failed_telegraph.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(archive_fallback.status, DELIVERY_STATUS_WAITING);
        assert!(!archive_fallback.telegraph);

        let late = repo
            .enqueue_eh_download(
                -300,
                724,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(late.job_id, archive_only.job_id);
        let restarted = eh_gallery_jobs::Entity::find_by_id(download.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(restarted.status, JOB_STATUS_DOWNLOADED);
        assert_eq!(
            restarted.zip_path.as_deref(),
            Some("archive-only-sibling.zip")
        );
        assert_eq!(restarted.telegraph_status, TELEGRAPH_STATUS_PENDING);
        assert!(restarted.telegraph_required);
        assert_eq!(restarted.error, None);
        assert_eq!(restarted.retry_count, 0);
        assert!(restarted.next_retry_at.is_none());
        assert_eq!(restarted.started_at, Some(failed_upload_generation));
        assert!(repo.get_next_eh_job_for_download().await.unwrap().is_none());
        let resumed_upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        assert_eq!(resumed_upload.id, download.id);
        assert!(resumed_upload.started_at.unwrap() > failed_upload_generation);
    }

    #[tokio::test]
    async fn missing_zip_reset_refuses_uploading_and_keeps_cleanup_ownership_until_finalized() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let delivery = repo
            .enqueue_eh_subscription_download(
                -100, 7215, 7216, "token", "Gallery", true, &variant, None, true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            123,
            "uploading-owned.zip",
            0,
        )
        .await
        .unwrap();
        let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        let upload_generation = upload.started_at.unwrap();

        assert_eq!(
            repo.reset_eh_job_for_missing_zip(
                upload.id,
                upload_generation,
                "uploading-owned.zip",
                3
            )
            .await
            .unwrap(),
            EhMissingZipResetOutcome::Stale
        );
        let after_rejected_reset = eh_gallery_jobs::Entity::find_by_id(upload.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after_rejected_reset.status, JOB_STATUS_DOWNLOADED);
        assert_eq!(
            after_rejected_reset.zip_path.as_deref(),
            Some("uploading-owned.zip")
        );
        assert_eq!(after_rejected_reset.started_at, Some(upload_generation));
        assert_eq!(
            after_rejected_reset.telegraph_status,
            TELEGRAPH_STATUS_UPLOADING
        );
        assert_eq!(
            eh_download_queue::Entity::find_by_id(delivery.id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap()
                .status,
            DELIVERY_STATUS_WAITING
        );

        repo.cancel_eh_subscription_queue_entries(7215, true)
            .await
            .unwrap();
        let EhJobUploadFailureOutcome::RetryScheduled(settled) = repo
            .record_eh_job_upload_failure(upload.id, upload_generation, "canceled upload", 3, true)
            .await
            .unwrap()
        else {
            panic!("settling the canceled upload must retain cleanup ownership");
        };
        assert_eq!(settled.status, JOB_STATUS_RETIRED);
        assert_eq!(settled.cleanup_status, CLEANUP_STATUS_PENDING);
        assert_eq!(settled.zip_path.as_deref(), Some("uploading-owned.zip"));

        let rebound = repo
            .enqueue_eh_download(
                -200,
                7216,
                "token",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(rebound.job_id, Some(upload.id));
        assert!(repo.get_next_eh_job_for_download().await.unwrap().is_none());

        let cleanup = repo.get_next_eh_job_for_cleanup().await.unwrap().unwrap();
        assert!(repo
            .record_eh_job_cleanup_failure(
                cleanup.id,
                cleanup.cleanup_started_at.unwrap(),
                "provider Abort failed",
                0,
            )
            .await
            .unwrap());
        assert!(repo.get_next_eh_job_for_download().await.unwrap().is_none());

        let retry_cleanup = repo.get_next_eh_job_for_cleanup().await.unwrap().unwrap();
        assert_eq!(
            repo.finalize_eh_job_cleanup(
                retry_cleanup.id,
                retry_cleanup.cleanup_started_at.unwrap(),
                true,
            )
            .await
            .unwrap(),
            Some(EhCleanupFinalizeOutcome::ReactivatedPending)
        );
        assert_eq!(
            repo.get_next_eh_job_for_download()
                .await
                .unwrap()
                .unwrap()
                .id,
            upload.id
        );
    }

    #[tokio::test]
    async fn missing_zip_reset_stops_at_retry_limit_and_fails_deliveries() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let delivery = repo
            .enqueue_eh_download(
                -100,
                731,
                "token",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            123,
            "at-limit.zip",
            0,
        )
        .await
        .unwrap();
        let generation = download.started_at.unwrap();
        let second = repo
            .enqueue_eh_download(
                -200,
                731,
                "token",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(second.job_id, Some(download.id));

        // Below the limit: one more shared redownload is still scheduled.
        eh_gallery_jobs::Entity::update_many()
            .col_expr(eh_gallery_jobs::Column::RetryCount, Expr::value(2_i32))
            .filter(eh_gallery_jobs::Column::Id.eq(download.id))
            .exec(repo.db())
            .await
            .unwrap();
        assert_eq!(
            repo.reset_eh_job_for_missing_zip(download.id, generation, "at-limit.zip", 3)
                .await
                .unwrap(),
            EhMissingZipResetOutcome::Reset
        );
        let reset = eh_gallery_jobs::Entity::find_by_id(download.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reset.status, JOB_STATUS_PENDING);
        assert_eq!(reset.retry_count, 3);
        assert!(reset.next_retry_at.is_some());
        assert!(reset.zip_path.is_none());
        assert_eq!(
            eh_download_queue::Entity::find_by_id(delivery.id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap()
                .status,
            DELIVERY_STATUS_WAITING
        );

        // Back to downloaded state at the limit for the terminal scenario.
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(download.id))
            .exec(repo.db())
            .await
            .unwrap();
        let redownload = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(redownload.id, download.id);
        repo.mark_eh_job_downloaded(
            redownload.id,
            redownload.started_at.unwrap(),
            123,
            "at-limit.zip",
            0,
        )
        .await
        .unwrap();
        let at_limit_generation = redownload.started_at.unwrap();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(eh_gallery_jobs::Column::RetryCount, Expr::value(3_i32))
            .filter(eh_gallery_jobs::Column::Id.eq(download.id))
            .exec(repo.db())
            .await
            .unwrap();

        assert_eq!(
            repo.reset_eh_job_for_missing_zip(download.id, at_limit_generation, "at-limit.zip", 3)
                .await
                .unwrap(),
            EhMissingZipResetOutcome::Exhausted
        );

        let failed = eh_gallery_jobs::Entity::find_by_id(download.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed.status, JOB_STATUS_FAILED);
        assert_eq!(failed.retry_count, 4);
        assert_eq!(failed.error.as_deref(), Some("cached EH ZIP is missing"));
        assert!(failed.next_retry_at.is_none());
        assert!(failed.completed_at.is_some());
        assert_eq!(failed.zip_path.as_deref(), Some("at-limit.zip"));
        assert_eq!(failed.cleanup_status, CLEANUP_STATUS_PENDING);

        for delivery_id in [delivery.id, second.id] {
            let failed_delivery = eh_download_queue::Entity::find_by_id(delivery_id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(failed_delivery.status, DELIVERY_STATUS_FAILED);
            assert!(failed_delivery.next_retry_at.is_none());
        }

        // A repeated reset attempt is stale: the failed generation is gone.
        assert_eq!(
            repo.reset_eh_job_for_missing_zip(download.id, at_limit_generation, "at-limit.zip", 3)
                .await
                .unwrap(),
            EhMissingZipResetOutcome::Stale
        );
        // Neither the normal nor the background source selector ever claims the
        // failed job again.
        assert!(repo.get_next_eh_job_for_download().await.unwrap().is_none());
        assert!(repo
            .get_next_eh_job_for_background_download()
            .await
            .unwrap()
            .is_none());
        // The durable cleanup lane now owns the leftover archive family.
        assert_eq!(
            repo.get_next_eh_job_for_cleanup()
                .await
                .unwrap()
                .unwrap()
                .id,
            download.id
        );
    }

    #[tokio::test]
    async fn job_telegraph_rewrite_retry_and_success_are_generation_guarded() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let (job, delivery) = seed_rewrite_ready_job(&repo, 74).await;
        repo.mark_eh_telegraph_delivery_sent(delivery.id, job.id, Some(0))
            .await
            .unwrap();

        let first = repo
            .get_next_eh_job_for_telegraph_rewrite()
            .await
            .unwrap()
            .unwrap();
        let first_generation = first.telegraph_rewrite_started_at.unwrap();
        assert!(!repo
            .schedule_eh_job_telegraph_rewrite_retry(
                job.id,
                first_generation,
                "gateway not ready",
                3,
            )
            .await
            .unwrap());
        let retry = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            retry.telegraph_rewrite_status.as_deref(),
            Some(TELEGRAPH_REWRITE_STATUS_PENDING)
        );
        assert_eq!(retry.telegraph_rewrite_started_at, Some(first_generation));
        assert_eq!(retry.telegraph_rewrite_retry_count, 1);
        assert!(retry.telegraph_rewrite_next_retry_at.is_some());
        assert!(retry.telegraph_rewrite_data.is_some());

        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .exec(repo.db())
            .await
            .unwrap();
        let second = repo
            .get_next_eh_job_for_telegraph_rewrite()
            .await
            .unwrap()
            .unwrap();
        let second_generation = second.telegraph_rewrite_started_at.unwrap();
        assert!(second_generation > first_generation);
        assert!(!repo
            .mark_eh_job_telegraph_rewritten(job.id, first_generation)
            .await
            .unwrap());
        assert!(!repo
            .schedule_eh_job_telegraph_rewrite_retry(job.id, first_generation, "stale", 0)
            .await
            .unwrap());

        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_DONE),
            )
            .filter(eh_download_queue::Column::Id.eq(delivery.id))
            .exec(repo.db())
            .await
            .unwrap();

        assert!(repo
            .mark_eh_job_telegraph_rewritten(job.id, second_generation)
            .await
            .unwrap());
        let before_liveness = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert!(before_liveness.telegraph_rewrite_data.is_some());
        assert!(before_liveness.telegraph_rewritten_at.is_some());
        repo.evaluate_eh_job_liveness(job.id, true).await.unwrap();
        let completed = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert!(completed.telegraph_rewrite_data.is_none());
        assert_eq!(completed.status, JOB_STATUS_RETIRED);
    }

    #[tokio::test]
    async fn stale_job_telegraph_rewrite_preserves_payload_and_advances_generation() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let (job, delivery) = seed_rewrite_ready_job(&repo, 75).await;
        repo.mark_eh_telegraph_delivery_sent(delivery.id, job.id, Some(0))
            .await
            .unwrap();
        let claimed = repo
            .get_next_eh_job_for_telegraph_rewrite()
            .await
            .unwrap()
            .unwrap();
        let stale_generation = Local::now().naive_local() - chrono::Duration::seconds(7200);
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteStartedAt,
                Expr::value(Some(stale_generation)),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(claimed.id))
            .exec(repo.db())
            .await
            .unwrap();

        assert_eq!(
            repo.reset_stale_eh_shared_work(3600, 3600)
                .await
                .unwrap()
                .rewrites,
            1
        );
        let reset = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reset.telegraph_rewrite_status.as_deref(),
            Some(TELEGRAPH_REWRITE_STATUS_PENDING)
        );
        assert_eq!(reset.telegraph_rewrite_started_at, Some(stale_generation));
        assert!(reset.telegraph_rewrite_data.is_some());

        let replacement = repo
            .get_next_eh_job_for_telegraph_rewrite()
            .await
            .unwrap()
            .unwrap();
        assert!(replacement.telegraph_rewrite_started_at.unwrap() > stale_generation);
    }

    #[tokio::test]
    async fn job_telegraph_rewrite_exhaustion_is_terminal_only_once() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let (job, delivery) = seed_rewrite_ready_job(&repo, 76).await;
        repo.mark_eh_telegraph_delivery_sent(delivery.id, job.id, Some(0))
            .await
            .unwrap();
        let claimed = repo
            .get_next_eh_job_for_telegraph_rewrite()
            .await
            .unwrap()
            .unwrap();
        let generation = claimed.telegraph_rewrite_started_at.unwrap();

        assert!(repo
            .schedule_eh_job_telegraph_rewrite_retry(job.id, generation, "edit denied", 0)
            .await
            .unwrap());
        assert!(!repo
            .schedule_eh_job_telegraph_rewrite_retry(job.id, generation, "duplicate", 0)
            .await
            .unwrap());
        let failed = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            failed.telegraph_rewrite_status.as_deref(),
            Some(TELEGRAPH_REWRITE_STATUS_FAILED)
        );
        assert_eq!(failed.telegraph_rewrite_retry_count, 1);
        assert_eq!(
            failed.telegraph_rewrite_error.as_deref(),
            Some("edit denied")
        );
        assert!(failed.telegraph_rewrite_data.is_some());
        assert!(repo
            .get_next_eh_job_for_telegraph_rewrite()
            .await
            .unwrap()
            .is_none());

        repo.evaluate_eh_job_liveness(job.id, true).await.unwrap();
        let after_liveness = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert!(after_liveness.telegraph_rewrite_data.is_none());
        assert_eq!(
            after_liveness.telegraph_rewrite_error.as_deref(),
            Some("edit denied")
        );
    }

    #[tokio::test]
    async fn telegraph_sent_without_job_rewrite_payload_only_marks_that_delivery() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let delivery = repo
            .enqueue_eh_download(
                -100,
                79,
                "token",
                "Gallery",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let job_id = delivery.job_id.unwrap();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_PUBLISHING),
            )
            .filter(eh_download_queue::Column::Id.eq(delivery.id))
            .exec(repo.db())
            .await
            .unwrap();

        repo.mark_eh_telegraph_delivery_sent(delivery.id, job_id, Some(0))
            .await
            .unwrap();
        let marked = eh_download_queue::Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert!(marked.telegraph_sent_at.is_some());
        assert!(job.telegraph_rewrite_status.is_none());
        assert!(job.telegraph_rewrite_after.is_none());
    }

    #[tokio::test]
    async fn cancel_before_job_upload_claim_removes_the_last_telegraph_demand() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let delivery = repo
            .enqueue_eh_subscription_download(
                -100, 123, 72, "token", "Gallery", true, &variant, None, true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            123,
            "shared.zip",
            0,
        )
        .await
        .unwrap();

        repo.cancel_eh_subscription_queue_entries(123, true)
            .await
            .unwrap();

        assert!(repo.get_next_eh_job_for_upload().await.unwrap().is_none());
        let job = eh_gallery_jobs::Entity::find_by_id(delivery.job_id.unwrap())
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert!(!job.telegraph_required);
        assert_eq!(job.telegraph_status, TELEGRAPH_STATUS_NOT_REQUIRED);
    }

    #[tokio::test]
    async fn stale_job_upload_claim_resumes_once_with_a_new_generation() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        repo.enqueue_eh_download(
            -100,
            73,
            "token",
            "Gallery",
            true,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            123,
            "shared.zip",
            0,
        )
        .await
        .unwrap();
        let first = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        let first_started_at = first.started_at.unwrap();

        assert_eq!(
            repo.reset_stale_eh_shared_work(3600, 3600)
                .await
                .unwrap()
                .uploads,
            1
        );
        let second = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        assert_eq!(second.id, first.id);
        assert!(second.started_at.unwrap() > first_started_at);
        assert_eq!(
            repo.record_eh_job_upload_failure(first.id, first_started_at, "stale", 0, true)
                .await
                .unwrap(),
            EhJobUploadFailureOutcome::Stale
        );
    }

    #[tokio::test]
    async fn normal_and_background_claims_cannot_own_the_same_job_generation() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let delivery = repo
            .enqueue_eh_download(
                -100, 77, "token", "Gallery", false, "direct", &variant, None, true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let job_id = delivery.job_id.unwrap();
        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, job_id);
        repo.schedule_eh_job_background_download(job_id, JOB_STATUS_DOWNLOADING, "slow")
            .await
            .unwrap();

        let (main, background) = tokio::join!(
            repo.get_next_eh_job_for_download(),
            repo.get_next_eh_job_for_background_download(),
        );
        assert!(main.unwrap().is_none());
        let background = background.unwrap().unwrap();
        assert_eq!(background.id, job_id);

        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStartedAt,
                Expr::value(Some(
                    Local::now().naive_local() - chrono::Duration::seconds(2),
                )),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .exec(repo.db())
            .await
            .unwrap();
        assert_eq!(
            repo.reset_stale_eh_shared_work(1, 1)
                .await
                .unwrap()
                .backgrounds,
            1
        );
        assert_eq!(
            repo.get_next_eh_job_for_background_download()
                .await
                .unwrap()
                .unwrap()
                .id,
            job_id
        );
    }

    #[tokio::test]
    async fn normal_late_consumer_prevents_lost_claim_retirement() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let first = repo
            .enqueue_eh_download(
                -100,
                771,
                "token",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_CANCELED),
            )
            .filter(eh_download_queue::Column::Id.eq(first.id))
            .exec(repo.db())
            .await
            .unwrap();

        let late = repo
            .enqueue_eh_download(
                -200,
                771,
                "token",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        assert!(!repo
            .retire_eh_job_without_active_deliveries(&claim)
            .await
            .unwrap());
        let job = eh_gallery_jobs::Entity::find_by_id(claim.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.status, JOB_STATUS_DOWNLOADING);
        assert_eq!(job.cleanup_status, CLEANUP_STATUS_NONE);
        assert_eq!(late.job_id, Some(claim.id));
    }

    #[tokio::test]
    async fn background_late_consumer_prevents_lost_claim_retirement() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let first = repo
            .enqueue_eh_download(
                -100,
                772,
                "token",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let normal_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.schedule_eh_job_background_download(
            normal_claim.id,
            normal_claim.status.as_str(),
            "test handoff",
        )
        .await
        .unwrap();
        let claim = repo
            .get_next_eh_job_for_background_download()
            .await
            .unwrap()
            .unwrap();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_CANCELED),
            )
            .filter(eh_download_queue::Column::Id.eq(first.id))
            .exec(repo.db())
            .await
            .unwrap();

        let late = repo
            .enqueue_eh_download(
                -200,
                772,
                "token",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        assert!(!repo
            .retire_eh_job_without_active_deliveries(&claim)
            .await
            .unwrap());
        let job = eh_gallery_jobs::Entity::find_by_id(claim.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.status, JOB_STATUS_PENDING);
        assert_eq!(
            job.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_RUNNING)
        );
        assert_eq!(job.cleanup_status, CLEANUP_STATUS_NONE);
        assert_eq!(late.job_id, Some(claim.id));
    }

    #[tokio::test]
    async fn stale_background_completion_cannot_append_a_ledger_row() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let delivery = repo
            .enqueue_eh_download(
                -100, 78, "token", "Gallery", false, "direct", &variant, None, true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let job_id = delivery.job_id.unwrap();
        let normal_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.schedule_eh_job_background_download(job_id, normal_claim.status.as_str(), "slow")
            .await
            .unwrap();
        let first_claim = repo
            .get_next_eh_job_for_background_download()
            .await
            .unwrap()
            .unwrap();
        let first_started_at = first_claim.started_at.unwrap();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStartedAt,
                Expr::value(Some(
                    Local::now().naive_local() - chrono::Duration::seconds(2),
                )),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .exec(repo.db())
            .await
            .unwrap();
        assert_eq!(
            repo.reset_stale_eh_shared_work(1, 1)
                .await
                .unwrap()
                .backgrounds,
            1
        );
        let replacement_claim = repo
            .get_next_eh_job_for_background_download()
            .await
            .unwrap()
            .unwrap();
        assert!(replacement_claim.started_at.unwrap() > first_started_at);

        let error = repo
            .mark_eh_job_background_downloaded(
                job_id,
                first_started_at,
                100,
                "/tmp/stale-background.zip",
                0,
            )
            .await
            .expect_err("stale background claim must not complete a newer generation");
        assert!(error.to_string().contains("claim changed concurrently"));
        assert!(eh_download_completions::Entity::find()
            .all(repo.db())
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn stale_job_completion_cannot_overwrite_a_new_claim_or_append_a_ledger_row() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        repo.enqueue_eh_download(
            1, 777, "token", "Gallery", false, "direct", &variant, None, true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");

        let first_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        let first_started_at = first_claim.started_at.unwrap();
        assert_eq!(
            repo.reset_stale_eh_shared_work(3600, 3600)
                .await
                .unwrap()
                .downloads,
            1
        );
        let second_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(second_claim.id, first_claim.id);
        assert!(second_claim.started_at.unwrap() > first_started_at);

        let error = repo
            .mark_eh_job_downloaded(first_claim.id, first_started_at, 100, "/tmp/stale.zip", 0)
            .await
            .expect_err("stale shared claim must not complete the newer generation");
        assert!(error.to_string().contains("claim changed concurrently"));
        assert!(eh_download_completions::Entity::find()
            .all(repo.db())
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn test_main_download_claim_rejects_stale_previous_generation() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let delivery = repo
            .enqueue_eh_download(
                -100,
                67,
                "tok",
                "Title",
                false,
                "subscription",
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let job_id = delivery.job_id.unwrap();
        let stale_snapshot = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();

        let first_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.defer_eh_job_download(first_claim.id, 0).await.unwrap();
        let second_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::Status,
                Expr::value(JOB_STATUS_PENDING),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADING))
            .exec(repo.db())
            .await
            .unwrap();

        assert_eq!(
            repo.claim_eh_job_download_from_snapshot_at(
                &stale_snapshot,
                Local::now().naive_local(),
                true,
            )
            .await
            .unwrap(),
            None,
            "the stale selector must not claim after a later generation is released"
        );
        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.status, JOB_STATUS_PENDING);
        assert!(second_claim.started_at > first_claim.started_at);
        assert_eq!(job.started_at, second_claim.started_at);
    }

    #[tokio::test]
    async fn cleanup_failure_reactivation_blocks_download_until_finalize() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let first = repo
            .enqueue_eh_download(
                -100,
                901,
                "cleanup",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let downloaded = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            downloaded.id,
            downloaded.started_at.unwrap(),
            10,
            "/tmp/cleanup.zip",
            0,
        )
        .await
        .unwrap();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_CANCELED),
            )
            .filter(eh_download_queue::Column::Id.eq(first.id))
            .exec(repo.db())
            .await
            .unwrap();
        repo.evaluate_eh_job_liveness(downloaded.id, true)
            .await
            .unwrap();

        let rebound = repo
            .enqueue_eh_download(
                -200,
                901,
                "cleanup",
                "Gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(rebound.job_id, Some(downloaded.id));
        assert!(repo.get_next_eh_job_for_download().await.unwrap().is_none());

        let cleanup = repo.get_next_eh_job_for_cleanup().await.unwrap().unwrap();
        let generation = cleanup.cleanup_started_at.unwrap();
        assert!(repo
            .record_eh_job_cleanup_failure(cleanup.id, generation, "Abort failed", 0)
            .await
            .unwrap());
        assert!(repo.get_next_eh_job_for_download().await.unwrap().is_none());

        let retry = repo.get_next_eh_job_for_cleanup().await.unwrap().unwrap();
        assert_eq!(
            repo.finalize_eh_job_cleanup(retry.id, retry.cleanup_started_at.unwrap(), true)
                .await
                .unwrap(),
            Some(EhCleanupFinalizeOutcome::ReactivatedPending)
        );
        let replacement = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            replacement.id,
            replacement.started_at.unwrap(),
            20,
            "/tmp/cleanup-replacement.zip",
            0,
        )
        .await
        .unwrap();
        assert_eq!(
            eh_download_completions::Entity::find()
                .filter(eh_download_completions::Column::JobId.eq(downloaded.id))
                .count(repo.db())
                .await
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn shared_job_crash_recovery_resets_each_claim_once() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let now = Local::now().naive_local() - chrono::Duration::seconds(120);
        let variant = EhGalleryVariant::archive("1280x");
        let download = repo
            .enqueue_eh_download(
                -100,
                911,
                "download",
                "Download",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let upload = repo
            .enqueue_eh_download(
                -101,
                912,
                "upload",
                "Upload",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let background = repo
            .enqueue_eh_download(
                -102,
                913,
                "background",
                "Background",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let rewrite = repo
            .enqueue_eh_download(
                -103,
                914,
                "rewrite",
                "Rewrite",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let cleanup = repo
            .enqueue_eh_download(
                -104,
                915,
                "cleanup",
                "Cleanup",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::Status,
                Expr::value(JOB_STATUS_DOWNLOADING),
            )
            .col_expr(eh_gallery_jobs::Column::StartedAt, Expr::value(Some(now)))
            .filter(eh_gallery_jobs::Column::Id.eq(download.job_id.unwrap()))
            .exec(repo.db())
            .await
            .unwrap();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::Status,
                Expr::value(JOB_STATUS_DOWNLOADED),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphStatus,
                Expr::value(TELEGRAPH_STATUS_UPLOADING),
            )
            .col_expr(eh_gallery_jobs::Column::StartedAt, Expr::value(Some(now)))
            .filter(eh_gallery_jobs::Column::Id.eq(upload.job_id.unwrap()))
            .exec(repo.db())
            .await
            .unwrap();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStatus,
                Expr::value(Some(BACKGROUND_STATUS_RUNNING.to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::BackgroundDownloadStartedAt,
                Expr::value(Some(now)),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(background.job_id.unwrap()))
            .exec(repo.db())
            .await
            .unwrap();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteStatus,
                Expr::value(Some(TELEGRAPH_REWRITE_STATUS_REWRITING.to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteData,
                Expr::value(Some("payload".to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteStartedAt,
                Expr::value(Some(now)),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(rewrite.job_id.unwrap()))
            .exec(repo.db())
            .await
            .unwrap();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::CleanupStatus,
                Expr::value(CLEANUP_STATUS_RUNNING),
            )
            .col_expr(
                eh_gallery_jobs::Column::CleanupStartedAt,
                Expr::value(Some(now)),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(cleanup.job_id.unwrap()))
            .exec(repo.db())
            .await
            .unwrap();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_PUBLISHING),
            )
            .filter(eh_download_queue::Column::Id.eq(upload.id))
            .exec(repo.db())
            .await
            .unwrap();

        assert_eq!(
            repo.reset_stale_eh_shared_work(60, 60).await.unwrap(),
            EhStaleResetCounts {
                downloads: 1,
                uploads: 1,
                backgrounds: 1,
                rewrites: 1,
                cleanups: 1,
                deliveries: 1,
            }
        );
        let reset_download = eh_gallery_jobs::Entity::find_by_id(download.job_id.unwrap())
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let reset_upload = eh_gallery_jobs::Entity::find_by_id(upload.job_id.unwrap())
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let reset_background = eh_gallery_jobs::Entity::find_by_id(background.job_id.unwrap())
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let reset_rewrite = eh_gallery_jobs::Entity::find_by_id(rewrite.job_id.unwrap())
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let reset_cleanup = eh_gallery_jobs::Entity::find_by_id(cleanup.job_id.unwrap())
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reset_download.status, JOB_STATUS_PENDING);
        assert_eq!(reset_upload.telegraph_status, TELEGRAPH_STATUS_PENDING);
        assert_eq!(
            reset_background.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_PENDING)
        );
        assert_eq!(
            reset_rewrite.telegraph_rewrite_status.as_deref(),
            Some(TELEGRAPH_REWRITE_STATUS_PENDING)
        );
        assert_eq!(
            reset_rewrite.telegraph_rewrite_data.as_deref(),
            Some("payload")
        );
        assert_eq!(reset_cleanup.cleanup_status, CLEANUP_STATUS_PENDING);
        assert_eq!(
            eh_download_queue::Entity::find_by_id(upload.id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap()
                .status,
            DELIVERY_STATUS_WAITING
        );
        assert_eq!(
            repo.reset_stale_eh_shared_work(60, 60).await.unwrap(),
            EhStaleResetCounts::default()
        );
    }

    #[tokio::test]
    async fn stale_cleanup_failure_and_finalization_cannot_change_new_generation() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let delivery = repo
            .enqueue_eh_download(
                -100,
                916,
                "cleanup-stale",
                "Cleanup stale",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            1,
            "/tmp/cleanup-stale.zip",
            0,
        )
        .await
        .unwrap();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_CANCELED),
            )
            .filter(eh_download_queue::Column::Id.eq(delivery.id))
            .exec(repo.db())
            .await
            .unwrap();
        repo.evaluate_eh_job_liveness(download.id, true)
            .await
            .unwrap();

        let first = repo.get_next_eh_job_for_cleanup().await.unwrap().unwrap();
        let first_generation = first.cleanup_started_at.unwrap();
        assert_eq!(
            repo.reset_stale_eh_shared_work(60, 60)
                .await
                .unwrap()
                .cleanups,
            1
        );
        let second = repo.get_next_eh_job_for_cleanup().await.unwrap().unwrap();
        assert!(second.cleanup_started_at.unwrap() > first_generation);
        assert!(!repo
            .record_eh_job_cleanup_failure(first.id, first_generation, "stale", 0)
            .await
            .unwrap());
        assert_eq!(
            repo.finalize_eh_job_cleanup(first.id, first_generation, true)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            repo.finalize_eh_job_cleanup(second.id, second.cleanup_started_at.unwrap(), true)
                .await
                .unwrap(),
            Some(EhCleanupFinalizeOutcome::CleanRetired)
        );
    }

    #[tokio::test]
    async fn terminal_delivery_reenqueue_starts_a_clean_requested_wave() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let initial = repo
            .enqueue_eh_download(
                -100,
                917,
                "old-token",
                "Old title",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("980x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let initial_created_at = initial.created_at;
        let old_job_id = initial.job_id.unwrap();
        let now = Local::now().naive_local();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(DELIVERY_STATUS_DONE),
            )
            .col_expr(eh_download_queue::Column::FileSize, Expr::value(123_i64))
            .col_expr(eh_download_queue::Column::GpCost, Expr::value(45_i64))
            .col_expr(
                eh_download_queue::Column::Error,
                Expr::value(Some("exhausted error".to_string())),
            )
            .col_expr(eh_download_queue::Column::RetryCount, Expr::value(7_i32))
            .col_expr(eh_download_queue::Column::StartedAt, Expr::value(Some(now)))
            .col_expr(
                eh_download_queue::Column::CompletedAt,
                Expr::value(Some(now)),
            )
            .col_expr(
                eh_download_queue::Column::ZipPath,
                Expr::value(Some("old.zip".to_string())),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphUrl,
                Expr::value(Some("https://telegra.ph/old".to_string())),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(Some(now)),
            )
            .col_expr(
                eh_download_queue::Column::ArchiveSentAt,
                Expr::value(Some(now)),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphSentAt,
                Expr::value(Some(now)),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStatus,
                Expr::value(Some(BACKGROUND_STATUS_RUNNING.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStartedAt,
                Expr::value(Some(now)),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                Expr::value(Some(now)),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadAttemptCount,
                Expr::value(2_i32),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadError,
                Expr::value(Some("background error".to_string())),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteData,
                Expr::value(Some("rewrite data".to_string())),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(Some(TELEGRAPH_REWRITE_STATUS_REWRITING.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteAfter,
                Expr::value(Some(now)),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(Some(now)),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(Some(now)),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteRetryCount,
                Expr::value(3_i32),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteError,
                Expr::value(Some("rewrite error".to_string())),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewrittenAt,
                Expr::value(Some(now)),
            )
            .filter(eh_download_queue::Column::Id.eq(initial.id))
            .exec(repo.db())
            .await
            .unwrap();

        let reenqueued = repo
            .enqueue_eh_download(
                -100,
                917,
                "new-token",
                "New title",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("original"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");

        assert_eq!(reenqueued.id, initial.id);
        assert_eq!(reenqueued.status, DELIVERY_STATUS_WAITING);
        assert_ne!(reenqueued.job_id, Some(old_job_id));
        assert_eq!(reenqueued.token, "new-token");
        assert_eq!(reenqueued.title, "New title");
        assert!(!reenqueued.telegraph);
        assert_eq!(reenqueued.source, SOURCE_DIRECT);
        assert_eq!(reenqueued.created_at, initial_created_at);
        assert_eq!(reenqueued.file_size, 0);
        assert_eq!(reenqueued.gp_cost, 0);
        assert!(reenqueued.error.is_none());
        assert_eq!(reenqueued.retry_count, 0);
        assert!(reenqueued.started_at.is_none());
        assert!(reenqueued.completed_at.is_none());
        assert!(reenqueued.zip_path.is_none());
        assert!(reenqueued.telegraph_url.is_none());
        assert!(reenqueued.next_retry_at.is_none());
        assert!(reenqueued.archive_sent_at.is_none());
        assert!(reenqueued.telegraph_sent_at.is_none());
        assert!(reenqueued.background_download_status.is_none());
        assert!(reenqueued.background_download_started_at.is_none());
        assert!(reenqueued.background_download_next_retry_at.is_none());
        assert_eq!(reenqueued.background_download_attempt_count, 0);
        assert!(reenqueued.background_download_error.is_none());
        assert!(reenqueued.telegraph_rewrite_data.is_none());
        assert!(reenqueued.telegraph_rewrite_status.is_none());
        assert!(reenqueued.telegraph_rewrite_after.is_none());
        assert!(reenqueued.telegraph_rewrite_started_at.is_none());
        assert!(reenqueued.telegraph_rewrite_next_retry_at.is_none());
        assert_eq!(reenqueued.telegraph_rewrite_retry_count, 0);
        assert!(reenqueued.telegraph_rewrite_error.is_none());
        assert!(reenqueued.telegraph_rewritten_at.is_none());
        assert_eq!(
            repo.get_next_eh_job_for_download()
                .await
                .unwrap()
                .unwrap()
                .id,
            reenqueued.job_id.unwrap(),
            "the clean terminal re-enqueue must start a claimable new wave"
        );
    }
}
