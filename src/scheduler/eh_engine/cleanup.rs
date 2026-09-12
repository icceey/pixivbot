use crate::db::{
    entities::eh_gallery_jobs,
    repo::{eh_gallery_jobs::EhCleanupFinalizeOutcome, Repo},
};
use anyhow::{Context, Result};
use eh_client::{ArchiveArtifacts, ImageUploader};
use tracing::warn;

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
pub(super) async fn ensure_job_upload_state_aborted(
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

pub(super) struct UploadStateAbortPermit;

pub(super) async fn remove_job_upload_state(
    job: &eh_gallery_jobs::Model,
    _permit: UploadStateAbortPermit,
) {
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
pub(super) async fn run_eh_job_cleanup_maintenance_once(
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
