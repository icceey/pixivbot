use crate::error::{Error, Result};
use crate::models::EhCookies;

mod artifacts;
mod assembly;
mod coordinator;
mod http;
mod initialization;
mod manifest;
mod part;
mod policy;
mod sequential;

pub use artifacts::ArchiveArtifacts;
use assembly::assemble_parts;
use coordinator::{MultipartCoordinator, MultipartOutcome};
pub(crate) use http::archive_http_error;
use initialization::{initialize_multipart, MultipartInitialization};
use manifest::{recover_manifest, recover_persisted_download, ManifestRecovery};
use sequential::download_sequential;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveDownloadOptions {
    pub max_concurrency: usize,
}

impl Default for ArchiveDownloadOptions {
    fn default() -> Self {
        Self { max_concurrency: 1 }
    }
}

impl ArchiveDownloadOptions {
    pub(crate) fn validate(self) -> Result<Self> {
        if self.max_concurrency == 0 {
            return Err(Error::Other(
                "archive download max_concurrency must be at least 1".into(),
            ));
        }
        Ok(self)
    }
}

pub(crate) async fn download_to_partial(
    http: &reqwest::Client,
    cookies: &EhCookies,
    download_url: &str,
    artifacts: &ArchiveArtifacts,
    options: ArchiveDownloadOptions,
    resuming_persisted_manifest: bool,
) -> Result<()> {
    let options = options.validate()?;
    let outcome = if tokio::fs::try_exists(artifacts.parts_dir()).await? {
        match recover_manifest(artifacts, download_url).await? {
            ManifestRecovery::Valid(manifest) => {
                artifacts.remove_assembly_scratch().await?;
                MultipartCoordinator::new(
                    http.clone(),
                    cookies.clone(),
                    download_url.to_owned(),
                    artifacts.clone(),
                    manifest,
                    options.max_concurrency,
                    None,
                    resuming_persisted_manifest,
                )
                .await?
                .run()
                .await?
            }
            ManifestRecovery::Invalid(reason) => {
                tracing::warn!(?reason, "discarding invalid archive multipart state");
                artifacts.remove_multipart_state().await?;
                return download_sequential(
                    http,
                    cookies,
                    download_url,
                    artifacts.assembly_scratch(),
                    None,
                )
                .await;
            }
        }
    } else if tokio::fs::try_exists(artifacts.assembly_scratch()).await?
        || options.max_concurrency == 1
    {
        return download_sequential(
            http,
            cookies,
            download_url,
            artifacts.assembly_scratch(),
            None,
        )
        .await;
    } else {
        match initialize_multipart(http, cookies, download_url, artifacts).await? {
            MultipartInitialization::Ready { manifest, seed } => {
                MultipartCoordinator::new(
                    http.clone(),
                    cookies.clone(),
                    download_url.to_owned(),
                    artifacts.clone(),
                    manifest,
                    options.max_concurrency,
                    Some(seed),
                    false,
                )
                .await?
                .run()
                .await?
            }
            MultipartInitialization::SequentialResponse(response) => {
                return download_sequential(
                    http,
                    cookies,
                    download_url,
                    artifacts.assembly_scratch(),
                    Some(response),
                )
                .await;
            }
            MultipartInitialization::SequentialRestart => {
                artifacts.remove_multipart_state().await?;
                return download_sequential(
                    http,
                    cookies,
                    download_url,
                    artifacts.assembly_scratch(),
                    None,
                )
                .await;
            }
        }
    };

    match outcome {
        MultipartOutcome::Complete(manifest) => assemble_parts(artifacts, &manifest).await,
        MultipartOutcome::RestartSequential => {
            artifacts.remove_multipart_state().await?;
            download_sequential(
                http,
                cookies,
                download_url,
                artifacts.assembly_scratch(),
                None,
            )
            .await
        }
    }
}

pub(crate) async fn resume_persisted_download_to_partial(
    http: &reqwest::Client,
    cookies: &EhCookies,
    artifacts: &ArchiveArtifacts,
    options: ArchiveDownloadOptions,
    max_size_bytes: Option<u64>,
) -> Result<bool> {
    let Some((download_url, total_len)) = recover_persisted_download(artifacts).await? else {
        return Ok(false);
    };
    ensure_archive_size_under_limit(total_len, max_size_bytes)?;
    match download_to_partial(http, cookies, &download_url, artifacts, options, true).await {
        Ok(()) => Ok(true),
        Err(error) if is_terminal_persisted_download_rejection(&error) => {
            artifacts.remove_multipart_state().await?;
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn ensure_archive_size_under_limit(
    total_len: u64,
    max_size_bytes: Option<u64>,
) -> Result<()> {
    if let Some(limit) = max_size_bytes.filter(|limit| total_len > *limit) {
        return Err(Error::Other(format!(
            "persisted EH archive size {total_len} bytes exceeds configured {limit} byte limit"
        )));
    }
    Ok(())
}

fn is_terminal_persisted_download_rejection(error: &Error) -> bool {
    match error {
        Error::Api { status, .. } => {
            (400..500).contains(status) && *status != 408 && *status != 429
        }
        _ => false,
    }
}
