use crate::bot::notifier::Notifier;
use crate::config::EhentaiConfig;
use crate::db::entities::{eh_download_queue, eh_gallery_jobs, subscriptions};
use crate::db::repo::Repo;
use crate::db::types::{
    EhFilter, EhPendingGallery, EhTagState, EhTaskKey, SubscriptionState, TaskType,
};
use crate::scheduler::helpers::{eh_tag_subscription_state, get_chat_if_should_notify};
use anyhow::{Context, Result};
use chrono::Local;
use eh_client::{
    parser::DownloadCost, rewrite_ipfs_gateway_nodes, ArchiveArtifacts, ArchiveDownloadOptions,
    EhClient, EhGallery, ImageUploadInput, ImageUploader, IpfS3PreviewRewriteConfig,
    TelegraphClient, TelegraphImageUrlPair, TelegraphRewriteData, UploadResumeContext,
    ZipArchiveUploadInput,
};
use rand::RngExt;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

use crate::db::repo::eh_download_queue::{
    EhDeliveryClaim, EH_CHAT_LOCKS, SOURCE_DIRECT, SOURCE_SUBSCRIPTION, STATUS_PUBLISHING,
};
use crate::db::repo::eh_gallery_jobs::{
    eh_gallery_job_artifact_path, EhCleanupFinalizeOutcome, EhGalleryVariant,
    EhJobUploadFailureOutcome, DOWNLOAD_MODE_ARCHIVE, DOWNLOAD_MODE_IMAGES, DOWNLOAD_MODE_LEGACY,
    JOB_STATUS_DOWNLOADED, JOB_STATUS_DOWNLOADING, TELEGRAPH_STATUS_READY,
};

/// Maximum search pages to fetch per tick (safety cap).
const MAX_FETCH_PAGES: u32 = 5;

/// Maximum metadata entries per api.php request.
const MAX_METADATA_BATCH: usize = 25;

/// Search rate limit: minimum delay between search requests (3s + buffer).
const SEARCH_RATE_LIMIT_MS: u64 = 3500;
const EH_UPLOAD_IMAGE_CHANNEL_CAPACITY: usize = 1;
const SLOW_DOWNLOAD_BYTES_PER_SEC: u64 = 1024 * 1024;

static EH_GP_BUDGET_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[cfg(test)]
fn archive_artifacts_for_entry(
    cache_dir: &std::path::Path,
    entry: &eh_download_queue::Model,
) -> ArchiveArtifacts {
    ArchiveArtifacts::new(
        cache_dir
            .join("eh_cache")
            .join(format!("{}_{}.zip", entry.gid, entry.token)),
    )
}

fn archive_artifacts_for_job(
    cache_dir: &std::path::Path,
    job: &eh_gallery_jobs::Model,
) -> ArchiveArtifacts {
    ArchiveArtifacts::new(eh_gallery_job_artifact_path(
        &cache_dir.join("eh_cache"),
        job,
    ))
}

#[cfg(test)]
async fn cleanup_archive_artifacts(cache_dir: &std::path::Path, entry: &eh_download_queue::Model) {
    cleanup_archive_artifacts_for_gid(
        cache_dir,
        entry.gid,
        archive_artifacts_for_entry(cache_dir, entry),
    )
    .await;
}

#[cfg(test)]
async fn cleanup_archive_artifacts_for_gid(
    _cache_dir: &std::path::Path,
    gid: i64,
    artifacts: ArchiveArtifacts,
) {
    if artifacts.uploads_dir().exists() {
        warn!(
            "Preserving EH archive artifacts with multipart upload state for gid={} because this cleanup path has no Abort uploader",
            gid
        );
        return;
    }
    if let Err(e) = artifacts.remove_all().await {
        warn!(
            "Failed to delete EH archive artifacts for gid={}: {}",
            gid, e
        );
    }
}

#[derive(Debug)]
enum EhUploadStateAbortGateError {
    NoAbortUploader { gid: i64 },
    AbortFailed { gid: i64 },
}

impl std::fmt::Display for EhUploadStateAbortGateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAbortUploader { gid } => write!(
                f,
                "Cannot safely remove incomplete EH multipart upload state for gid={}: no Abort uploader configured",
                gid
            ),
            Self::AbortFailed { gid } => write!(
                f,
                "Failed to abort incomplete EH multipart uploads for gid={}",
                gid
            ),
        }
    }
}

impl std::error::Error for EhUploadStateAbortGateError {}

/// Require a provider-specific terminal Abort before deleting persisted multipart
/// upload state. The error intentionally identifies only the gallery gid: local
/// manifests can contain remote upload identifiers and must remain private.
async fn ensure_job_upload_state_aborted(
    job: &eh_gallery_jobs::Model,
    abort_uploader: Option<&dyn ImageUploader>,
) -> Result<UploadStateAbortPermit> {
    let Some(zip_path) = job.zip_path.as_deref() else {
        return Ok(UploadStateAbortPermit);
    };
    let uploads_dir = ArchiveArtifacts::new(zip_path).uploads_dir().to_path_buf();
    if !uploads_dir.exists() {
        return Ok(UploadStateAbortPermit);
    }
    let abort_uploader = abort_uploader.ok_or_else(|| {
        anyhow::Error::new(EhUploadStateAbortGateError::NoAbortUploader { gid: job.gid })
    })?;
    abort_uploader
        .abort_upload_state(&uploads_dir)
        .await
        .map_err(|_| {
            anyhow::Error::new(EhUploadStateAbortGateError::AbortFailed { gid: job.gid })
        })?;
    Ok(UploadStateAbortPermit)
}

struct UploadStateAbortPermit;

async fn remove_job_upload_state(job: &eh_gallery_jobs::Model, _permit: UploadStateAbortPermit) {
    let Some(zip_path) = job.zip_path.as_deref() else {
        return;
    };
    if let Err(error) = ArchiveArtifacts::new(zip_path).remove_upload_state().await {
        warn!(
            "Failed to delete shared EH upload state for job {} gid={}: {}",
            job.id, job.gid, error
        );
    }
}

/// Execute exactly one durable artifact-cleanup claim.  Provider Abort always
/// completes before local removal; any error records a retryable internal
/// failure and leaves the job non-claimable for normal downloads.
async fn execute_eh_job_cleanup(
    repo: &Repo,
    job: &eh_gallery_jobs::Model,
    abort_uploader: Option<&dyn ImageUploader>,
    retry_delay_secs: i64,
    send_archive: bool,
) -> Result<EhCleanupFinalizeOutcome> {
    let generation = job
        .cleanup_started_at
        .context("Claimed shared EH artifact cleanup is missing its generation")?;
    let result: Result<Option<EhCleanupFinalizeOutcome>> = async {
        let _permit = ensure_job_upload_state_aborted(job, abort_uploader).await?;
        if let Some(zip_path) = job.zip_path.as_deref() {
            ArchiveArtifacts::new(zip_path)
                .remove_all()
                .await
                .context("Failed to remove shared EH archive artifact family after Abort")?;
        }
        repo.finalize_eh_job_cleanup(job.id, generation, send_archive)
            .await
    }
    .await;
    match result {
        Ok(Some(outcome)) => Ok(outcome),
        Ok(None) => Ok(EhCleanupFinalizeOutcome::Stale),
        Err(error) => {
            let record_error = repo
                .record_eh_job_cleanup_failure(
                    job.id,
                    generation,
                    &format!("{error:#}"),
                    retry_delay_secs,
                )
                .await;
            if let Err(record_error) = record_error {
                return Err(error.context(format!(
                    "Failed to persist shared EH cleanup failure: {record_error:#}"
                )));
            }
            Err(error)
        }
    }
}

/// Claim and execute one due shared-artifact cleanup generation.
async fn run_eh_job_cleanup_maintenance_once(
    repo: &Repo,
    abort_uploader: Option<&dyn ImageUploader>,
    retry_delay_secs: i64,
    send_archive: bool,
) -> Result<Option<EhCleanupFinalizeOutcome>> {
    let Some(job) = repo.get_next_eh_job_for_cleanup().await? else {
        return Ok(None);
    };
    execute_eh_job_cleanup(repo, &job, abort_uploader, retry_delay_secs, send_archive)
        .await
        .map(Some)
}

/// Startup drains due cleanup work before workers can claim normal sources.
pub async fn drain_eh_job_cleanup_maintenance(
    repo: &Repo,
    abort_uploader: Option<&dyn ImageUploader>,
    retry_delay_secs: i64,
    send_archive: bool,
) -> Result<u64> {
    let mut drained = 0;
    while run_eh_job_cleanup_maintenance_once(repo, abort_uploader, retry_delay_secs, send_archive)
        .await?
        .is_some()
    {
        drained += 1;
    }
    Ok(drained)
}

fn gp_rate_defer_delay_secs(window_hours: u64) -> i64 {
    i64::try_from(window_hours)
        .ok()
        .and_then(|hours| hours.checked_mul(3600))
        .map(|seconds| seconds / 4)
        .unwrap_or(i64::MAX)
}

fn should_schedule_background_download(failures: i32, bytes_delta: u64, elapsed: Duration) -> bool {
    failures > 3
        && elapsed.as_secs() > 0
        && bytes_delta / elapsed.as_secs() < SLOW_DOWNLOAD_BYTES_PER_SEC
}

/// Convert a byte count to whole MiB, rounding up so partial MiB is not under-reported.
fn format_mib(bytes: u64) -> u64 {
    bytes.div_ceil(1024 * 1024)
}

/// Selected-archive size gate for logged-in EH archive downloads.
///
/// Runs after `prepare_archive_download()` and before the GP reservation / archive
/// POST. The gate is a no-op when `max_archive_size_bytes()` is `None` (i.e.
/// `max_archive_size_mb = 0`), the archiver page has no trustworthy estimate, or
/// that estimate is `0`. Only a strict estimate greater than the limit rejects;
/// equal size is allowed.
fn ensure_eh_archive_under_size_limit(
    config: &EhentaiConfig,
    estimated_size_bytes: Option<u64>,
) -> Result<()> {
    let Some(limit_bytes) = config.max_archive_size_bytes() else {
        return Ok(());
    };
    let Some(estimated_size_bytes) = estimated_size_bytes else {
        return Ok(());
    };
    if estimated_size_bytes == 0 || estimated_size_bytes <= limit_bytes {
        return Ok(());
    }

    anyhow::bail!(
        "selected EH archive size is too large: {} MiB exceeds configured {} MiB limit",
        format_mib(estimated_size_bytes),
        format_mib(limit_bytes)
    );
}

/// Outcome of `check_and_reserve_archive_cost` for a prepared archive request.
enum ArchiveCostCheck {
    /// Safe to POST `download_archive_with_request`.
    Proceed,
    /// Download should be deferred without POSTing. Caller should NOT retry the
    /// POST in this tick; the entry stays pending so it is retried after backoff.
    Defer { delay_secs: i64, reason: String },
    /// A known numeric GP cost exceeds the configured per-archive maximum and
    /// the claimed entry must fail permanently before the archive POST.
    Reject { reason: String },
}

#[derive(Debug)]
struct ArchivePolicyTransitionError;

impl std::fmt::Display for ArchivePolicyTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("archive policy failure transition failed")
    }
}

/// Outcome of `EhBackgroundDownloadWorker::download_claimed`.
///
/// `Deferred` is a non-error outcome: the entry stays in the background queue
/// with `next_retry_at = now + delay_secs` and `attempt_count` unchanged.
/// `Completed` means the ZIP was downloaded successfully and is ready to mark.
enum BackgroundDownloadOutcome {
    Completed {
        file_size: u64,
        zip_path: std::path::PathBuf,
        gp_cost: i64,
    },
    Deferred {
        reason: String,
    },
    Rejected {
        reason: String,
    },
}

/// Return the permanent-policy reason for a known numeric archive cost.
///
/// This has no database or network side effects so workers can apply it before
/// any temporary gate (including the selected archive-size gate).
fn archive_cost_policy_reject_reason(
    config: &EhentaiConfig,
    cost: &DownloadCost,
) -> Option<String> {
    let DownloadCost::Gp(gp) = cost else {
        return None;
    };
    if config.allows_archive_gp_cost(cost) {
        return None;
    }
    Some(format!(
        "EH archive GP cost {} exceeds configured max_archive_gp_cost={}",
        gp, config.max_archive_gp_cost
    ))
}

/// Shared check-and-reserve gate invoked after `prepare_archive_download()` (which GETs the
/// archiver.php page without spending GP) and before
/// `download_archive_with_request()` (which POSTs and spends GP).
///
/// Returns `Proceed` when the POST is safe to attempt, `Defer` for temporary
/// limits, or `Reject` for a numeric GP cost over the configured single-archive
/// ceiling. Both the main download worker and the background download worker
/// route through this to keep their GP guards consistent. Positive GP attempts
/// are appended to the ledger before `Proceed`; the ledger is the rolling GP
/// budget source.
///
/// Checks, in order:
/// 1. Numeric per-archive GP cost: above `max_archive_gp_cost`, reject.
/// 2. Byte rate limit: if `download_rate_window_hours` is saturated, defer.
/// 3. Unavailable / unknown costs defer conservatively.
/// 4. Positive GP costs reserve budget by appending an attempt before POSTing.
async fn check_and_reserve_archive_cost(
    repo: &Repo,
    config: &EhentaiConfig,
    job_id: Option<i32>,
    queue_id: Option<i32>,
    gid: i64,
    cost: &DownloadCost,
) -> Result<ArchiveCostCheck> {
    // 1. A static numeric policy rejection must win over all temporary quotas.
    if let Some(reason) = archive_cost_policy_reject_reason(config, cost) {
        return Ok(ArchiveCostCheck::Reject { reason });
    }

    // 2. Byte rate limit
    let window_hours = i64::try_from(config.download_rate_window_hours)
        .context("EH download rate window hours exceed the supported range")?;
    let downloaded_bytes = repo.get_eh_downloaded_bytes_in_window(window_hours).await?;
    if downloaded_bytes >= config.download_rate_limit_bytes() as i64 {
        return Ok(ArchiveCostCheck::Defer {
            delay_secs: config.download_poll_interval_sec.max(60) as i64,
            reason: format!(
                "EH byte rate limit reached ({} bytes in last {}h)",
                downloaded_bytes, config.download_rate_window_hours
            ),
        });
    }

    // 3. The page did not provide a trustworthy numeric or free cost. These
    // are transient: defer rather than permanently failing the queue entry.
    if matches!(
        cost,
        DownloadCost::Insufficient | DownloadCost::Unavailable | DownloadCost::Unknown
    ) {
        return Ok(ArchiveCostCheck::Defer {
            delay_secs: config.download_poll_interval_sec.max(60) as i64,
            reason: format!(
                "EH archive download cost is temporarily unavailable: {:?}",
                cost
            ),
        });
    }

    let DownloadCost::Gp(gp) = cost else {
        return Ok(ArchiveCostCheck::Proceed);
    };
    if *gp == 0 {
        return Ok(ArchiveCostCheck::Proceed);
    }
    let gp_cost = i64::try_from(*gp).context("EH archive GP cost exceeds supported range")?;

    if config.gp_rate_limit > 0 {
        let _budget_lock = EH_GP_BUDGET_LOCK.lock().await;
        let window_hours = config.gp_rate_window_hours_clamped();
        let spent = repo.get_eh_gp_cost_in_window(window_hours).await?;
        if i128::from(spent) + i128::from(gp_cost) > i128::from(config.gp_rate_limit) {
            return Ok(ArchiveCostCheck::Defer {
                delay_secs: gp_rate_defer_delay_secs(window_hours),
                reason: format!(
                    "EH GP rate limit would be exceeded ({} + {} > {} in last {}h)",
                    spent, gp_cost, config.gp_rate_limit, window_hours
                ),
            });
        }
        append_archive_cost_attempt(repo, job_id, queue_id, gid, gp_cost).await?;
    } else {
        append_archive_cost_attempt(repo, job_id, queue_id, gid, gp_cost).await?;
    }

    Ok(ArchiveCostCheck::Proceed)
}

async fn append_archive_cost_attempt(
    repo: &Repo,
    job_id: Option<i32>,
    queue_id: Option<i32>,
    gid: i64,
    gp_cost: i64,
) -> Result<()> {
    match (job_id, queue_id) {
        (Some(job_id), None) => {
            repo.append_eh_job_gp_spend_attempt(job_id, gid, gp_cost)
                .await?;
        }
        (None, Some(queue_id)) => {
            repo.append_eh_gp_spend_attempt(queue_id, gid, gp_cost)
                .await?;
        }
        _ => anyhow::bail!("EH archive cost reservation requires exactly one ledger owner"),
    }
    Ok(())
}

pub struct EhBackgroundDownloadWorker {
    repo: Arc<Repo>,
    client: Arc<EhClient>,
    config: Arc<EhentaiConfig>,
    cache_dir: std::path::PathBuf,
}

impl EhBackgroundDownloadWorker {
    pub fn new(
        repo: Arc<Repo>,
        client: Arc<EhClient>,
        config: Arc<EhentaiConfig>,
        cache_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            repo,
            client,
            config,
            cache_dir,
        }
    }

    pub async fn run(self) {
        let poll = self.config.download_poll_interval_sec.max(10);
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(poll));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            if let Err(e) = self.tick().await {
                error!("EhBackgroundDownloadWorker tick error: {:#}", e);
            }
        }
    }

    async fn tick(&self) -> Result<()> {
        // Pre-flight byte rate-limit check: skip claiming any entries when the
        // configured window is already saturated. Without this, the background
        // worker would happily spawn N concurrent archive POSTs (each of which
        // can spend GP) even when the main worker has already deferred.
        let window_hours = i64::try_from(self.config.download_rate_window_hours)
            .context("EH download rate window hours exceed the supported range")?;
        let downloaded_bytes = self
            .repo
            .get_eh_downloaded_bytes_in_window(window_hours)
            .await?;
        if downloaded_bytes >= self.config.download_rate_limit_bytes() as i64 {
            info!(
                "EH background download byte rate limit reached ({} bytes in last {}h), skipping this tick",
                downloaded_bytes, self.config.download_rate_window_hours
            );
            return Ok(());
        }

        let concurrency = self.config.background_download_concurrency.max(1);
        let mut tasks = JoinSet::new();
        for _ in 0..concurrency {
            let Some(job) = self
                .repo
                .get_next_eh_job_for_background_download_with_policy(self.config.send_archive)
                .await?
            else {
                break;
            };
            let worker = Self::new(
                Arc::clone(&self.repo),
                Arc::clone(&self.client),
                Arc::clone(&self.config),
                self.cache_dir.clone(),
            );
            tasks.spawn(async move { worker.process_claimed(job).await });
        }

        drain_background_download_tasks(&mut tasks).await
    }

    async fn process_claimed(&self, job: eh_gallery_jobs::Model) -> Result<()> {
        let expected_started_at = job.started_at.context(
            "Cannot process shared EH gallery background job: missing download claim started_at",
        )?;
        if !self.repo.eh_job_has_active_deliveries(job.id).await? {
            self.repo
                .retire_eh_job_without_active_deliveries(&job)
                .await?;
            info!("Retired consumerless shared EH background job {}", job.id);
            return Ok(());
        }

        let zip_path = archive_artifacts_for_job(&self.cache_dir, &job)
            .final_zip()
            .to_path_buf();
        let zip_path_str = zip_path.to_string_lossy().to_string();
        if !self
            .repo
            .persist_eh_job_archive_artifact_ownership(
                job.id,
                expected_started_at,
                &zip_path_str,
                true,
            )
            .await?
        {
            info!(
                "Skipping stale shared EH background job {} before touching its archive family",
                job.id
            );
            return Ok(());
        }

        let deliveries = self.repo.get_active_eh_job_deliveries(job.id).await?;
        let mut has_active_delivery = false;
        let mut has_notifiable_delivery = false;
        for delivery in deliveries {
            if !self
                .repo
                .eh_download_is_active(delivery.id, &delivery.status, self.config.send_archive)
                .await?
            {
                continue;
            }
            has_active_delivery = true;
            if get_chat_if_should_notify(&self.repo, delivery.chat_id)
                .await?
                .is_some()
            {
                has_notifiable_delivery = true;
                break;
            }
        }
        if !has_active_delivery {
            self.repo
                .retire_eh_job_without_active_deliveries(&job)
                .await?;
            info!("Retired canceled shared EH background job {}", job.id);
            return Ok(());
        }
        if !has_notifiable_delivery {
            let reason = "no active destination is notifiable";
            info!(
                "Deferring shared EH background gid={} because {}",
                job.gid, reason
            );
            self.repo
                .defer_eh_job_background_download(
                    job.id,
                    self.config.download_poll_interval_sec as i64,
                    reason,
                )
                .await?;
            return Ok(());
        }

        match self.download_claimed(&job).await {
            Ok(BackgroundDownloadOutcome::Completed {
                file_size,
                zip_path,
                gp_cost,
            }) => {
                self.repo
                    .mark_eh_job_background_downloaded(
                        job.id,
                        expected_started_at,
                        file_size as i64,
                        &zip_path.to_string_lossy(),
                        gp_cost,
                    )
                    .await?;
            }
            Ok(BackgroundDownloadOutcome::Deferred { reason }) => {
                // Non-error defer: `download_claimed` has already returned the
                // shared job to the background queue. Do NOT schedule a retry:
                // quota defer is not a failure and must not burn attempt_count.
                debug!(
                    "EH background download gid={} deferred without retry increment: {}",
                    job.gid, reason
                );
                self.repo
                    .evaluate_eh_job_liveness(job.id, self.config.send_archive)
                    .await?;
            }
            Ok(BackgroundDownloadOutcome::Rejected { reason }) => {
                self.repo
                    .fail_eh_job_background_download_for_archive_policy(&job, &reason)
                    .await
                    .map_err(|error| error.context(ArchivePolicyTransitionError))?;
                warn!(
                    "Rejecting EH background download for gid={} due to archive policy: {}",
                    job.gid, reason
                );
            }
            Err(e) => {
                // Real failure (network, parse, etc.): schedule a retry and
                // increment attempt_count. May become permanent.
                let (failed_job, permanent) = self
                    .repo
                    .schedule_eh_job_background_retry(
                        job.id,
                        expected_started_at,
                        &e.to_string(),
                        self.config.background_download_max_attempts,
                    )
                    .await?;
                if permanent {
                    warn!(
                        "Permanent background EH download failure for gid={}: {}",
                        job.gid, e
                    );
                    self.repo
                        .evaluate_eh_job_liveness(failed_job.id, self.config.send_archive)
                        .await?;
                } else {
                    self.repo
                        .evaluate_eh_job_liveness(failed_job.id, self.config.send_archive)
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn download_claimed(
        &self,
        job: &eh_gallery_jobs::Model,
    ) -> Result<BackgroundDownloadOutcome> {
        let gid = job.gid as u64;
        let token = &job.token;
        let eh_cache = self.cache_dir.join("eh_cache");
        tokio::fs::create_dir_all(&eh_cache).await?;
        let artifacts = archive_artifacts_for_job(&self.cache_dir, job);
        let zip_path = artifacts.final_zip().to_path_buf();

        let archive_resolution = match job.download_mode.as_str() {
            DOWNLOAD_MODE_ARCHIVE => Some(job.resolution.as_str()),
            DOWNLOAD_MODE_IMAGES => None,
            DOWNLOAD_MODE_LEGACY if self.client.is_logged_in() => match job.resolution.as_str() {
                SOURCE_DIRECT => Some(self.config.download_resolution.as_str()),
                SOURCE_SUBSCRIPTION => Some(self.config.subscription_resolution.as_str()),
                resolution => anyhow::bail!(
                    "Cannot resolve legacy shared EH gallery background job {} with resolution '{}'",
                    job.id,
                    resolution
                ),
            },
            DOWNLOAD_MODE_LEGACY => None,
            mode => anyhow::bail!(
                "Cannot download shared EH gallery background job {} with unsupported mode '{}'",
                job.id,
                mode
            ),
        };
        let (file_size, gp_cost) = if let Some(resolution) = archive_resolution {
            let archive_request = self
                .client
                .prepare_archive_download(gid, token, resolution)
                .await
                .context("Failed to prepare archive download")?;
            if let Some(reason) =
                archive_cost_policy_reject_reason(self.config.as_ref(), archive_request.cost())
            {
                return Ok(BackgroundDownloadOutcome::Rejected { reason });
            }
            ensure_eh_archive_under_size_limit(
                self.config.as_ref(),
                archive_request.estimated_size_bytes(),
            )?;

            // GP / quota reservation: same as main worker. Background downloads
            // must also reserve the shared ledger budget before archive POSTs.
            match check_and_reserve_archive_cost(
                self.repo.as_ref(),
                self.config.as_ref(),
                Some(job.id),
                None,
                job.gid,
                archive_request.cost(),
            )
            .await?
            {
                ArchiveCostCheck::Proceed => {}
                ArchiveCostCheck::Defer { delay_secs, reason } => {
                    info!(
                        "Deferring EH background download for gid={} ({}), no reservation or POST",
                        gid, reason
                    );
                    // Non-error defer: keep the entry in the background queue
                    // but push next_retry_at out by `delay_secs`. Do NOT
                    // increment attempt_count - quota exhaustion is not a
                    // retryable failure, it just needs to wait for the window
                    // to recover.
                    self.repo
                        .defer_eh_job_background_download(job.id, delay_secs, &reason)
                        .await?;
                    return Ok(BackgroundDownloadOutcome::Deferred { reason });
                }
                ArchiveCostCheck::Reject { reason } => {
                    return Ok(BackgroundDownloadOutcome::Rejected { reason });
                }
            };

            let downloaded_file_size = self
                .client
                .download_archive_with_request_and_options(
                    &archive_request,
                    &zip_path,
                    ArchiveDownloadOptions {
                        max_concurrency: self.config.archive_download_concurrency,
                    },
                )
                .await
                .context("Failed to download archive")?;
            let gp_cost = archive_request.cost().gp_amount().unwrap_or(0) as i64;
            (downloaded_file_size, gp_cost)
        } else {
            let file_size = self
                .client
                .download_gallery_images(gid, token, &zip_path)
                .await
                .context("Failed to download gallery images")?;
            (file_size, 0)
        };
        Ok(BackgroundDownloadOutcome::Completed {
            file_size,
            zip_path,
            gp_cost,
        })
    }
}

async fn drain_background_download_tasks(tasks: &mut JoinSet<Result<()>>) -> Result<()> {
    let mut first_error = None;
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                error!("EH background download task failed: {:#}", e);
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
            Err(e) => {
                let err = anyhow::Error::new(e).context("background download task failed");
                error!("EH background download task join failed: {:#}", err);
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
    }

    if let Some(err) = first_error {
        Err(err.context("one or more EH background download tasks failed"))
    } else {
        Ok(())
    }
}
// ============================================================================
// Stage 1: EhEngine — Collect (search → metadata → filter → enqueue downloads)
// ============================================================================

pub struct EhEngine {
    repo: Arc<Repo>,
    client: Arc<EhClient>,
    config: Arc<EhentaiConfig>,
    telegraph_available: bool,
    tick_interval_sec: u64,
}

impl EhEngine {
    pub fn new(
        repo: Arc<Repo>,
        client: Arc<EhClient>,
        config: Arc<EhentaiConfig>,
        telegraph_available: bool,
        tick_interval_sec: u64,
    ) -> Self {
        Self {
            repo,
            client,
            config,
            telegraph_available,
            tick_interval_sec,
        }
    }

    pub async fn run(self) {
        let mut interval =
            tokio::time::interval(tokio::time::Duration::from_secs(self.tick_interval_sec));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            if let Err(e) = self.tick().await {
                error!("EhEngine tick error: {:#}", e);
            }
        }
    }

    async fn tick(&self) -> Result<()> {
        let tasks = self
            .repo
            .get_pending_tasks_by_type(TaskType::Ehentai, 1)
            .await
            .context("Failed to fetch pending eh tasks")?;

        if let Some(task) = tasks.into_iter().next() {
            if let Err(e) = self.execute_eh_task(&task).await {
                error!("Failed to execute eh task {}: {:#}", task.id, e);
                let backoff = Local::now() + chrono::Duration::hours(1);
                if let Err(e2) = self.repo.update_task_after_poll(task.id, backoff).await {
                    error!("Failed to backoff eh task {}: {:#}", task.id, e2);
                }
            }
        }

        Ok(())
    }

    async fn execute_eh_task(&self, task: &crate::db::entities::tasks::Model) -> Result<()> {
        let key = EhTaskKey::parse(&task.value).context("Failed to parse eh task value")?;

        let subs = self
            .repo
            .list_subscriptions_by_task(task.id)
            .await
            .context("Failed to list eh subscriptions")?;

        if subs.is_empty() {
            self.schedule_next_poll(task.id).await;
            return Ok(());
        }

        let mut prepared_subs = Vec::new();
        for sub in subs {
            let state = eh_tag_subscription_state(&sub).unwrap_or_else(EhTagState::cleared);
            if state.pending_galleries.is_empty() {
                prepared_subs.push((sub, self.config.max_push_per_tick));
                continue;
            }

            let telegraph_default = self.telegraph_default(sub.eh_filter.as_ref());
            let (updated_sub, updated_state, remaining_slots) = self
                .drain_pending_backlog(
                    &sub,
                    state,
                    self.config.max_push_per_tick,
                    telegraph_default,
                )
                .await?;
            if updated_state.pending_galleries.is_empty() && remaining_slots > 0 {
                prepared_subs.push((updated_sub, remaining_slots));
            }
        }

        if prepared_subs.is_empty() {
            self.schedule_next_poll(task.id).await;
            return Ok(());
        }

        // Compute aggregate filter across subs that still have per-tick capacity.
        let eh_filters: Vec<Option<&EhFilter>> = prepared_subs
            .iter()
            .map(|(s, _)| s.eh_filter.as_ref())
            .collect();
        let agg_filter = EhFilter::aggregate(&eh_filters);

        // Determine the oldest latest_posted_ts across subs (cursor)
        let oldest_ts = prepared_subs
            .iter()
            .filter_map(|(s, _)| eh_tag_subscription_state(s).map(|st| st.latest_posted_ts))
            .min()
            .unwrap_or(0);

        // Fetch gallery refs from search
        let refs = if agg_filter.has_rating_filter() {
            self.fetch_galleries_48h(&key.query, key.category_bitmask, oldest_ts)
                .await?
        } else {
            self.fetch_galleries_since(&key.query, key.category_bitmask, oldest_ts)
                .await?
        };

        if refs.is_empty() {
            for (sub, _) in &prepared_subs {
                self.update_sub_state_no_new(sub, oldest_ts).await;
            }
            self.schedule_next_poll(task.id).await;
            return Ok(());
        }

        // Batch fetch full metadata (gives us real posted timestamp)
        let gidlist: Vec<(u64, &str)> = refs.iter().map(|g| (g.gid, g.token.as_str())).collect();

        let mut all_metadata = Vec::new();
        for chunk in gidlist.chunks(MAX_METADATA_BATCH) {
            let metadata = self
                .client
                .get_metadata(chunk)
                .await
                .context("Failed to fetch gallery metadata")?;
            all_metadata.extend(metadata);
            if chunk.len() == MAX_METADATA_BATCH {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
        }

        // Filter by real posted timestamp + aggregate filter
        let now_ts = Local::now().timestamp();
        let scan_cutoff = now_ts - (self.config.scan_window_hours as i64 * 3600);

        let filtered: Vec<EhGallery> = all_metadata
            .into_iter()
            .filter(|g| {
                if oldest_ts > 0 && g.posted <= oldest_ts {
                    return false;
                }
                if agg_filter.has_rating_filter() && g.posted < scan_cutoff.max(oldest_ts) {
                    return false;
                }
                true
            })
            .filter(|g| agg_filter.matches(g))
            .collect();

        if filtered.is_empty() {
            for (sub, _) in &prepared_subs {
                self.update_sub_state_no_new(sub, oldest_ts).await;
            }
            self.schedule_next_poll(task.id).await;
            return Ok(());
        }

        // Process each subscription
        for (sub, remaining_slots) in &prepared_subs {
            self.process_eh_sub_with_slots(sub, &filtered, *remaining_slots)
                .await?;
        }

        self.schedule_next_poll(task.id).await;
        Ok(())
    }

    /// Fetch gallery refs from search. Returns all refs found (up to MAX_FETCH_PAGES).
    async fn fetch_galleries_since(
        &self,
        query: &str,
        cats: u32,
        _oldest_ts: i64,
    ) -> Result<Vec<eh_client::EhGalleryRef>> {
        let mut all_refs = Vec::new();

        for page in 0..MAX_FETCH_PAGES {
            // Rate limit between search requests (skip before the first request)
            if page > 0 {
                tokio::time::sleep(tokio::time::Duration::from_millis(SEARCH_RATE_LIMIT_MS)).await;
            }

            let refs = self
                .client
                .search(query, cats, page)
                .await
                .context("Failed to search eh galleries")?;

            if refs.is_empty() {
                break;
            }

            all_refs.extend(refs);
        }

        // Deduplicate search results by GID
        let mut seen_gids = std::collections::HashSet::new();
        all_refs.retain(|r| seen_gids.insert(r.gid));

        Ok(all_refs)
    }

    /// 48h scan mode: same as normal mode — timestamp filtering done after metadata fetch.
    async fn fetch_galleries_48h(
        &self,
        query: &str,
        cats: u32,
        _oldest_ts: i64,
    ) -> Result<Vec<eh_client::EhGalleryRef>> {
        self.fetch_galleries_since(query, cats, 0).await
    }

    fn telegraph_default(&self, sub_filter: Option<&EhFilter>) -> bool {
        self.telegraph_available
            && (self.config.upload_telegraph || sub_filter.map(|f| f.telegraph).unwrap_or(false))
    }

    async fn drain_pending_backlog(
        &self,
        sub: &subscriptions::Model,
        mut state: EhTagState,
        mut remaining_slots: usize,
        telegraph_default: bool,
    ) -> Result<(subscriptions::Model, EhTagState, usize)> {
        let variant = EhGalleryVariant::for_request(
            self.client.is_logged_in(),
            SOURCE_SUBSCRIPTION,
            self.config.as_ref(),
        );
        if !self.repo.subscription_exists(sub.id).await? {
            info!(
                "Skipping pending EH backlog for removed subscription {}",
                sub.id
            );
            return Ok((sub.clone(), state, 0));
        }
        let mut still_pending = Vec::new();
        let backlog: Vec<_> = state.pending_galleries.drain(..).collect();
        let mut backlog_iter = backlog.into_iter();
        while let Some(pending) = backlog_iter.next() {
            if remaining_slots == 0 {
                still_pending.push(pending);
                continue;
            }
            if !self.repo.subscription_exists(sub.id).await? {
                info!(
                    "Skipping pending EH gallery {} for removed subscription {}",
                    pending.gid, sub.id
                );
                continue;
            }
            if let Err(e) = self
                .repo
                .enqueue_eh_subscription_download(
                    sub.chat_id,
                    sub.id,
                    pending.gid as i64,
                    &pending.token,
                    &pending.title,
                    telegraph_default,
                    &variant,
                )
                .await
            {
                if !self.repo.subscription_exists(sub.id).await? {
                    self.repo
                        .cancel_eh_subscription_queue_entries(sub.id, self.config.send_archive)
                        .await?;
                    info!(
                        "Skipping pending EH gallery {} for removed subscription {}",
                        pending.gid, sub.id
                    );
                    continue;
                }
                let failed_gid = pending.gid;
                still_pending.push(pending);
                still_pending.extend(backlog_iter);
                state.pending_galleries = still_pending;
                state.trim_pushed(self.config.pushed_cap);
                self.repo
                    .update_subscription_latest_data(sub.id, Some(SubscriptionState::EhTag(state)))
                    .await
                    .context("Failed to persist eh pending backlog after enqueue failure")?;
                return Err(e)
                    .with_context(|| format!("Failed to enqueue pending gallery {}", failed_gid));
            }
            if !self.repo.subscription_exists(sub.id).await? {
                self.repo
                    .cancel_eh_subscription_queue_entries(sub.id, self.config.send_archive)
                    .await?;
                info!(
                    "Removed pending EH gallery {} owner for deleted subscription {}",
                    pending.gid, sub.id
                );
                continue;
            }
            state.add_pushed_gid(pending.gid);
            remaining_slots -= 1;
        }

        state.pending_galleries = still_pending;
        if state.pending_galleries.is_empty() && state.pending_high_water_ts > 0 {
            state.latest_posted_ts = state.latest_posted_ts.max(state.pending_high_water_ts);
            state.pending_high_water_ts = 0;
        }
        state.trim_pushed(self.config.pushed_cap);
        if !self.repo.subscription_exists(sub.id).await? {
            self.repo
                .cancel_eh_subscription_queue_entries(sub.id, self.config.send_archive)
                .await?;
            return Ok((sub.clone(), state, 0));
        }
        let updated_sub = self
            .repo
            .update_subscription_latest_data(sub.id, Some(SubscriptionState::EhTag(state.clone())))
            .await
            .context("Failed to update eh subscription state")?;
        Ok((updated_sub, state, remaining_slots))
    }

    async fn process_eh_sub_with_slots(
        &self,
        sub: &crate::db::entities::subscriptions::Model,
        galleries: &[EhGallery],
        max_push: usize,
    ) -> Result<()> {
        if !self.repo.subscription_exists(sub.id).await? {
            info!("Skipping EH collect for removed subscription {}", sub.id);
            return Ok(());
        }
        let mut state = eh_tag_subscription_state(sub).unwrap_or_else(EhTagState::cleared);
        let variant = EhGalleryVariant::for_request(
            self.client.is_logged_in(),
            SOURCE_SUBSCRIPTION,
            self.config.as_ref(),
        );

        let sub_filter = sub.eh_filter.as_ref();
        let mut remaining_slots = max_push;
        let telegraph_default = self.telegraph_default(sub_filter);

        // Step 1: Consume pending backlog first (galleries from previous overflow).
        if !state.pending_galleries.is_empty() {
            let (_updated_sub, updated_state, remaining) = self
                .drain_pending_backlog(sub, state, remaining_slots, telegraph_default)
                .await?;
            state = updated_state;
            remaining_slots = remaining;
            if !state.pending_galleries.is_empty() || remaining_slots == 0 {
                return Ok(());
            }
        }

        // Step 2: Pending backlog drained. Now process new filtered galleries.
        let eligible: Vec<EhPendingGallery> = galleries
            .iter()
            .filter(|g| !state.pushed_gids.contains(&g.gid))
            .filter(|g| sub_filter.map(|f| f.matches(g)).unwrap_or(true))
            .map(|g| EhPendingGallery {
                gid: g.gid,
                token: g.token.clone(),
                title: g.title.clone(),
                posted: g.posted,
            })
            .collect();

        // Record the high-water mark: max posted timestamp among eligible galleries
        // this tick. If some overflow, this prevents cursor advance beyond unconsumed.
        let max_eligible_posted = eligible
            .iter()
            .map(|g| g.posted)
            .max()
            .unwrap_or(state.pending_high_water_ts);
        state.pending_high_water_ts = state.pending_high_water_ts.max(max_eligible_posted);

        let mut eligible_iter = eligible.into_iter();
        let mut max_enqueued_posted = state.latest_posted_ts;
        while let Some(gallery) = eligible_iter.next() {
            if remaining_slots == 0 {
                // Overflow: store in pending backlog for next tick.
                state.pending_galleries.push(gallery);
                continue;
            }
            if !self.repo.subscription_exists(sub.id).await? {
                info!(
                    "Skipping EH gallery {} for removed subscription {}",
                    gallery.gid, sub.id
                );
                continue;
            }
            if let Err(e) = self
                .repo
                .enqueue_eh_subscription_download(
                    sub.chat_id,
                    sub.id,
                    gallery.gid as i64,
                    &gallery.token,
                    &gallery.title,
                    telegraph_default,
                    &variant,
                )
                .await
            {
                if !self.repo.subscription_exists(sub.id).await? {
                    self.repo
                        .cancel_eh_subscription_queue_entries(sub.id, self.config.send_archive)
                        .await?;
                    info!(
                        "Skipping EH gallery {} for removed subscription {}",
                        gallery.gid, sub.id
                    );
                    continue;
                }
                let failed_gid = gallery.gid;
                state.pending_galleries.push(gallery);
                state.pending_galleries.extend(eligible_iter);
                state.trim_pushed(self.config.pushed_cap);
                self.repo
                    .update_subscription_latest_data(sub.id, Some(SubscriptionState::EhTag(state)))
                    .await
                    .context("Failed to persist eh collect state after enqueue failure")?;
                return Err(e).with_context(|| {
                    format!("Failed to enqueue download for gallery {}", failed_gid)
                });
            }
            if !self.repo.subscription_exists(sub.id).await? {
                self.repo
                    .cancel_eh_subscription_queue_entries(sub.id, self.config.send_archive)
                    .await?;
                info!(
                    "Removed EH gallery {} owner for deleted subscription {}",
                    gallery.gid, sub.id
                );
                continue;
            }
            state.add_pushed_gid(gallery.gid);
            max_enqueued_posted = max_enqueued_posted.max(gallery.posted);
            remaining_slots -= 1;
        }

        // Step 3: If no overflow, safely advance cursor past the entire batch.
        if state.pending_galleries.is_empty() {
            state.latest_posted_ts = state
                .latest_posted_ts
                .max(max_enqueued_posted)
                .max(state.pending_high_water_ts);
            state.pending_high_water_ts = 0;
        }

        state.trim_pushed(self.config.pushed_cap);
        if !self.repo.subscription_exists(sub.id).await? {
            self.repo
                .cancel_eh_subscription_queue_entries(sub.id, self.config.send_archive)
                .await?;
            return Ok(());
        }

        self.repo
            .update_subscription_latest_data(sub.id, Some(SubscriptionState::EhTag(state)))
            .await
            .context("Failed to update eh subscription state")?;

        Ok(())
    }

    /// Update state when no new galleries were found.
    async fn update_sub_state_no_new(
        &self,
        sub: &crate::db::entities::subscriptions::Model,
        latest_ts: i64,
    ) {
        let state = eh_tag_subscription_state(sub).unwrap_or_else(EhTagState::cleared);
        if state.latest_posted_ts == latest_ts {
            return;
        }
        let new_state = EhTagState {
            pushed_gids: state.pushed_gids,
            latest_posted_ts: if latest_ts > 0 {
                state.latest_posted_ts.max(latest_ts)
            } else {
                state.latest_posted_ts
            },
            pending_galleries: state.pending_galleries,
            pending_high_water_ts: state.pending_high_water_ts,
        };
        if let Err(e) = self
            .repo
            .update_subscription_latest_data(sub.id, Some(SubscriptionState::EhTag(new_state)))
            .await
        {
            warn!("Failed to update eh sub state: {:#}", e);
        }
    }

    async fn schedule_next_poll(&self, task_id: i32) {
        let min = self.config.min_interval_sec;
        let max = self.config.max_interval_sec;
        let delay = if max > min {
            rand::rng().random_range(min..=max)
        } else {
            max
        };
        let next = Local::now() + chrono::Duration::seconds(delay as i64);
        if let Err(e) = self.repo.update_task_after_poll(task_id, next).await {
            error!("Failed to schedule next eh poll: {:#}", e);
        }
    }
}

// ============================================================================
// Stage 2: EhDownloadWorker — Download archives from e-hentai, cache locally
// ============================================================================

pub struct EhDownloadWorker {
    repo: Arc<Repo>,
    client: Arc<EhClient>,
    config: Arc<EhentaiConfig>,
    cache_dir: std::path::PathBuf,
    startup_abort_uploader: Option<Arc<dyn ImageUploader>>,
}

impl EhDownloadWorker {
    pub fn new(
        repo: Arc<Repo>,
        client: Arc<EhClient>,
        config: Arc<EhentaiConfig>,
        cache_dir: std::path::PathBuf,
        startup_abort_uploader: Option<Arc<dyn ImageUploader>>,
    ) -> Self {
        Self {
            repo,
            client,
            config,
            cache_dir,
            startup_abort_uploader,
        }
    }

    pub async fn run(self) {
        let poll = self.config.download_poll_interval_sec.max(10);
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(poll));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            if let Err(e) = self.tick().await {
                error!("EhDownloadWorker tick error: {:#}", e);
            }
        }
    }

    async fn tick(&self) -> Result<()> {
        if let Err(error) = run_eh_job_cleanup_maintenance_once(
            self.repo.as_ref(),
            self.startup_abort_uploader.as_deref(),
            self.config.download_poll_interval_sec as i64,
            self.config.send_archive,
        )
        .await
        {
            error!(
                "Shared EH cleanup maintenance failed; continuing normal download selection: {:#}",
                error
            );
        }

        // Rate limit check
        let window_hours = i64::try_from(self.config.download_rate_window_hours)
            .context("EH download rate window hours exceed the supported range")?;
        let downloaded_bytes = self
            .repo
            .get_eh_downloaded_bytes_in_window(window_hours)
            .await?;

        if downloaded_bytes >= self.config.download_rate_limit_bytes() as i64 {
            info!("EH download rate limit reached, skipping this tick");
            return Ok(());
        }

        let job = self
            .repo
            .get_next_eh_job_for_download_with_policy(self.config.send_archive)
            .await?;
        let Some(job) = job else {
            return Ok(());
        };

        if let Err(e) = self.process(&job).await {
            error!("Download failed for shared EH job {}: {:#}", job.id, e);

            if e.downcast_ref::<ArchivePolicyTransitionError>().is_some() {
                return Err(e);
            }

            // process() wraps errors with .context(); downcast_ref only checks the
            // outermost layer. Must traverse the error chain to find eh_client::Error.
            let download_progress = e
                .chain()
                .find_map(|c| c.downcast_ref::<eh_client::Error>())
                .and_then(|client_err| match client_err {
                    eh_client::Error::DownloadInProgress {
                        attempts,
                        bytes_delta,
                        elapsed,
                        ..
                    } => Some((*attempts, *bytes_delta, *elapsed)),
                    _ => None,
                });

            if let Some((attempts, bytes_delta, elapsed)) = download_progress {
                // Transfer made real progress (>10KB/s): don't increment retry_count,
                // preserve .part file for resumption on the next tick.
                let failures = attempts as i32;
                if self.config.background_download_enabled
                    && should_schedule_background_download(failures, bytes_delta, elapsed)
                {
                    info!(
                        "Handing shared EH gid={} to background download after {} failed attempts, {} bytes in {:?} over {} archive attempts",
                        job.gid, failures, bytes_delta, elapsed, attempts
                    );
                    self.repo
                        .schedule_eh_job_background_download(
                            job.id,
                            JOB_STATUS_DOWNLOADING,
                            &e.to_string(),
                        )
                        .await?;
                } else {
                    self.repo
                        .defer_eh_job_download(
                            job.id,
                            self.config.download_poll_interval_sec as i64,
                        )
                        .await?;
                }
                self.repo
                    .evaluate_eh_job_liveness(job.id, self.config.send_archive)
                    .await?;
            } else {
                let expected_started_at = job.started_at.context(
                    "Cannot schedule shared EH gallery job retry: missing download claim started_at",
                )?;
                let (failed_job, permanent) = self
                    .repo
                    .schedule_eh_job_download_retry(
                        job.id,
                        expected_started_at,
                        &e.to_string(),
                        self.config.max_retry_count,
                    )
                    .await?;
                if permanent {
                    warn!(
                        "Permanent shared EH download failure for gid={}: {}",
                        job.gid, e
                    );
                    self.repo
                        .evaluate_eh_job_liveness(failed_job.id, self.config.send_archive)
                        .await?;
                } else {
                    self.repo
                        .evaluate_eh_job_liveness(failed_job.id, self.config.send_archive)
                        .await?;
                }
            }
        }

        Ok(())
    }

    async fn process(&self, job: &eh_gallery_jobs::Model) -> Result<()> {
        let gid = job.gid as u64;
        let token = &job.token;
        let expected_started_at = job
            .started_at
            .context("Cannot process shared EH gallery job: missing download claim started_at")?;

        if !self.repo.eh_job_has_active_deliveries(job.id).await? {
            self.repo
                .retire_eh_job_without_active_deliveries(job)
                .await?;
            info!("Retired consumerless shared EH gallery job {}", job.id);
            return Ok(());
        }

        let zip_path = archive_artifacts_for_job(&self.cache_dir, job)
            .final_zip()
            .to_path_buf();
        let zip_path_str = zip_path.to_string_lossy().to_string();
        if !self
            .repo
            .persist_eh_job_archive_artifact_ownership(
                job.id,
                expected_started_at,
                &zip_path_str,
                false,
            )
            .await?
        {
            info!(
                "Skipping stale shared EH gallery job {} before touching its archive family",
                job.id
            );
            return Ok(());
        }

        let deliveries = self.repo.get_active_eh_job_deliveries(job.id).await?;
        let mut has_active_delivery = false;
        let mut has_notifiable_delivery = false;
        for delivery in deliveries {
            if !self
                .repo
                .eh_download_is_active(delivery.id, &delivery.status, self.config.send_archive)
                .await?
            {
                continue;
            }
            has_active_delivery = true;
            if get_chat_if_should_notify(&self.repo, delivery.chat_id)
                .await?
                .is_some()
            {
                has_notifiable_delivery = true;
                break;
            }
        }
        if !has_active_delivery {
            self.repo
                .retire_eh_job_without_active_deliveries(job)
                .await?;
            info!("Retired canceled shared EH gallery job {}", job.id);
            return Ok(());
        }
        if !has_notifiable_delivery {
            info!(
                "Deferring shared EH gallery gid={} because no active destination is notifiable",
                gid
            );
            self.repo
                .defer_eh_job_download(job.id, self.config.download_poll_interval_sec as i64)
                .await?;
            return Ok(());
        }

        // Artifact ownership was durably recorded before this filesystem write.
        // Ensure cache dir exists
        let eh_cache = self.cache_dir.join("eh_cache");
        tokio::fs::create_dir_all(&eh_cache).await?;
        let artifacts = archive_artifacts_for_job(&self.cache_dir, job);
        let zip_path = artifacts.final_zip().to_path_buf();

        // Download
        let archive_resolution = match job.download_mode.as_str() {
            DOWNLOAD_MODE_ARCHIVE => Some(job.resolution.as_str()),
            DOWNLOAD_MODE_IMAGES => None,
            DOWNLOAD_MODE_LEGACY if self.client.is_logged_in() => match job.resolution.as_str() {
                SOURCE_DIRECT => Some(self.config.download_resolution.as_str()),
                SOURCE_SUBSCRIPTION => Some(self.config.subscription_resolution.as_str()),
                resolution => anyhow::bail!(
                    "Cannot resolve legacy shared EH gallery job {} with resolution '{}'",
                    job.id,
                    resolution
                ),
            },
            DOWNLOAD_MODE_LEGACY => None,
            mode => anyhow::bail!(
                "Cannot download shared EH gallery job {} with unsupported mode '{}'",
                job.id,
                mode
            ),
        };
        let (file_size, gp_cost) = if let Some(resolution) = archive_resolution {
            let archive_request = self
                .client
                .prepare_archive_download(gid, token, resolution)
                .await
                .context("Failed to prepare archive download")?;
            if let Some(reason) =
                archive_cost_policy_reject_reason(self.config.as_ref(), archive_request.cost())
            {
                self.repo
                    .fail_eh_job_for_archive_policy(job, &reason)
                    .await
                    .map_err(|error| error.context(ArchivePolicyTransitionError))?;
                warn!(
                    "Rejecting EH download for gid={} due to archive policy: {}",
                    gid, reason
                );
                return Ok(());
            }
            ensure_eh_archive_under_size_limit(
                self.config.as_ref(),
                archive_request.estimated_size_bytes(),
            )?;

            // Parse the archiver-page cost, then reserve any positive GP attempt
            // in the ledger before POSTing the archive request.
            match check_and_reserve_archive_cost(
                self.repo.as_ref(),
                self.config.as_ref(),
                Some(job.id),
                None,
                job.gid,
                archive_request.cost(),
            )
            .await?
            {
                ArchiveCostCheck::Proceed => {}
                ArchiveCostCheck::Defer { delay_secs, reason } => {
                    info!(
                        "Deferring EH download for gid={} ({}), no reservation or POST",
                        gid, reason
                    );
                    self.repo.defer_eh_job_download(job.id, delay_secs).await?;
                    return Ok(());
                }
                ArchiveCostCheck::Reject { reason } => {
                    self.repo
                        .fail_eh_job_for_archive_policy(job, &reason)
                        .await
                        .map_err(|error| error.context(ArchivePolicyTransitionError))?;
                    warn!(
                        "Rejecting EH download for gid={} due to archive policy: {}",
                        gid, reason
                    );
                    return Ok(());
                }
            };

            let downloaded_file_size = self
                .client
                .download_archive_with_request_and_options(
                    &archive_request,
                    &zip_path,
                    ArchiveDownloadOptions {
                        max_concurrency: self.config.archive_download_concurrency,
                    },
                )
                .await
                .context("Failed to download archive")?;
            let gp_cost = archive_request.cost().gp_amount().unwrap_or(0) as i64;
            (downloaded_file_size, gp_cost)
        } else {
            info!("Not logged in, using direct image download for gid={}", gid);
            let file_size = self
                .client
                .download_gallery_images(gid, token, &zip_path)
                .await
                .context("Failed to download gallery images")?;
            // Direct image downloads do not go through archiver.php and do not
            // spend GP; gp_cost is 0.
            (file_size, 0)
        };

        info!(
            "Downloaded eh gallery gid={} size={} bytes gp_cost={}",
            gid, file_size, gp_cost
        );

        self.repo
            .mark_eh_job_downloaded(
                job.id,
                expected_started_at,
                file_size as i64,
                &zip_path_str,
                gp_cost,
            )
            .await?;

        Ok(())
    }
}

// ============================================================================
// Stage 3: EhUploadWorker — Extract images from ZIP, upload images, create Telegraph page
// ============================================================================

pub struct EhUploadWorker {
    repo: Arc<Repo>,
    notifier: Notifier,
    telegraph: Arc<TelegraphClient>,
    image_uploader: Arc<dyn ImageUploader>,
    abort_uploader: Option<Arc<dyn ImageUploader>>,
    rewrite_config: Option<IpfS3PreviewRewriteConfig>,
    config: Arc<EhentaiConfig>,
}

struct ZipImageData {
    filename: String,
    data: Vec<u8>,
    uploadable_order: usize,
}

fn is_uploadable_zip_image_name(name: &str) -> bool {
    name.ends_with(".jpg")
        || name.ends_with(".jpeg")
        || name.ends_with(".png")
        || name.ends_with(".gif")
        || name.ends_with(".webp")
}

/// Collect the entry names of uploadable image files inside a ZIP archive,
/// preserving their original `ZipFile::name()` spelling and archive order.
///
/// Non-image entries (directories, metadata, thumbnails) are omitted from the
/// returned names but remain in the archive for complete uploader preflight.
fn collect_uploadable_zip_entry_names(zip_path: &std::path::Path) -> Result<Vec<String>> {
    let zip_file = std::fs::File::open(zip_path).context("Failed to open zip")?;
    let archive = zip::ZipArchive::new(zip_file).context("Failed to read zip archive")?;
    let mut names = Vec::new();
    for raw_name in archive.file_names() {
        if !raw_name.ends_with('/') && is_uploadable_zip_image_name(&raw_name.to_lowercase()) {
            names.push(raw_name.to_string());
        }
    }
    Ok(names)
}

impl EhUploadWorker {
    #[cfg(test)]
    pub fn new(
        repo: Arc<Repo>,
        notifier: Notifier,
        telegraph: Arc<TelegraphClient>,
        image_uploader: Arc<dyn ImageUploader>,
        rewrite_config: Option<IpfS3PreviewRewriteConfig>,
        config: Arc<EhentaiConfig>,
    ) -> Self {
        Self::new_with_abort_uploader(
            repo,
            notifier,
            telegraph,
            image_uploader,
            None,
            rewrite_config,
            config,
        )
    }

    pub fn new_with_abort_uploader(
        repo: Arc<Repo>,
        notifier: Notifier,
        telegraph: Arc<TelegraphClient>,
        image_uploader: Arc<dyn ImageUploader>,
        abort_uploader: Option<Arc<dyn ImageUploader>>,
        rewrite_config: Option<IpfS3PreviewRewriteConfig>,
        config: Arc<EhentaiConfig>,
    ) -> Self {
        Self {
            repo,
            notifier,
            telegraph,
            image_uploader,
            abort_uploader,
            rewrite_config,
            config,
        }
    }

    async fn abort_job_upload_state(
        &self,
        job: &eh_gallery_jobs::Model,
    ) -> Result<UploadStateAbortPermit> {
        ensure_job_upload_state_aborted(job, self.abort_uploader.as_deref()).await
    }

    pub async fn run(self) {
        let poll = self.config.download_poll_interval_sec.max(10);
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(poll));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            if let Err(e) = self.tick().await {
                error!("EhUploadWorker tick error: {:#}", e);
            }
        }
    }

    async fn tick(&self) -> Result<()> {
        let job = self.repo.get_next_eh_job_for_upload().await?;
        let Some(job) = job else {
            return Ok(());
        };
        let expected_started_at = job
            .started_at
            .context("Claimed shared EH gallery upload is missing started_at")?;

        if let Err(error) = self.process(&job).await {
            error!("Upload failed for shared EH job {}: {:#}", job.id, error);
            match self
                .repo
                .record_eh_job_upload_failure(
                    job.id,
                    expected_started_at,
                    &format!("{error:#}"),
                    self.config.max_retry_count,
                    self.config.send_archive,
                )
                .await?
            {
                EhJobUploadFailureOutcome::RetryScheduled(_) => {}
                EhJobUploadFailureOutcome::Stale => return Err(error),
                EhJobUploadFailureOutcome::Terminal { job: _, deliveries } => {
                    for delivery in deliveries {
                        let title = teloxide::utils::markdown::escape(&delivery.title);
                        let message = format!("⚠️ Telegraph 上传失败，请稍后重试\n\n📦 {}", title);
                        if let Err(notify_error) = self
                            .notifier
                            .send_text(teloxide::types::ChatId(delivery.chat_id), &message, false)
                            .await
                        {
                            error!(
                                "Failed to notify EH Telegraph delivery {} in chat {} after terminal upload failure: {:#}",
                                delivery.delivery_id, delivery.chat_id, notify_error
                            );
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn process(&self, job: &eh_gallery_jobs::Model) -> Result<()> {
        let zip_path = job
            .zip_path
            .as_ref()
            .context("zip_path is None for downloaded shared EH job")?;
        let zip_path = std::path::Path::new(zip_path);
        let artifacts = ArchiveArtifacts::new(zip_path);

        // Collect uploadable image entry names once, preserving archive order.
        // This drives both the ZIP-first upload capability and the empty-ZIP
        // guard, so an archive with no uploadable images fails fast instead of
        // creating an empty Telegraph page.
        let entry_names = collect_uploadable_zip_entry_names(zip_path)?;
        if entry_names.is_empty() {
            anyhow::bail!("No images found in downloaded EH ZIP");
        }

        // ZIP-first path: if the configured uploader can accept the whole
        // archive, upload it once and build Telegraph URLs from its per-entry
        // extraction CIDs. A `None` response falls through to per-image upload.
        if self.image_uploader.supports_zip_archive_upload() {
            let zip_bytes = tokio::fs::read(zip_path)
                .await
                .context("Failed to read zip for archive upload")?;
            let archive_manifest_path = artifacts.uploads_dir().join("archive.json");
            let archive_input = ZipArchiveUploadInput {
                filename: zip_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("gallery.zip"),
                bytes: zip_bytes.as_slice(),
                entry_names: &entry_names,
                resume_context: Some(UploadResumeContext {
                    manifest_path: &archive_manifest_path,
                    logical_object_id: "archive",
                }),
            };
            if let Some(url_pairs) = self
                .image_uploader
                .upload_zip_archive_with_url_pairs(archive_input)
                .await
                .context("Failed to upload EH ZIP archive for Telegraph page")?
            {
                if url_pairs.len() != entry_names.len() {
                    anyhow::bail!(
                        "ZIP archive uploader returned {} URLs for {} image entries",
                        url_pairs.len(),
                        entry_names.len()
                    );
                }
                self.create_telegraph_page_for_job(job, &url_pairs).await?;
                return Ok(());
            }
        }

        let (image_tx, mut image_rx) = mpsc::channel(EH_UPLOAD_IMAGE_CHANNEL_CAPACITY);
        let zip_path_owned = zip_path.to_path_buf();
        let reader = tokio::task::spawn_blocking(move || -> Result<()> {
            let zip_file = std::fs::File::open(&zip_path_owned).context("Failed to open zip")?;
            let mut archive =
                zip::ZipArchive::new(zip_file).context("Failed to read zip archive")?;

            let uploadable_image_indices = archive
                .file_names()
                .enumerate()
                .filter_map(|(i, name)| {
                    (!name.ends_with('/') && is_uploadable_zip_image_name(&name.to_lowercase()))
                        .then_some(i)
                })
                .collect::<Vec<_>>();

            for (uploadable_order, archive_index) in
                uploadable_image_indices.into_iter().enumerate()
            {
                let mut file = archive
                    .by_index(archive_index)
                    .context("Failed to read zip entry")?;

                let mut data = Vec::new();
                std::io::Read::read_to_end(&mut file, &mut data)
                    .context("Failed to read image from zip")?;
                let filename = std::path::Path::new(file.name())
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("image.jpg")
                    .to_string();

                if image_tx
                    .blocking_send(ZipImageData {
                        filename,
                        data,
                        uploadable_order,
                    })
                    .is_err()
                {
                    return Ok(());
                }
            }

            Ok(())
        });

        let mut all_url_pairs: Vec<TelegraphImageUrlPair> = Vec::new();
        while let Some(image) = image_rx.recv().await {
            let logical_object_id = format!("image-{}", image.uploadable_order);
            let manifest_path = artifacts
                .uploads_dir()
                .join(format!("{logical_object_id}.json"));
            let input = ImageUploadInput {
                filename: &image.filename,
                bytes: image.data.as_slice(),
                resume_context: Some(UploadResumeContext {
                    manifest_path: &manifest_path,
                    logical_object_id: &logical_object_id,
                }),
            };
            let urls = self
                .image_uploader
                .upload_images_with_url_pairs(&[input])
                .await
                .context("Failed to upload images for Telegraph page")?;
            all_url_pairs.extend(urls);
        }

        reader.await.context("spawn_blocking failed")??;

        if all_url_pairs.is_empty() {
            anyhow::bail!("No images uploaded by configured image uploader");
        }

        self.create_telegraph_page_for_job(job, &all_url_pairs)
            .await?;

        Ok(())
    }

    /// Create the Telegraph gallery page for a queue entry using the supplied
    /// image URL pairs, persist the resulting page URL + rewrite data, and mark
    /// the entry as uploaded.
    ///
    /// Shared by the ZIP-first path (when the uploader returns URL pairs for
    /// the whole archive) and the per-image extraction path.
    async fn create_telegraph_page_for_job(
        &self,
        job: &eh_gallery_jobs::Model,
        all_url_pairs: &[TelegraphImageUrlPair],
    ) -> Result<()> {
        let title = if job.title.is_empty() {
            "Gallery"
        } else {
            &job.title
        };

        let result = self
            .telegraph
            .create_gallery_page_with_url_pairs(
                title,
                all_url_pairs,
                self.rewrite_config
                    .as_ref()
                    .map(|config| config.preview_gateway_url.as_str()),
                self.rewrite_config
                    .as_ref()
                    .map(|config| config.public_gateway_url.as_str()),
            )
            .await
            .context("Failed to create telegraph page")?;
        let rewrite_data_json = result
            .rewrite_data
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .context("Failed to serialize Telegraph rewrite data")?;
        let page_url = result.first_page_url;

        info!(
            "Created telegraph page for shared EH job {} gid={}: {}",
            job.id, job.gid, page_url
        );

        self.repo
            .mark_eh_job_telegraph_ready(
                job.id,
                job.started_at
                    .context("Claimed shared EH upload is missing started_at")?,
                &page_url,
                rewrite_data_json.as_deref(),
                self.config.send_archive,
            )
            .await?;

        match self.abort_job_upload_state(job).await {
            Ok(abort_permit) => remove_job_upload_state(job, abort_permit).await,
            Err(abort_error) => warn!(
                "Preserving completed shared EH upload state for job {} because Abort cleanup failed: {:#}",
                job.id, abort_error
            ),
        }

        Ok(())
    }
}

// ============================================================================
// Stage 4: EhPublishWorker — Send archive ZIP and/or Telegraph link to Telegram chat
// ============================================================================

pub struct EhPublishWorker {
    repo: Arc<Repo>,
    notifier: Notifier,
    client: Arc<EhClient>,
    rewrite_delay_sec: Option<u64>,
    config: Arc<EhentaiConfig>,
    #[cfg(test)]
    publish_send_hook: Option<EhPublishSendHook>,
}

#[cfg(test)]
#[derive(Clone)]
struct EhPublishSendHook {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    after_done: Option<EhPublishCompletionHook>,
}

#[cfg(test)]
#[derive(Clone)]
struct EhPublishCompletionHook {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl EhPublishWorker {
    #[cfg(test)]
    pub fn new(
        repo: Arc<Repo>,
        notifier: Notifier,
        client: Arc<EhClient>,
        rewrite_delay_sec: Option<u64>,
        config: Arc<EhentaiConfig>,
    ) -> Self {
        Self::new_with_abort_uploader(repo, notifier, client, rewrite_delay_sec, None, config)
    }

    pub fn new_with_abort_uploader(
        repo: Arc<Repo>,
        notifier: Notifier,
        client: Arc<EhClient>,
        rewrite_delay_sec: Option<u64>,
        _abort_uploader: Option<Arc<dyn ImageUploader>>,
        config: Arc<EhentaiConfig>,
    ) -> Self {
        Self {
            repo,
            notifier,
            client,
            rewrite_delay_sec,
            config,
            #[cfg(test)]
            publish_send_hook: None,
        }
    }

    #[cfg(test)]
    fn with_test_send_hook(mut self, publish_send_hook: EhPublishSendHook) -> Self {
        self.publish_send_hook = Some(publish_send_hook);
        self
    }

    pub async fn run(self) {
        let poll = self.config.download_poll_interval_sec.max(10);
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(poll));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            if let Err(e) = self.tick().await {
                error!("EhPublishWorker tick error: {:#}", e);
            }
        }
    }

    async fn tick(&self) -> Result<()> {
        let concurrency = self.config.publish_concurrency_clamped();
        let mut tasks = JoinSet::new();
        let mut no_more_claims = false;
        let mut first_error = None;

        loop {
            while tasks.len() < concurrency && !no_more_claims {
                match self
                    .repo
                    .get_next_eh_delivery_for_publish(self.config.send_archive)
                    .await
                {
                    Ok(Some(claim)) => {
                        let worker = Self {
                            repo: Arc::clone(&self.repo),
                            notifier: self.notifier.clone(),
                            client: Arc::clone(&self.client),
                            rewrite_delay_sec: self.rewrite_delay_sec,
                            config: Arc::clone(&self.config),
                            #[cfg(test)]
                            publish_send_hook: self.publish_send_hook.clone(),
                        };
                        tasks.spawn(async move { worker.process_claimed(claim).await });
                    }
                    Ok(None) => {
                        no_more_claims = true;
                        break;
                    }
                    Err(error) => {
                        error!("Failed to claim shared EH publish delivery: {:#}", error);
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                        no_more_claims = true;
                        break;
                    }
                };
            }

            let Some(result) = tasks.join_next().await else {
                break;
            };
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    error!("Shared EH publish delivery task failed: {:#}", error);
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
                Err(error) => {
                    let error =
                        anyhow::Error::new(error).context("shared EH publish task join failed");
                    error!("Shared EH publish delivery task join failed: {:#}", error);
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        if let Some(error) = first_error {
            Err(error.context("one or more shared EH publish deliveries failed"))
        } else {
            Ok(())
        }
    }

    async fn process_claimed(&self, claim: EhDeliveryClaim) -> Result<()> {
        let _chat_guard = EH_CHAT_LOCKS.lock_chat(claim.delivery.chat_id).await;
        let Some(EhDeliveryClaim { delivery, job }) = self
            .repo
            .get_eh_delivery_publish_claim(claim.delivery.id)
            .await?
        else {
            return Ok(());
        };
        if !self
            .repo
            .eh_delivery_is_active(delivery.id, STATUS_PUBLISHING, self.config.send_archive)
            .await?
        {
            info!(
                "Skipping inactive shared EH publish delivery {} for chat {}",
                delivery.id, delivery.chat_id
            );
            return Ok(());
        }
        if get_chat_if_should_notify(&self.repo, delivery.chat_id)
            .await?
            .is_none()
        {
            self.repo
                .defer_eh_delivery_publish(
                    delivery.id,
                    self.config.download_poll_interval_sec as i64,
                )
                .await?;
            info!(
                "Deferred shared EH publish delivery {} because chat {} is not notifiable",
                delivery.id, delivery.chat_id
            );
            return Ok(());
        }

        let archive_required = self.config.send_archive && delivery.archive_sent_at.is_none();
        let telegraph_required = delivery.telegraph && delivery.telegraph_sent_at.is_none();
        if telegraph_required
            && (job.telegraph_status != TELEGRAPH_STATUS_READY || job.telegraph_url.is_none())
        {
            self.repo
                .defer_eh_delivery_publish(
                    delivery.id,
                    self.config.download_poll_interval_sec as i64,
                )
                .await?;
            return Ok(());
        }
        if archive_required && (job.status != JOB_STATUS_DOWNLOADED || job.zip_path.is_none()) {
            self.repo
                .defer_eh_delivery_publish(
                    delivery.id,
                    self.config.download_poll_interval_sec as i64,
                )
                .await?;
            return Ok(());
        }

        if archive_required {
            let zip_path = job.zip_path.as_deref().expect("archive path checked above");
            if !std::path::Path::new(zip_path).exists() {
                self.handle_missing_zip(&delivery, &job).await?;
                return Ok(());
            }
        }

        let chat_id = teloxide::types::ChatId(delivery.chat_id);
        if archive_required {
            let zip_path = std::path::Path::new(
                job.zip_path
                    .as_deref()
                    .expect("archive path checked before send"),
            );
            let caption = self.build_caption(&delivery);
            let filename = format!("{}.zip", sanitize_filename(&delivery.title));
            #[cfg(test)]
            if let Some(hook) = &self.publish_send_hook {
                hook.entered.notify_one();
                hook.release.notified().await;
            }
            if let Err(error) = self
                .notifier
                .send_document(chat_id, zip_path, &filename, &caption)
                .await
                .context("Failed to send archive document")
            {
                self.retry_delivery_after_send_failure(&delivery, error)
                    .await?;
                return Ok(());
            }
            self.repo.mark_eh_archive_delivery_sent(delivery.id).await?;
        }

        if telegraph_required {
            let telegraph_url = job
                .telegraph_url
                .as_deref()
                .expect("Telegraph readiness checked before send");
            let link_text = format!(
                "📄 [Telegraph 链接]({})",
                teloxide::utils::markdown::escape_link_url(telegraph_url)
            );
            if let Err(error) = self
                .notifier
                .send_text(chat_id, &link_text, false)
                .await
                .context("Failed to send Telegraph link")
            {
                self.retry_delivery_after_send_failure(&delivery, error)
                    .await?;
                return Ok(());
            }
            self.repo
                .mark_eh_telegraph_delivery_sent(
                    delivery.id,
                    job.id,
                    self.rewrite_delay_sec.map(|delay| delay as i64),
                )
                .await?;
        }

        self.repo
            .mark_eh_delivery_done(delivery.id, job.id, self.config.send_archive)
            .await?;
        #[cfg(test)]
        if let Some(hook) = self
            .publish_send_hook
            .as_ref()
            .and_then(|hook| hook.after_done.as_ref())
        {
            hook.entered.notify_one();
            hook.release.notified().await;
        }
        info!(
            "Published shared EH gallery gid={} job={} to chat {}",
            job.gid, job.id, delivery.chat_id
        );
        Ok(())
    }

    async fn handle_missing_zip(
        &self,
        delivery: &eh_download_queue::Model,
        job: &eh_gallery_jobs::Model,
    ) -> Result<()> {
        self.repo
            .defer_eh_delivery_publish(delivery.id, self.config.download_poll_interval_sec as i64)
            .await?;
        let expected_zip_path = job
            .zip_path
            .as_deref()
            .context("Missing shared EH ZIP reset requires a persisted path")?;
        let expected_started_at = job
            .started_at
            .context("Missing shared EH ZIP reset requires a persisted generation")?;
        let reset = self
            .repo
            .reset_eh_job_for_missing_zip(job.id, expected_started_at, expected_zip_path)
            .await?;
        if reset {
            warn!(
                "Reset shared EH job {} after its cached ZIP disappeared during delivery {}",
                job.id, delivery.id
            );
        }
        Ok(())
    }

    async fn retry_delivery_after_send_failure(
        &self,
        delivery: &eh_download_queue::Model,
        error: anyhow::Error,
    ) -> Result<()> {
        let (_updated, terminal) = self
            .repo
            .schedule_eh_delivery_retry(
                delivery.id,
                &format!("{:#}", error),
                self.config.max_retry_count,
                self.config.send_archive,
            )
            .await?;
        if terminal {
            warn!(
                "Shared EH publish delivery {} for chat {} exhausted retries: {:#}",
                delivery.id, delivery.chat_id, error
            );
        } else {
            warn!(
                "Shared EH publish delivery {} for chat {} will retry: {:#}",
                delivery.id, delivery.chat_id, error
            );
        }
        Ok(())
    }

    fn build_caption(&self, entry: &eh_download_queue::Model) -> String {
        let title = teloxide::utils::markdown::escape(&entry.title);
        let base_url = self.client.base_url();
        let gallery_url = format!(
            "{}/g/{}/{}",
            base_url.trim_end_matches('/'),
            entry.gid,
            entry.token
        );
        let url_escaped = teloxide::utils::markdown::escape_link_url(&gallery_url);
        format!("📦 {}\n\n🔗 [来源]({})", title, url_escaped)
    }
}

// ============================================================================
// Stage 5: EhTelegraphRewriteWorker — Rewrite Telegraph image URLs after send
// ============================================================================

pub struct EhTelegraphRewriteWorker {
    repo: Arc<Repo>,
    telegraph: Arc<TelegraphClient>,
    send_archive: bool,
    config: Arc<EhentaiConfig>,
}

impl EhTelegraphRewriteWorker {
    pub fn new(
        repo: Arc<Repo>,
        telegraph: Arc<TelegraphClient>,
        send_archive: bool,
        config: Arc<EhentaiConfig>,
    ) -> Self {
        Self {
            repo,
            telegraph,
            send_archive,
            config,
        }
    }

    pub async fn run(self) {
        let poll = self.config.download_poll_interval_sec.max(10);
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(poll));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            if let Err(e) = self.tick().await {
                error!("EhTelegraphRewriteWorker tick error: {:#}", e);
            }
        }
    }

    async fn tick(&self) -> Result<()> {
        let job = self.repo.get_next_eh_job_for_telegraph_rewrite().await?;
        let Some(job) = job else {
            return Ok(());
        };
        let generation = job
            .telegraph_rewrite_started_at
            .context("claimed shared EH Telegraph rewrite is missing its generation")?;

        if let Err(e) = self.process(&job).await {
            error!("Telegraph rewrite failed for job {}: {:#}", job.id, e);
            let terminal = self
                .repo
                .schedule_eh_job_telegraph_rewrite_retry(
                    job.id,
                    generation,
                    &format!("{:#}", e),
                    self.config.max_retry_count,
                )
                .await?;
            if terminal {
                self.repo
                    .evaluate_eh_job_liveness(job.id, self.send_archive)
                    .await?;
                warn!(
                    "Telegraph rewrite permanently failed for job {} after retries",
                    job.id
                );
            }
            return Ok(());
        }

        if self
            .repo
            .mark_eh_job_telegraph_rewritten(job.id, generation)
            .await?
        {
            self.repo
                .evaluate_eh_job_liveness(job.id, self.send_archive)
                .await?;
            info!(
                "Rewrote Telegraph page URLs for shared EH gid={} job {}",
                job.gid, job.id
            );
        } else {
            warn!(
                "Ignoring stale completion for shared EH Telegraph rewrite job {}",
                job.id
            );
        }

        Ok(())
    }

    async fn process(&self, job: &eh_gallery_jobs::Model) -> Result<()> {
        let data_json = job
            .telegraph_rewrite_data
            .as_deref()
            .context("shared job telegraph_rewrite_data missing for claimed rewrite")?;
        let data: TelegraphRewriteData = serde_json::from_str(data_json)
            .context("Failed to deserialize Telegraph rewrite data")?;

        for page in &data.pages {
            let content = rewrite_ipfs_gateway_nodes(
                &page.content,
                &data.preview_gateway_url,
                &data.public_gateway_url,
            );
            self.telegraph
                .edit_page(&page.path, &page.title, &content)
                .await
                .with_context(|| format!("Failed to edit Telegraph page {}", page.path))?;
        }
        Ok(())
    }
}

/// Sanitize a string for use as a filename.
fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect()
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename("test/file:name"), "test_file_name");
        assert_eq!(sanitize_filename("normal"), "normal");
        assert_eq!(sanitize_filename("a\\b/c|d*e?f"), "a_b_c_d_e_f");
    }

    #[test]
    fn test_backoff_delay() {
        assert_eq!(Repo::backoff_delay_secs(0), 60);
        assert_eq!(Repo::backoff_delay_secs(1), 60);
        assert_eq!(Repo::backoff_delay_secs(2), 300);
        assert_eq!(Repo::backoff_delay_secs(3), 900);
        assert_eq!(Repo::backoff_delay_secs(4), 3600);
        assert_eq!(Repo::backoff_delay_secs(99), 3600);
    }

    #[test]
    fn test_should_schedule_background_download_after_slow_repeated_resume_attempts() {
        assert!(should_schedule_background_download(
            4,
            2 * 1024 * 1024,
            Duration::from_secs(5)
        ));
        assert!(!should_schedule_background_download(
            3,
            2 * 1024 * 1024,
            Duration::from_secs(5)
        ));
        assert!(!should_schedule_background_download(
            4,
            10 * 1024 * 1024,
            Duration::from_secs(5)
        ));
        assert!(!should_schedule_background_download(
            4,
            1,
            Duration::from_secs(0)
        ));
    }

    #[tokio::test]
    async fn test_drain_background_download_tasks_waits_for_siblings_after_error() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let sibling_completed = Arc::new(AtomicBool::new(false));
        let mut tasks = JoinSet::new();
        tasks.spawn(async { anyhow::bail!("first task failed") });
        let sibling_completed_for_task = Arc::clone(&sibling_completed);
        tasks.spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            sibling_completed_for_task.store(true, Ordering::SeqCst);
            Ok(())
        });

        let err = drain_background_download_tasks(&mut tasks)
            .await
            .expect_err("drain should return the first task error after all tasks finish");

        assert!(err
            .to_string()
            .contains("one or more EH background download tasks failed"));
        assert!(
            sibling_completed.load(Ordering::SeqCst),
            "drain must not abort sibling tasks after the first error"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::notifier::Notifier;
    use crate::cache::FileCacheManager;
    use crate::config::EhentaiConfig;
    use crate::db::entities::tasks;
    use crate::db::entities::{
        eh_download_completions, eh_download_queue, eh_gallery_jobs, eh_gp_spend_attempts,
    };
    use crate::db::repo::eh_download_queue::{
        BACKGROUND_STATUS_PENDING, SOURCE_DIRECT, SOURCE_SUBSCRIPTION, STATUS_CANCELED,
        STATUS_DONE, STATUS_DOWNLOADED, STATUS_FAILED, STATUS_PENDING,
    };
    use crate::db::repo::tests_helpers;
    use crate::pixiv::downloader::Downloader;
    use eh_client::PixiUploader;
    use eh_client::{EhClientBuilder, EhCookies, TelegraphClient};
    use reqwest::Client;
    use sea_orm::sea_query::Expr;
    use sea_orm::{
        ActiveModelTrait, ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, PaginatorTrait,
        QueryFilter, Set, Statement,
    };
    use std::io::Write;
    use teloxide::requests::RequesterExt;
    use teloxide::Bot;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_notifier(tg_server: &MockServer) -> Notifier {
        let url = url::Url::parse(&tg_server.uri()).unwrap();
        let bot = Bot::new("fake_token").set_api_url(url);
        let throttled = bot.throttle(teloxide::adaptors::throttle::Limits::default());
        let http = Client::new();
        let cache = FileCacheManager::new("data/test_cache", 7);
        let downloader = Arc::new(Downloader::new(http, cache));
        Notifier::new(throttled, downloader)
    }

    fn make_eh_client(eh_server: &MockServer) -> Arc<EhClient> {
        Arc::new(
            EhClientBuilder::new()
                .base_url(&eh_server.uri())
                .api_url(&format!("{}/api.php", eh_server.uri()))
                .cookies(EhCookies {
                    ipb_member_id: Some("12345".into()),
                    ipb_pass_hash: Some("abc".into()),
                    igneous: None,
                    nw: true,
                })
                .build(),
        )
    }

    fn make_telegraph_client(tg_server: &MockServer) -> Arc<TelegraphClient> {
        Arc::new(TelegraphClient::new_with_urls(
            "test_token".to_string(),
            format!("{}/pixi/upload", tg_server.uri()),
            tg_server.uri(),
        ))
    }

    fn make_image_uploader(tg_server: &MockServer) -> Arc<dyn ImageUploader> {
        Arc::new(PixiUploader::new_with_url(format!(
            "{}/pixi/upload",
            tg_server.uri()
        )))
    }

    fn make_config() -> EhentaiConfig {
        EhentaiConfig {
            download_rate_limit_gb: 7,
            download_rate_window_hours: 168,
            download_poll_interval_sec: 60,
            max_push_per_tick: 3,
            max_retry_count: 3,
            send_archive: true,
            upload_telegraph: true,
            ..Default::default()
        }
    }

    fn create_test_zip(path: &std::path::Path, image_count: usize) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for i in 0..image_count {
            let name = format!("page{:03}.jpg", i);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file(name, options).unwrap();
            let data = format!("fake_image_data_{}", i);
            zip.write_all(data.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    fn seed_archive_artifact_family(zip_path: &std::path::Path) -> ArchiveArtifacts {
        let artifacts = ArchiveArtifacts::new(zip_path);
        std::fs::write(artifacts.assembly_scratch(), b"partial").unwrap();
        std::fs::create_dir_all(artifacts.parts_dir().join("nested")).unwrap();
        std::fs::write(artifacts.parts_dir().join("nested/part-0001"), b"part").unwrap();
        std::fs::create_dir_all(artifacts.uploads_dir().join("nested")).unwrap();
        std::fs::write(artifacts.uploads_dir().join("archive.json"), b"archive").unwrap();
        std::fs::write(
            artifacts.uploads_dir().join("nested/image-0.json"),
            b"image",
        )
        .unwrap();
        artifacts
    }

    #[tokio::test]
    async fn cleanup_archive_artifacts_preserves_upload_state_without_abort_capability() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let temp = tempfile::tempdir().unwrap();
        let cache_dir = temp.path().join("cache");
        let zip_path = cache_dir.join("eh_cache").join("880_cleanup-token.zip");
        std::fs::create_dir_all(zip_path.parent().unwrap()).unwrap();
        create_test_zip(&zip_path, 1);
        let artifacts = seed_archive_artifact_family(&zip_path);
        let entry = insert_queue_entry(
            &repo,
            -100,
            880,
            "cleanup-token",
            "Cleanup",
            false,
            STATUS_DOWNLOADED,
            Some(zip_path.to_str().unwrap()),
            None,
        )
        .await;

        cleanup_archive_artifacts(&cache_dir, &entry).await;

        assert!(artifacts.final_zip().exists());
        assert!(artifacts.assembly_scratch().exists());
        assert!(artifacts.parts_dir().exists());
        assert!(artifacts.uploads_dir().join("archive.json").exists());
    }

    #[tokio::test]
    async fn shared_zip_survives_first_delivery_and_is_removed_after_final_consumer() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("shared-consumers.zip");
        create_test_zip(&zip_path, 1);
        let (job, deliveries) = seed_downloaded_job_with_deliveries(
            &repo,
            881,
            "consumers",
            "Shared consumers",
            &zip_path,
            &[(-100, false, "First"), (-200, false, "Second")],
        )
        .await;

        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_DONE),
            )
            .filter(eh_download_queue::Column::Id.eq(deliveries[0].id))
            .exec(repo.db())
            .await
            .unwrap();
        repo.evaluate_eh_job_liveness(job.id, true).await.unwrap();
        let after_first = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after_first.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_NONE
        );
        assert!(
            zip_path.exists(),
            "the second active consumer still owns the ZIP"
        );

        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_DONE),
            )
            .filter(eh_download_queue::Column::Id.eq(deliveries[1].id))
            .exec(repo.db())
            .await
            .unwrap();
        repo.evaluate_eh_job_liveness(job.id, true).await.unwrap();
        let scheduled = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            scheduled.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_PENDING
        );
        assert!(
            zip_path.exists(),
            "liveness schedules but never removes artifacts"
        );

        assert_eq!(
            run_eh_job_cleanup_maintenance_once(repo.as_ref(), None, 1, true)
                .await
                .unwrap(),
            Some(EhCleanupFinalizeOutcome::CleanRetired)
        );
        assert!(
            !zip_path.exists(),
            "maintenance removes the final consumer ZIP"
        );
    }

    #[tokio::test]
    async fn abort_failure_then_enqueue_blocks_download_until_cleanup_succeeds() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("abort-first.zip");
        create_test_zip(&zip_path, 1);
        let variant = EhGalleryVariant::archive("1280x");
        let first = repo
            .enqueue_eh_download(
                -100,
                882,
                "abort",
                "Abort first",
                false,
                SOURCE_DIRECT,
                &variant,
            )
            .await
            .unwrap();
        let downloaded = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            downloaded.id,
            downloaded.started_at.unwrap(),
            10,
            &zip_path.to_string_lossy(),
            0,
        )
        .await
        .unwrap();
        let artifacts = seed_archive_artifact_family(&zip_path);
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_CANCELED),
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
                882,
                "abort",
                "Abort first",
                false,
                SOURCE_DIRECT,
                &variant,
            )
            .await
            .unwrap();
        assert_eq!(rebound.job_id, Some(downloaded.id));
        let failing_uploader = TerminalCleanupMockUploader {
            fail_abort: true,
            ..Default::default()
        };
        assert!(run_eh_job_cleanup_maintenance_once(
            repo.as_ref(),
            Some(&failing_uploader),
            0,
            true
        )
        .await
        .is_err());
        assert_terminal_cleanup_precedes_local_removal(&failing_uploader, &artifacts);
        let failed = eh_gallery_jobs::Entity::find_by_id(downloaded.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            failed.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_FAILED
        );
        assert!(failed.cleanup_next_retry_at.is_some());
        assert!(artifacts.final_zip().exists());
        assert!(artifacts.uploads_dir().join("archive.json").exists());
        assert!(repo.get_next_eh_job_for_download().await.unwrap().is_none());

        let successful_uploader = TerminalCleanupMockUploader::default();
        assert_eq!(
            run_eh_job_cleanup_maintenance_once(repo.as_ref(), Some(&successful_uploader), 0, true)
                .await
                .unwrap(),
            Some(EhCleanupFinalizeOutcome::ReactivatedPending)
        );
        assert!(!artifacts.final_zip().exists());
        assert!(!artifacts.uploads_dir().exists());
        let replacement = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            replacement.id,
            replacement.started_at.unwrap(),
            20,
            "/tmp/replacement.zip",
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
    async fn download_tick_continues_after_due_cleanup_abort_failure() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let dirty_zip = temp.path().join("dirty-cleanup.zip");
        create_test_zip(&dirty_zip, 1);
        let artifacts = seed_archive_artifact_family(&dirty_zip);
        setup_chat(&repo, -100, true).await;
        setup_chat(&repo, -200, true).await;
        setup_chat(&repo, -300, true).await;

        let dirty_delivery = repo
            .enqueue_eh_download(
                -100,
                883,
                "dirty",
                "Dirty cleanup",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let dirty_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            dirty_claim.id,
            dirty_claim.started_at.unwrap(),
            10,
            &dirty_zip.to_string_lossy(),
            0,
        )
        .await
        .unwrap();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_CANCELED),
            )
            .filter(eh_download_queue::Column::Id.eq(dirty_delivery.id))
            .exec(repo.db())
            .await
            .unwrap();
        repo.evaluate_eh_job_liveness(dirty_claim.id, true)
            .await
            .unwrap();

        let first = repo
            .enqueue_eh_download(
                -200,
                884,
                "abcd000001",
                "First valid job",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        mock_eh_gallery_page(&eh_server, 884, "abcd000001").await;
        let first_download_url = format!("{}/archive/884/token/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &first_download_url).await;
        let first_source_zip = temp.path().join("first-source.zip");
        create_test_zip(&first_source_zip, 1);
        mock_eh_archive_download(
            &eh_server,
            "/archive/884/token/0",
            std::fs::read(&first_source_zip).unwrap(),
        )
        .await;

        let failing_uploader = Arc::new(TerminalCleanupMockUploader {
            fail_abort: true,
            ..Default::default()
        });
        let mut config = make_config();
        config.background_download_enabled = false;
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
            Some(failing_uploader.clone()),
        );

        worker.tick().await.unwrap();
        let first_job = job_for_delivery(&repo, &first).await;
        assert_eq!(
            first_job.status, STATUS_DOWNLOADED,
            "unrelated job must complete after cleanup failure: {first_job:#?}"
        );

        let second = repo
            .enqueue_eh_download(
                -300,
                885,
                "abcd000002",
                "Second valid job",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        mock_eh_gallery_page(&eh_server, 885, "abcd000002").await;
        let second_download_url = format!("{}/archive/885/token/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &second_download_url).await;
        let second_source_zip = temp.path().join("second-source.zip");
        create_test_zip(&second_source_zip, 1);
        mock_eh_archive_download(
            &eh_server,
            "/archive/885/token/0",
            std::fs::read(&second_source_zip).unwrap(),
        )
        .await;
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::CleanupNextRetryAt,
                Expr::value(Some(
                    Local::now().naive_local() - chrono::Duration::seconds(1),
                )),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(dirty_claim.id))
            .exec(repo.db())
            .await
            .unwrap();

        worker.tick().await.unwrap();
        assert_eq!(
            job_for_delivery(&repo, &second).await.status,
            STATUS_DOWNLOADED
        );

        let dirty_job = eh_gallery_jobs::Entity::find_by_id(dirty_claim.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            dirty_job.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_FAILED
        );
        assert!(dirty_job.cleanup_next_retry_at.is_some());
        assert!(dirty_job.cleanup_error.is_some());
        assert_eq!(
            *failing_uploader.cleanup_calls.lock().unwrap(),
            vec![
                (artifacts.uploads_dir().to_path_buf(), true),
                (artifacts.uploads_dir().to_path_buf(), true),
            ],
            "each due cleanup must Abort before preserving local artifacts"
        );
        assert!(artifacts.final_zip().exists());
        assert!(artifacts.uploads_dir().join("archive.json").exists());
        assert!(
            repo.get_next_eh_job_for_download().await.unwrap().is_none(),
            "failed cleanup remains nonclaimable after unrelated jobs progress"
        );
    }

    #[tokio::test]
    async fn stale_consumerless_upload_keeps_owned_family_through_abort_failure() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("stale-upload-owned.zip");
        create_test_zip(&zip_path, 1);
        let artifacts = seed_archive_artifact_family(&zip_path);
        let variant = EhGalleryVariant::archive("1280x");
        let canceled = repo
            .enqueue_eh_subscription_download(
                -100,
                8821,
                8822,
                "stale-upload",
                "Stale upload",
                true,
                &variant,
            )
            .await
            .unwrap();
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            download.started_at.unwrap(),
            10,
            &zip_path.to_string_lossy(),
            0,
        )
        .await
        .unwrap();
        let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        repo.cancel_eh_subscription_queue_entries(8821, true)
            .await
            .unwrap();
        assert_eq!(
            eh_gallery_jobs::Entity::find_by_id(upload.id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap()
                .cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_NONE
        );

        assert_eq!(
            repo.reset_stale_eh_shared_work(60, 60)
                .await
                .unwrap()
                .uploads,
            1
        );
        let stale = eh_gallery_jobs::Entity::find_by_id(upload.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stale.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_NOT_REQUIRED
        );
        assert_eq!(
            stale.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_PENDING
        );
        assert_eq!(stale.zip_path.as_deref(), zip_path.to_str());

        let rebound = repo
            .enqueue_eh_download(
                -200,
                8822,
                "stale-upload",
                "Stale upload",
                true,
                SOURCE_DIRECT,
                &variant,
            )
            .await
            .unwrap();
        assert_eq!(rebound.job_id, Some(upload.id));
        assert!(repo.get_next_eh_job_for_download().await.unwrap().is_none());
        assert!(repo
            .get_next_eh_delivery_for_publish(false)
            .await
            .unwrap()
            .is_none());

        let failing_uploader = TerminalCleanupMockUploader {
            fail_abort: true,
            ..Default::default()
        };
        assert!(run_eh_job_cleanup_maintenance_once(
            repo.as_ref(),
            Some(&failing_uploader),
            0,
            true
        )
        .await
        .is_err());
        let failed = eh_gallery_jobs::Entity::find_by_id(upload.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            failed.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_FAILED
        );
        assert_eq!(failed.zip_path.as_deref(), zip_path.to_str());
        assert!(artifacts.final_zip().exists());
        assert!(artifacts.uploads_dir().exists());
        assert!(repo.get_next_eh_job_for_download().await.unwrap().is_none());

        let successful_uploader = TerminalCleanupMockUploader::default();
        assert_eq!(
            run_eh_job_cleanup_maintenance_once(
                repo.as_ref(),
                Some(&successful_uploader),
                0,
                true,
            )
            .await
            .unwrap(),
            Some(EhCleanupFinalizeOutcome::ReactivatedPending)
        );
        assert!(!artifacts.final_zip().exists());
        assert!(!artifacts.uploads_dir().exists());
        assert_eq!(
            repo.get_next_eh_job_for_download()
                .await
                .unwrap()
                .unwrap()
                .id,
            upload.id
        );
        assert_eq!(canceled.job_id, Some(upload.id));
    }

    #[tokio::test]
    async fn normal_late_completion_keeps_owned_family_until_cleanup_reactivates() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let temp_dir = tempfile::tempdir().unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let first = repo
            .enqueue_eh_download(
                -100,
                883,
                "normal-late",
                "Normal late completion",
                false,
                SOURCE_DIRECT,
                &variant,
            )
            .await
            .unwrap();
        let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        let zip_path = archive_artifacts_for_job(temp_dir.path(), &claimed)
            .final_zip()
            .to_path_buf();
        std::fs::create_dir_all(zip_path.parent().unwrap()).unwrap();
        create_test_zip(&zip_path, 1);
        let artifacts = seed_archive_artifact_family(&zip_path);
        assert!(repo
            .persist_eh_job_archive_artifact_ownership(
                claimed.id,
                claimed.started_at.unwrap(),
                &zip_path.to_string_lossy(),
                false,
            )
            .await
            .unwrap());
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_CANCELED),
            )
            .filter(eh_download_queue::Column::Id.eq(first.id))
            .exec(repo.db())
            .await
            .unwrap();

        repo.evaluate_eh_job_liveness(claimed.id, true)
            .await
            .unwrap();
        let in_flight = job_for_delivery(&repo, &first).await;
        assert_eq!(
            in_flight.status,
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_DOWNLOADING,
            "cancellation must not retire an in-flight normal writer"
        );
        assert_eq!(
            in_flight.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_NONE,
            "cleanup must not race a normal writer"
        );

        repo.mark_eh_job_downloaded(
            claimed.id,
            claimed.started_at.unwrap(),
            10,
            &zip_path.to_string_lossy(),
            0,
        )
        .await
        .unwrap();
        let settled = job_for_delivery(&repo, &first).await;
        assert_eq!(
            settled.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_PENDING
        );

        let rebound = repo
            .enqueue_eh_download(
                -200,
                883,
                "normal-late",
                "Normal late completion",
                false,
                SOURCE_DIRECT,
                &variant,
            )
            .await
            .unwrap();
        assert_eq!(rebound.job_id, Some(claimed.id));
        assert!(repo.get_next_eh_job_for_download().await.unwrap().is_none());

        let uploader = TerminalCleanupMockUploader::default();
        assert_eq!(
            run_eh_job_cleanup_maintenance_once(repo.as_ref(), Some(&uploader), 0, true)
                .await
                .unwrap(),
            Some(EhCleanupFinalizeOutcome::ReactivatedPending)
        );
        assert!(!artifacts.final_zip().exists());
        assert!(!artifacts.assembly_scratch().exists());
        assert!(!artifacts.parts_dir().exists());
        assert!(!artifacts.uploads_dir().exists());
        assert_eq!(
            repo.get_next_eh_job_for_download()
                .await
                .unwrap()
                .unwrap()
                .id,
            claimed.id
        );
    }

    #[tokio::test]
    async fn background_late_completion_keeps_owned_family_until_cleanup_reactivates() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let temp_dir = tempfile::tempdir().unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let first = repo
            .enqueue_eh_download(
                -100,
                884,
                "background-late",
                "Background late completion",
                false,
                SOURCE_DIRECT,
                &variant,
            )
            .await
            .unwrap();
        let normal_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.schedule_eh_job_background_download(
            normal_claim.id,
            normal_claim.status.as_str(),
            "test handoff",
        )
        .await
        .unwrap();
        let claimed = repo
            .get_next_eh_job_for_background_download()
            .await
            .unwrap()
            .unwrap();
        let zip_path = archive_artifacts_for_job(temp_dir.path(), &claimed)
            .final_zip()
            .to_path_buf();
        std::fs::create_dir_all(zip_path.parent().unwrap()).unwrap();
        create_test_zip(&zip_path, 1);
        let artifacts = seed_archive_artifact_family(&zip_path);
        assert!(repo
            .persist_eh_job_archive_artifact_ownership(
                claimed.id,
                claimed.started_at.unwrap(),
                &zip_path.to_string_lossy(),
                true,
            )
            .await
            .unwrap());
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_CANCELED),
            )
            .filter(eh_download_queue::Column::Id.eq(first.id))
            .exec(repo.db())
            .await
            .unwrap();

        repo.evaluate_eh_job_liveness(claimed.id, true)
            .await
            .unwrap();
        let in_flight = job_for_delivery(&repo, &first).await;
        assert_eq!(
            in_flight.status,
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_PENDING,
            "cancellation must not retire an in-flight background writer"
        );
        assert_eq!(
            in_flight.background_download_status.as_deref(),
            Some(crate::db::repo::eh_gallery_jobs::BACKGROUND_STATUS_RUNNING)
        );
        assert_eq!(
            in_flight.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_NONE,
            "cleanup must not race a background writer"
        );

        repo.mark_eh_job_background_downloaded(
            claimed.id,
            claimed.started_at.unwrap(),
            10,
            &zip_path.to_string_lossy(),
            0,
        )
        .await
        .unwrap();
        let settled = job_for_delivery(&repo, &first).await;
        assert_eq!(
            settled.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_PENDING
        );

        let rebound = repo
            .enqueue_eh_download(
                -200,
                884,
                "background-late",
                "Background late completion",
                false,
                SOURCE_DIRECT,
                &variant,
            )
            .await
            .unwrap();
        assert_eq!(rebound.job_id, Some(claimed.id));
        assert!(repo
            .get_next_eh_job_for_background_download()
            .await
            .unwrap()
            .is_none());

        let uploader = TerminalCleanupMockUploader::default();
        assert_eq!(
            run_eh_job_cleanup_maintenance_once(repo.as_ref(), Some(&uploader), 0, true)
                .await
                .unwrap(),
            Some(EhCleanupFinalizeOutcome::ReactivatedPending)
        );
        assert!(!artifacts.final_zip().exists());
        assert!(!artifacts.assembly_scratch().exists());
        assert!(!artifacts.parts_dir().exists());
        assert!(!artifacts.uploads_dir().exists());
        assert_eq!(
            repo.get_next_eh_job_for_download()
                .await
                .unwrap()
                .unwrap()
                .id,
            claimed.id
        );
    }

    fn create_test_zip_with_names(path: &std::path::Path, names: &[&str]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for name in names {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"fake_image_data").unwrap();
        }
        zip.finish().unwrap();
    }

    fn create_unsupported_encrypted_metadata_zip(path: &std::path::Path) {
        fn push_u16(bytes: &mut Vec<u8>, value: u16) {
            bytes.extend_from_slice(&value.to_le_bytes());
        }

        fn push_u32(bytes: &mut Vec<u8>, value: u32) {
            bytes.extend_from_slice(&value.to_le_bytes());
        }

        let name = b"folder/Photo.JPG";
        let mut bytes = Vec::new();
        push_u32(&mut bytes, 0x0403_4b50);
        push_u16(&mut bytes, 20);
        push_u16(&mut bytes, 1);
        push_u16(&mut bytes, 12);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u32(&mut bytes, 0);
        push_u32(&mut bytes, 0);
        push_u32(&mut bytes, 0);
        push_u16(&mut bytes, name.len() as u16);
        push_u16(&mut bytes, 0);
        bytes.extend_from_slice(name);

        let central_start = bytes.len() as u32;
        push_u32(&mut bytes, 0x0201_4b50);
        push_u16(&mut bytes, 20);
        push_u16(&mut bytes, 20);
        push_u16(&mut bytes, 1);
        push_u16(&mut bytes, 12);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u32(&mut bytes, 0);
        push_u32(&mut bytes, 0);
        push_u32(&mut bytes, 0);
        push_u16(&mut bytes, name.len() as u16);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u32(&mut bytes, 0);
        push_u32(&mut bytes, 0);
        bytes.extend_from_slice(name);

        let central_size = bytes.len() as u32 - central_start;
        push_u32(&mut bytes, 0x0605_4b50);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 1);
        push_u16(&mut bytes, 1);
        push_u32(&mut bytes, central_size);
        push_u32(&mut bytes, central_start);
        push_u16(&mut bytes, 0);
        std::fs::write(path, bytes).unwrap();
    }

    fn create_test_zip_with_unsupported_encrypted_non_image(path: &std::path::Path) {
        create_test_zip_with_names(path, &["page001.jpg", "notes.txt"]);

        let mut bytes = std::fs::read(path).unwrap();
        let mut modified_local = false;
        let mut modified_central = false;
        for offset in 0..=bytes.len() - 4 {
            if &bytes[offset..offset + 4] == b"PK\x03\x04" {
                let name_len = usize::from(u16::from_le_bytes(
                    bytes[offset + 26..offset + 28].try_into().unwrap(),
                ));
                let extra_len = usize::from(u16::from_le_bytes(
                    bytes[offset + 28..offset + 30].try_into().unwrap(),
                ));
                let name_start = offset + 30 + extra_len;
                let name_end = name_start + name_len;
                if &bytes[name_start..name_end] == b"notes.txt" {
                    bytes[offset + 6..offset + 8].copy_from_slice(&1u16.to_le_bytes());
                    bytes[offset + 8..offset + 10].copy_from_slice(&12u16.to_le_bytes());
                    modified_local = true;
                }
            } else if &bytes[offset..offset + 4] == b"PK\x01\x02" {
                let name_len = usize::from(u16::from_le_bytes(
                    bytes[offset + 28..offset + 30].try_into().unwrap(),
                ));
                let extra_len = usize::from(u16::from_le_bytes(
                    bytes[offset + 30..offset + 32].try_into().unwrap(),
                ));
                let name_start = offset + 46 + extra_len;
                let name_end = name_start + name_len;
                if &bytes[name_start..name_end] == b"notes.txt" {
                    bytes[offset + 8..offset + 10].copy_from_slice(&1u16.to_le_bytes());
                    bytes[offset + 10..offset + 12].copy_from_slice(&12u16.to_le_bytes());
                    modified_central = true;
                }
            }
        }
        assert!(modified_local && modified_central);
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn collect_uploadable_zip_entry_names_reads_unsupported_encrypted_metadata() {
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("metadata-only.zip");
        create_unsupported_encrypted_metadata_zip(&zip_path);

        let names = collect_uploadable_zip_entry_names(&zip_path).unwrap();

        assert_eq!(names, ["folder/Photo.JPG"]);
    }

    fn create_test_zip_with_sizes(path: &std::path::Path, image_sizes: &[usize]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (i, size) in image_sizes.iter().enumerate() {
            let name = format!("page{:03}.jpg", i);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file(name, options).unwrap();
            zip.write_all(&vec![b'a'; *size]).unwrap();
        }
        zip.finish().unwrap();
    }

    #[derive(Debug)]
    struct MultipartFileCount(usize);

    impl wiremock::Match for MultipartFileCount {
        fn matches(&self, request: &wiremock::Request) -> bool {
            let body = String::from_utf8_lossy(&request.body);
            body.matches("name=\"files[]\"").count() == self.0
        }
    }

    async fn mock_tg_send_document(server: &MockServer) {
        let body = serde_json::json!({
            "ok": true,
            "result": {"message_id": 42, "date": 1700000000, "chat": {"id": -100, "type": "private"}}
        });
        Mock::given(method("POST"))
            .and(path("/botfake_token/SendDocument"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    #[derive(Debug)]
    struct TelegramDocumentChat(i64);

    impl wiremock::Match for TelegramDocumentChat {
        fn matches(&self, request: &wiremock::Request) -> bool {
            let body = String::from_utf8_lossy(&request.body);
            let chat_id = self.0.to_string();
            body.contains("name=\"chat_id\"") && body.lines().any(|line| line.trim() == chat_id)
        }
    }

    async fn mock_tg_send_document_for_chat(
        server: &MockServer,
        chat_id: i64,
        status: u16,
        delay: Option<Duration>,
    ) {
        let body = if status == 200 {
            serde_json::json!({
                "ok": true,
                "result": {"message_id": 42, "date": 1700000000, "chat": {"id": chat_id, "type": "private"}}
            })
        } else {
            serde_json::json!({"ok": false, "description": "mock send failure"})
        };
        let mut response = ResponseTemplate::new(status).set_body_json(body);
        if let Some(delay) = delay {
            response = response.set_delay(delay);
        }
        Mock::given(method("POST"))
            .and(path("/botfake_token/SendDocument"))
            .and(TelegramDocumentChat(chat_id))
            .respond_with(response)
            .mount(server)
            .await;
    }

    async fn mock_tg_send_message(server: &MockServer) {
        let body = serde_json::json!({
            "ok": true,
            "result": {"message_id": 43, "date": 1700000000, "chat": {"id": -100, "type": "private"}}
        });
        Mock::given(method("POST"))
            .and(path("/botfake_token/SendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    async fn mock_eh_gallery_page(server: &MockServer, gid: u64, token: &str) {
        let html = format!(
            r#"<html><body>
            <a onclick="return popUp('/archiver.php?gid={gid}&amp;token={token}',480,320)">Archive Download</a>
            </body></html>"#,
            gid = gid,
            token = token
        );
        Mock::given(method("GET"))
            .and(path(format!("/g/{}/{}/", gid, token)))
            .respond_with(ResponseTemplate::new(200).set_body_string(html))
            .mount(server)
            .await;

        // archiver.php page: both forms marked Free! so the GP guard allows the
        // POST. The original-archive form carries the archiver_key in its
        // `action` URL so `parse_archiver_key` still finds it (taking the
        // archiver-key path in `prepare_archive_download`), and
        // `parse_archive_download_cost` correctly returns DownloadCost::Free
        // for both original and resample resolutions.
        let archiver_key = format!("{}--abc123def456", gid);
        let archiver_page_html = format!(
            r##"<html><body>
            <div>
                <div>Download Cost: &nbsp; <strong>Free!</strong></div>
                <form action="/archiver.php?gid={gid}&amp;token={token}&amp;or={archiver_key}" method="post">
                    <input type="hidden" name="dltype" value="org" />
                    <input type="submit" name="dlcheck" value="Download Original Archive" />
                </form>
            </div>
            <div>
                <div>Download Cost: &nbsp; <strong>Free!</strong></div>
                <form action="/archiver.php?gid={gid}&amp;token={token}" method="post">
                    <input type="hidden" name="dltype" value="res" />
                    <input type="submit" name="dlcheck" value="Download Resample Archive" />
                </form>
            </div>
            </body></html>"##,
            gid = gid,
            token = token,
            archiver_key = archiver_key
        );
        Mock::given(method("GET"))
            .and(path("/archiver.php"))
            .and(query_param("gid", gid.to_string()))
            .and(query_param("token", token))
            .respond_with(ResponseTemplate::new(200).set_body_string(archiver_page_html))
            .mount(server)
            .await;
    }

    async fn mock_eh_archiver_page_with_estimated_sizes(
        server: &MockServer,
        gid: u64,
        token: &str,
        original_size: &str,
        resample_size: &str,
    ) {
        let gallery_html = format!(
            r#"<html><body>
            <a onclick="return popUp('/archiver.php?gid={gid}&amp;token={token}',480,320)">Archive Download</a>
            </body></html>"#,
            gid = gid,
            token = token
        );
        Mock::given(method("GET"))
            .and(path(format!("/g/{}/{}/", gid, token)))
            .respond_with(ResponseTemplate::new(200).set_body_string(gallery_html))
            .expect(1)
            .mount(server)
            .await;

        let archiver_page_html = format!(
            r#"<html><body>
            <div style="width:180px; float:left">
                <div>Download Cost: &nbsp; <strong>Free!</strong></div>
                <form action="/archiver.php?gid={gid}&amp;token={token}" method="post">
                    <input type="hidden" name="dltype" value="org" />
                    <input type="submit" name="dlcheck" value="Download Original Archive" />
                </form>
                <p>Estimated Size: <strong>{original_size}</strong></p>
            </div>
            <div style="width:180px; float:right">
                <div>Download Cost: &nbsp; <strong>Free!</strong></div>
                <form action="/archiver.php?gid={gid}&amp;token={token}" method="post">
                    <input type="hidden" name="dltype" value="res" />
                    <input type="submit" name="dlcheck" value="Download Resample Archive" />
                </form>
                <p>Estimated Size: <strong>{resample_size}</strong></p>
            </div>
            </body></html>"#,
            gid = gid,
            token = token,
            original_size = original_size,
            resample_size = resample_size,
        );
        Mock::given(method("GET"))
            .and(path("/archiver.php"))
            .and(query_param("gid", gid.to_string()))
            .and(query_param("token", token))
            .respond_with(ResponseTemplate::new(200).set_body_string(archiver_page_html))
            .expect(1)
            .mount(server)
            .await;
    }

    async fn mock_eh_archiver_post(server: &MockServer, download_url: &str) {
        let html = format!(
            r#"<html><script>function gotonext() {{ document.location = "{}?autostart=1"; }}</script></html>"#,
            download_url
        );
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string(html))
            .mount(server)
            .await;
    }

    async fn mock_eh_archive_download(server: &MockServer, path_str: &str, zip_bytes: Vec<u8>) {
        Mock::given(method("GET"))
            .and(path(path_str))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_bytes))
            .mount(server)
            .await;
    }

    /// Mock the gallery page and archiver.php page so the archive request reports
    /// the given GP cost for the requested resolution. The archiver page uses
    /// the two-form layout (dltype=org / dltype=res) so the parser picks the
    /// correct cost based on the configured resolution.
    ///
    /// `original_cost` and `resample_cost` are the inner `<strong>` text, e.g.
    /// `"Free!"`, `"8,800 GP"`, `"Insufficient Funds"`, `"N/A"`.
    async fn mock_eh_archiver_page_with_cost(
        server: &MockServer,
        gid: u64,
        token: &str,
        original_cost: &str,
        resample_cost: &str,
    ) {
        let gallery_html = format!(
            r#"<html><body>
            <a onclick="return popUp('/archiver.php?gid={gid}&amp;token={token}',480,320)">Archive Download</a>
            </body></html>"#,
            gid = gid,
            token = token
        );
        Mock::given(method("GET"))
            .and(path(format!("/g/{}/{}/", gid, token)))
            .respond_with(ResponseTemplate::new(200).set_body_string(gallery_html))
            .mount(server)
            .await;

        let archiver_page_html = format!(
            r##"<html><body>
            <div style="width:180px; float:left">
                <div>Download Cost: &nbsp; <strong>{original_cost}</strong></div>
                <form action="/archiver.php?gid={gid}&amp;token={token}" method="post">
                    <input type="hidden" name="dltype" value="org" />
                    <input type="submit" name="dlcheck" value="Download Original Archive" />
                </form>
            </div>
            <div style="width:180px; float:right">
                <div>Download Cost: &nbsp; <strong>{resample_cost}</strong></div>
                <form action="/archiver.php?gid={gid}&amp;token={token}" method="post">
                    <input type="hidden" name="dltype" value="res" />
                    <input type="submit" name="dlcheck" value="Download Resample Archive" />
                </form>
            </div>
            </body></html>"##,
            original_cost = original_cost,
            resample_cost = resample_cost,
            gid = gid,
            token = token
        );
        Mock::given(method("GET"))
            .and(path("/archiver.php"))
            .and(query_param("gid", gid.to_string()))
            .and(query_param("token", token))
            .respond_with(ResponseTemplate::new(200).set_body_string(archiver_page_html))
            .mount(server)
            .await;
    }

    async fn mock_eh_archiver_page_with_cost_and_estimated_sizes(
        server: &MockServer,
        gid: u64,
        token: &str,
        original_cost: &str,
        resample_cost: &str,
        original_size: &str,
        resample_size: &str,
    ) {
        let gallery_html = format!(
            r#"<html><body>
            <a onclick="return popUp('/archiver.php?gid={gid}&amp;token={token}',480,320)">Archive Download</a>
            </body></html>"#,
            gid = gid,
            token = token
        );
        Mock::given(method("GET"))
            .and(path(format!("/g/{}/{}/", gid, token)))
            .respond_with(ResponseTemplate::new(200).set_body_string(gallery_html))
            .expect(1)
            .mount(server)
            .await;

        let archiver_page_html = format!(
            r##"<html><body>
            <div style="width:180px; float:left">
                <div>Download Cost: &nbsp; <strong>{original_cost}</strong></div>
                <form action="/archiver.php?gid={gid}&amp;token={token}" method="post">
                    <input type="hidden" name="dltype" value="org" />
                    <input type="submit" name="dlcheck" value="Download Original Archive" />
                </form>
                <p>Estimated Size: <strong>{original_size}</strong></p>
            </div>
            <div style="width:180px; float:right">
                <div>Download Cost: &nbsp; <strong>{resample_cost}</strong></div>
                <form action="/archiver.php?gid={gid}&amp;token={token}" method="post">
                    <input type="hidden" name="dltype" value="res" />
                    <input type="submit" name="dlcheck" value="Download Resample Archive" />
                </form>
                <p>Estimated Size: <strong>{resample_size}</strong></p>
            </div>
            </body></html>"##,
            original_cost = original_cost,
            resample_cost = resample_cost,
            original_size = original_size,
            resample_size = resample_size,
            gid = gid,
            token = token,
        );
        Mock::given(method("GET"))
            .and(path("/archiver.php"))
            .and(query_param("gid", gid.to_string()))
            .and(query_param("token", token))
            .respond_with(ResponseTemplate::new(200).set_body_string(archiver_page_html))
            .expect(1)
            .mount(server)
            .await;
    }

    async fn mock_telegraph_upload(server: &MockServer, expected_requests: u64) {
        let body =
            serde_json::json!({"success": true, "direct_url": "https://i.pixi.mg/i/abc123.jpg"});
        Mock::given(method("POST"))
            .and(path("/pixi/upload"))
            .and(MultipartFileCount(1))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(expected_requests)
            .mount(server)
            .await;
    }

    async fn mock_telegraph_create_page(server: &MockServer) {
        let body = serde_json::json!({"ok": true, "result": {"url": "https://telegra.ph/Test-Gallery-01-01"}});
        Mock::given(method("POST"))
            .and(path("/createPage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    async fn setup_chat(repo: &Repo, chat_id: i64, enabled: bool) {
        repo.upsert_chat(chat_id, "private".into(), None, enabled, Default::default())
            .await
            .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_queue_entry(
        repo: &Repo,
        chat_id: i64,
        gid: i64,
        token: &str,
        title: &str,
        telegraph: bool,
        status: &str,
        zip_path: Option<&str>,
        telegraph_url: Option<&str>,
    ) -> eh_download_queue::Model {
        let now = Local::now().naive_local();
        let job_id = if matches!(status, STATUS_PENDING | STATUS_DOWNLOADED) {
            Some(
                eh_gallery_jobs::ActiveModel {
                    gid: Set(gid),
                    token: Set(token.to_string()),
                    download_mode: Set(DOWNLOAD_MODE_ARCHIVE.to_string()),
                    resolution: Set("1280x".to_string()),
                    title: Set(title.to_string()),
                    status: Set(status.to_string()),
                    telegraph_status: Set(if status == STATUS_DOWNLOADED && telegraph {
                        crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_PENDING.to_string()
                    } else {
                        crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_NOT_REQUIRED.to_string()
                    }),
                    telegraph_required: Set(telegraph),
                    zip_path: Set(zip_path.map(str::to_string)),
                    created_at: Set(now),
                    ..Default::default()
                }
                .insert(repo.db())
                .await
                .unwrap()
                .id,
            )
        } else {
            None
        };
        let active = eh_download_queue::ActiveModel {
            job_id: Set(job_id),
            chat_id: Set(chat_id),
            gid: Set(gid),
            token: Set(token.to_string()),
            title: Set(title.to_string()),
            telegraph: Set(telegraph),
            source: Set(SOURCE_DIRECT.to_string()),
            status: Set(status.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            zip_path: Set(zip_path.map(|s| s.to_string())),
            telegraph_url: Set(telegraph_url.map(|s| s.to_string())),
            next_retry_at: Set(None),
            ..Default::default()
        };
        active.insert(repo.db()).await.unwrap()
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

    async fn migrate_seeded_delivery_to_waiting(
        repo: &Repo,
        delivery: eh_download_queue::Model,
    ) -> eh_download_queue::Model {
        let mut active: eh_download_queue::ActiveModel = delivery.into();
        active.status = Set(crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING.to_string());
        active.update(repo.db()).await.unwrap()
    }

    async fn seed_downloaded_job_with_deliveries(
        repo: &Repo,
        gid: i64,
        token: &str,
        job_title: &str,
        zip_path: &std::path::Path,
        deliveries: &[(i64, bool, &str)],
    ) -> (eh_gallery_jobs::Model, Vec<eh_download_queue::Model>) {
        let now = Local::now().naive_local();
        let telegraph_required = deliveries.iter().any(|(_, telegraph, _)| *telegraph);
        let job = eh_gallery_jobs::ActiveModel {
            gid: Set(gid),
            token: Set(token.to_string()),
            download_mode: Set(DOWNLOAD_MODE_ARCHIVE.to_string()),
            resolution: Set("1280x".to_string()),
            title: Set(job_title.to_string()),
            status: Set(crate::db::repo::eh_gallery_jobs::JOB_STATUS_DOWNLOADED.to_string()),
            telegraph_status: Set(if telegraph_required {
                crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_PENDING.to_string()
            } else {
                crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_NOT_REQUIRED.to_string()
            }),
            telegraph_required: Set(telegraph_required),
            file_size: Set(std::fs::metadata(zip_path).unwrap().len() as i64),
            zip_path: Set(Some(zip_path.to_string_lossy().to_string())),
            cleanup_status: Set(crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_NONE.to_string()),
            created_at: Set(now),
            ..Default::default()
        }
        .insert(repo.db())
        .await
        .unwrap();
        let mut seeded = Vec::with_capacity(deliveries.len());
        for (chat_id, telegraph, title) in deliveries {
            seeded.push(
                eh_download_queue::ActiveModel {
                    job_id: Set(Some(job.id)),
                    chat_id: Set(*chat_id),
                    gid: Set(gid),
                    token: Set(token.to_string()),
                    title: Set((*title).to_string()),
                    telegraph: Set(*telegraph),
                    source: Set(SOURCE_DIRECT.to_string()),
                    status: Set(
                        crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING.to_string()
                    ),
                    created_at: Set(now),
                    ..Default::default()
                }
                .insert(repo.db())
                .await
                .unwrap(),
            );
        }
        (job, seeded)
    }

    async fn seed_ready_telegraph_job_with_deliveries(
        repo: &Repo,
        gid: i64,
        deliveries: &[(i64, Option<i32>)],
    ) -> (eh_gallery_jobs::Model, Vec<eh_download_queue::Model>) {
        let variant = EhGalleryVariant::archive("1280x");
        let mut seeded = Vec::with_capacity(deliveries.len());
        for (chat_id, subscription_id) in deliveries {
            let delivery = if let Some(subscription_id) = subscription_id {
                repo.enqueue_eh_subscription_download(
                    *chat_id,
                    *subscription_id,
                    gid,
                    "token",
                    "Shared Gallery",
                    true,
                    &variant,
                )
                .await
                .unwrap()
            } else {
                repo.enqueue_eh_download(
                    *chat_id,
                    gid,
                    "token",
                    "Shared Gallery",
                    true,
                    SOURCE_DIRECT,
                    &variant,
                )
                .await
                .unwrap()
            };
            seeded.push(delivery);
        }

        let job_id = seeded[0].job_id.unwrap();
        let rewrite_data = serde_json::json!({
            "pages": [{
                "path": "Shared-Gallery-01-01",
                "title": "Shared Gallery",
                "content": [{
                    "tag": "img",
                    "attrs": {"src": "https://preview.example/ipfs/cid"}
                }]
            }],
            "preview_gateway_url": "https://preview.example",
            "public_gateway_url": "https://public.example"
        })
        .to_string();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::Status,
                Expr::value(crate::db::repo::eh_gallery_jobs::JOB_STATUS_DOWNLOADED),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphStatus,
                Expr::value(crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_READY),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphUrl,
                Expr::value(Some("https://telegra.ph/Shared-Gallery-01-01".to_string())),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteData,
                Expr::value(Some(rewrite_data)),
            )
            .col_expr(
                eh_gallery_jobs::Column::ZipPath,
                Expr::value(Some("shared-gallery.zip".to_string())),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .exec(repo.db())
            .await
            .unwrap();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_PUBLISHING),
            )
            .filter(eh_download_queue::Column::JobId.eq(job_id))
            .exec(repo.db())
            .await
            .unwrap();

        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let deliveries = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::JobId.eq(job_id))
            .all(repo.db())
            .await
            .unwrap();
        (job, deliveries)
    }

    async fn handoff_job_to_background(repo: &Repo, delivery: &eh_download_queue::Model) {
        repo.schedule_eh_job_background_download(
            delivery
                .job_id
                .expect("background worker test delivery has a shared job"),
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_PENDING,
            "test setup",
        )
        .await
        .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_subscription_queue_entry(
        repo: &Repo,
        chat_id: i64,
        subscription_ids: &str,
        gid: i64,
        token: &str,
        title: &str,
        telegraph: bool,
        status: &str,
        zip_path: Option<&str>,
        telegraph_url: Option<&str>,
    ) -> eh_download_queue::Model {
        let now = Local::now().naive_local();
        let job_id = if matches!(status, STATUS_PENDING | STATUS_DOWNLOADED) {
            Some(
                eh_gallery_jobs::ActiveModel {
                    gid: Set(gid),
                    token: Set(token.to_string()),
                    download_mode: Set(DOWNLOAD_MODE_ARCHIVE.to_string()),
                    resolution: Set("1280x".to_string()),
                    title: Set(title.to_string()),
                    status: Set(status.to_string()),
                    telegraph_status: Set(if status == STATUS_DOWNLOADED && telegraph {
                        crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_PENDING.to_string()
                    } else {
                        crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_NOT_REQUIRED.to_string()
                    }),
                    telegraph_required: Set(telegraph),
                    zip_path: Set(zip_path.map(str::to_string)),
                    created_at: Set(now),
                    ..Default::default()
                }
                .insert(repo.db())
                .await
                .unwrap()
                .id,
            )
        } else {
            None
        };
        let active = eh_download_queue::ActiveModel {
            job_id: Set(job_id),
            chat_id: Set(chat_id),
            gid: Set(gid),
            token: Set(token.to_string()),
            title: Set(title.to_string()),
            telegraph: Set(telegraph),
            source: Set(SOURCE_SUBSCRIPTION.to_string()),
            subscription_ids: Set(Some(subscription_ids.to_string())),
            status: Set(status.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            zip_path: Set(zip_path.map(|s| s.to_string())),
            telegraph_url: Set(telegraph_url.map(|s| s.to_string())),
            next_retry_at: Set(None),
            ..Default::default()
        };
        active.insert(repo.db()).await.unwrap()
    }

    async fn gp_attempts(repo: &Repo) -> Vec<eh_gp_spend_attempts::Model> {
        eh_gp_spend_attempts::Entity::find()
            .all(repo.db())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_collect_overflow_pending_enqueued_on_next_tick() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;

        // Create task and subscription
        let task_key =
            crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::new());
        let task_value = task_key.to_task_value();
        let task = repo
            .get_or_create_task(
                crate::db::types::TaskType::Ehentai,
                task_value.clone(),
                None,
            )
            .await
            .unwrap();

        // Make the task immediately available (get_or_create_task sets next_poll_at 60s in future)
        let task_id = task.id;
        let mut active: tasks::ActiveModel = task.into();
        active.next_poll_at =
            Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
        active.update(repo.db()).await.unwrap();

        repo.upsert_eh_subscription(-100, task_id, crate::db::types::TagFilter::default(), None)
            .await
            .unwrap();

        let eh_server = MockServer::start().await;
        let _tg_server = MockServer::start().await;

        mock_eh_search_with_four_galleries(&eh_server).await;
        mock_eh_metadata_for_four_galleries(&eh_server).await;

        let config = Arc::new(make_config());
        let engine = EhEngine::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::clone(&config),
            true,
            60,
        );
        engine.tick().await.unwrap();

        let queued_after_first = repo.count_pending_eh_downloads().await.unwrap();
        assert_eq!(
            queued_after_first, 3,
            "first tick should enqueue 3 galleries (max_push_per_tick=3)"
        );

        // Second tick: should consume the pending backlog (4th gallery) without re-fetching
        // from cursor. The 4th gallery was overflow, not silently dropped.
        // Reset next_poll_at to make the task available again.
        let task_model = repo
            .get_task_by_type_value(crate::db::types::TaskType::Ehentai, &task_value)
            .await
            .unwrap()
            .unwrap();
        let mut active: tasks::ActiveModel = task_model.into();
        active.next_poll_at =
            Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
        active.update(repo.db()).await.unwrap();

        engine.tick().await.unwrap();
        let queued_after_second = repo.count_pending_eh_downloads().await.unwrap();
        assert_eq!(
            queued_after_second, 4,
            "second tick should drain pending backlog: 4 total enqueued"
        );

        let sub = repo
            .list_subscriptions_by_task(task_id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let state = eh_tag_subscription_state(&sub).unwrap();
        assert!(state.pending_galleries.is_empty());
        assert_eq!(state.latest_posted_ts, 400);
        assert_eq!(state.pending_high_water_ts, 0);
    }

    #[tokio::test]
    async fn test_collect_overflow_does_not_advance_cursor_until_pending_drained() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;

        let task_key =
            crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::new());
        let task_value = task_key.to_task_value();
        let task = repo
            .get_or_create_task(
                crate::db::types::TaskType::Ehentai,
                task_value.clone(),
                None,
            )
            .await
            .unwrap();
        let task_id = task.id;
        let mut active: tasks::ActiveModel = task.into();
        active.next_poll_at =
            Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
        active.update(repo.db()).await.unwrap();

        repo.upsert_eh_subscription(-100, task_id, crate::db::types::TagFilter::default(), None)
            .await
            .unwrap();

        let eh_server = MockServer::start().await;
        mock_eh_search_with_four_galleries(&eh_server).await;
        mock_eh_metadata_for_four_galleries(&eh_server).await;

        let engine = EhEngine::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(make_config()),
            true,
            60,
        );
        engine.tick().await.unwrap();

        let sub = repo
            .list_subscriptions_by_task(task_id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let state = eh_tag_subscription_state(&sub).unwrap();
        assert_eq!(state.latest_posted_ts, 0);
        assert_eq!(state.pending_galleries.len(), 1);
        assert_eq!(state.pending_galleries[0].gid, 1004);
        assert_eq!(state.pending_high_water_ts, 400);
    }

    #[tokio::test]
    async fn test_collect_telegraph_subscription_without_token_enqueues_upload_intent() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;

        let task_key =
            crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::new());
        let task_value = task_key.to_task_value();
        let task = repo
            .get_or_create_task(
                crate::db::types::TaskType::Ehentai,
                task_value.clone(),
                None,
            )
            .await
            .unwrap();
        let task_id = task.id;
        let mut active: tasks::ActiveModel = task.into();
        active.next_poll_at =
            Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
        active.update(repo.db()).await.unwrap();

        repo.upsert_eh_subscription(
            -100,
            task_id,
            crate::db::types::TagFilter::default(),
            Some(crate::db::types::EhFilter {
                telegraph: true,
                ..Default::default()
            }),
        )
        .await
        .unwrap();

        let eh_server = MockServer::start().await;
        mock_eh_search_with_four_galleries(&eh_server).await;
        mock_eh_metadata_for_four_galleries(&eh_server).await;

        let mut config = make_config();
        config.upload_telegraph = true;
        config.telegraph_access_token = None;
        let engine = EhEngine::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            true,
            60,
        );
        engine.tick().await.unwrap();

        let claimed_download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert!(claimed_download.telegraph_required);
        repo.mark_eh_job_downloaded(
            claimed_download.id,
            claimed_download.started_at.unwrap(),
            100,
            "data/test_cache/archive.zip",
            0,
        )
        .await
        .unwrap();

        let claimed_upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        assert_eq!(claimed_upload.id, claimed_download.id);
    }

    #[tokio::test]
    async fn test_collect_telegraph_unavailable_enqueues_archive_only() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;

        let task_key =
            crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::new());
        let task_value = task_key.to_task_value();
        let task = repo
            .get_or_create_task(crate::db::types::TaskType::Ehentai, task_value, None)
            .await
            .unwrap();
        let task_id = task.id;
        let mut active: tasks::ActiveModel = task.into();
        active.next_poll_at =
            Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
        active.update(repo.db()).await.unwrap();

        repo.upsert_eh_subscription(
            -100,
            task_id,
            crate::db::types::TagFilter::default(),
            Some(crate::db::types::EhFilter {
                telegraph: true,
                ..Default::default()
            }),
        )
        .await
        .unwrap();

        let eh_server = MockServer::start().await;
        mock_eh_search_with_four_galleries(&eh_server).await;
        mock_eh_metadata_for_four_galleries(&eh_server).await;

        let mut config = make_config();
        config.upload_telegraph = true;
        config.telegraph_access_token = None;
        let engine = EhEngine::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            false,
            60,
        );
        engine.tick().await.unwrap();

        let claimed_download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert!(!claimed_download.telegraph_required);
        let downloaded = repo
            .mark_eh_job_downloaded(
                claimed_download.id,
                claimed_download.started_at.unwrap(),
                100,
                "data/test_cache/archive.zip",
                0,
            )
            .await
            .unwrap();

        assert!(!downloaded.telegraph_required);
        assert!(repo.get_next_eh_job_for_upload().await.unwrap().is_none());
        let delivery = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::JobId.eq(downloaded.id))
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert!(!delivery.telegraph);
        assert_eq!(
            delivery.status,
            crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING
        );
    }

    #[tokio::test]
    async fn test_collect_drains_pending_backlog_when_search_empty() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;

        let task_key =
            crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::new());
        let task_value = task_key.to_task_value();
        let task = repo
            .get_or_create_task(
                crate::db::types::TaskType::Ehentai,
                task_value.clone(),
                None,
            )
            .await
            .unwrap();
        let task_id = task.id;
        let mut active: tasks::ActiveModel = task.into();
        active.next_poll_at =
            Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
        active.update(repo.db()).await.unwrap();

        repo.upsert_eh_subscription(-100, task_id, crate::db::types::TagFilter::default(), None)
            .await
            .unwrap();
        let sub = repo
            .list_subscriptions_by_task(task_id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        repo.update_subscription_latest_data(
            sub.id,
            Some(SubscriptionState::EhTag(EhTagState {
                pushed_gids: Vec::new(),
                latest_posted_ts: 0,
                pending_galleries: vec![EhPendingGallery {
                    gid: 2001,
                    token: "eeeeeeeeee".to_string(),
                    title: "Pending Gallery".to_string(),
                    posted: 500,
                }],
                pending_high_water_ts: 500,
            })),
        )
        .await
        .unwrap();

        let eh_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&eh_server)
            .await;

        let engine = EhEngine::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(make_config()),
            true,
            60,
        );
        engine.tick().await.unwrap();

        assert_eq!(repo.count_pending_eh_downloads().await.unwrap(), 1);
        let sub = repo
            .list_subscriptions_by_task(task_id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let state = eh_tag_subscription_state(&sub).unwrap();
        assert!(state.pending_galleries.is_empty());
        assert_eq!(state.latest_posted_ts, 500);
        assert_eq!(state.pending_high_water_ts, 0);
    }

    #[tokio::test]
    async fn test_collect_drains_pending_backlog_before_search_failure() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;

        let task_key =
            crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::new());
        let task_value = task_key.to_task_value();
        let task = repo
            .get_or_create_task(
                crate::db::types::TaskType::Ehentai,
                task_value.clone(),
                None,
            )
            .await
            .unwrap();
        let task_id = task.id;
        let mut active: tasks::ActiveModel = task.into();
        active.next_poll_at =
            Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
        active.update(repo.db()).await.unwrap();

        repo.upsert_eh_subscription(-100, task_id, crate::db::types::TagFilter::default(), None)
            .await
            .unwrap();
        let sub = repo
            .list_subscriptions_by_task(task_id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        repo.update_subscription_latest_data(
            sub.id,
            Some(SubscriptionState::EhTag(EhTagState {
                pushed_gids: Vec::new(),
                latest_posted_ts: 0,
                pending_galleries: vec![EhPendingGallery {
                    gid: 2101,
                    token: "ffffffffff".to_string(),
                    title: "Pending Before Failure".to_string(),
                    posted: 600,
                }],
                pending_high_water_ts: 600,
            })),
        )
        .await
        .unwrap();

        let eh_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&eh_server)
            .await;

        let engine = EhEngine::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(make_config()),
            true,
            60,
        );
        engine.tick().await.unwrap();

        assert_eq!(repo.count_pending_eh_downloads().await.unwrap(), 1);
        let sub = repo
            .list_subscriptions_by_task(task_id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let state = eh_tag_subscription_state(&sub).unwrap();
        assert!(state.pending_galleries.is_empty());
        assert_eq!(state.latest_posted_ts, 600);
        let task = repo
            .get_task_by_type_value(crate::db::types::TaskType::Ehentai, &task_value)
            .await
            .unwrap()
            .unwrap();
        assert!(task.next_poll_at > chrono::Local::now().naive_local());
    }

    #[tokio::test]
    async fn test_collect_empty_search_does_not_write_zero_state_for_fresh_sub() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        setup_chat(&repo, -200, true).await;

        let task_key =
            crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::new());
        let task_value = task_key.to_task_value();
        let task = repo
            .get_or_create_task(
                crate::db::types::TaskType::Ehentai,
                task_value.clone(),
                None,
            )
            .await
            .unwrap();
        let task_id = task.id;
        let mut active: tasks::ActiveModel = task.into();
        active.next_poll_at =
            Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
        active.update(repo.db()).await.unwrap();

        repo.upsert_eh_subscription(-100, task_id, crate::db::types::TagFilter::default(), None)
            .await
            .unwrap();
        repo.upsert_eh_subscription(-200, task_id, crate::db::types::TagFilter::default(), None)
            .await
            .unwrap();
        let subs = repo.list_subscriptions_by_task(task_id).await.unwrap();
        let existing = subs.iter().find(|s| s.chat_id == -100).unwrap();
        repo.update_subscription_latest_data(
            existing.id,
            Some(SubscriptionState::EhTag(EhTagState {
                pushed_gids: vec![999],
                latest_posted_ts: 500,
                pending_galleries: Vec::new(),
                pending_high_water_ts: 0,
            })),
        )
        .await
        .unwrap();

        let eh_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&eh_server)
            .await;

        let engine = EhEngine::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(make_config()),
            true,
            60,
        );
        engine.tick().await.unwrap();

        let fresh = repo
            .list_subscriptions_by_task(task_id)
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.chat_id == -200)
            .unwrap();
        let state = eh_tag_subscription_state(&fresh).unwrap();
        assert_eq!(state.latest_posted_ts, 500);
    }

    #[tokio::test]
    async fn test_collect_enqueue_failure_persists_failed_and_remaining_backlog() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;

        repo.db()
            .execute(Statement::from_string(
                DbBackend::Sqlite,
                r#"
                CREATE TRIGGER fail_eh_enqueue_1002
                BEFORE INSERT ON eh_download_queue
                WHEN NEW.gid = 1002
                BEGIN
                    SELECT RAISE(FAIL, 'injected enqueue failure');
                END
                "#,
            ))
            .await
            .unwrap();

        let task_key =
            crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::new());
        let task_value = task_key.to_task_value();
        let task = repo
            .get_or_create_task(
                crate::db::types::TaskType::Ehentai,
                task_value.clone(),
                None,
            )
            .await
            .unwrap();
        let task_id = task.id;
        let mut active: tasks::ActiveModel = task.into();
        active.next_poll_at =
            Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
        active.update(repo.db()).await.unwrap();

        repo.upsert_eh_subscription(-100, task_id, crate::db::types::TagFilter::default(), None)
            .await
            .unwrap();

        let eh_server = MockServer::start().await;
        mock_eh_search_with_four_galleries(&eh_server).await;
        mock_eh_metadata_for_four_galleries(&eh_server).await;

        let engine = EhEngine::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(make_config()),
            true,
            60,
        );
        engine.tick().await.unwrap();

        assert_eq!(repo.count_pending_eh_downloads().await.unwrap(), 1);
        let sub = repo
            .list_subscriptions_by_task(task_id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let state = eh_tag_subscription_state(&sub).unwrap();
        assert_eq!(state.latest_posted_ts, 0);
        assert_eq!(state.pending_galleries.len(), 3);
        assert_eq!(state.pending_galleries[0].gid, 1002);
        assert_eq!(state.pending_galleries[1].gid, 1003);
        assert_eq!(state.pending_galleries[2].gid, 1004);
        assert_eq!(state.pending_high_water_ts, 400);
        let task = repo
            .get_task_by_type_value(crate::db::types::TaskType::Ehentai, &task_value)
            .await
            .unwrap()
            .unwrap();
        assert!(task.next_poll_at > chrono::Local::now().naive_local());
    }

    #[tokio::test]
    async fn test_update_sub_state_no_new_does_not_rewind_cursor() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let task = repo
            .get_or_create_task(
                crate::db::types::TaskType::Ehentai,
                "eh:artist:test".to_string(),
                None,
            )
            .await
            .unwrap();
        repo.upsert_eh_subscription(-100, task.id, crate::db::types::TagFilter::default(), None)
            .await
            .unwrap();
        let sub = repo
            .list_subscriptions_by_task(task.id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        repo.update_subscription_latest_data(
            sub.id,
            Some(SubscriptionState::EhTag(EhTagState {
                pushed_gids: vec![1],
                latest_posted_ts: 500,
                pending_galleries: Vec::new(),
                pending_high_water_ts: 0,
            })),
        )
        .await
        .unwrap();
        let sub = repo
            .list_subscriptions_by_task(task.id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let engine = EhEngine::new(
            Arc::clone(&repo),
            make_eh_client(&MockServer::start().await),
            Arc::new(make_config()),
            true,
            60,
        );

        engine.update_sub_state_no_new(&sub, 100).await;

        let sub = repo
            .list_subscriptions_by_task(task.id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let state = eh_tag_subscription_state(&sub).unwrap();
        assert_eq!(state.latest_posted_ts, 500);
    }

    async fn mock_eh_search_with_four_galleries(server: &MockServer) {
        let html = r#"
        <div class="gl1t"><a href="https://e-hentai.org/g/1001/aaaaaaaaaa/"><div class="glink">Gallery 1</div></a></div>
        <div class="gl1t"><a href="https://e-hentai.org/g/1002/bbbbbbbbbb/"><div class="glink">Gallery 2</div></a></div>
        <div class="gl1t"><a href="https://e-hentai.org/g/1003/cccccccccc/"><div class="glink">Gallery 3</div></a></div>
        <div class="gl1t"><a href="https://e-hentai.org/g/1004/dddddddddd/"><div class="glink">Gallery 4</div></a></div>
        "#;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(html))
            .mount(server)
            .await;
    }

    async fn mock_eh_metadata_for_four_galleries(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/api.php"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "gmetadata": [
                    {"gid": 1001, "token": "aaaaaaaaaa", "title": "Gallery 1", "title_jpn": null, "category": "Doujinshi", "thumb": "https://ehgt.org/t/1.jpg", "uploader": "tester", "posted": "100", "filecount": "10", "filesize": 1000, "expunged": false, "rating": "4.0", "tags": ["artist:test"]},
                    {"gid": 1002, "token": "bbbbbbbbbb", "title": "Gallery 2", "title_jpn": null, "category": "Doujinshi", "thumb": "https://ehgt.org/t/2.jpg", "uploader": "tester", "posted": "200", "filecount": "10", "filesize": 1000, "expunged": false, "rating": "4.0", "tags": ["artist:test"]},
                    {"gid": 1003, "token": "cccccccccc", "title": "Gallery 3", "title_jpn": null, "category": "Doujinshi", "thumb": "https://ehgt.org/t/3.jpg", "uploader": "tester", "posted": "300", "filecount": "10", "filesize": 1000, "expunged": false, "rating": "4.0", "tags": ["artist:test"]},
                    {"gid": 1004, "token": "dddddddddd", "title": "Gallery 4", "title_jpn": null, "category": "Doujinshi", "thumb": "https://ehgt.org/t/4.jpg", "uploader": "tester", "posted": "400", "filecount": "10", "filesize": 1000, "expunged": false, "rating": "4.0", "tags": ["artist:test"]}
                ]
            })))
            .mount(server)
            .await;
    }

    #[test]
    fn download_in_progress_downcasts_through_anyhow_context() {
        // Simulate the error propagation path in process():
        // eh_client::Error::DownloadInProgress → .context("...") → anyhow::Error
        let inner = eh_client::Error::Other("simulated failure".into());
        let client_err = eh_client::Error::DownloadInProgress {
            inner: Box::new(inner),
            attempts: 4,
            bytes_delta: 12_345,
            elapsed: Duration::from_secs(10),
        };
        // Context trait is implemented on Result<T, E>, not bare E.
        // Wrap in Err to match how process() propagates the error.
        let result: eh_client::Result<()> = Err(client_err);
        let wrapped: anyhow::Error = result.context("Failed to download archive").unwrap_err();

        let found = wrapped
            .chain()
            .find_map(|c| c.downcast_ref::<eh_client::Error>())
            .map(|e| matches!(e, eh_client::Error::DownloadInProgress { .. }))
            .unwrap_or(false);
        assert!(
            found,
            "DownloadInProgress must be findable through anyhow error chain"
        );
    }

    // === Download Worker Tests ===

    #[tokio::test]
    async fn two_chats_share_one_archive_post_one_gp_attempt_and_one_artifact() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let variant = EhGalleryVariant::archive("1280x");

        setup_chat(&repo, -100, true).await;
        setup_chat(&repo, -200, true).await;
        let first = repo
            .enqueue_eh_download(
                -100,
                123456,
                "abcdef0123",
                "Shared paid gallery",
                false,
                SOURCE_DIRECT,
                &variant,
            )
            .await
            .unwrap();
        let second = repo
            .enqueue_eh_download(
                -200,
                123456,
                "abcdef0123",
                "Shared paid gallery",
                false,
                SOURCE_DIRECT,
                &variant,
            )
            .await
            .unwrap();
        assert_eq!(first.job_id, second.job_id);

        mock_eh_archiver_page_with_cost(&eh_server, 123456, "abcdef0123", "218 GP", "218 GP").await;
        let download_url = format!("{}/archive/123456/token/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;
        let zip_temp = tempfile::tempdir().unwrap();
        let source_zip = zip_temp.path().join("shared.zip");
        create_test_zip(&source_zip, 1);
        mock_eh_archive_download(
            &eh_server,
            "/archive/123456/token/0",
            std::fs::read(source_zip).unwrap(),
        )
        .await;

        let mut config = make_config();
        config.max_archive_gp_cost = 218;
        config.background_download_enabled = false;
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
            None,
        );
        worker.tick().await.unwrap();

        let job_id = first.job_id.unwrap();
        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.status, "downloaded");
        assert_eq!(job.gp_cost, 218);
        assert!(std::path::Path::new(job.zip_path.as_deref().unwrap()).exists());
        let deliveries = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::JobId.eq(job_id))
            .all(repo.db())
            .await
            .unwrap();
        assert_eq!(deliveries.len(), 2);
        assert!(deliveries
            .iter()
            .all(|delivery| delivery.status == "waiting"));
        let attempts = gp_attempts(repo.as_ref()).await;
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].job_id, Some(job_id));
        assert_eq!(attempts[0].queue_id, None);
        let completions = eh_download_completions::Entity::find()
            .filter(eh_download_completions::Column::JobId.eq(job_id))
            .all(repo.db())
            .await
            .unwrap();
        assert_eq!(completions.len(), 1);
        assert_eq!(
            eh_server
                .received_requests()
                .await
                .unwrap()
                .into_iter()
                .filter(|request| {
                    request.method.as_str() == "POST" && request.url.path() == "/archiver.php"
                })
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn artifact_identity_contains_job_and_sanitized_variant() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let archive = eh_gallery_jobs::ActiveModel {
            gid: Set(42),
            token: Set("tok/en?".to_string()),
            download_mode: Set("archive".to_string()),
            resolution: Set("1280x/unsafe".to_string()),
            title: Set("Archive".to_string()),
            ..Default::default()
        }
        .insert(repo.db())
        .await
        .unwrap();
        let images = eh_gallery_jobs::ActiveModel {
            gid: Set(43),
            token: Set("im:age".to_string()),
            download_mode: Set("images".to_string()),
            resolution: Set(String::new()),
            title: Set("Images".to_string()),
            ..Default::default()
        }
        .insert(repo.db())
        .await
        .unwrap();
        let cache = tempfile::tempdir().unwrap();

        let archive_name = archive_artifacts_for_job(cache.path(), &archive)
            .final_zip()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let images_name = archive_artifacts_for_job(cache.path(), &images)
            .final_zip()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            archive_name,
            format!("42_tok_en__j{}_archive_1280x_unsafe.zip", archive.id)
        );
        assert_eq!(
            images_name,
            format!("43_im_age_j{}_images_none.zip", images.id)
        );
    }

    #[tokio::test]
    async fn test_download_worker_downloads_archive() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Test Gallery",
            false,
            "pending",
            None,
            None,
        )
        .await;

        mock_eh_gallery_page(&eh_server, 123456, "abcdef0123").await;
        let download_url = format!("{}/archive/123456/token/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;

        let zip_temp = tempfile::tempdir().unwrap();
        let zip_path = zip_temp.path().join("test.zip");
        create_test_zip(&zip_path, 3);
        let zip_bytes = std::fs::read(&zip_path).unwrap();
        mock_eh_archive_download(&eh_server, "/archive/123456/token/0", zip_bytes).await;

        let mut config = make_config();
        config.background_download_enabled = false;
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
            None,
        );

        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(updated.status, STATUS_DOWNLOADED);
        assert!(updated.zip_path.is_some());
        assert!(updated.file_size > 0);
        assert!(std::path::Path::new(updated.zip_path.as_ref().unwrap()).exists());
    }

    #[tokio::test]
    async fn test_download_worker_threads_archive_download_concurrency() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Concurrent archive",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;

        mock_eh_gallery_page(&eh_server, 123456, "abcdef0123").await;
        let download_url = format!("{}/archive/123456/token/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;
        let zip_temp = tempfile::tempdir().unwrap();
        let zip_path = zip_temp.path().join("concurrent.zip");
        create_test_zip(&zip_path, 1);
        let zip_bytes = std::fs::read(&zip_path).unwrap();
        let content_range = format!("bytes 0-{}/{}", zip_bytes.len() - 1, zip_bytes.len());
        Mock::given(method("GET"))
            .and(path("/archive/123456/token/0"))
            .and(header("range", "bytes=0-"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", content_range)
                    .set_body_bytes(zip_bytes),
            )
            .expect(1)
            .mount(&eh_server)
            .await;

        let mut config = make_config();
        config.background_download_enabled = false;
        config.archive_download_concurrency = 2;
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
            None,
        );

        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(updated.status, STATUS_DOWNLOADED);
    }

    #[tokio::test]
    async fn test_download_worker_rate_limit_skips() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;

        // Pre-fill the completion ledger to hit the shared rate limit.
        let now = Local::now().naive_local();
        eh_download_completions::ActiveModel {
            job_id: Set(None),
            gid: Set(999999),
            file_size: Set(11_000_000_000),
            created_at: Set(now),
            ..Default::default()
        }
        .insert(repo.db())
        .await
        .unwrap();

        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Test",
            false,
            "pending",
            None,
            None,
        )
        .await;

        let mut config = make_config();
        config.background_download_enabled = false;
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
            None,
        );
        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            updated.status, "pending",
            "should remain pending due to rate limit"
        );
    }

    #[tokio::test]
    async fn test_download_worker_chat_disabled_schedules_retry() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, false).await; // disabled
        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Test",
            false,
            "pending",
            None,
            None,
        )
        .await;

        let mut config = make_config();
        config.background_download_enabled = false;
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
            None,
        );
        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        // Chat disabled → shared job goes back to pending with retry scheduled.
        assert_eq!(
            updated.status, "pending",
            "should be pending for retry, not silently done"
        );
        assert_eq!(
            updated.retry_count, 0,
            "chat disabled defer should not increment retry_count"
        );
        assert!(
            updated.next_retry_at.is_some(),
            "should have next_retry_at set"
        );
    }

    #[tokio::test]
    async fn test_download_worker_failure_schedules_retry() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Test",
            false,
            "pending",
            None,
            None,
        )
        .await;

        mock_eh_gallery_page(&eh_server, 123456, "abcdef0123").await;
        // archiver.php POST returns 500
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&eh_server)
            .await;

        let mut config = make_config();
        config.background_download_enabled = false;
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
            None,
        );
        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            updated.status, "pending",
            "should be back to pending for retry"
        );
        assert_eq!(updated.retry_count, 1);
        assert!(
            updated.next_retry_at.is_some(),
            "should have next_retry_at set"
        );
    }

    #[tokio::test]
    async fn test_download_worker_permanent_failure_cleans_partial_archive() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Test",
            false,
            "pending",
            None,
            None,
        )
        .await;

        let job = job_for_delivery(&repo, &entry).await;
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::RetryCount,
                Expr::value(make_config().max_retry_count as i32),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .exec(repo.db())
            .await
            .unwrap();

        let eh_cache = temp.path().join("eh_cache");
        std::fs::create_dir_all(&eh_cache).unwrap();
        let zip_path = archive_artifacts_for_job(temp.path(), &job)
            .final_zip()
            .to_path_buf();
        let part_path = zip_path.with_extension("zip.part");
        let parts_dir = zip_path.with_extension("zip.parts");
        std::fs::write(&zip_path, b"PK\x03\x04stale").unwrap();
        std::fs::write(&part_path, b"PK\x03\x04partial").unwrap();
        std::fs::create_dir_all(parts_dir.join("nested")).unwrap();
        std::fs::write(parts_dir.join("manifest.json"), b"manifest").unwrap();
        std::fs::write(parts_dir.join("nested").join("part-0001"), b"part").unwrap();

        mock_eh_gallery_page(&eh_server, 123456, "abcdef0123").await;
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&eh_server)
            .await;

        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(make_config()),
            temp.path().to_path_buf(),
            None,
        );
        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            updated.status,
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_RETIRED
        );
        assert_eq!(
            updated.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_PENDING
        );
        assert!(
            zip_path.exists(),
            "cleanup ownership is durable before local removal"
        );
        assert_eq!(
            run_eh_job_cleanup_maintenance_once(repo.as_ref(), None, 0, true)
                .await
                .unwrap(),
            Some(EhCleanupFinalizeOutcome::CleanRetired)
        );
        assert!(!zip_path.exists(), "final ZIP should be cleaned");
        assert!(!part_path.exists(), "partial ZIP should be cleaned");
        assert!(
            !parts_dir.exists(),
            "multipart parts directory should be removed recursively"
        );
    }

    #[tokio::test]
    async fn test_download_worker_progress_failure_defers_without_retry() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Test",
            false,
            "pending",
            None,
            None,
        )
        .await;
        let job = job_for_delivery(&repo, &entry).await;

        mock_eh_gallery_page(&eh_server, 123456, "abcdef0123").await;
        let download_url = format!("{}/archive/123456/token/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;

        // Pre-seed 1-byte .part so the 206 response takes the append path and
        // runs validate_content_range. Content-Range start=1 matches existing_len=1.
        let eh_cache = temp.path().join("eh_cache");
        tokio::fs::create_dir_all(&eh_cache).await.unwrap();
        let part_path = archive_artifacts_for_job(temp.path(), &job)
            .final_zip()
            .with_extension("zip.part");
        tokio::fs::write(&part_path, b"x").await.unwrap();

        // 206 with valid Content-Range (start=1==existing_len, end+1==total → validate passes)
        // but body smaller than claimed (>10KB) → written < expected_total → error
        // after writing >10KB → made_progress=true → DownloadInProgress.
        // Note: the mock returns the same fixed Content-Range on every attempt. After the
        // first append the start no longer matches existing_len, so validate_content_range
        // fails before writing further bytes; only the first attempt appends 20000 bytes.
        let partial_body = vec![0u8; 20000];
        Mock::given(method("GET"))
            .and(path("/archive/123456/token/0"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", "bytes 1-99999/100000")
                    .set_body_bytes(partial_body.clone()),
            )
            // 4 attempts per ARCHIVE_DOWNLOAD_MAX_ATTEMPTS
            .expect(4)
            .mount(&eh_server)
            .await;

        let mut config = make_config();
        config.background_download_enabled = false;
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
            None,
        );
        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            updated.status, STATUS_PENDING,
            "should be pending for deferred retry"
        );
        assert_eq!(
            updated.retry_count, 0,
            "DownloadInProgress should NOT increment retry_count"
        );
        assert!(
            updated.next_retry_at.is_some(),
            "should have next_retry_at set by defer_eh_job_download"
        );

        // .part file should be preserved for resumption.
        assert!(
            part_path.exists(),
            ".part file should be preserved for resumption"
        );
        let part_size = std::fs::metadata(&part_path).unwrap().len();
        assert_eq!(
            part_size, 20001,
            ".part should contain 20001 bytes (1 pre-seeded + 20000 written on first attempt), got {}",
            part_size
        );
    }

    #[tokio::test]
    async fn test_download_worker_slow_progress_hands_off_shared_job_to_background() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Test",
            false,
            "pending",
            None,
            None,
        )
        .await;
        let job = job_for_delivery(&repo, &entry).await;

        mock_eh_gallery_page(&eh_server, 123456, "abcdef0123").await;
        let download_url = format!("{}/archive/123456/token/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;

        let eh_cache = temp.path().join("eh_cache");
        tokio::fs::create_dir_all(&eh_cache).await.unwrap();
        let part_path = archive_artifacts_for_job(temp.path(), &job)
            .final_zip()
            .with_extension("zip.part");
        tokio::fs::write(&part_path, b"x").await.unwrap();

        Mock::given(method("GET"))
            .and(path("/archive/123456/token/0"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", "bytes 1-99999/100000")
                    .set_body_bytes(vec![0u8; 20000]),
            )
            .expect(4)
            .mount(&eh_server)
            .await;

        let mut config = make_config();
        config.background_download_enabled = true;
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
            None,
        );
        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(updated.status, STATUS_PENDING);
        assert_eq!(updated.retry_count, 0);
        assert_eq!(
            updated.background_download_status.as_deref(),
            Some(crate::db::repo::eh_gallery_jobs::BACKGROUND_STATUS_PENDING)
        );
        assert!(updated.background_download_next_retry_at.is_some());
        assert!(updated.next_retry_at.is_none());
        assert!(part_path.exists());
    }

    #[tokio::test]
    async fn test_download_size_limit_blocks_oversized_selected_archive_before_post() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let eh_server = MockServer::start().await;
        let temp_dir = tempfile::tempdir().unwrap();

        mock_eh_archiver_page_with_estimated_sizes(
            &eh_server,
            900,
            "abcdef0123",
            "400.0 MiB",
            "300.01 MiB",
        )
        .await;
        // Any POST to /archiver.php (the paid archive request) must never happen.
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string("unexpected"))
            .expect(0)
            .mount(&eh_server)
            .await;

        let mut cfg = make_config();
        cfg.max_archive_size_mb = 300;
        cfg.max_retry_count = 0;
        let entry = insert_queue_entry(
            &repo,
            -100,
            900,
            "abcdef0123",
            "Title",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(cfg),
            temp_dir.path().to_path_buf(),
            None,
        );

        worker.tick().await.unwrap();

        let model = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            model.status,
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_RETIRED
        );
        assert_eq!(
            model.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_PENDING
        );
        assert!(
            model
                .error
                .as_ref()
                .is_some_and(|e| e.contains("selected EH archive size is too large")),
            "error should mention the configured limit, got: {:?}",
            model.error
        );
        assert_eq!(
            eh_server
                .received_requests()
                .await
                .unwrap()
                .into_iter()
                .filter(
                    |request| request.method.as_str() == "POST" && request.url.path() == "/api.php"
                )
                .count(),
            0,
            "selected archive-size checks must not request gallery metadata"
        );
    }

    #[tokio::test]
    async fn test_download_worker_oversize_over_gp_policy_rejects_before_size_retry() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let eh_server = MockServer::start().await;
        let temp_dir = tempfile::tempdir().unwrap();
        mock_eh_archiver_page_with_cost_and_estimated_sizes(
            &eh_server,
            904,
            "abcdef0123",
            "218 GP",
            "218 GP",
            "400.0 MiB",
            "300.01 MiB",
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string("unexpected"))
            .expect(0)
            .mount(&eh_server)
            .await;

        let mut cfg = make_config();
        cfg.background_download_enabled = false;
        cfg.max_archive_size_mb = 300;
        cfg.max_archive_gp_cost = 0;
        let entry = insert_queue_entry(
            &repo,
            -100,
            904,
            "abcdef0123",
            "Title",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(cfg),
            temp_dir.path().to_path_buf(),
            None,
        );

        worker.tick().await.unwrap();

        let model = job_for_delivery(&repo, &entry).await;
        assert_eq!(model.status, STATUS_FAILED);
        assert_eq!(
            model.error.as_deref(),
            Some("EH archive GP cost 218 exceeds configured max_archive_gp_cost=0")
        );
        assert!(model.completed_at.is_some());
        assert_eq!(model.retry_count, 0);
        assert!(model.background_download_status.is_none());
        assert_eq!(model.background_download_attempt_count, 0);
        assert!(gp_attempts(repo.as_ref()).await.is_empty());
    }

    #[tokio::test]
    async fn test_download_size_limit_allows_small_selected_resample_without_metadata() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let eh_server = MockServer::start().await;
        let temp_dir = tempfile::tempdir().unwrap();

        // The selected 1280x archive is small even though the original archive
        // estimate is above the configured limit. No gallery metadata route is
        // mounted: a metadata request is a regression.
        mock_eh_archiver_page_with_estimated_sizes(
            &eh_server,
            903,
            "abcdef0123",
            "301.0 MiB",
            "2.33 MiB",
        )
        .await;
        let download_url = format!("{}/archive/903/token/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;
        let zip_temp = tempfile::tempdir().unwrap();
        let zip_path = zip_temp.path().join("small_resample.zip");
        create_test_zip(&zip_path, 2);
        mock_eh_archive_download(
            &eh_server,
            "/archive/903/token/0",
            std::fs::read(zip_path).unwrap(),
        )
        .await;

        let mut cfg = make_config();
        cfg.max_archive_size_mb = 300;
        cfg.background_download_enabled = false;
        let entry = insert_queue_entry(
            &repo,
            -100,
            903,
            "abcdef0123",
            "Title",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(cfg),
            temp_dir.path().to_path_buf(),
            None,
        );

        worker.tick().await.unwrap();

        let model = job_for_delivery(&repo, &entry).await;
        assert_eq!(model.status, STATUS_DOWNLOADED);
        let requests = eh_server.received_requests().await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.method.as_str() == "POST"
                    && request.url.path() == "/archiver.php")
                .count(),
            1,
            "the prepared selected archive should be posted"
        );
        assert!(
            !requests.iter().any(
                |request| request.method.as_str() == "POST" && request.url.path() == "/api.php"
            ),
            "selected archive-size checks must not request gallery metadata"
        );
    }

    #[tokio::test]
    async fn test_download_size_limit_allows_equal_size() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let eh_server = MockServer::start().await;
        let temp_dir = tempfile::tempdir().unwrap();

        // A selected archive equal to the limit must be allowed (strict `>` rejects).
        mock_eh_archiver_page_with_estimated_sizes(
            &eh_server,
            901,
            "abcdef0123",
            "400.0 MiB",
            "300.0 MiB",
        )
        .await;
        let download_url = format!("{}/archive/901/token/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;

        let zip_temp = tempfile::tempdir().unwrap();
        let zip_path = zip_temp.path().join("equal_size.zip");
        create_test_zip(&zip_path, 2);
        let zip_bytes = std::fs::read(&zip_path).unwrap();
        mock_eh_archive_download(&eh_server, "/archive/901/token/0", zip_bytes).await;

        let mut cfg = make_config();
        cfg.max_archive_size_mb = 300;
        cfg.background_download_enabled = false;
        let entry = insert_queue_entry(
            &repo,
            -100,
            901,
            "abcdef0123",
            "Title",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(cfg),
            temp_dir.path().to_path_buf(),
            None,
        );

        worker.tick().await.unwrap();

        let model = job_for_delivery(&repo, &entry).await;
        assert_eq!(model.status, STATUS_DOWNLOADED);
        assert!(model.zip_path.is_some());
        assert!(model.file_size > 0);
    }

    #[test]
    fn test_shared_size_limit_guard_uses_selected_archive_estimate() {
        let mut cfg = make_config();
        cfg.max_archive_size_mb = 300;

        let err =
            ensure_eh_archive_under_size_limit(&cfg, Some(300 * 1024 * 1024 + 1)).unwrap_err();

        assert!(
            err.to_string()
                .contains("selected EH archive size is too large"),
            "error should mention the configured limit, got: {err}"
        );
        assert!(ensure_eh_archive_under_size_limit(&cfg, Some(300 * 1024 * 1024)).is_ok());
        assert!(ensure_eh_archive_under_size_limit(&cfg, Some(0)).is_ok());
        assert!(ensure_eh_archive_under_size_limit(&cfg, None).is_ok());
    }

    // === Upload Worker Tests ===

    #[tokio::test]
    async fn test_upload_worker_full_flow() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;

        setup_chat(&repo, -100, true).await;

        // Create a real ZIP with images
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("gallery.zip");
        create_test_zip(&zip_path, 3);
        let zip_path_str = zip_path.to_string_lossy().to_string();

        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Test Gallery",
            true,
            STATUS_DOWNLOADED,
            Some(&zip_path_str),
            None,
        )
        .await;

        mock_telegraph_upload(&tg_server, 3).await;
        mock_telegraph_create_page(&tg_server).await;

        let worker = EhUploadWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_telegraph_client(&tg_server),
            make_image_uploader(&tg_server),
            None,
            Arc::new(make_config()),
        );
        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            updated.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_READY
        );
        assert!(updated.telegraph_url.is_some());
    }

    #[tokio::test]
    async fn test_upload_worker_includes_images_larger_than_six_mib() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;

        setup_chat(&repo, -100, true).await;

        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("large_gallery.zip");
        create_test_zip_with_sizes(&zip_path, &[1024, 6 * 1024 * 1024 + 1, 2048]);
        let zip_path_str = zip_path.to_string_lossy().to_string();

        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Large Gallery",
            true,
            STATUS_DOWNLOADED,
            Some(&zip_path_str),
            None,
        )
        .await;

        let upload_body = serde_json::json!({
            "success": true,
            "direct_url": "https://i.pixi.mg/i/large.jpg"
        });
        Mock::given(method("POST"))
            .and(path("/pixi/upload"))
            .and(MultipartFileCount(1))
            .respond_with(ResponseTemplate::new(200).set_body_json(upload_body))
            .expect(3)
            .mount(&tg_server)
            .await;
        mock_telegraph_create_page(&tg_server).await;
        let worker = EhUploadWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_telegraph_client(&tg_server),
            make_image_uploader(&tg_server),
            None,
            Arc::new(make_config()),
        );
        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            updated.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_READY
        );
    }

    #[tokio::test]
    async fn test_upload_worker_no_images_fails() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;

        setup_chat(&repo, -100, true).await;

        // Create ZIP with only .txt files
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("no_images.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("readme.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"no images").unwrap();
            zip.finish().unwrap();
        }
        let zip_path_str = zip_path.to_string_lossy().to_string();

        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Test",
            true,
            STATUS_DOWNLOADED,
            Some(&zip_path_str),
            None,
        )
        .await;

        let worker = EhUploadWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_telegraph_client(&tg_server),
            make_image_uploader(&tg_server),
            None,
            Arc::new(make_config()),
        );
        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            updated.status, STATUS_DOWNLOADED,
            "should be back to downloaded for retry"
        );
        assert_eq!(updated.retry_count, 1);
    }

    // === ZIP-archive uploader tests ===

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct SeenResumeContext {
        manifest_path: std::path::PathBuf,
        logical_object_id: String,
    }

    fn copy_resume_context(context: eh_client::UploadResumeContext<'_>) -> SeenResumeContext {
        SeenResumeContext {
            manifest_path: context.manifest_path.to_path_buf(),
            logical_object_id: context.logical_object_id.to_string(),
        }
    }

    /// Mock uploader that records whether the ZIP-archive path or the per-image
    /// path was used, remembers the entry names it observed, and copies resume
    /// contexts before their borrowed input is dropped.
    #[derive(Default)]
    struct ZipFirstMockUploader {
        zip_calls: std::sync::atomic::AtomicUsize,
        image_calls: std::sync::atomic::AtomicUsize,
        seen_entries: std::sync::Mutex<Vec<String>>,
        seen_zip_resume_contexts: std::sync::Mutex<Vec<SeenResumeContext>>,
        seen_image_resume_contexts: std::sync::Mutex<Vec<SeenResumeContext>>,
        zip_fallback: bool,
        fail_image_call: Option<usize>,
    }

    #[async_trait::async_trait]
    impl ImageUploader for ZipFirstMockUploader {
        fn supports_zip_archive_upload(&self) -> bool {
            true
        }

        async fn upload_images(
            &self,
            images: &[ImageUploadInput<'_>],
        ) -> eh_client::Result<Vec<String>> {
            let image_call = self
                .image_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut seen_contexts = self.seen_image_resume_contexts.lock().unwrap();
            seen_contexts.extend(
                images
                    .iter()
                    .filter_map(|image| image.resume_context.map(copy_resume_context)),
            );
            drop(seen_contexts);
            if self.fail_image_call == Some(image_call) {
                return Err(eh_client::Error::Other(
                    "mock image upload failure".to_string(),
                ));
            }
            Ok(images
                .iter()
                .map(|image| format!("https://images.example/{}", image.filename))
                .collect())
        }

        async fn upload_zip_archive_with_url_pairs(
            &self,
            archive: ZipArchiveUploadInput<'_>,
        ) -> eh_client::Result<Option<Vec<TelegraphImageUrlPair>>> {
            self.zip_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(context) = archive.resume_context {
                self.seen_zip_resume_contexts
                    .lock()
                    .unwrap()
                    .push(copy_resume_context(context));
            }
            *self.seen_entries.lock().unwrap() = archive.entry_names.to_vec();
            if self.zip_fallback {
                return Ok(None);
            }
            Ok(Some(
                archive
                    .entry_names
                    .iter()
                    .map(|name| TelegraphImageUrlPair {
                        preview_url: format!("https://preview.example/ipfs/root/{name}"),
                        public_url: format!("https://public.example/ipfs/root/{name}"),
                    })
                    .collect(),
            ))
        }
    }

    #[derive(Default)]
    struct TerminalCleanupMockUploader {
        cleanup_calls: std::sync::Mutex<Vec<(std::path::PathBuf, bool)>>,
        fail_abort: bool,
    }

    struct AlwaysFailUploader {
        message: String,
    }

    #[async_trait::async_trait]
    impl ImageUploader for AlwaysFailUploader {
        async fn upload_images(
            &self,
            _images: &[ImageUploadInput<'_>],
        ) -> eh_client::Result<Vec<String>> {
            Err(eh_client::Error::Other(self.message.clone()))
        }
    }

    #[async_trait::async_trait]
    impl ImageUploader for TerminalCleanupMockUploader {
        async fn upload_images(
            &self,
            _images: &[ImageUploadInput<'_>],
        ) -> eh_client::Result<Vec<String>> {
            Err(eh_client::Error::Other(
                "mock image upload failure".to_string(),
            ))
        }

        async fn abort_upload_state(&self, uploads_dir: &std::path::Path) -> eh_client::Result<()> {
            self.cleanup_calls
                .lock()
                .unwrap()
                .push((uploads_dir.to_path_buf(), uploads_dir.exists()));
            if self.fail_abort {
                return Err(eh_client::Error::Other(
                    "mock terminal Abort failure".to_string(),
                ));
            }
            Ok(())
        }
    }

    fn assert_terminal_cleanup_precedes_local_removal(
        uploader: &TerminalCleanupMockUploader,
        artifacts: &ArchiveArtifacts,
    ) {
        assert_eq!(
            *uploader.cleanup_calls.lock().unwrap(),
            vec![(artifacts.uploads_dir().to_path_buf(), true)]
        );
    }

    #[tokio::test]
    async fn two_telegraph_deliveries_upload_zip_and_create_page_once() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("shared-gallery.zip");
        create_test_zip(&zip_path, 2);
        let (job, deliveries) = seed_downloaded_job_with_deliveries(
            &repo,
            710,
            "token",
            "Shared Gallery",
            &zip_path,
            &[(-100, true, "T1"), (-200, true, "T2")],
        )
        .await;
        let uploader = Arc::new(ZipFirstMockUploader::default());
        let body = serde_json::json!({
            "ok": true,
            "result": {"url": "https://telegra.ph/Shared-Gallery-01-01"}
        });
        Mock::given(method("POST"))
            .and(path("/createPage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&tg_server)
            .await;
        let worker = EhUploadWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_telegraph_client(&tg_server),
            uploader.clone(),
            Some(IpfS3PreviewRewriteConfig {
                preview_gateway_url: "https://preview.example".to_string(),
                public_gateway_url: "https://public.example".to_string(),
                delay_sec: 60,
            }),
            Arc::new(make_config()),
        );

        worker.tick().await.unwrap();
        worker.tick().await.unwrap();

        assert_eq!(
            uploader.zip_calls.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            uploader
                .image_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        let ready = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            ready.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_READY
        );
        assert_eq!(
            ready.telegraph_url.as_deref(),
            Some("https://telegra.ph/Shared-Gallery-01-01")
        );
        assert!(
            ready.telegraph_rewrite_data.is_some(),
            "create-page rewrite payload must be persisted once on the shared job"
        );
        for delivery in deliveries {
            let delivery = eh_download_queue::Entity::find_by_id(delivery.id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                delivery.status,
                crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING
            );
            assert_eq!(delivery.telegraph_url, None);
            assert_eq!(delivery.telegraph_rewrite_data, None);
        }
    }

    #[tokio::test]
    async fn first_telegraph_delivery_schedules_one_job_rewrite() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;
        let (job, deliveries) =
            seed_ready_telegraph_job_with_deliveries(&repo, 712, &[(-100, None), (-200, None)])
                .await;

        repo.mark_eh_telegraph_delivery_sent(deliveries[0].id, job.id, Some(0))
            .await
            .unwrap();
        let after_first = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let scheduled_after = after_first.telegraph_rewrite_after.unwrap();
        assert_eq!(
            after_first.telegraph_rewrite_status.as_deref(),
            Some(crate::db::repo::eh_gallery_jobs::TELEGRAPH_REWRITE_STATUS_PENDING)
        );
        let marked_first = eh_download_queue::Entity::find_by_id(deliveries[0].id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let untouched_second = eh_download_queue::Entity::find_by_id(deliveries[1].id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert!(marked_first.telegraph_sent_at.is_some());
        assert!(untouched_second.telegraph_sent_at.is_none());

        repo.mark_eh_telegraph_delivery_sent(deliveries[0].id, job.id, Some(7200))
            .await
            .unwrap();

        repo.mark_eh_telegraph_delivery_sent(deliveries[1].id, job.id, Some(3600))
            .await
            .unwrap();
        let after_second = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after_second.telegraph_rewrite_after, Some(scheduled_after));

        Mock::given(method("POST"))
            .and(path("/editPage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {"url": "https://telegra.ph/Shared-Gallery-01-01"}
            })))
            .expect(1)
            .mount(&tg_server)
            .await;
        let worker = EhTelegraphRewriteWorker::new(
            Arc::clone(&repo),
            make_telegraph_client(&tg_server),
            true,
            Arc::new(make_config()),
        );
        worker.tick().await.unwrap();

        let requests = tg_server.received_requests().await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.url.path() == "/editPage")
                .count(),
            1
        );
        let rewritten = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert!(rewritten.telegraph_rewritten_at.is_some());
        assert!(repo
            .get_next_eh_job_for_telegraph_rewrite()
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn job_telegraph_rewrite_worker_edits_each_page_once() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;
        let (job, deliveries) =
            seed_ready_telegraph_job_with_deliveries(&repo, 714, &[(-100, None)]).await;
        let rewrite_data = serde_json::json!({
            "pages": [
                {
                    "path": "Shared-Gallery-01-01",
                    "title": "Shared Gallery 1",
                    "content": [{
                        "tag": "img",
                        "attrs": {"src": "https://preview.example/ipfs/first"}
                    }]
                },
                {
                    "path": "Shared-Gallery-01-02",
                    "title": "Shared Gallery 2",
                    "content": [{
                        "tag": "img",
                        "attrs": {"src": "https://preview.example/ipfs/second"}
                    }]
                }
            ],
            "preview_gateway_url": "https://preview.example",
            "public_gateway_url": "https://public.example"
        })
        .to_string();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteData,
                Expr::value(Some(rewrite_data)),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .exec(repo.db())
            .await
            .unwrap();
        repo.mark_eh_telegraph_delivery_sent(deliveries[0].id, job.id, Some(0))
            .await
            .unwrap();

        Mock::given(method("POST"))
            .and(path("/editPage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {"url": "https://telegra.ph/Shared-Gallery"}
            })))
            .expect(2)
            .mount(&tg_server)
            .await;
        let worker = EhTelegraphRewriteWorker::new(
            Arc::clone(&repo),
            make_telegraph_client(&tg_server),
            true,
            Arc::new(make_config()),
        );
        worker.tick().await.unwrap();

        let requests = tg_server.received_requests().await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.url.path() == "/editPage")
                .count(),
            2
        );
        for request in requests
            .iter()
            .filter(|request| request.url.path() == "/editPage")
        {
            let content = url::form_urlencoded::parse(&request.body)
                .find(|(key, _)| key == "content")
                .unwrap()
                .1
                .into_owned();
            assert!(content.contains("https://public.example/ipfs/"));
            assert!(!content.contains("https://preview.example/ipfs/"));
        }
    }

    #[tokio::test]
    async fn final_delivery_with_delayed_rewrite_keeps_payload_until_rewrite_is_terminal() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;
        let (job, deliveries) =
            seed_ready_telegraph_job_with_deliveries(&repo, 713, &[(-100, Some(812))]).await;

        repo.mark_eh_telegraph_delivery_sent(deliveries[0].id, job.id, Some(60))
            .await
            .unwrap();
        repo.cancel_eh_subscription_queue_entries(812, true)
            .await
            .unwrap();
        let interleaved = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_ne!(
            interleaved.status,
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_RETIRED
        );
        assert_eq!(
            interleaved.telegraph_rewrite_status.as_deref(),
            Some(crate::db::repo::eh_gallery_jobs::TELEGRAPH_REWRITE_STATUS_PENDING)
        );
        assert!(interleaved.telegraph_rewrite_data.is_some());

        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRewriteAfter,
                Expr::value(Local::now().naive_local() - chrono::Duration::seconds(1)),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .exec(repo.db())
            .await
            .unwrap();
        Mock::given(method("POST"))
            .and(path("/editPage"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&tg_server)
            .await;
        let mut config = make_config();
        config.max_retry_count = 0;
        let worker = EhTelegraphRewriteWorker::new(
            Arc::clone(&repo),
            make_telegraph_client(&tg_server),
            true,
            Arc::new(config),
        );
        worker.tick().await.unwrap();

        let terminal = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            terminal.telegraph_rewrite_status.as_deref(),
            Some(crate::db::repo::eh_gallery_jobs::TELEGRAPH_REWRITE_STATUS_FAILED)
        );
        assert_eq!(
            terminal.status,
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_RETIRED
        );
        assert!(terminal.telegraph_rewrite_data.is_none());
        assert_eq!(
            terminal.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_PENDING
        );
    }

    #[tokio::test]
    async fn terminal_upload_notifies_each_telegraph_chat_once_and_never_archive_only() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("terminal-shared-gallery.zip");
        create_test_zip(&zip_path, 1);
        let (job, deliveries) = seed_downloaded_job_with_deliveries(
            &repo,
            711,
            "token",
            "Shared Gallery",
            &zip_path,
            &[
                (-100, true, "T1"),
                (-200, true, "T2"),
                (-300, false, "Archive"),
            ],
        )
        .await;
        let response = serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 43,
                "date": 1700000000,
                "chat": {"id": -100, "type": "private"}
            }
        });
        Mock::given(method("POST"))
            .and(path("/botfake_token/SendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .expect(2)
            .mount(&tg_server)
            .await;
        let mut config = make_config();
        config.max_retry_count = 0;
        assert!(config.send_archive);
        let worker = EhUploadWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_telegraph_client(&tg_server),
            Arc::new(AlwaysFailUploader {
                message: "sqlite secret; /private/path; multipart upload id=abc".to_string(),
            }),
            None,
            Arc::new(config),
        );

        worker.tick().await.unwrap();
        worker.tick().await.unwrap();

        let requests = tg_server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.url.path() == "/botfake_token/SendMessage")
            .map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            requests
                .iter()
                .map(|body| body["chat_id"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            vec![-100, -200]
        );
        assert_eq!(
            requests
                .iter()
                .map(|body| body["text"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "⚠️ Telegraph 上传失败，请稍后重试\n\n📦 T1",
                "⚠️ Telegraph 上传失败，请稍后重试\n\n📦 T2",
            ]
        );
        assert!(requests.iter().all(|body| {
            let text = body["text"].as_str().unwrap();
            !text.contains("sqlite secret")
                && !text.contains("/private/path")
                && !text.contains("upload id")
        }));

        let failed_job = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            failed_job.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_FAILED
        );
        assert!(failed_job.error.unwrap().contains("sqlite secret"));
        for (delivery, (expected_status, expected_telegraph)) in deliveries.iter().zip([
            (
                crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING,
                false,
            ),
            (
                crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING,
                false,
            ),
            (
                crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING,
                false,
            ),
        ]) {
            let delivery = eh_download_queue::Entity::find_by_id(delivery.id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(delivery.status, expected_status);
            assert_eq!(delivery.telegraph, expected_telegraph);
            assert_eq!(delivery.error, None);
        }
        let fallback_claim = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fallback_claim.delivery.id, deliveries[0].id);
        assert!(!fallback_claim.delivery.telegraph);
    }

    #[tokio::test]
    async fn upload_worker_passes_stable_zip_resume_context() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        mock_telegraph_create_page(&tg_server).await;
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("archive.zip");
        create_test_zip(&zip_path, 2);
        let zip_path_str = zip_path.to_string_lossy().to_string();
        let entry = insert_queue_entry(
            &repo,
            -100,
            703,
            "tok",
            "Title",
            true,
            STATUS_DOWNLOADED,
            Some(&zip_path_str),
            None,
        )
        .await;
        let uploader = Arc::new(ZipFirstMockUploader::default());
        let worker = EhUploadWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_telegraph_client(&tg_server),
            uploader.clone(),
            None,
            Arc::new(make_config()),
        );

        worker.tick().await.unwrap();

        assert_eq!(
            *uploader.seen_zip_resume_contexts.lock().unwrap(),
            vec![SeenResumeContext {
                manifest_path: ArchiveArtifacts::new(&zip_path)
                    .uploads_dir()
                    .join("archive.json"),
                logical_object_id: "archive".to_string(),
            }]
        );
        let job = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            job.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_READY
        );
    }

    #[tokio::test]
    async fn upload_worker_uses_original_uploadable_order_for_image_contexts() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        mock_telegraph_create_page(&tg_server).await;
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("mixed.zip");
        create_test_zip_with_names(
            &zip_path,
            &[
                "notes.txt",
                "directory/",
                "first.jpg",
                "metadata.json",
                "second.png",
            ],
        );
        let artifacts = ArchiveArtifacts::new(&zip_path);
        std::fs::create_dir_all(artifacts.uploads_dir()).unwrap();
        std::fs::write(
            artifacts.uploads_dir().join("archive.json"),
            b"upload state",
        )
        .unwrap();
        let zip_path_str = zip_path.to_string_lossy().to_string();
        let entry = insert_queue_entry(
            &repo,
            -100,
            704,
            "tok",
            "Title",
            true,
            STATUS_DOWNLOADED,
            Some(&zip_path_str),
            None,
        )
        .await;
        let uploader = Arc::new(ZipFirstMockUploader {
            zip_fallback: true,
            fail_image_call: Some(1),
            ..Default::default()
        });
        let abort_uploader = Arc::new(TerminalCleanupMockUploader::default());
        let worker = EhUploadWorker::new_with_abort_uploader(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_telegraph_client(&tg_server),
            uploader.clone(),
            Some(abort_uploader),
            None,
            Arc::new(make_config()),
        );

        worker.tick().await.unwrap();
        let retried = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            retried.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_PENDING
        );
        assert!(
            artifacts.uploads_dir().exists(),
            "retryable upload failure should retain upload state"
        );
        let mut retry_active: eh_gallery_jobs::ActiveModel = retried.into();
        retry_active.next_retry_at = Set(None);
        retry_active.update(repo.db()).await.unwrap();
        worker.tick().await.unwrap();

        let image_0 = SeenResumeContext {
            manifest_path: artifacts.uploads_dir().join("image-0.json"),
            logical_object_id: "image-0".to_string(),
        };
        let image_1 = SeenResumeContext {
            manifest_path: artifacts.uploads_dir().join("image-1.json"),
            logical_object_id: "image-1".to_string(),
        };
        assert_eq!(
            *uploader.seen_image_resume_contexts.lock().unwrap(),
            vec![image_0.clone(), image_1.clone(), image_0, image_1]
        );
        let job = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            job.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_READY
        );
    }

    #[tokio::test]
    async fn successful_telegraph_upload_removes_upload_state_only() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        mock_telegraph_upload(&tg_server, 2).await;
        mock_telegraph_create_page(&tg_server).await;
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("archive.zip");
        create_test_zip(&zip_path, 2);
        let artifacts = ArchiveArtifacts::new(&zip_path);
        std::fs::create_dir_all(artifacts.uploads_dir()).unwrap();
        std::fs::write(
            artifacts.uploads_dir().join("archive.json"),
            b"upload state",
        )
        .unwrap();
        let zip_path_str = zip_path.to_string_lossy().to_string();
        let entry = insert_queue_entry(
            &repo,
            -100,
            705,
            "tok",
            "Title",
            true,
            STATUS_DOWNLOADED,
            Some(&zip_path_str),
            None,
        )
        .await;
        let abort_uploader = Arc::new(TerminalCleanupMockUploader::default());
        let worker = EhUploadWorker::new_with_abort_uploader(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_telegraph_client(&tg_server),
            make_image_uploader(&tg_server),
            Some(abort_uploader),
            None,
            Arc::new(make_config()),
        );

        worker.tick().await.unwrap();

        let job = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            job.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_READY
        );
        assert!(!artifacts.uploads_dir().exists());
        assert!(artifacts.final_zip().exists());
    }

    #[tokio::test]
    async fn test_upload_worker_prefers_zip_archive_uploader() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        mock_telegraph_create_page(&tg_server).await;
        let notifier = make_notifier(&tg_server);
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("zip_first.zip");
        create_test_zip(&zip_path, 2);
        let zip_path_str = zip_path.to_string_lossy().to_string();
        let entry = insert_queue_entry(
            &repo,
            -100,
            700,
            "tok",
            "Title",
            true,
            STATUS_DOWNLOADED,
            Some(&zip_path_str),
            None,
        )
        .await;
        let uploader = Arc::new(ZipFirstMockUploader::default());
        let worker = EhUploadWorker::new(
            Arc::clone(&repo),
            notifier,
            make_telegraph_client(&tg_server),
            uploader.clone(),
            None,
            Arc::new(make_config()),
        );

        worker.tick().await.unwrap();

        assert_eq!(
            uploader.zip_calls.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            uploader
                .image_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            *uploader.seen_entries.lock().unwrap(),
            vec!["page000.jpg".to_string(), "page001.jpg".to_string()]
        );
        let job = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            job.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_READY
        );
    }

    #[tokio::test]
    async fn test_upload_worker_falls_back_to_per_image_when_zip_uploader_returns_none() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        mock_telegraph_create_page(&tg_server).await;
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("zip_fallback.zip");
        create_test_zip_with_names(&zip_path, &["dir\\page000.jpg", "page001.jpg"]);
        let zip_path_str = zip_path.to_string_lossy().to_string();
        let entry = insert_queue_entry(
            &repo,
            -100,
            701,
            "tok",
            "Title",
            true,
            STATUS_DOWNLOADED,
            Some(&zip_path_str),
            None,
        )
        .await;
        let uploader = Arc::new(ZipFirstMockUploader {
            zip_fallback: true,
            ..Default::default()
        });
        let worker = EhUploadWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_telegraph_client(&tg_server),
            uploader.clone(),
            None,
            Arc::new(make_config()),
        );

        worker.tick().await.unwrap();

        assert_eq!(
            uploader.zip_calls.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            uploader
                .image_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert_eq!(
            *uploader.seen_entries.lock().unwrap(),
            vec!["dir\\page000.jpg".to_string(), "page001.jpg".to_string()]
        );
        let job = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            job.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_READY
        );
        assert!(job.telegraph_url.is_some());
    }

    #[tokio::test]
    async fn test_upload_worker_fallback_skips_unsupported_non_image_zip_entry() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        mock_telegraph_create_page(&tg_server).await;
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("zip_fallback_with_metadata.zip");
        create_test_zip_with_unsupported_encrypted_non_image(&zip_path);
        let zip_path_str = zip_path.to_string_lossy().to_string();
        let entry = insert_queue_entry(
            &repo,
            -100,
            702,
            "tok",
            "Title",
            true,
            STATUS_DOWNLOADED,
            Some(&zip_path_str),
            None,
        )
        .await;
        let uploader = Arc::new(ZipFirstMockUploader {
            zip_fallback: true,
            ..Default::default()
        });
        let worker = EhUploadWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_telegraph_client(&tg_server),
            uploader.clone(),
            None,
            Arc::new(make_config()),
        );

        worker.tick().await.unwrap();

        assert_eq!(
            uploader.zip_calls.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            uploader
                .image_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let job = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            job.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_READY
        );
    }

    // === Publish Worker Tests ===

    #[tokio::test]
    async fn test_publish_success_records_liveness_without_cleanup() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;

        setup_chat(&repo, -100, true).await;

        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("gallery.zip");
        create_test_zip(&zip_path, 2);
        let artifacts = seed_archive_artifact_family(&zip_path);
        let (job, deliveries) = seed_downloaded_job_with_deliveries(
            &repo,
            123456,
            "abcdef0123",
            "Test Gallery",
            &zip_path,
            &[(-100, false, "Test Gallery")],
        )
        .await;
        let entry = &deliveries[0];

        mock_tg_send_document(&tg_server).await;

        let eh_server = MockServer::start().await;
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            None,
            Arc::new(make_config()),
        );
        worker.tick().await.unwrap();

        let updated = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, "done");
        assert!(artifacts.final_zip().exists());
        assert!(artifacts.assembly_scratch().exists());
        assert!(artifacts.parts_dir().exists());
        assert!(artifacts.uploads_dir().exists());
        assert_eq!(
            job_for_delivery(&repo, &updated).await.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_PENDING
        );
        assert_eq!(updated.job_id, Some(job.id));
    }

    #[tokio::test]
    async fn publish_send_serializes_concurrent_enqueue_into_a_clean_next_wave() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("serialized-wave.zip");
        create_test_zip(&zip_path, 1);
        let (old_job, deliveries) = seed_downloaded_job_with_deliveries(
            &repo,
            910,
            "old-token",
            "Old wave",
            &zip_path,
            &[(-100, false, "Old wave")],
        )
        .await;
        mock_tg_send_document(&tg_server).await;

        let send_entered = Arc::new(tokio::sync::Notify::new());
        let release_send = Arc::new(tokio::sync::Notify::new());
        let done_entered = Arc::new(tokio::sync::Notify::new());
        let release_done = Arc::new(tokio::sync::Notify::new());
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            None,
            Arc::new(make_config()),
        )
        .with_test_send_hook(EhPublishSendHook {
            entered: Arc::clone(&send_entered),
            release: Arc::clone(&release_send),
            after_done: Some(EhPublishCompletionHook {
                entered: Arc::clone(&done_entered),
                release: Arc::clone(&release_done),
            }),
        });
        let claim = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        let publish = tokio::spawn(async move { worker.process_claimed(claim).await });
        tokio::time::timeout(Duration::from_secs(5), send_entered.notified())
            .await
            .expect("publisher must enter the real document send");

        let enqueue_waiting = Arc::new(tokio::sync::Notify::new());
        let enqueue_acquired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let enqueue_hook = crate::db::repo::eh_gallery_jobs::EhEnqueueChatLockHook {
            waiting: Arc::clone(&enqueue_waiting),
            acquired: Arc::clone(&enqueue_acquired),
        };
        let enqueue_repo = Arc::clone(&repo);
        let enqueue = tokio::spawn(async move {
            crate::db::repo::eh_gallery_jobs::EH_ENQUEUE_CHAT_LOCK_HOOK
                .scope(enqueue_hook, async move {
                    enqueue_repo
                        .enqueue_eh_download(
                            -100,
                            910,
                            "new-token",
                            "New wave",
                            false,
                            SOURCE_DIRECT,
                            &EhGalleryVariant::archive("original"),
                        )
                        .await
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), enqueue_waiting.notified())
            .await
            .expect("enqueue must attempt the chat lock after send begins");
        assert!(
            !enqueue_acquired.load(std::sync::atomic::Ordering::SeqCst),
            "enqueue must remain behind the in-flight publisher's chat lock"
        );

        let in_flight = eh_download_queue::Entity::find_by_id(deliveries[0].id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(in_flight.status, STATUS_PUBLISHING);
        assert!(in_flight.archive_sent_at.is_none());

        release_send.notify_one();
        tokio::time::timeout(Duration::from_secs(5), done_entered.notified())
            .await
            .expect("the old wave must persist completion before releasing the chat lock");
        let old_wave = eh_download_queue::Entity::find_by_id(deliveries[0].id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(old_wave.status, STATUS_DONE);
        assert!(old_wave.archive_sent_at.is_some());

        release_done.notify_one();
        publish.await.unwrap().unwrap();
        let new_wave = enqueue.await.unwrap().unwrap();
        assert!(enqueue_acquired.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            new_wave.status,
            crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING
        );
        assert_ne!(new_wave.job_id, Some(old_job.id));
        assert!(new_wave.archive_sent_at.is_none());
        assert!(new_wave.telegraph_sent_at.is_none());
        assert!(new_wave.started_at.is_none());
        assert!(new_wave.completed_at.is_none());
        assert_eq!(new_wave.retry_count, 0);
        assert_eq!(
            tg_server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|request| request.url.path().ends_with("/SendDocument"))
                .count(),
            1,
            "the old wave must send exactly once before a new wave can begin"
        );
    }

    #[tokio::test]
    async fn markerless_publish_claim_rebinds_to_direct_original_before_chat_lock() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("old-1280x.zip");
        create_test_zip(&zip_path, 1);
        let (old_job, deliveries) = seed_downloaded_job_with_deliveries(
            &repo,
            912,
            "same-token",
            "Old 1280x wave",
            &zip_path,
            &[(-100, false, "Old 1280x wave")],
        )
        .await;
        let old_claim = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(old_claim.delivery.id, deliveries[0].id);
        assert!(old_claim.delivery.archive_sent_at.is_none());
        assert!(old_claim.delivery.telegraph_sent_at.is_none());

        let rebound = repo
            .enqueue_eh_download(
                -100,
                912,
                "same-token",
                "Requested original",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("original"),
            )
            .await
            .unwrap();
        let requested_job = job_for_delivery(&repo, &rebound).await;
        assert_eq!(
            rebound.status,
            crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING
        );
        assert_ne!(rebound.job_id, Some(old_job.id));
        assert_eq!(requested_job.resolution, "original");
        assert_eq!(rebound.token, "same-token");
        assert_eq!(rebound.title, "Requested original");
        assert!(rebound.archive_sent_at.is_none());
        assert!(rebound.telegraph_sent_at.is_none());
        assert!(rebound.started_at.is_none());
        assert!(rebound.completed_at.is_none());
        assert!(rebound.error.is_none());
        assert_eq!(rebound.retry_count, 0);
        assert!(rebound.next_retry_at.is_none());

        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            None,
            Arc::new(make_config()),
        );
        worker.process_claimed(old_claim).await.unwrap();
        assert!(
            tg_server.received_requests().await.unwrap().is_empty(),
            "the stale publisher must re-read the re-bound delivery and send nothing"
        );
        assert_eq!(
            eh_download_queue::Entity::find_by_id(rebound.id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap()
                .status,
            crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING
        );

        let selected = repo
            .get_next_eh_job_for_download_with_policy(true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(selected.id, requested_job.id);
        assert_eq!(selected.resolution, "original");
    }

    #[tokio::test]
    async fn publish_tick_drains_started_sibling_before_returning_refill_claim_error() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        setup_chat(&repo, -200, true).await;
        let tg_server = MockServer::start().await;
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("claim-error-drain.zip");
        create_test_zip(&zip_path, 1);
        let (_, deliveries) = seed_downloaded_job_with_deliveries(
            &repo,
            911,
            "claim-error",
            "Claim error drain",
            &zip_path,
            &[(-100, false, "First"), (-200, false, "Second")],
        )
        .await;
        mock_tg_send_document_for_chat(&tg_server, -100, 200, None).await;
        repo.db()
            .execute(Statement::from_string(
                DbBackend::Sqlite,
                format!(
                    "CREATE TRIGGER delete_second_publish_readback AFTER UPDATE OF status ON eh_download_queue \
                     WHEN NEW.id = {} AND NEW.status = 'publishing' BEGIN \
                         DELETE FROM eh_download_queue WHERE id = NEW.id; \
                     END;",
                    deliveries[1].id
                ),
            ))
            .await
            .unwrap();

        let send_entered = Arc::new(tokio::sync::Notify::new());
        let release_send = Arc::new(tokio::sync::Notify::new());
        let mut config = make_config();
        config.publish_concurrency = 2;
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            None,
            Arc::new(config),
        )
        .with_test_send_hook(EhPublishSendHook {
            entered: Arc::clone(&send_entered),
            release: Arc::clone(&release_send),
            after_done: None,
        });
        let tick = tokio::spawn(async move { worker.tick().await });
        tokio::time::timeout(Duration::from_secs(5), send_entered.notified())
            .await
            .expect("the first claimed sibling must reach its send hook");
        tokio::task::yield_now().await;
        assert!(
            !tick.is_finished(),
            "a refill claim error must drain the already-started sibling rather than drop it"
        );

        release_send.notify_one();
        let error = tick
            .await
            .unwrap()
            .expect_err("the refill readback failure must be returned after draining");
        assert!(format!("{:#}", error)
            .contains("Shared EH delivery changed before publish claim readback"));
        let first = eh_download_queue::Entity::find_by_id(deliveries[0].id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let second = eh_download_queue::Entity::find_by_id(deliveries[1].id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.status, STATUS_DONE);
        assert_eq!(
            second.status,
            crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING,
            "the failed refill transaction must not leave a publishing claim"
        );
    }

    #[tokio::test]
    async fn test_publish_archive_only_keeps_family_for_task9_cleanup() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;
        setup_chat(&repo, -100, true).await;
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("abort-gated-archive-only.zip");
        create_test_zip(&zip_path, 2);
        let artifacts = seed_archive_artifact_family(&zip_path);
        let (_, deliveries) = seed_downloaded_job_with_deliveries(
            &repo,
            123457,
            "abort-gated",
            "Archive Only",
            &zip_path,
            &[(-100, false, "Archive Only")],
        )
        .await;
        let entry = &deliveries[0];

        mock_tg_send_document(&tg_server).await;
        let eh_server = MockServer::start().await;
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            None,
            Arc::new(make_config()),
        );

        worker.tick().await.unwrap();
        let done = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.status, STATUS_DONE);
        assert!(done.archive_sent_at.is_some());
        assert!(artifacts.final_zip().exists());
        assert!(artifacts.assembly_scratch().exists());
        assert!(artifacts.parts_dir().exists());
        assert!(artifacts.uploads_dir().join("archive.json").exists());
        let document_sends = tg_server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path().ends_with("/SendDocument"))
            .count();
        assert_eq!(
            document_sends, 1,
            "delivery completion must not create a cleanup resend"
        );
    }

    #[tokio::test]
    async fn test_publish_archive_only_needs_no_abort_uploader() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;
        setup_chat(&repo, -100, true).await;
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("no-abort-uploader.zip");
        create_test_zip(&zip_path, 2);
        let artifacts = seed_archive_artifact_family(&zip_path);
        let (_, deliveries) = seed_downloaded_job_with_deliveries(
            &repo,
            123458,
            "no-abort",
            "Archive Only",
            &zip_path,
            &[(-100, false, "Archive Only")],
        )
        .await;
        let entry = &deliveries[0];

        mock_tg_send_document(&tg_server).await;
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&MockServer::start().await),
            None,
            Arc::new(make_config()),
        );

        worker.tick().await.unwrap();
        let done = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.status, STATUS_DONE);
        assert!(done.archive_sent_at.is_some());
        assert!(artifacts.final_zip().exists());
        assert!(artifacts.uploads_dir().exists());
    }

    #[tokio::test]
    async fn test_publish_worker_with_telegraph() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;

        setup_chat(&repo, -100, true).await;

        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("gallery.zip");
        create_test_zip(&zip_path, 2);
        let (job, deliveries) = seed_downloaded_job_with_deliveries(
            &repo,
            123456,
            "abcdef0123",
            "Test Gallery",
            &zip_path,
            &[(-100, true, "Test Gallery")],
        )
        .await;
        let entry = &deliveries[0];
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphStatus,
                Expr::value(crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_READY),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphUrl,
                Expr::value(Some("https://telegra.ph/Test-01-01".to_string())),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .exec(repo.db())
            .await
            .unwrap();

        mock_tg_send_document(&tg_server).await;
        mock_tg_send_message(&tg_server).await;

        let eh_server = MockServer::start().await;
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            None,
            Arc::new(make_config()),
        );
        worker.tick().await.unwrap();

        let updated = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, "done");

        // Verify TG received both sendDocument and sendMessage
        let received = tg_server.received_requests().await.unwrap();
        assert!(received
            .iter()
            .any(|r| r.url.path().ends_with("/SendDocument")));
        assert!(received
            .iter()
            .any(|r| r.url.path().ends_with("/SendMessage")));
    }

    #[tokio::test]
    async fn test_publish_worker_chat_disabled_schedules_retry() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;

        setup_chat(&repo, -100, false).await; // disabled

        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("gallery.zip");
        create_test_zip(&zip_path, 2);
        let (_, deliveries) = seed_downloaded_job_with_deliveries(
            &repo,
            123456,
            "abcdef0123",
            "Test",
            &zip_path,
            &[(-100, false, "Test")],
        )
        .await;
        let entry = &deliveries[0];

        let eh_server = MockServer::start().await;
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            None,
            Arc::new(make_config()),
        );
        worker.tick().await.unwrap();

        let updated = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        // Chat disabled → delivery goes back to waiting with retry (not silently done)
        assert_eq!(
            updated.status,
            crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING,
            "should be back to waiting for retry"
        );
        assert_eq!(
            updated.retry_count, 0,
            "chat disabled defer should not increment retry_count"
        );
        assert!(
            updated.next_retry_at.is_some(),
            "should have next_retry_at set"
        );
    }

    #[tokio::test]
    async fn test_publish_retry_skips_archive_after_marker() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let config = Arc::new(make_config());
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("501.zip");
        create_test_zip(&zip_path, 2);
        let (job, deliveries) = seed_downloaded_job_with_deliveries(
            &repo,
            501,
            "tok",
            "Title",
            &zip_path,
            &[(-100, true, "Title")],
        )
        .await;
        let entry = &deliveries[0];
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphStatus,
                Expr::value(crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_READY),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphUrl,
                Expr::value(Some("https://telegra.ph/page".to_string())),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .exec(repo.db())
            .await
            .unwrap();

        // Pre-set archive_sent_at directly (bypassing the publishing guard for test setup)
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::ArchiveSentAt,
                Expr::value(Some(Local::now().naive_local())),
            )
            .filter(eh_download_queue::Column::Id.eq(entry.id))
            .exec(repo.db())
            .await
            .unwrap();

        // Only mock SendMessage (telegraph link); do NOT mock SendDocument (archive)
        mock_tg_send_message(&tg_server).await;

        let eh_server = MockServer::start().await;
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            notifier,
            make_eh_client(&eh_server),
            None,
            config,
        );
        worker.tick().await.unwrap();

        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.status, STATUS_DONE);
        assert!(model.telegraph_sent_at.is_some());
    }

    #[tokio::test]
    async fn test_publish_missing_zip_resets_shared_download_instead_of_done() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let config = Arc::new(make_config());
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("missing.zip");
        create_test_zip(&zip_path, 1);
        let (job, deliveries) = seed_downloaded_job_with_deliveries(
            &repo,
            502,
            "tok",
            "Title",
            &zip_path,
            &[(-100, false, "Title")],
        )
        .await;
        let entry = &deliveries[0];
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::StartedAt,
                Expr::value(Some(Local::now().naive_local())),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .exec(repo.db())
            .await
            .unwrap();
        std::fs::remove_file(&zip_path).unwrap();

        let eh_server = MockServer::start().await;
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            notifier,
            make_eh_client(&eh_server),
            None,
            config,
        );
        worker.tick().await.unwrap();

        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            model.status,
            crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING
        );
        assert_eq!(model.retry_count, 0);
        assert!(model.next_retry_at.is_some());
        let reset = job_for_delivery(&repo, &model).await;
        assert_eq!(reset.id, job.id);
        assert_eq!(
            reset.status,
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_PENDING
        );
        assert_eq!(reset.retry_count, 1);
        assert!(reset.zip_path.is_none());
    }

    #[tokio::test]
    async fn test_publish_skips_entry_canceled_after_claim() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let config = Arc::new(make_config());
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("509.zip");
        create_test_zip(&zip_path, 2);
        let artifacts = ArchiveArtifacts::new(&zip_path);
        std::fs::create_dir_all(artifacts.uploads_dir()).unwrap();
        std::fs::write(
            artifacts.uploads_dir().join("archive.json"),
            b"upload state",
        )
        .unwrap();
        let entry = insert_subscription_queue_entry(
            &repo,
            -100,
            "123",
            509,
            "tok",
            "Title",
            false,
            STATUS_DOWNLOADED,
            Some(zip_path.to_str().unwrap()),
            None,
        )
        .await;
        let entry = migrate_seeded_delivery_to_waiting(&repo, entry).await;
        let claimed = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.delivery.id, entry.id);
        repo.cancel_eh_subscription_queue_entries(123, true)
            .await
            .unwrap();

        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            notifier,
            make_eh_client(&MockServer::start().await),
            None,
            config,
        );
        worker.process_claimed(claimed).await.unwrap();

        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.status, STATUS_CANCELED);
        assert!(model.archive_sent_at.is_none());
        assert!(
            zip_path.exists(),
            "canceled publish must not clean shared ZIP"
        );
        assert!(
            artifacts.uploads_dir().exists(),
            "Task 9 cleanup owns the shared upload-state lifecycle"
        );
    }

    #[tokio::test]
    async fn test_publish_cancellation_leaves_upload_state_for_liveness_cleanup() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("publish-canceled-abort-fails.zip");
        create_test_zip(&zip_path, 2);
        let artifacts = ArchiveArtifacts::new(&zip_path);
        std::fs::create_dir_all(artifacts.uploads_dir()).unwrap();
        std::fs::write(
            artifacts.uploads_dir().join("archive.json"),
            b"upload state",
        )
        .unwrap();
        let entry = insert_subscription_queue_entry(
            &repo,
            -100,
            "123",
            510,
            "tok",
            "Title",
            false,
            STATUS_DOWNLOADED,
            Some(zip_path.to_str().unwrap()),
            None,
        )
        .await;
        let entry = migrate_seeded_delivery_to_waiting(&repo, entry).await;
        let claimed = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        repo.cancel_eh_subscription_queue_entries(123, true)
            .await
            .unwrap();
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&MockServer::start().await),
            None,
            Arc::new(make_config()),
        );

        worker.process_claimed(claimed).await.unwrap();
        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.status, STATUS_CANCELED);
        assert!(zip_path.exists());
        assert!(artifacts.uploads_dir().exists());
    }

    #[tokio::test]
    async fn test_publish_terminal_send_failure_is_delivery_local_and_keeps_shared_family() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let mut cfg = make_config();
        cfg.send_archive = true;
        cfg.max_retry_count = 0;
        let config = Arc::new(cfg);
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("503.zip");
        create_test_zip(&zip_path, 2);
        let artifacts = seed_archive_artifact_family(&zip_path);
        let (job, deliveries) = seed_downloaded_job_with_deliveries(
            &repo,
            503,
            "tok",
            "Title",
            &zip_path,
            &[(-100, false, "Title")],
        )
        .await;
        let entry = &deliveries[0];
        mock_tg_send_document_for_chat(&tg_server, -100, 500, None).await;
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&MockServer::start().await),
            None,
            config,
        );

        worker.tick().await.unwrap();
        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.status, STATUS_FAILED);
        assert!(model
            .error
            .as_deref()
            .unwrap()
            .contains("Failed to send archive document"));
        assert_eq!(model.retry_count, 1);
        let retired = job_for_delivery(&repo, &model).await;
        assert_eq!(retired.id, job.id);
        assert_eq!(
            retired.status,
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_RETIRED
        );
        assert_eq!(
            retired.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_PENDING
        );
        assert!(artifacts.final_zip().exists());
        assert!(artifacts.assembly_scratch().exists());
        assert!(artifacts.parts_dir().exists());
        assert!(artifacts.uploads_dir().exists());
    }

    #[tokio::test]
    async fn test_chat_disabled_defer_does_not_increment_retry() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, false).await; // disabled
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let config = Arc::new(make_config());
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("504.zip");
        create_test_zip(&zip_path, 2);
        let (_, deliveries) = seed_downloaded_job_with_deliveries(
            &repo,
            504,
            "tok",
            "Title",
            &zip_path,
            &[(-100, false, "Title")],
        )
        .await;
        let entry = &deliveries[0];

        let eh_server = MockServer::start().await;
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            notifier,
            make_eh_client(&eh_server),
            None,
            config,
        );
        worker.tick().await.unwrap();
        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            model.status,
            crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING
        );
        assert_eq!(model.retry_count, 0);
        assert!(model.next_retry_at.is_some());
    }

    #[tokio::test]
    async fn test_upload_terminal_failure_keeps_archive_fallback_and_upload_state() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let mut cfg = make_config();
        cfg.max_retry_count = 0;
        cfg.send_archive = true;
        let config = Arc::new(cfg);
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("505.zip");
        create_test_zip(&zip_path, 2);
        let artifacts = seed_archive_artifact_family(&zip_path);
        let entry = insert_queue_entry(
            &repo,
            -100,
            505,
            "tok",
            "Title",
            true,
            STATUS_DOWNLOADED,
            Some(zip_path.to_str().unwrap()),
            None,
        )
        .await;
        let entry = migrate_seeded_delivery_to_waiting(&repo, entry).await;
        let uploader = Arc::new(TerminalCleanupMockUploader::default());

        let worker = EhUploadWorker::new_with_abort_uploader(
            Arc::clone(&repo),
            notifier,
            make_telegraph_client(&tg_server),
            uploader.clone(),
            Some(uploader.clone()),
            None,
            config,
        );
        worker.tick().await.unwrap();
        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            model.status,
            crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING
        );
        assert!(!model.telegraph);
        assert_eq!(model.error, None);
        assert!(model.started_at.is_none());
        assert!(model.completed_at.is_none());
        assert!(model.next_retry_at.is_none());
        let job = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            job.status,
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_DOWNLOADED
        );
        assert_eq!(
            job.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_FAILED
        );
        assert_eq!(job.retry_count, 1);
        assert!(job.next_retry_at.is_none());
        assert_eq!(
            job.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_NONE
        );
        assert!(zip_path.exists());
        assert!(artifacts.uploads_dir().exists());
        assert!(artifacts.assembly_scratch().exists());
        assert!(artifacts.parts_dir().exists());
        let fallback_claim = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fallback_claim.delivery.id, entry.id);
        assert!(!fallback_claim.delivery.telegraph);
        assert!(uploader.cleanup_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_upload_terminal_failure_defers_abort_until_archive_fallback_finishes() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let mut cfg = make_config();
        cfg.max_retry_count = 0;
        cfg.send_archive = true;
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("505-abort-fails.zip");
        create_test_zip(&zip_path, 2);
        let artifacts = seed_archive_artifact_family(&zip_path);
        let entry = insert_queue_entry(
            &repo,
            -100,
            505,
            "tok",
            "Title",
            true,
            STATUS_DOWNLOADED,
            Some(zip_path.to_str().unwrap()),
            None,
        )
        .await;
        let entry = migrate_seeded_delivery_to_waiting(&repo, entry).await;
        let uploader = Arc::new(TerminalCleanupMockUploader {
            fail_abort: true,
            ..Default::default()
        });
        let worker = EhUploadWorker::new_with_abort_uploader(
            Arc::clone(&repo),
            notifier,
            make_telegraph_client(&tg_server),
            uploader.clone(),
            Some(uploader.clone()),
            None,
            Arc::new(cfg),
        );

        worker.tick().await.unwrap();
        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            model.status,
            crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING
        );
        assert!(!model.telegraph);
        let job = job_for_delivery(&repo, &entry).await;
        assert_eq!(job.retry_count, 1);
        assert_eq!(
            job.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_FAILED
        );
        assert_eq!(
            job.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_NONE
        );
        assert!(zip_path.exists());
        assert!(artifacts.uploads_dir().exists());
        assert!(artifacts.uploads_dir().join("archive.json").exists());
        assert!(uploader.cleanup_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_upload_terminal_failure_without_abort_uploader_preserves_archive_fallback() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;
        let mut config = make_config();
        config.max_retry_count = 0;
        assert!(config.send_archive);
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("505-no-abort-uploader.zip");
        create_test_zip(&zip_path, 2);
        let artifacts = seed_archive_artifact_family(&zip_path);
        let entry = insert_queue_entry(
            &repo,
            -100,
            515,
            "tok",
            "Title",
            true,
            STATUS_DOWNLOADED,
            Some(zip_path.to_str().unwrap()),
            None,
        )
        .await;
        let entry = migrate_seeded_delivery_to_waiting(&repo, entry).await;
        let worker = EhUploadWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_telegraph_client(&tg_server),
            Arc::new(TerminalCleanupMockUploader::default()),
            None,
            Arc::new(config),
        );

        worker.tick().await.unwrap();
        let job = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            job.status,
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_DOWNLOADED
        );
        assert_eq!(
            job.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_FAILED
        );
        assert_eq!(job.retry_count, 1);
        assert_eq!(
            job.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_NONE
        );
        let delivery = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            delivery.status,
            crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING
        );
        assert!(!delivery.telegraph);
        assert!(artifacts.final_zip().exists());
        assert!(artifacts.assembly_scratch().exists());
        assert!(artifacts.parts_dir().exists());
        assert!(artifacts.uploads_dir().join("archive.json").exists());
    }

    #[tokio::test]
    async fn test_upload_permanent_failure_without_fallback_removes_whole_archive_family() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let mut cfg = make_config();
        cfg.max_retry_count = 0;
        cfg.send_archive = false;
        let config = Arc::new(cfg);
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("506.zip");
        create_test_zip(&zip_path, 2);
        let artifacts = seed_archive_artifact_family(&zip_path);
        let entry = insert_queue_entry(
            &repo,
            -100,
            506,
            "tok",
            "Title",
            true,
            STATUS_DOWNLOADED,
            Some(zip_path.to_str().unwrap()),
            None,
        )
        .await;
        let entry = migrate_seeded_delivery_to_waiting(&repo, entry).await;
        let uploader = Arc::new(TerminalCleanupMockUploader::default());

        let worker = EhUploadWorker::new_with_abort_uploader(
            Arc::clone(&repo),
            notifier,
            make_telegraph_client(&tg_server),
            uploader.clone(),
            Some(uploader.clone()),
            None,
            config,
        );
        worker.tick().await.unwrap();

        let delivery = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivery.status, STATUS_FAILED);
        assert_eq!(delivery.error, None);
        let job = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            job.status,
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_RETIRED
        );
        assert_eq!(
            job.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_FAILED
        );
        assert_eq!(job.retry_count, 1);
        assert!(job.next_retry_at.is_none());
        assert_eq!(
            job.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_PENDING
        );
        assert_eq!(
            run_eh_job_cleanup_maintenance_once(repo.as_ref(), Some(uploader.as_ref()), 1, true)
                .await
                .unwrap(),
            Some(EhCleanupFinalizeOutcome::CleanRetired)
        );
        assert!(!artifacts.final_zip().exists());
        assert!(!artifacts.assembly_scratch().exists());
        assert!(!artifacts.parts_dir().exists());
        assert!(!artifacts.uploads_dir().exists());
        assert_terminal_cleanup_precedes_local_removal(&uploader, &artifacts);
    }

    #[tokio::test]
    async fn test_upload_permanent_failure_without_fallback_preserves_family_when_abort_fails() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let mut cfg = make_config();
        cfg.max_retry_count = 0;
        cfg.send_archive = false;
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("506-abort-fails.zip");
        create_test_zip(&zip_path, 2);
        let artifacts = seed_archive_artifact_family(&zip_path);
        let entry = insert_queue_entry(
            &repo,
            -100,
            506,
            "tok",
            "Title",
            true,
            STATUS_DOWNLOADED,
            Some(zip_path.to_str().unwrap()),
            None,
        )
        .await;
        let entry = migrate_seeded_delivery_to_waiting(&repo, entry).await;
        let uploader = Arc::new(TerminalCleanupMockUploader {
            fail_abort: true,
            ..Default::default()
        });
        let worker = EhUploadWorker::new_with_abort_uploader(
            Arc::clone(&repo),
            notifier,
            make_telegraph_client(&tg_server),
            uploader.clone(),
            Some(uploader.clone()),
            None,
            Arc::new(cfg),
        );

        worker.tick().await.unwrap();
        let delivery = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            delivery.status,
            crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_FAILED
        );
        assert_eq!(delivery.error, None);
        let job = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            job.status,
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_RETIRED
        );
        assert_eq!(
            job.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_FAILED
        );
        assert_eq!(job.retry_count, 1);
        assert_eq!(
            job.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_PENDING
        );
        assert!(run_eh_job_cleanup_maintenance_once(
            repo.as_ref(),
            Some(uploader.as_ref()),
            1,
            true
        )
        .await
        .is_err());
        let failed_cleanup = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            failed_cleanup.cleanup_status,
            crate::db::repo::eh_gallery_jobs::CLEANUP_STATUS_FAILED
        );
        assert!(artifacts.final_zip().exists());
        assert!(artifacts.assembly_scratch().exists());
        assert!(artifacts.parts_dir().exists());
        assert!(artifacts.uploads_dir().exists());
        assert!(artifacts.uploads_dir().join("archive.json").exists());
        assert_terminal_cleanup_precedes_local_removal(&uploader, &artifacts);
    }

    #[tokio::test]
    async fn test_upload_canceled_after_claim_removes_upload_state_without_sending() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("507-canceled.zip");
        create_test_zip(&zip_path, 2);
        let artifacts = ArchiveArtifacts::new(&zip_path);
        std::fs::create_dir_all(artifacts.uploads_dir()).unwrap();
        std::fs::write(
            artifacts.uploads_dir().join("archive.json"),
            b"upload state",
        )
        .unwrap();
        let entry = insert_subscription_queue_entry(
            &repo,
            -100,
            "123",
            507,
            "tok",
            "Title",
            true,
            STATUS_DOWNLOADED,
            Some(zip_path.to_str().unwrap()),
            None,
        )
        .await;
        let entry = migrate_seeded_delivery_to_waiting(&repo, entry).await;
        mock_telegraph_create_page(&tg_server).await;
        let claimed = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        assert_eq!(claimed.id, entry.job_id.unwrap());
        repo.cancel_eh_subscription_queue_entries(123, true)
            .await
            .unwrap();
        let uploader = Arc::new(ZipFirstMockUploader::default());
        let abort_uploader = Arc::new(TerminalCleanupMockUploader::default());

        let worker = EhUploadWorker::new_with_abort_uploader(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_telegraph_client(&tg_server),
            uploader.clone(),
            Some(abort_uploader.clone()),
            None,
            Arc::new(make_config()),
        );
        worker.process(&claimed).await.unwrap();

        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.status, STATUS_CANCELED);
        assert!(zip_path.exists(), "canceled upload should not clean ZIP");
        assert!(!artifacts.uploads_dir().exists());
        let job = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            job.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_READY
        );
        assert_eq!(
            uploader.zip_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "cancellation after claim must not interrupt the shared upload"
        );
        assert_terminal_cleanup_precedes_local_removal(&abort_uploader, &artifacts);
    }

    #[tokio::test]
    async fn test_upload_canceled_after_claim_preserves_upload_state_when_abort_fails() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("507-canceled-abort-fails.zip");
        create_test_zip(&zip_path, 2);
        let artifacts = ArchiveArtifacts::new(&zip_path);
        std::fs::create_dir_all(artifacts.uploads_dir()).unwrap();
        let manifest = artifacts.uploads_dir().join("archive.json");
        std::fs::write(&manifest, b"upload state").unwrap();
        let entry = insert_subscription_queue_entry(
            &repo,
            -100,
            "123",
            507,
            "tok",
            "Title",
            true,
            STATUS_DOWNLOADED,
            Some(zip_path.to_str().unwrap()),
            None,
        )
        .await;
        let entry = migrate_seeded_delivery_to_waiting(&repo, entry).await;
        mock_telegraph_create_page(&tg_server).await;
        let claimed = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        assert_eq!(claimed.id, entry.job_id.unwrap());
        repo.cancel_eh_subscription_queue_entries(123, true)
            .await
            .unwrap();
        let uploader = Arc::new(ZipFirstMockUploader::default());
        let abort_uploader = Arc::new(TerminalCleanupMockUploader {
            fail_abort: true,
            ..Default::default()
        });
        let worker = EhUploadWorker::new_with_abort_uploader(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_telegraph_client(&tg_server),
            uploader.clone(),
            Some(abort_uploader.clone()),
            None,
            Arc::new(make_config()),
        );

        worker.process(&claimed).await.unwrap();
        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.status, STATUS_CANCELED);
        assert!(zip_path.exists(), "canceled upload must not clean ZIP");
        assert!(artifacts.uploads_dir().exists());
        assert!(
            manifest.exists(),
            "failed Abort must retain upload manifest"
        );
        let job = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            job.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_READY
        );
        assert_eq!(
            uploader.zip_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "cancellation after claim must not suppress page creation"
        );
        assert_terminal_cleanup_precedes_local_removal(&abort_uploader, &artifacts);
    }

    #[tokio::test]
    async fn test_upload_permanent_failure_with_missing_zip_enters_archive_fallback() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let mut cfg = make_config();
        cfg.max_retry_count = 0;
        cfg.send_archive = true;
        let config = Arc::new(cfg);
        let entry = insert_queue_entry(
            &repo,
            -100,
            506,
            "tok",
            "Title",
            true,
            STATUS_DOWNLOADED,
            Some("data/test_cache/missing_506.zip"),
            None,
        )
        .await;
        let entry = migrate_seeded_delivery_to_waiting(&repo, entry).await;

        let worker = EhUploadWorker::new(
            Arc::clone(&repo),
            notifier,
            make_telegraph_client(&tg_server),
            make_image_uploader(&tg_server),
            None,
            config,
        );
        worker.tick().await.unwrap();
        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        // The terminal upload failure preserves the DB-visible archive surface;
        // the publish worker owns filesystem validation and missing-ZIP recovery.
        assert_eq!(
            model.status,
            crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING
        );
        assert!(!model.telegraph);
        assert_eq!(model.error, None, "provider errors stay on the shared job");
        assert!(job_for_delivery(&repo, &entry).await.error.is_some());
    }

    #[tokio::test]
    async fn test_upload_worker_ignores_per_chat_notification_gate() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, false).await; // disabled
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let config = Arc::new(make_config());
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("507.zip");
        create_test_zip(&zip_path, 2);
        let entry = insert_queue_entry(
            &repo,
            -100,
            507,
            "tok",
            "Title",
            true,
            STATUS_DOWNLOADED,
            Some(zip_path.to_str().unwrap()),
            None,
        )
        .await;
        let entry = migrate_seeded_delivery_to_waiting(&repo, entry).await;
        mock_telegraph_upload(&tg_server, 2).await;
        mock_telegraph_create_page(&tg_server).await;

        let worker = EhUploadWorker::new(
            Arc::clone(&repo),
            notifier,
            make_telegraph_client(&tg_server),
            make_image_uploader(&tg_server),
            None,
            config,
        );
        worker.tick().await.unwrap();
        let job = job_for_delivery(&repo, &entry).await;
        assert_eq!(job.retry_count, 0);
        assert_eq!(job.next_retry_at, None);
        assert_eq!(
            job.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_READY
        );
        assert!(zip_path.exists());
    }

    #[tokio::test]
    async fn test_publish_both_markers_already_set_skips_to_done() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        // No mocks mounted on tg_server — any outbound request would hang/fail.
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let config = Arc::new(make_config());
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("508.zip");
        create_test_zip(&zip_path, 2);
        let (_, deliveries) = seed_downloaded_job_with_deliveries(
            &repo,
            508,
            "tok",
            "Title",
            &zip_path,
            &[(-100, true, "Title")],
        )
        .await;
        let entry = &deliveries[0];

        // Pre-set both markers to simulate a completed-but-not-done entry.
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::ArchiveSentAt,
                Expr::value(Some(Local::now().naive_local())),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphSentAt,
                Expr::value(Some(Local::now().naive_local())),
            )
            .filter(eh_download_queue::Column::Id.eq(entry.id))
            .exec(repo.db())
            .await
            .unwrap();

        let eh_server = MockServer::start().await;
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            notifier,
            make_eh_client(&eh_server),
            None,
            config,
        );
        worker.tick().await.unwrap();

        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.status, STATUS_DONE);
    }

    #[tokio::test]
    async fn publish_worker_claims_at_most_two_deliveries_and_refills() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("shared.zip");
        create_test_zip(&zip_path, 1);
        for chat_id in [-100, -200, -300] {
            setup_chat(&repo, chat_id, true).await;
            mock_tg_send_document_for_chat(
                &tg_server,
                chat_id,
                200,
                Some(Duration::from_millis(200)),
            )
            .await;
        }
        let (job, _) = seed_downloaded_job_with_deliveries(
            &repo,
            900,
            "publish-two",
            "Shared Publish",
            &zip_path,
            &[
                (-100, false, "One"),
                (-200, false, "Two"),
                (-300, false, "Three"),
            ],
        )
        .await;
        let mut config = make_config();
        config.publish_concurrency = 2;
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            None,
            Arc::new(config),
        );

        let running = tokio::spawn(async move { worker.tick().await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let in_flight = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::JobId.eq(job.id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .all(repo.db())
            .await
            .unwrap();
        let waiting = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::JobId.eq(job.id))
            .filter(
                eh_download_queue::Column::Status
                    .eq(crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING),
            )
            .all(repo.db())
            .await
            .unwrap();
        assert_eq!(in_flight.len(), 2);
        assert_eq!(waiting.len(), 1);

        running.await.unwrap().unwrap();
        let done = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::JobId.eq(job.id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_DONE))
            .all(repo.db())
            .await
            .unwrap();
        assert_eq!(done.len(), 3);
    }

    #[tokio::test]
    async fn telegram_failure_retries_only_one_delivery_without_repeating_shared_work() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("shared.zip");
        create_test_zip(&zip_path, 1);
        setup_chat(&repo, -100, true).await;
        setup_chat(&repo, -200, true).await;
        mock_tg_send_document_for_chat(&tg_server, -100, 500, None).await;
        mock_tg_send_document_for_chat(&tg_server, -200, 200, None).await;
        let (job, deliveries) = seed_downloaded_job_with_deliveries(
            &repo,
            901,
            "publish-retry",
            "Shared Retry",
            &zip_path,
            &[(-100, false, "Failing"), (-200, false, "Working")],
        )
        .await;
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            None,
            Arc::new(make_config()),
        );

        worker.tick().await.unwrap();

        let failed = eh_download_queue::Entity::find_by_id(deliveries[0].id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let succeeded = eh_download_queue::Entity::find_by_id(deliveries[1].id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            failed.status,
            crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING
        );
        assert_eq!(failed.retry_count, 1);
        assert_eq!(succeeded.status, STATUS_DONE);
        assert_eq!(
            job_for_delivery(&repo, &succeeded).await.status,
            JOB_STATUS_DOWNLOADED
        );
        assert!(eh_server.received_requests().await.unwrap().is_empty());
        assert_eq!(job.id, succeeded.job_id.unwrap());
    }

    #[tokio::test]
    async fn archive_only_delivery_bypasses_upload_wait_and_disabled_chat_does_not_block_sibling() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("archive-only.zip");
        create_test_zip(&zip_path, 1);
        setup_chat(&repo, -100, false).await;
        setup_chat(&repo, -200, true).await;
        mock_tg_send_document_for_chat(&tg_server, -200, 200, None).await;
        let (job, deliveries) = seed_downloaded_job_with_deliveries(
            &repo,
            902,
            "archive-only",
            "Archive Only",
            &zip_path,
            &[(-100, false, "Disabled"), (-200, false, "Enabled")],
        )
        .await;
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::TelegraphRequired,
                Expr::value(true),
            )
            .col_expr(
                eh_gallery_jobs::Column::TelegraphStatus,
                Expr::value(crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_UPLOADING),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .exec(repo.db())
            .await
            .unwrap();
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            None,
            Arc::new(make_config()),
        );

        worker.tick().await.unwrap();

        let deferred = eh_download_queue::Entity::find_by_id(deliveries[0].id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let done = eh_download_queue::Entity::find_by_id(deliveries[1].id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            deferred.status,
            crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING
        );
        assert_eq!(deferred.retry_count, 0);
        assert!(deferred.next_retry_at.is_some());
        assert_eq!(done.status, STATUS_DONE);
        assert_eq!(
            job_for_delivery(&repo, &done).await.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_UPLOADING
        );
    }

    #[tokio::test]
    async fn missing_ready_zip_resets_one_job_generation_for_all_archive_consumers() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("gone.zip");
        create_test_zip(&zip_path, 1);
        setup_chat(&repo, -100, true).await;
        setup_chat(&repo, -200, true).await;
        let (job, _) = seed_downloaded_job_with_deliveries(
            &repo,
            903,
            "missing-zip",
            "Missing ZIP",
            &zip_path,
            &[(-100, false, "First"), (-200, false, "Second")],
        )
        .await;
        let expected_started_at = Local::now().naive_local();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::StartedAt,
                Expr::value(Some(expected_started_at)),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .exec(repo.db())
            .await
            .unwrap();
        std::fs::remove_file(&zip_path).unwrap();
        let claim = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        let expected_zip_path = claim.job.zip_path.clone().unwrap();
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            None,
            Arc::new(make_config()),
        );

        worker
            .handle_missing_zip(&claim.delivery, &claim.job)
            .await
            .unwrap();

        assert!(
            repo.get_next_eh_delivery_for_publish(true)
                .await
                .unwrap()
                .is_none(),
            "the reset job is not publishable until its shared redownload is due"
        );
        let reset = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reset.status,
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_PENDING
        );
        assert!(reset.zip_path.is_none());
        assert_eq!(reset.retry_count, 1);
        let waiting = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::JobId.eq(job.id))
            .filter(
                eh_download_queue::Column::Status
                    .eq(crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING),
            )
            .all(repo.db())
            .await
            .unwrap();
        assert_eq!(waiting.len(), 2);
        assert!(
            !repo
                .reset_eh_job_for_missing_zip(job.id, expected_started_at, &expected_zip_path,)
                .await
                .unwrap(),
            "the second concurrent-style reset must not consume another shared retry"
        );
    }

    #[tokio::test]
    async fn late_telegraph_demand_missing_zip_defers_archive_and_advances_upload_generation() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("late-demand-gone.zip");
        create_test_zip(&zip_path, 1);
        let variant = EhGalleryVariant::archive("1280x");
        let archive = repo
            .enqueue_eh_download(
                -100,
                905,
                "late-demand",
                "Late demand",
                false,
                SOURCE_DIRECT,
                &variant,
            )
            .await
            .unwrap();
        repo.enqueue_eh_download(
            -200,
            905,
            "late-demand",
            "Late demand",
            true,
            SOURCE_DIRECT,
            &variant,
        )
        .await
        .unwrap();
        let download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        let downloaded_generation = download.started_at.unwrap();
        repo.mark_eh_job_downloaded(
            download.id,
            downloaded_generation,
            10,
            zip_path.to_str().unwrap(),
            0,
        )
        .await
        .unwrap();
        let failed_upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        let failed_upload_generation = failed_upload.started_at.unwrap();
        assert!(matches!(
            repo.record_eh_job_upload_failure(
                failed_upload.id,
                failed_upload_generation,
                "terminal provider failure",
                0,
                true,
            )
            .await
            .unwrap(),
            crate::db::repo::eh_gallery_jobs::EhJobUploadFailureOutcome::Terminal { .. }
        ));

        let late = repo
            .enqueue_eh_download(
                -300,
                905,
                "late-demand",
                "Late demand",
                true,
                SOURCE_DIRECT,
                &variant,
            )
            .await
            .unwrap();
        assert_eq!(late.job_id, Some(download.id));
        let after_late_demand = job_for_delivery(&repo, &late).await;
        assert_eq!(
            after_late_demand.started_at,
            Some(failed_upload_generation),
            "late upload demand must retain the prior generation fence"
        );
        assert_eq!(
            after_late_demand.telegraph_status,
            crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_PENDING
        );

        std::fs::remove_file(&zip_path).unwrap();
        let archive_claim = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(archive_claim.delivery.id, archive.id);
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            None,
            Arc::new(make_config()),
        );
        worker
            .handle_missing_zip(&archive_claim.delivery, &archive_claim.job)
            .await
            .unwrap();

        let reset = job_for_delivery(&repo, &archive).await;
        assert_eq!(
            reset.status,
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_PENDING
        );
        assert!(reset.zip_path.is_none());
        assert_eq!(reset.started_at, Some(failed_upload_generation));
        assert_eq!(
            eh_download_queue::Entity::find_by_id(archive.id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap()
                .status,
            crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING
        );

        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::NextRetryAt,
                Expr::value(None::<chrono::NaiveDateTime>),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(download.id))
            .exec(repo.db())
            .await
            .unwrap();
        let replacement_download = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        let replacement_generation = replacement_download.started_at.unwrap();
        assert!(replacement_generation > failed_upload_generation);
        create_test_zip(&zip_path, 1);
        repo.mark_eh_job_downloaded(
            replacement_download.id,
            replacement_generation,
            10,
            zip_path.to_str().unwrap(),
            0,
        )
        .await
        .unwrap();
        let replacement_upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
        assert!(
            replacement_upload.started_at.unwrap() > replacement_generation,
            "the replacement upload must own a strictly newer generation"
        );
    }

    #[tokio::test]
    async fn malformed_missing_zip_state_defers_publishing_delivery_before_error() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("malformed.zip");
        create_test_zip(&zip_path, 1);
        let (job, deliveries) = seed_downloaded_job_with_deliveries(
            &repo,
            906,
            "malformed",
            "Malformed",
            &zip_path,
            &[(-100, false, "Archive")],
        )
        .await;
        let delivery = deliveries.into_iter().next().unwrap();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(crate::db::repo::eh_download_queue::STATUS_PUBLISHING),
            )
            .filter(eh_download_queue::Column::Id.eq(delivery.id))
            .exec(repo.db())
            .await
            .unwrap();
        let publishing = eh_download_queue::Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        let malformed_job = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert!(malformed_job.started_at.is_none());
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            None,
            Arc::new(make_config()),
        );

        let error = worker
            .handle_missing_zip(&publishing, &malformed_job)
            .await
            .expect_err("missing generation must still release the delivery first");
        assert!(error
            .to_string()
            .contains("Missing shared EH ZIP reset requires a persisted generation"));
        let deferred = eh_download_queue::Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            deferred.status,
            crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING
        );
        assert!(deferred.next_retry_at.is_some());
    }

    #[tokio::test]
    async fn old_publish_claim_cannot_reset_a_newer_same_path_job_generation() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("same-path.zip");
        create_test_zip(&zip_path, 1);
        let delivery = repo
            .enqueue_eh_download(
                -100,
                904,
                "same-path",
                "Generation fence",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
            )
            .await
            .unwrap();
        let claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        repo.mark_eh_job_downloaded(
            claim.id,
            claim.started_at.unwrap(),
            10,
            zip_path.to_str().unwrap(),
            0,
        )
        .await
        .unwrap();
        let job = job_for_delivery(&repo, &delivery).await;
        let old_generation = job.started_at.unwrap();
        let new_generation = old_generation + chrono::Duration::seconds(1);
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::StartedAt,
                Expr::value(Some(new_generation)),
            )
            .col_expr(eh_gallery_jobs::Column::RetryCount, Expr::value(7_i32))
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .exec(repo.db())
            .await
            .unwrap();
        let before = job_for_delivery(&repo, &delivery).await;

        assert!(
            !repo
                .reset_eh_job_for_missing_zip(job.id, old_generation, zip_path.to_str().unwrap(),)
                .await
                .unwrap(),
            "an old publishing claim must not reset a newer generation sharing the same path"
        );

        let after = job_for_delivery(&repo, &delivery).await;
        assert_eq!(after.status, before.status);
        assert_eq!(after.started_at, Some(new_generation));
        assert_eq!(after.zip_path, before.zip_path);
        assert_eq!(after.retry_count, before.retry_count);
        assert_eq!(
            eh_download_queue::Entity::find_by_id(delivery.id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap()
                .status,
            crate::db::repo::eh_gallery_jobs::DELIVERY_STATUS_WAITING
        );
    }

    // ---- GP guard tests ----

    #[tokio::test]
    async fn test_download_worker_gp_cost_exceeds_policy_permanently_fails_without_post() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            2284788,
            "7841d194d4",
            "GP Required Gallery",
            false,
            "pending",
            None,
            None,
        )
        .await;

        // archiver page reports 8,800 GP for original, 218 GP for resample.
        // Default config uses subscription_resolution = "1280x" (resample), so
        // the parser picks the resample form -> DownloadCost::Gp(218).
        mock_eh_archiver_page_with_cost(&eh_server, 2284788, "7841d194d4", "8,800 GP", "218 GP")
            .await;

        // The POST to archiver.php must NEVER happen - if it did, it would
        // spend GP. We mount a matcher with expect(0) so any POST fails the test.
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string("should not be called"))
            .expect(0)
            .mount(&eh_server)
            .await;

        let mut config = make_config();
        config.background_download_enabled = false;
        // max_archive_gp_cost defaults to 0, so positive GP costs are rejected.
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
            None,
        );

        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(updated.status, STATUS_FAILED);
        assert_eq!(
            updated.error.as_deref(),
            Some("EH archive GP cost 218 exceeds configured max_archive_gp_cost=0")
        );
        assert!(updated.completed_at.is_some());
        assert!(updated.started_at.is_some());
        assert!(updated.next_retry_at.is_none());
        assert_eq!(
            updated.retry_count, 0,
            "policy reject must not consume retries"
        );
        let delivery = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivery.status, STATUS_FAILED);
        assert!(delivery.error.is_none());
        assert!(gp_attempts(repo.as_ref()).await.is_empty());
    }

    #[tokio::test]
    async fn test_download_worker_free_cost_proceeds_with_post() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            4053260,
            "53ad37062b",
            "Free Gallery",
            false,
            "pending",
            None,
            None,
        )
        .await;

        // archiver page reports Free! for both forms. Default config uses
        // subscription_resolution = "1280x" (resample) -> DownloadCost::Free.
        mock_eh_archiver_page_with_cost(&eh_server, 4053260, "53ad37062b", "Free!", "Free!").await;

        let download_url = format!("{}/archive/4053260/token/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;
        let zip_temp = tempfile::tempdir().unwrap();
        let zip_path = zip_temp.path().join("test.zip");
        create_test_zip(&zip_path, 2);
        let zip_bytes = std::fs::read(&zip_path).unwrap();
        mock_eh_archive_download(&eh_server, "/archive/4053260/token/0", zip_bytes).await;

        let mut config = make_config();
        config.background_download_enabled = false;
        // max_archive_gp_cost defaults to 0; Free cost still passes.
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
            None,
        );

        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            updated.status, STATUS_DOWNLOADED,
            "Free download must proceed"
        );
        assert_eq!(updated.gp_cost, 0, "free download must record gp_cost = 0");
    }

    #[tokio::test]
    async fn test_download_worker_gp_cost_within_limit_proceeds() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            2284788,
            "7841d194d4",
            "GP Required Gallery",
            false,
            "pending",
            None,
            None,
        )
        .await;

        // resample costs 218 GP; we set max_archive_gp_cost = 500 so 218 is allowed.
        mock_eh_archiver_page_with_cost(&eh_server, 2284788, "7841d194d4", "8,800 GP", "218 GP")
            .await;

        let download_url = format!("{}/archive/2284788/token/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;
        let zip_temp = tempfile::tempdir().unwrap();
        let zip_path = zip_temp.path().join("test.zip");
        create_test_zip(&zip_path, 2);
        let zip_bytes = std::fs::read(&zip_path).unwrap();
        mock_eh_archive_download(&eh_server, "/archive/2284788/token/0", zip_bytes).await;

        let mut config = make_config();
        config.background_download_enabled = false;
        config.max_archive_gp_cost = 500;
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
            None,
        );

        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            updated.status, STATUS_DOWNLOADED,
            "GP-cost within limit must proceed"
        );
        assert_eq!(updated.gp_cost, 218, "gp_cost must be recorded as 218");
    }

    #[tokio::test]
    async fn test_download_worker_gp_attempt_survives_malformed_archive_redirect() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            2284788,
            "7841d194d4",
            "Paid malformed redirect gallery",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        mock_eh_archiver_page_with_cost(&eh_server, 2284788, "7841d194d4", "8,800 GP", "218 GP")
            .await;
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>no redirect</html>"))
            .expect(1)
            .mount(&eh_server)
            .await;

        let mut config = make_config();
        config.background_download_enabled = false;
        config.max_archive_gp_cost = 218;
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
            None,
        );

        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(updated.status, STATUS_PENDING);
        assert_eq!(updated.retry_count, 1);
        let attempts = gp_attempts(repo.as_ref()).await;
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].job_id, entry.job_id);
        assert_eq!(attempts[0].queue_id, None);
        assert_eq!(attempts[0].gp_cost, 218);
        let archiver_posts = eh_server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| {
                request.method.as_str() == "POST" && request.url.path() == "/archiver.php"
            })
            .count();
        assert_eq!(archiver_posts, 1);
    }

    #[tokio::test]
    async fn test_download_worker_gp_attempt_insert_failure_retries_without_post() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            2284788,
            "7841d194d4",
            "Paid trigger failure gallery",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        mock_eh_archiver_page_with_cost(&eh_server, 2284788, "7841d194d4", "8,800 GP", "218 GP")
            .await;
        repo.db()
            .execute(Statement::from_string(
                DbBackend::Sqlite,
                "CREATE TRIGGER fail_eh_gp_spend_attempt_insert BEFORE INSERT ON eh_gp_spend_attempts BEGIN SELECT RAISE(FAIL, 'ledger insert blocked'); END;".to_owned(),
            ))
            .await
            .unwrap();
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string("must not be called"))
            .expect(0)
            .mount(&eh_server)
            .await;

        let mut config = make_config();
        config.background_download_enabled = false;
        config.max_archive_gp_cost = 218;
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
            None,
        );

        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(updated.status, STATUS_PENDING);
        assert_eq!(updated.retry_count, 1);
        assert!(gp_attempts(repo.as_ref()).await.is_empty());
    }

    #[tokio::test]
    async fn test_download_worker_gp_rate_limit_defers_without_post() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;

        // Pre-fill the append-only ledger with 1000 GP in the current window.
        // Queue `gp_cost` metadata must not affect the rate-limit calculation.
        let prior_entry = insert_queue_entry(
            &repo,
            -100,
            999999,
            "a1b2c3d4",
            "Previous paid gallery",
            false,
            STATUS_DONE,
            None,
            None,
        )
        .await;
        repo.append_eh_gp_spend_attempt(prior_entry.id, prior_entry.gid, 1000)
            .await
            .unwrap();

        let entry = insert_queue_entry(
            &repo,
            -100,
            2284788,
            "7841d194d4",
            "GP Required Gallery",
            false,
            "pending",
            None,
            None,
        )
        .await;

        // New download costs 218 GP. With gp_rate_limit = 1000 and 1000 already
        // spent, the new download would push total to 1218 > 1000, so it must defer.
        mock_eh_archiver_page_with_cost(&eh_server, 2284788, "7841d194d4", "8,800 GP", "218 GP")
            .await;

        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string("should not be called"))
            .expect(0)
            .mount(&eh_server)
            .await;

        let mut config = make_config();
        config.background_download_enabled = false;
        config.max_archive_gp_cost = 500; // per-archive allows 218
        config.gp_rate_limit = 1000; // but window budget is exhausted
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
            None,
        );

        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            updated.status, STATUS_PENDING,
            "GP rate limit must defer without POSTing"
        );
    }

    #[tokio::test]
    async fn test_check_and_reserve_archive_cost_allows_one_concurrent_gp_attempt() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let first_entry = insert_queue_entry(
            &repo,
            -100,
            1001,
            "a1b2c3d4",
            "First paid gallery",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        let second_entry = insert_queue_entry(
            &repo,
            -100,
            1002,
            "e5f6a7b8",
            "Second paid gallery",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        let mut config = make_config();
        config.max_archive_gp_cost = 218;
        config.gp_rate_limit = 218;

        let (first, second) = tokio::join!(
            check_and_reserve_archive_cost(
                repo.as_ref(),
                &config,
                None,
                Some(first_entry.id),
                first_entry.gid,
                &DownloadCost::Gp(218),
            ),
            check_and_reserve_archive_cost(
                repo.as_ref(),
                &config,
                None,
                Some(second_entry.id),
                second_entry.gid,
                &DownloadCost::Gp(218),
            ),
        );

        let mut proceeds = 0;
        let mut defers = 0;
        for outcome in [first.unwrap(), second.unwrap()] {
            match outcome {
                ArchiveCostCheck::Proceed => proceeds += 1,
                ArchiveCostCheck::Defer { .. } => defers += 1,
                ArchiveCostCheck::Reject { .. } => {
                    panic!("allowed GP cost must not be rejected")
                }
            }
        }
        assert_eq!(proceeds, 1, "exactly one concurrent attempt may reserve GP");
        assert_eq!(defers, 1, "the other concurrent attempt must defer");

        let attempts = gp_attempts(repo.as_ref()).await;
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].gp_cost, 218);
        assert_eq!(repo.get_eh_gp_cost_in_window(24).await.unwrap(), 218);
    }

    #[tokio::test]
    async fn test_check_and_reserve_gp_attempt_when_rate_limit_disabled() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let entry = insert_queue_entry(
            &repo,
            -100,
            1001,
            "a1b2c3d4",
            "Paid gallery",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        let mut config = make_config();
        config.max_archive_gp_cost = 218;
        config.gp_rate_limit = 0;

        for _ in 0..2 {
            let outcome = check_and_reserve_archive_cost(
                repo.as_ref(),
                &config,
                None,
                Some(entry.id),
                entry.gid,
                &DownloadCost::Gp(218),
            )
            .await
            .unwrap();
            assert!(matches!(outcome, ArchiveCostCheck::Proceed));
        }

        let attempts = gp_attempts(repo.as_ref()).await;
        assert_eq!(attempts.len(), 2);
        assert_eq!(
            attempts.iter().map(|attempt| attempt.gp_cost).sum::<i64>(),
            436
        );
    }

    #[tokio::test]
    async fn test_check_and_reserve_rejects_extreme_gp_window_without_ledger() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let entry = insert_queue_entry(
            &repo,
            -100,
            1001,
            "a1b2c3d4",
            "Paid gallery",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        let mut config = make_config();
        config.max_archive_gp_cost = 218;
        config.gp_rate_limit = 218;
        config.gp_rate_window_hours = u64::MAX;

        let result = check_and_reserve_archive_cost(
            repo.as_ref(),
            &config,
            None,
            Some(entry.id),
            entry.gid,
            &DownloadCost::Gp(218),
        )
        .await;

        assert!(result.is_err());
        assert!(
            gp_attempts(repo.as_ref()).await.is_empty(),
            "window validation must fail before a ledger reservation or archive POST"
        );
    }

    #[tokio::test]
    async fn test_check_and_reserve_keeps_24_hour_gp_defer_delay() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let entry = insert_queue_entry(
            &repo,
            -100,
            1001,
            "a1b2c3d4",
            "Paid gallery",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        repo.append_eh_gp_spend_attempt(entry.id, entry.gid, 218)
            .await
            .unwrap();
        let mut config = make_config();
        config.max_archive_gp_cost = 218;
        config.gp_rate_limit = 218;
        config.gp_rate_window_hours = 24;

        let outcome = check_and_reserve_archive_cost(
            repo.as_ref(),
            &config,
            None,
            Some(entry.id),
            entry.gid,
            &DownloadCost::Gp(218),
        )
        .await
        .unwrap();

        match outcome {
            ArchiveCostCheck::Defer { delay_secs, .. } => {
                assert_eq!(delay_secs, 24 * 3600 / 4);
            }
            ArchiveCostCheck::Proceed => panic!("exhausted 24-hour GP budget must defer"),
            ArchiveCostCheck::Reject { .. } => {
                panic!("within-limit GP cost must not be rejected")
            }
        }
        assert_eq!(gp_attempts(repo.as_ref()).await.len(), 1);
    }

    #[tokio::test]
    async fn test_check_and_reserve_non_positive_or_free_costs_skip_ledger() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let entry = insert_queue_entry(
            &repo,
            -100,
            1001,
            "a1b2c3d4",
            "Free gallery",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        let mut config = make_config();
        config.max_archive_gp_cost = 218;
        config.gp_rate_limit = 218;

        for cost in [
            DownloadCost::Free,
            DownloadCost::Unlocked,
            DownloadCost::Gp(0),
        ] {
            let outcome = check_and_reserve_archive_cost(
                repo.as_ref(),
                &config,
                None,
                Some(entry.id),
                entry.gid,
                &cost,
            )
            .await
            .unwrap();
            assert!(matches!(outcome, ArchiveCostCheck::Proceed));
        }

        assert!(gp_attempts(repo.as_ref()).await.is_empty());
    }

    #[tokio::test]
    async fn test_check_and_reserve_archive_cost_policy_classification_without_ledger() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let entry = insert_queue_entry(
            &repo,
            -100,
            1001,
            "a1b2c3d4",
            "Rejected gallery",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        let mut config = make_config();
        config.max_archive_gp_cost = 218;
        config.download_rate_limit_gb = 0;

        let outcome = check_and_reserve_archive_cost(
            repo.as_ref(),
            &config,
            None,
            Some(entry.id),
            entry.gid,
            &DownloadCost::Gp(219),
        )
        .await
        .unwrap();
        assert!(matches!(
            outcome,
            ArchiveCostCheck::Reject { reason }
                if reason == "EH archive GP cost 219 exceeds configured max_archive_gp_cost=218"
        ));

        config.max_archive_gp_cost = 0;
        let outcome = check_and_reserve_archive_cost(
            repo.as_ref(),
            &config,
            None,
            Some(entry.id),
            entry.gid,
            &DownloadCost::Gp(1),
        )
        .await
        .unwrap();
        assert!(matches!(
            outcome,
            ArchiveCostCheck::Reject { reason }
                if reason == "EH archive GP cost 1 exceeds configured max_archive_gp_cost=0"
        ));

        config.max_archive_gp_cost = 218;
        config.download_rate_limit_gb = 7;
        for cost in [
            DownloadCost::Unknown,
            DownloadCost::Unavailable,
            DownloadCost::Insufficient,
        ] {
            let outcome = check_and_reserve_archive_cost(
                repo.as_ref(),
                &config,
                None,
                Some(entry.id),
                entry.gid,
                &cost,
            )
            .await
            .unwrap();
            assert!(matches!(outcome, ArchiveCostCheck::Defer { .. }));
        }

        config.download_rate_limit_gb = 0;
        let outcome = check_and_reserve_archive_cost(
            repo.as_ref(),
            &config,
            None,
            Some(entry.id),
            entry.gid,
            &DownloadCost::Free,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, ArchiveCostCheck::Defer { .. }));

        assert!(gp_attempts(repo.as_ref()).await.is_empty());
    }

    #[test]
    fn test_config_allows_archive_gp_cost() {
        let mut cfg = EhentaiConfig::default();
        // default max_archive_gp_cost = 0
        assert!(cfg.allows_archive_gp_cost(&DownloadCost::Free));
        assert!(cfg.allows_archive_gp_cost(&DownloadCost::Unlocked));
        assert!(!cfg.allows_archive_gp_cost(&DownloadCost::Gp(1)));
        assert!(cfg.allows_archive_gp_cost(&DownloadCost::Gp(0)));
        assert!(!cfg.allows_archive_gp_cost(&DownloadCost::Insufficient));
        assert!(!cfg.allows_archive_gp_cost(&DownloadCost::Unavailable));
        assert!(!cfg.allows_archive_gp_cost(&DownloadCost::Unknown));

        cfg.max_archive_gp_cost = 500;
        assert!(cfg.allows_archive_gp_cost(&DownloadCost::Gp(0)));
        assert!(cfg.allows_archive_gp_cost(&DownloadCost::Gp(500)));
        assert!(!cfg.allows_archive_gp_cost(&DownloadCost::Gp(501)));
        // Free / Unlocked always pass regardless of limit
        assert!(cfg.allows_archive_gp_cost(&DownloadCost::Free));
        assert!(cfg.allows_archive_gp_cost(&DownloadCost::Unlocked));
        // Insufficient / Unavailable / Unknown always reject
        assert!(!cfg.allows_archive_gp_cost(&DownloadCost::Insufficient));
        assert!(!cfg.allows_archive_gp_cost(&DownloadCost::Unavailable));
        assert!(!cfg.allows_archive_gp_cost(&DownloadCost::Unknown));
    }

    #[test]
    fn test_config_gp_rate_window_hours_clamped() {
        let mut cfg = EhentaiConfig::default();
        assert_eq!(cfg.gp_rate_window_hours_clamped(), 24);
        cfg.gp_rate_window_hours = 0;
        assert_eq!(
            cfg.gp_rate_window_hours_clamped(),
            1,
            "zero must clamp to 1"
        );
    }

    #[test]
    fn test_gp_rate_defer_delay_saturates_extreme_windows() {
        assert_eq!(gp_rate_defer_delay_secs(24), 24 * 3600 / 4);
        assert_eq!(gp_rate_defer_delay_secs(i64::MAX as u64), i64::MAX);
        assert_eq!(gp_rate_defer_delay_secs(u64::MAX), i64::MAX);
    }

    /// Verify the background worker's GP guard: when the archiver page reports
    /// a GP cost that exceeds `max_archive_gp_cost`, the background worker must
    /// fail permanently without POSTing, reserving ledger GP, or consuming retries.
    #[tokio::test]
    async fn test_background_worker_gp_cost_exceeds_policy_permanently_fails_without_retry_increment(
    ) {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            2284788,
            "7841d194d4",
            "BG GP Gallery",
            false,
            "pending",
            None,
            None,
        )
        .await;

        handoff_job_to_background(&repo, &entry).await;

        mock_eh_archiver_page_with_cost(&eh_server, 2284788, "7841d194d4", "8,800 GP", "218 GP")
            .await;

        // POST to archiver.php must never happen.
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string("should not be called"))
            .expect(0)
            .mount(&eh_server)
            .await;

        let mut config = make_config();
        config.background_download_enabled = true;
        config.background_download_concurrency = 1;
        // max_archive_gp_cost defaults to 0, so 218 GP must be rejected.
        let worker = EhBackgroundDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
        );

        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(updated.status, STATUS_FAILED);
        assert_eq!(
            updated.error.as_deref(),
            Some("EH archive GP cost 218 exceeds configured max_archive_gp_cost=0")
        );
        assert!(updated.completed_at.is_some());
        assert!(updated.started_at.is_some());
        assert!(updated.next_retry_at.is_none());
        assert_eq!(updated.retry_count, 0);
        assert!(updated.background_download_status.is_none());
        assert!(updated.background_download_started_at.is_none());
        assert!(updated.background_download_next_retry_at.is_none());
        assert!(updated.background_download_error.is_none());
        assert_eq!(updated.background_download_attempt_count, 0);
        assert!(gp_attempts(repo.as_ref()).await.is_empty());
    }

    #[tokio::test]
    async fn test_background_download_worker_threads_archive_download_concurrency() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        setup_chat(&repo, -100, true).await;

        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Concurrent background archive",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        handoff_job_to_background(&repo, &entry).await;

        mock_eh_gallery_page(&eh_server, 123456, "abcdef0123").await;
        let download_url = format!("{}/archive/123456/token/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;
        let zip_temp = tempfile::tempdir().unwrap();
        let zip_path = zip_temp.path().join("concurrent-background.zip");
        create_test_zip(&zip_path, 1);
        let zip_bytes = std::fs::read(&zip_path).unwrap();
        let content_range = format!("bytes 0-{}/{}", zip_bytes.len() - 1, zip_bytes.len());
        Mock::given(method("GET"))
            .and(path("/archive/123456/token/0"))
            .and(header("range", "bytes=0-"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", content_range)
                    .set_body_bytes(zip_bytes),
            )
            .expect(1)
            .mount(&eh_server)
            .await;

        let mut config = make_config();
        config.archive_download_concurrency = 2;
        let worker = EhBackgroundDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
        );

        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(updated.status, STATUS_DOWNLOADED);
        assert_eq!(updated.background_download_status, None);
    }

    #[tokio::test]
    async fn test_background_worker_permanent_failure_cleans_archive_artifact_family() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        setup_chat(&repo, -100, true).await;

        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Background permanent failure",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        handoff_job_to_background(&repo, &entry).await;

        let job = job_for_delivery(&repo, &entry).await;
        let zip_path = archive_artifacts_for_job(temp.path(), &job)
            .final_zip()
            .to_path_buf();
        std::fs::create_dir_all(zip_path.parent().unwrap()).unwrap();
        let part_path = zip_path.with_extension("zip.part");
        let parts_dir = zip_path.with_extension("zip.parts");
        std::fs::write(&zip_path, b"PK\x03\x04stale").unwrap();
        std::fs::write(&part_path, b"PK\x03\x04partial").unwrap();
        std::fs::create_dir_all(parts_dir.join("nested")).unwrap();
        std::fs::write(parts_dir.join("manifest.json"), b"manifest").unwrap();
        std::fs::write(parts_dir.join("nested").join("part-0001"), b"part").unwrap();

        mock_eh_gallery_page(&eh_server, 123456, "abcdef0123").await;
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&eh_server)
            .await;

        let mut config = make_config();
        config.background_download_max_attempts = 1;
        let worker = EhBackgroundDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
        );

        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(
            updated.status,
            crate::db::repo::eh_gallery_jobs::JOB_STATUS_RETIRED
        );
        assert_eq!(updated.background_download_status, None);
        assert!(
            zip_path.exists(),
            "cleanup ownership is durable before local removal"
        );
        assert_eq!(
            run_eh_job_cleanup_maintenance_once(repo.as_ref(), None, 0, true)
                .await
                .unwrap(),
            Some(EhCleanupFinalizeOutcome::CleanRetired)
        );
        assert!(!zip_path.exists(), "final ZIP should be cleaned");
        assert!(!part_path.exists(), "partial ZIP should be cleaned");
        assert!(
            !parts_dir.exists(),
            "multipart parts directory should be removed recursively"
        );
    }

    #[tokio::test]
    async fn test_background_worker_selected_size_limit_runs_after_prepare_without_metadata() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            2284789,
            "7841d194d4",
            "Oversized background archive",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        handoff_job_to_background(&repo, &entry).await;
        mock_eh_archiver_page_with_estimated_sizes(
            &eh_server,
            2284789,
            "7841d194d4",
            "400.0 MiB",
            "300.01 MiB",
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string("should not be called"))
            .expect(0)
            .mount(&eh_server)
            .await;

        let mut config = make_config();
        config.background_download_enabled = true;
        config.background_download_concurrency = 1;
        config.max_archive_size_mb = 300;
        let worker = EhBackgroundDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
        );

        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(updated.status, STATUS_PENDING);
        assert_eq!(
            updated.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_PENDING)
        );
        assert_eq!(updated.background_download_attempt_count, 1);

        let requests = eh_server.received_requests().await.unwrap();
        assert!(requests.iter().any(|request| {
            request.method.as_str() == "GET" && request.url.path() == "/g/2284789/7841d194d4/"
        }));
        assert!(requests.iter().any(|request| {
            request.method.as_str() == "GET" && request.url.path() == "/archiver.php"
        }));
        assert!(
            !requests.iter().any(
                |request| request.method.as_str() == "POST" && request.url.path() == "/api.php"
            ),
            "background selected archive-size checks must not request gallery metadata"
        );
    }

    #[tokio::test]
    async fn test_background_worker_oversize_over_gp_policy_rejects_before_size_retry() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp_dir = tempfile::tempdir().unwrap();
        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            2290,
            "7841d194d4",
            "Oversized paid background archive",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        handoff_job_to_background(&repo, &entry).await;
        mock_eh_archiver_page_with_cost_and_estimated_sizes(
            &eh_server,
            2290,
            "7841d194d4",
            "218 GP",
            "218 GP",
            "400.0 MiB",
            "300.01 MiB",
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string("unexpected"))
            .expect(0)
            .mount(&eh_server)
            .await;

        let mut cfg = make_config();
        cfg.background_download_enabled = true;
        cfg.background_download_concurrency = 1;
        cfg.max_archive_size_mb = 300;
        cfg.max_archive_gp_cost = 0;
        let worker = EhBackgroundDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(cfg),
            temp_dir.path().to_path_buf(),
        );

        worker.tick().await.unwrap();

        let model = job_for_delivery(&repo, &entry).await;
        assert_eq!(model.status, STATUS_FAILED);
        assert_eq!(
            model.error.as_deref(),
            Some("EH archive GP cost 218 exceeds configured max_archive_gp_cost=0")
        );
        assert!(model.completed_at.is_some());
        assert_eq!(model.retry_count, 0);
        assert!(model.background_download_status.is_none());
        assert!(model.background_download_started_at.is_none());
        assert!(model.background_download_next_retry_at.is_none());
        assert!(model.background_download_error.is_none());
        assert_eq!(model.background_download_attempt_count, 0);
        assert!(gp_attempts(repo.as_ref()).await.is_empty());
    }

    #[tokio::test]
    async fn test_background_worker_gp_attempt_survives_malformed_archive_redirect() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            2284788,
            "7841d194d4",
            "Background paid malformed redirect gallery",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        handoff_job_to_background(&repo, &entry).await;
        mock_eh_archiver_page_with_cost(&eh_server, 2284788, "7841d194d4", "8,800 GP", "218 GP")
            .await;
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>no redirect</html>"))
            .expect(1)
            .mount(&eh_server)
            .await;

        let mut config = make_config();
        config.background_download_enabled = true;
        config.background_download_concurrency = 1;
        config.max_archive_gp_cost = 218;
        let worker = EhBackgroundDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
        );

        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(updated.status, STATUS_PENDING);
        assert_eq!(
            updated.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_PENDING)
        );
        assert_eq!(updated.background_download_attempt_count, 1);
        let attempts = gp_attempts(repo.as_ref()).await;
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].gp_cost, 218);
        let archiver_posts = eh_server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| {
                request.method.as_str() == "POST" && request.url.path() == "/archiver.php"
            })
            .count();
        assert_eq!(archiver_posts, 1);
    }

    #[tokio::test]
    async fn two_chats_share_one_background_archive_post_gp_attempt_and_completion() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        setup_chat(&repo, -100, true).await;
        setup_chat(&repo, -200, true).await;
        let variant = EhGalleryVariant::archive("1280x");
        let first = repo
            .enqueue_eh_download(
                -100,
                2284790,
                "7841d194d4",
                "Shared paid background gallery",
                false,
                SOURCE_DIRECT,
                &variant,
            )
            .await
            .unwrap();
        let second = repo
            .enqueue_eh_download(
                -200,
                2284790,
                "7841d194d4",
                "Shared paid background gallery",
                false,
                SOURCE_DIRECT,
                &variant,
            )
            .await
            .unwrap();
        assert_eq!(first.job_id, second.job_id);
        handoff_job_to_background(&repo, &first).await;

        mock_eh_archiver_page_with_cost(&eh_server, 2284790, "7841d194d4", "218 GP", "218 GP")
            .await;
        let download_url = format!("{}/archive/shared/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;
        let zip_temp = tempfile::tempdir().unwrap();
        let fixture_path = zip_temp.path().join("shared-paid.zip");
        create_test_zip(&fixture_path, 2);
        let zip_bytes = std::fs::read(&fixture_path).unwrap();
        mock_eh_archive_download(&eh_server, "/archive/shared/0", zip_bytes.clone()).await;

        let mut config = make_config();
        config.background_download_enabled = true;
        config.background_download_concurrency = 2;
        config.max_archive_size_mb = 0;
        config.max_archive_gp_cost = 218;
        let worker = EhBackgroundDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
        );

        worker.tick().await.unwrap();

        let job = job_for_delivery(&repo, &first).await;
        assert_eq!(job.status, STATUS_DOWNLOADED);
        let attempts = gp_attempts(repo.as_ref()).await;
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].job_id, Some(job.id));
        let completions = eh_download_completions::Entity::find()
            .filter(eh_download_completions::Column::JobId.eq(job.id))
            .all(repo.db())
            .await
            .unwrap();
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].job_id, Some(job.id));
        assert_eq!(
            repo.get_eh_downloaded_bytes_in_window(24).await.unwrap(),
            completions[0].file_size,
            "the shared completion must contribute one byte-window entry"
        );
        assert_eq!(
            completions[0].file_size,
            i64::try_from(zip_bytes.len()).unwrap()
        );
        let archiver_posts = eh_server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| {
                request.method.as_str() == "POST" && request.url.path() == "/archiver.php"
            })
            .count();
        assert_eq!(archiver_posts, 1);
    }

    #[tokio::test]
    async fn test_background_gp_rate_limit_allows_only_one_post() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        setup_chat(&repo, -100, true).await;

        let first = insert_queue_entry(
            &repo,
            -100,
            1001,
            "a1b2c3d4",
            "First paid background gallery",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        let second = insert_queue_entry(
            &repo,
            -100,
            1002,
            "e5f6a7b8",
            "Second paid background gallery",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        for entry in [&first, &second] {
            handoff_job_to_background(&repo, entry).await;
        }

        mock_eh_archiver_page_with_cost(&eh_server, 1001, "a1b2c3d4", "218 GP", "218 GP").await;
        mock_eh_archiver_page_with_cost(&eh_server, 1002, "e5f6a7b8", "218 GP", "218 GP").await;
        let download_url = format!("{}/archive/paid/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;
        let zip_temp = tempfile::tempdir().unwrap();
        let zip_path = zip_temp.path().join("paid.zip");
        create_test_zip(&zip_path, 2);
        mock_eh_archive_download(
            &eh_server,
            "/archive/paid/0",
            std::fs::read(zip_path).unwrap(),
        )
        .await;

        let mut config = make_config();
        config.background_download_enabled = true;
        config.background_download_concurrency = 2;
        config.max_archive_size_mb = 0;
        config.max_archive_gp_cost = 218;
        config.gp_rate_limit = 218;
        let worker = EhBackgroundDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
        );

        worker.tick().await.unwrap();

        let first = job_for_delivery(&repo, &first).await;
        let second = job_for_delivery(&repo, &second).await;
        assert_eq!(
            [first.status.as_str(), second.status.as_str()]
                .into_iter()
                .filter(|status| *status == STATUS_DOWNLOADED)
                .count(),
            1,
            "exactly one background job must download"
        );
        let deferred = [&first, &second]
            .into_iter()
            .find(|job| job.status == STATUS_PENDING)
            .expect("one background job must remain pending");
        assert_eq!(
            deferred.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_PENDING),
            "deferred background entry must remain eligible for a later tick"
        );
        assert_eq!(gp_attempts(repo.as_ref()).await.len(), 1);
        assert_eq!(repo.get_eh_gp_cost_in_window(24).await.unwrap(), 218);

        let archiver_posts = eh_server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| {
                request.method.as_str() == "POST" && request.url.path() == "/archiver.php"
            })
            .count();
        assert_eq!(
            archiver_posts, 1,
            "exactly one paid archive POST is allowed"
        );
    }

    #[tokio::test]
    async fn test_main_and_background_gp_rate_limit_allows_only_one_post() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        setup_chat(&repo, -100, true).await;

        let main_entry = insert_queue_entry(
            &repo,
            -100,
            2001,
            "c1d2e3f4",
            "Paid main gallery",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        let background_entry = insert_queue_entry(
            &repo,
            -100,
            2002,
            "a5b6c7d8",
            "Paid background gallery",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        handoff_job_to_background(&repo, &background_entry).await;

        mock_eh_archiver_page_with_cost(&eh_server, 2001, "c1d2e3f4", "218 GP", "218 GP").await;
        mock_eh_archiver_page_with_cost(&eh_server, 2002, "a5b6c7d8", "218 GP", "218 GP").await;
        let download_url = format!("{}/archive/paid/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;
        let zip_temp = tempfile::tempdir().unwrap();
        let zip_path = zip_temp.path().join("paid.zip");
        create_test_zip(&zip_path, 2);
        mock_eh_archive_download(
            &eh_server,
            "/archive/paid/0",
            std::fs::read(zip_path).unwrap(),
        )
        .await;

        let mut config = make_config();
        config.background_download_enabled = true;
        config.background_download_concurrency = 2;
        config.max_archive_size_mb = 0;
        config.max_archive_gp_cost = 218;
        config.gp_rate_limit = 218;
        let config = Arc::new(config);
        let client = make_eh_client(&eh_server);
        let main_worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            Arc::clone(&client),
            Arc::clone(&config),
            temp.path().to_path_buf(),
            None,
        );
        let background_worker = EhBackgroundDownloadWorker::new(
            Arc::clone(&repo),
            Arc::clone(&client),
            Arc::clone(&config),
            temp.path().to_path_buf(),
        );

        let (main_result, background_result) =
            tokio::join!(main_worker.tick(), background_worker.tick());
        main_result.unwrap();
        background_result.unwrap();

        let main_entry = job_for_delivery(&repo, &main_entry).await;
        let background_entry = job_for_delivery(&repo, &background_entry).await;
        assert_eq!(
            [main_entry.status.as_str(), background_entry.status.as_str()]
                .into_iter()
                .filter(|status| *status == STATUS_DOWNLOADED)
                .count(),
            1,
            "the process-wide lock must allow only one worker to spend the GP budget"
        );
        if main_entry.status == STATUS_PENDING {
            assert!(
                main_entry.next_retry_at.is_some(),
                "deferred main entry must remain processable"
            );
        } else {
            assert_eq!(background_entry.status, STATUS_PENDING);
            assert_eq!(
                background_entry.background_download_status.as_deref(),
                Some(BACKGROUND_STATUS_PENDING),
                "deferred background entry must remain processable"
            );
            assert!(background_entry.background_download_next_retry_at.is_some());
        }
        assert_eq!(gp_attempts(repo.as_ref()).await.len(), 1);
        assert_eq!(repo.get_eh_gp_cost_in_window(24).await.unwrap(), 218);

        let archiver_posts = eh_server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| {
                request.method.as_str() == "POST" && request.url.path() == "/archiver.php"
            })
            .count();
        assert_eq!(
            archiver_posts, 1,
            "exactly one paid archive POST is allowed"
        );
    }

    /// Verify the conservative "Unknown cost => defer" rule: when the archiver
    /// page contains an archiver_key but no recognizable Download Cost text,
    /// the download must defer rather than be treated as Unlocked.
    #[tokio::test]
    async fn test_download_worker_unknown_cost_defers_without_post() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Unknown Cost Gallery",
            false,
            "pending",
            None,
            None,
        )
        .await;

        // archiver page with an archiver_key in a hidden input but NO Download
        // Cost text. This is the "simplified page" case where the parser cannot
        // determine the cost -> must return Unknown -> must defer.
        let gallery_html = r#"<html><body>
            <a onclick="return popUp('/archiver.php?gid=123456&amp;token=abcdef0123',480,320)">Archive Download</a>
            </body></html>"#;
        Mock::given(method("GET"))
            .and(path("/g/123456/abcdef0123/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(gallery_html))
            .mount(&eh_server)
            .await;
        let archiver_page_html = r#"<html><body><input type="hidden" name="or" value="123456--abc123def456" /></body></html>"#;
        Mock::given(method("GET"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string(archiver_page_html))
            .mount(&eh_server)
            .await;

        // POST must never happen.
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string("should not be called"))
            .expect(0)
            .mount(&eh_server)
            .await;

        let mut config = make_config();
        config.background_download_enabled = false;
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
            None,
        );

        worker.tick().await.unwrap();

        let updated = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            updated.status, STATUS_PENDING,
            "Unknown cost must defer without POSTing"
        );
    }

    /// Verify the parser picks the original-archive cost when resolution is
    /// "original" - the GP-required sample's original form says 8,800 GP, so
    /// with default config (max_archive_gp_cost = 0) it must be rejected.
    #[tokio::test]
    async fn test_download_worker_original_resolution_gp_cost_rejects() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            2284788,
            "7841d194d4",
            "GP Original Gallery",
            false,
            "pending",
            None,
            None,
        )
        .await;

        mock_eh_archiver_page_with_cost(&eh_server, 2284788, "7841d194d4", "8,800 GP", "218 GP")
            .await;

        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string("should not be called"))
            .expect(0)
            .mount(&eh_server)
            .await;

        let mut config = make_config();
        config.background_download_enabled = false;
        config.download_resolution = "original".to_string();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(eh_gallery_jobs::Column::Resolution, Expr::value("original"))
            .filter(eh_gallery_jobs::Column::Id.eq(entry.job_id.unwrap()))
            .exec(repo.db())
            .await
            .unwrap();
        // max_archive_gp_cost defaults to 0 -> 8,800 GP must be rejected.
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
            None,
        );

        worker.tick().await.unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(updated.status, STATUS_FAILED);
        assert_eq!(
            updated.error.as_deref(),
            Some("EH archive GP cost 8800 exceeds configured max_archive_gp_cost=0")
        );
        assert!(updated.completed_at.is_some());
        assert!(updated.started_at.is_some());
        assert!(updated.next_retry_at.is_none());
        assert_eq!(updated.retry_count, 0);
        assert!(gp_attempts(repo.as_ref()).await.is_empty());
    }
}
