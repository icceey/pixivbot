use super::cleanup::run_eh_job_cleanup_maintenance_once;
use crate::config::EhentaiConfig;
use crate::db::repo::eh_download_queue::{SOURCE_DIRECT, SOURCE_SUBSCRIPTION};
use crate::db::repo::eh_gallery_jobs::{
    eh_gallery_job_artifact_path, DOWNLOAD_MODE_ARCHIVE, DOWNLOAD_MODE_IMAGES,
    DOWNLOAD_MODE_LEGACY, JOB_STATUS_DOWNLOADING,
};
use crate::db::{entities::eh_gallery_jobs, repo::Repo};
use crate::scheduler::helpers::get_chat_if_should_notify;
use anyhow::{Context, Result};
use eh_client::{
    parser::DownloadCost, ArchiveArtifacts, ArchiveDownloadOptions, EhClient, ImageUploader,
};
use sea_orm::prelude::DateTime;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

const SLOW_DOWNLOAD_BYTES_PER_SEC: u64 = 1024 * 1024;
static EH_GP_BUDGET_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

pub(super) fn archive_artifacts_for_job(
    cache_dir: &std::path::Path,
    job: &eh_gallery_jobs::Model,
) -> ArchiveArtifacts {
    ArchiveArtifacts::new(eh_gallery_job_artifact_path(
        &cache_dir.join("eh_cache"),
        job,
    ))
}
pub(super) fn gp_rate_defer_delay_secs(window_hours: u64) -> i64 {
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
pub(super) enum ArchiveCostCheck {
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
pub(super) async fn check_and_reserve_archive_cost(
    repo: &Repo,
    config: &EhentaiConfig,
    job_id: i32,
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

    let _budget_lock = if config.gp_rate_limit > 0 {
        let guard = EH_GP_BUDGET_LOCK.lock().await;
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
        Some(guard)
    } else {
        None
    };
    repo.append_eh_job_gp_spend_attempt(job_id, gid, gp_cost)
        .await?;

    Ok(ArchiveCostCheck::Proceed)
}

pub use crate::db::repo::eh_gallery_jobs::EhDownloadQueue;

enum DownloadOutcome {
    Completed { file_size: u64, gp_cost: i64 },
    Deferred { reason: String },
    Rejected { reason: String },
    Stale,
}

#[derive(Clone)]
pub struct EhDownloadWorker {
    repo: Arc<Repo>,
    client: Arc<EhClient>,
    config: Arc<EhentaiConfig>,
    cache_dir: std::path::PathBuf,
    queue: EhDownloadQueue,
    startup_abort_uploader: Option<Arc<dyn ImageUploader>>,
}

impl EhDownloadWorker {
    pub fn new(
        repo: Arc<Repo>,
        client: Arc<EhClient>,
        config: Arc<EhentaiConfig>,
        cache_dir: std::path::PathBuf,
        queue: EhDownloadQueue,
        startup_abort_uploader: Option<Arc<dyn ImageUploader>>,
    ) -> Self {
        Self {
            repo,
            client,
            config,
            cache_dir,
            queue,
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
                match self.queue {
                    EhDownloadQueue::Main => error!("EhDownloadWorker tick error: {:#}", e),
                    EhDownloadQueue::Background => {
                        error!("EhBackgroundDownloadWorker tick error: {:#}", e)
                    }
                }
            }
        }
    }

    pub(super) async fn tick(&self) -> Result<()> {
        if self.queue == EhDownloadQueue::Main {
            if let Err(error) = run_eh_job_cleanup_maintenance_once(
                self.repo.as_ref(),
                self.startup_abort_uploader.as_deref(),
                self.config.download_poll_interval_sec as i64,
                self.config.send_archive,
            )
            .await
            {
                error!("Shared EH cleanup maintenance failed; continuing normal download selection: {:#}", error);
            }
        }
        let window_hours = i64::try_from(self.config.download_rate_window_hours)
            .context("EH download rate window hours exceed the supported range")?;
        let downloaded_bytes = self
            .repo
            .get_eh_downloaded_bytes_in_window(window_hours)
            .await?;
        if downloaded_bytes >= self.config.download_rate_limit_bytes() as i64 {
            match self.queue {
                EhDownloadQueue::Main => info!("EH download rate limit reached, skipping this tick"),
                EhDownloadQueue::Background => info!(
                    "EH background download byte rate limit reached ({} bytes in last {}h), skipping this tick",
                    downloaded_bytes, self.config.download_rate_window_hours),
            }
            return Ok(());
        }

        if self.queue == EhDownloadQueue::Main {
            if let Some(job) = self
                .repo
                .claim_eh_download_job(self.queue, self.config.send_archive)
                .await?
            {
                self.process(&job).await?;
            }
            return Ok(());
        }
        let mut tasks = JoinSet::new();
        for _ in 0..self.config.background_download_concurrency.max(1) {
            let Some(job) = self
                .repo
                .claim_eh_download_job(self.queue, self.config.send_archive)
                .await?
            else {
                break;
            };
            let worker = self.clone();
            tasks.spawn(async move { worker.process(&job).await });
        }
        drain_background_download_tasks(&mut tasks).await
    }

    async fn process(&self, job: &eh_gallery_jobs::Model) -> Result<()> {
        let Err(error) = self.process_generation(job).await else {
            return Ok(());
        };
        if self.queue == EhDownloadQueue::Main {
            return self.handle_main_failure(job, error).await;
        }
        // Preflight/settlement errors release a background claim without spending
        // an attempt. Source failures are retried inside process_generation instead.
        if let Some(expected_started_at) = job.started_at {
            let reason = format!("EH background claim failed before completion: {error:#}");
            match self
                .repo
                .defer_eh_job_background_download(
                    job.id,
                    expected_started_at,
                    self.config.download_poll_interval_sec.max(1) as i64,
                    &reason,
                )
                .await
            {
                Ok(true) => warn!(
                    "Released failed EH background claim gid={} back to the background queue",
                    job.gid
                ),
                Ok(false) => debug!(
                    "Skipped releasing stale failed EH background claim gid={}",
                    job.gid
                ),
                Err(defer_error) => error!(
                    "Failed to release EH background claim gid={} after error: {:#}",
                    job.gid, defer_error
                ),
            }
        }
        Err(error)
    }

    async fn process_generation(&self, job: &eh_gallery_jobs::Model) -> Result<()> {
        let background = self.queue == EhDownloadQueue::Background;
        let label = if background { "background" } else { "gallery" };
        let gid = if background {
            job.gid.to_string()
        } else {
            (job.gid as u64).to_string()
        };
        let expected_started_at = job.started_at.context(match self.queue {
            EhDownloadQueue::Main => {
                "Cannot process shared EH gallery job: missing download claim started_at"
            }
            EhDownloadQueue::Background => {
                "Cannot process shared EH gallery background job: missing download claim started_at"
            }
        })?;
        if !self.repo.eh_job_has_active_deliveries(job.id).await? {
            self.repo
                .retire_eh_job_without_active_deliveries(job)
                .await?;
            info!("Retired consumerless shared EH {} job {}", label, job.id);
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
                background,
            )
            .await?
        {
            info!(
                "Skipping stale shared EH {} job {} before touching its archive family",
                label, job.id
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
            info!("Retired canceled shared EH {} job {}", label, job.id);
            return Ok(());
        }
        if !has_notifiable_delivery {
            let reason = "no active destination is notifiable";
            info!(
                "Deferring shared EH {} gid={} because {}",
                label, gid, reason
            );
            match self.queue {
                EhDownloadQueue::Main => {
                    self.repo
                        .defer_eh_job_download(
                            job.id,
                            self.config.download_poll_interval_sec as i64,
                        )
                        .await?
                }
                EhDownloadQueue::Background => {
                    if !self
                        .repo
                        .defer_eh_job_background_download(
                            job.id,
                            expected_started_at,
                            self.config.download_poll_interval_sec as i64,
                            reason,
                        )
                        .await?
                    {
                        info!("Skipping stale shared EH background job {} after notification preflight", job.id);
                    }
                }
            }
            return Ok(());
        }

        // Keep the source-attempt boundary: mkdir, mode resolution, transfer and
        // quota deferral failures consume a background attempt; settlement does not.
        match self.download(job, expected_started_at, &zip_path).await {
            Ok(DownloadOutcome::Completed { file_size, gp_cost }) => {
                if !background {
                    info!(
                        "Downloaded eh gallery gid={} size={} bytes gp_cost={}",
                        job.gid as u64, file_size, gp_cost
                    );
                }
                self.repo
                    .mark_eh_job_downloaded(
                        self.queue,
                        job.id,
                        expected_started_at,
                        file_size as i64,
                        &zip_path_str,
                        gp_cost,
                    )
                    .await?;
            }
            Ok(DownloadOutcome::Deferred { reason }) => {
                if background {
                    debug!(
                        "EH background download gid={} deferred without retry increment: {}",
                        job.gid, reason
                    );
                    self.repo
                        .evaluate_eh_job_liveness(job.id, self.config.send_archive)
                        .await?;
                }
            }
            Ok(DownloadOutcome::Stale) => {
                info!(
                    "Skipping stale shared EH background job {} after background defer",
                    job.id
                );
            }
            Ok(DownloadOutcome::Rejected { reason }) => {
                self.repo
                    .fail_eh_job_for_archive_policy(self.queue, job, &reason)
                    .await
                    .map_err(|error| error.context(ArchivePolicyTransitionError))?;
                let label = if background { " background" } else { "" };
                warn!(
                    "Rejecting EH{} download for gid={} due to archive policy: {}",
                    label, gid, reason
                );
            }
            Err(error) if background => {
                let (failed_job, permanent) = self
                    .repo
                    .schedule_eh_job_download_retry(
                        self.queue,
                        job.id,
                        expected_started_at,
                        &error.to_string(),
                        self.config.background_download_max_attempts,
                    )
                    .await?;
                if permanent {
                    warn!(
                        "Permanent background EH download failure for gid={}: {}",
                        job.gid, error
                    );
                }
                self.repo
                    .evaluate_eh_job_liveness(failed_job.id, self.config.send_archive)
                    .await?;
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

    async fn download(
        &self,
        job: &eh_gallery_jobs::Model,
        expected_started_at: DateTime,
        zip_path: &std::path::Path,
    ) -> Result<DownloadOutcome> {
        // The caller has already durably recorded ownership of this artifact family.
        tokio::fs::create_dir_all(self.cache_dir.join("eh_cache")).await?;
        let background = if self.queue == EhDownloadQueue::Background {
            " background"
        } else {
            ""
        };
        let archive_resolution = match job.download_mode.as_str() {
            DOWNLOAD_MODE_ARCHIVE => Some(job.resolution.as_str()),
            DOWNLOAD_MODE_IMAGES => None,
            DOWNLOAD_MODE_LEGACY if self.client.is_logged_in() => match job.resolution.as_str() {
                SOURCE_DIRECT => Some(self.config.download_resolution.as_str()),
                SOURCE_SUBSCRIPTION => Some(self.config.subscription_resolution.as_str()),
                resolution => anyhow::bail!("Cannot resolve legacy shared EH gallery{background} job {} with resolution '{}'", job.id, resolution),
            },
            DOWNLOAD_MODE_LEGACY => None,
            mode => anyhow::bail!("Cannot download shared EH gallery{background} job {} with unsupported mode '{}'", job.id, mode),
        };
        let Some(resolution) = archive_resolution else {
            if self.queue == EhDownloadQueue::Main {
                info!(
                    "Not logged in, using direct image download for gid={}",
                    job.gid as u64
                );
            }
            let file_size = self
                .client
                .download_gallery_images(job.gid as u64, &job.token, zip_path)
                .await
                .context("Failed to download gallery images")?;
            return Ok(DownloadOutcome::Completed {
                file_size,
                gp_cost: 0,
            });
        };
        let options = ArchiveDownloadOptions {
            max_concurrency: self.config.archive_download_concurrency,
        };
        if let Some(file_size) = self
            .client
            .resume_archive_from_persisted_manifest(
                zip_path,
                options,
                self.config.max_archive_size_bytes(),
            )
            .await
            .context("Failed to resume persisted archive download")?
        {
            return Ok(DownloadOutcome::Completed {
                file_size,
                gp_cost: 0,
            });
        }
        let request = self
            .client
            .prepare_archive_download(job.gid as u64, &job.token, resolution)
            .await
            .context("Failed to prepare archive download")?;
        if let Some(reason) = archive_cost_policy_reject_reason(&self.config, request.cost()) {
            return Ok(DownloadOutcome::Rejected { reason });
        }
        ensure_eh_archive_under_size_limit(&self.config, request.estimated_size_bytes())?;
        match check_and_reserve_archive_cost(
            &self.repo,
            &self.config,
            job.id,
            job.gid,
            request.cost(),
        )
        .await?
        {
            ArchiveCostCheck::Proceed => {}
            ArchiveCostCheck::Reject { reason } => return Ok(DownloadOutcome::Rejected { reason }),
            ArchiveCostCheck::Defer { delay_secs, reason } => {
                info!(
                    "Deferring EH{} download for gid={} ({}), no reservation or POST",
                    background, job.gid as u64, reason
                );
                match self.queue {
                    EhDownloadQueue::Main => {
                        self.repo.defer_eh_job_download(job.id, delay_secs).await?
                    }
                    EhDownloadQueue::Background => {
                        if !self
                            .repo
                            .defer_eh_job_background_download(
                                job.id,
                                expected_started_at,
                                delay_secs,
                                &reason,
                            )
                            .await?
                        {
                            return Ok(DownloadOutcome::Stale);
                        }
                    }
                }
                return Ok(DownloadOutcome::Deferred { reason });
            }
        }
        let file_size = self
            .client
            .download_archive_with_request_and_options(&request, zip_path, options)
            .await
            .context("Failed to download archive")?;
        Ok(DownloadOutcome::Completed {
            file_size,
            gp_cost: request.cost().gp_amount().unwrap_or(0) as i64,
        })
    }
    async fn handle_main_failure(
        &self,
        job: &eh_gallery_jobs::Model,
        e: anyhow::Error,
    ) -> Result<()> {
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
                    .defer_eh_job_download(job.id, self.config.download_poll_interval_sec as i64)
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
                    self.queue,
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
            }
            self.repo
                .evaluate_eh_job_liveness(failed_job.id, self.config.send_archive)
                .await?;
        }
        Ok(())
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

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[tokio::test]
    async fn test_drain_background_download_tasks_waits_for_siblings_after_error() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let sibling_completed = Arc::new(AtomicBool::new(false));
        let mut tasks = JoinSet::new();
        let first = tasks.spawn(async { anyhow::bail!("first task failed") });
        let (release, released) = tokio::sync::oneshot::channel();
        let sibling_completed_for_task = Arc::clone(&sibling_completed);
        tasks.spawn(async move {
            released.await.unwrap();
            sibling_completed_for_task.store(true, Ordering::SeqCst);
            Ok(())
        });

        while !first.is_finished() {
            tokio::task::yield_now().await;
        }
        let mut drain = std::pin::pin!(drain_background_download_tasks(&mut tasks));
        std::future::poll_fn(|cx| {
            use std::future::Future;
            assert!(drain.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        release.send(()).unwrap();
        let err = drain
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
