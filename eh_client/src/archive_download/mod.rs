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
use manifest::{recover_manifest, ManifestRecovery};
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_download_options_default_to_one_and_reject_zero() {
        assert_eq!(ArchiveDownloadOptions::default().max_concurrency, 1);

        let one = ArchiveDownloadOptions { max_concurrency: 1 }
            .validate()
            .unwrap();
        assert_eq!(one.max_concurrency, 1);

        let error = ArchiveDownloadOptions { max_concurrency: 0 }
            .validate()
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "archive download max_concurrency must be at least 1"
        );

        let three = ArchiveDownloadOptions { max_concurrency: 3 }
            .validate()
            .unwrap();
        assert_eq!(three.max_concurrency, 3);
    }
}
