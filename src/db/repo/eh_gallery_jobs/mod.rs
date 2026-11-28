mod download;
mod enqueue;
mod lifecycle;
#[cfg(test)]
mod tests;
mod upload;

pub(crate) use lifecycle::retire_consumerless_eh_job_in_txn;

use super::Repo;
use crate::config::EhentaiConfig;
use crate::db::entities::{eh_download_queue, eh_gallery_jobs, eh_gallery_push_ledger};
use crate::db::repo::eh_download_queue::{
    claim_generation_filter, eh_pending_telegraph_delivery_filter, is_retryable_sqlite_lock_error,
    merge_subscription_ids, merge_telegraph_subscription_ids, EH_CHAT_LOCKS, SOURCE_DIRECT,
    SOURCE_SUBSCRIPTION, SQLITE_TRANSACTION_ATTEMPTS,
};
use crate::db::repo::eh_gallery_push_ledger::{record_eh_push_in_txn, EhPushSurface};
use crate::db::repo::eh_gallery_results::{
    clear_rewritten_eh_gallery_result_in_txn, find_eh_gallery_result_in_txn,
    upsert_eh_gallery_result_in_txn,
};
use anyhow::{Context, Result};
use chrono::{Local, Timelike};
use eh_client::ArchiveArtifacts;
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

pub const LEGACY_ARTIFACT_HANDOFF_PENDING: &str = "pending";
pub const LEGACY_ARTIFACT_HANDOFF_MOVING: &str = "moving";
pub const LEGACY_ARTIFACT_HANDOFF_CONFLICT: &str = "conflict";

pub const DELIVERY_STATUS_WAITING: &str = "waiting";
pub const DELIVERY_STATUS_PUBLISHING: &str = "publishing";
pub const DELIVERY_STATUS_DONE: &str = "done";
pub const DELIVERY_STATUS_FAILED: &str = "failed";
pub const DELIVERY_STATUS_CANCELED: &str = "canceled";

const MAX_ENQUEUE_TRANSACTION_ATTEMPTS: usize = 3;
const MAIN_DOWNLOAD_RECENT_WINDOW_HOURS: i64 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EhDownloadQueue {
    Main,
    Background,
}

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

pub(crate) fn legacy_eh_gallery_job_artifact_path(
    cache_dir: &std::path::Path,
    job: &eh_gallery_jobs::Model,
) -> std::path::PathBuf {
    cache_dir.join(format!("{}_{}.zip", job.gid, job.token))
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
    source_surface
        .add(eh_pending_telegraph_delivery_filter())
        .into()
}

fn clear_background_download_state(
    update: sea_orm::UpdateMany<eh_gallery_jobs::Entity>,
) -> sea_orm::UpdateMany<eh_gallery_jobs::Entity> {
    update.set(eh_gallery_jobs::ActiveModel {
        background_download_status: Set(None),
        background_download_started_at: Set(None),
        background_download_next_retry_at: Set(None),
        background_download_attempt_count: Set(0_i32),
        background_download_error: Set(None),
        ..Default::default()
    })
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

fn archive_artifact_members(artifacts: &ArchiveArtifacts) -> [&std::path::Path; 5] {
    [
        artifacts.final_zip(),
        artifacts.assembly_scratch(),
        artifacts.parts_dir(),
        artifacts.uploads_dir(),
        artifacts.original_fallback_marker(),
    ]
}

async fn archive_artifacts_have_members(artifacts: &ArchiveArtifacts) -> Result<bool> {
    for member in archive_artifact_members(artifacts) {
        if tokio::fs::try_exists(member).await.with_context(|| {
            format!("Failed to inspect EH archive artifact {}", member.display())
        })? {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn move_archive_artifacts_without_overwrite(
    source: &ArchiveArtifacts,
    target: &ArchiveArtifacts,
) -> Result<()> {
    for (source_member, target_member) in archive_artifact_members(source)
        .into_iter()
        .zip(archive_artifact_members(target))
    {
        let source_exists = tokio::fs::try_exists(source_member)
            .await
            .with_context(|| {
                format!(
                    "Failed to inspect EH archive source {}",
                    source_member.display()
                )
            })?;
        if !source_exists {
            continue;
        }
        if tokio::fs::try_exists(target_member)
            .await
            .with_context(|| {
                format!(
                    "Failed to inspect EH archive target {}",
                    target_member.display()
                )
            })?
        {
            anyhow::bail!(
                "Refusing to overwrite existing EH archive target {}",
                target_member.display()
            );
        }
        match tokio::fs::rename(source_member, target_member).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !tokio::fs::try_exists(target_member)
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to recheck moved EH archive target {}",
                            target_member.display()
                        )
                    })?
                {
                    return Err(error.into());
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn nullable_eq<C: ColumnTrait, V: Into<sea_orm::Value>>(column: C, value: Option<V>) -> SimpleExpr {
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
    source_generation: i64,
) -> Result<()> {
    let mut update = eh_gallery_jobs::Entity::update_many()
        .set(eh_gallery_jobs::ActiveModel {
            status: Set(JOB_STATUS_PENDING.to_string()),
            telegraph_status: Set(TELEGRAPH_STATUS_NOT_REQUIRED.to_string()),
            source_generation: Set(source_generation),
            telegraph_required: Set(false),
            file_size: Set(0_i64),
            gp_cost: Set(0_i64),
            zip_path: Set(None),
            telegraph_url: Set(None),
            error: Set(None),
            retry_count: Set(0_i32),
            next_retry_at: Set(None),
            completed_at: Set(None),
            background_download_status: Set(None),
            background_download_started_at: Set(None),
            background_download_next_retry_at: Set(None),
            background_download_attempt_count: Set(0_i32),
            background_download_error: Set(None),
            telegraph_rewrite_data: Set(None),
            telegraph_rewrite_status: Set(None),
            telegraph_rewrite_after: Set(None),
            telegraph_rewrite_started_at: Set(None),
            telegraph_rewrite_next_retry_at: Set(None),
            telegraph_rewrite_retry_count: Set(0_i32),
            telegraph_rewrite_error: Set(None),
            telegraph_rewritten_at: Set(None),
            cleanup_error: Set(None),
            cleanup_next_retry_at: Set(None),
            ..Default::default()
        })
        .filter(eh_gallery_jobs::Column::Id.eq(job_id))
        .filter(nullable_eq(
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

async fn next_eh_gallery_source_generation_in_txn(
    txn: &DatabaseTransaction,
    gid: i64,
    token: &str,
    variant: &EhGalleryVariant,
) -> Result<i64> {
    let maximum = eh_gallery_jobs::Entity::find()
        .filter(eh_gallery_jobs::Column::Gid.eq(gid))
        .filter(eh_gallery_jobs::Column::Token.eq(token))
        .filter(eh_gallery_jobs::Column::DownloadMode.eq(&variant.download_mode))
        .filter(eh_gallery_jobs::Column::Resolution.eq(&variant.resolution))
        .order_by_desc(eh_gallery_jobs::Column::SourceGeneration)
        .one(txn)
        .await
        .context("Failed to read the current shared EH gallery source generation")?
        .map(|job| job.source_generation)
        .unwrap_or(0);
    maximum
        .checked_add(1)
        .context("Shared EH gallery source generation overflow")
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
