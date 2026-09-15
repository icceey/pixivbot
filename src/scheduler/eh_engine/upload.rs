use super::cleanup::{ensure_job_upload_state_aborted, remove_job_upload_state};
use crate::bot::notifier::Notifier;
use crate::config::EhentaiConfig;
use crate::db::repo::eh_gallery_jobs::EhJobUploadFailureOutcome;
use crate::db::{entities::eh_gallery_jobs, repo::Repo};
use crate::scheduler::helpers::get_chat_if_should_notify;
use anyhow::{Context, Result};
use eh_client::{
    ArchiveArtifacts, ImageUploadInput, ImageUploader, IpfS3PreviewRewriteConfig, TelegraphClient,
    TelegraphImageUrlPair, UploadResumeContext, ZipArchiveUploadInput,
};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

const EH_UPLOAD_IMAGE_CHANNEL_CAPACITY: usize = 1;

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
pub(super) fn collect_uploadable_zip_entry_names(
    zip_path: &std::path::Path,
) -> Result<Vec<String>> {
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

#[derive(serde::Serialize)]
struct EhGalleryMediaCid<'a> {
    name: &'a str,
    cid: &'a str,
}

fn serialize_eh_gallery_media_cids(
    entry_names: &[String],
    url_pairs: &[TelegraphImageUrlPair],
) -> Result<Option<String>> {
    if entry_names.len() != url_pairs.len() {
        anyhow::bail!(
            "Cannot serialize EH gallery media CIDs: {} entry names for {} URL pairs",
            entry_names.len(),
            url_pairs.len()
        );
    }
    let Some(media_cids) = entry_names
        .iter()
        .zip(url_pairs)
        .map(|(name, pair)| {
            pair.cid.as_deref().map(|cid| EhGalleryMediaCid {
                name: name.as_str(),
                cid,
            })
        })
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(None);
    };
    serde_json::to_string(&media_cids)
        .map(Some)
        .context("Failed to serialize ordered EH gallery media CIDs")
}

impl EhUploadWorker {
    pub fn new(
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

    async fn has_notifiable_telegraph_delivery(&self, job_id: i32) -> Result<bool> {
        let deliveries = self.repo.get_active_eh_job_deliveries(job_id).await?;
        for delivery in deliveries {
            if !delivery.telegraph || delivery.telegraph_sent_at.is_some() {
                continue;
            }
            if !self
                .repo
                .eh_download_is_active(delivery.id, &delivery.status, self.config.send_archive)
                .await?
            {
                continue;
            }
            if get_chat_if_should_notify(&self.repo, delivery.chat_id)
                .await?
                .is_some()
            {
                return Ok(true);
            }
        }
        Ok(false)
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

    pub(super) async fn tick(&self) -> Result<()> {
        let job = self.repo.get_next_eh_job_for_upload().await?;
        let Some(job) = job else {
            return Ok(());
        };
        let expected_started_at = job
            .started_at
            .context("Claimed shared EH gallery upload is missing started_at")?;

        let defer_delay_secs = self.config.download_poll_interval_sec.max(10);
        let has_notifiable_delivery = match self.has_notifiable_telegraph_delivery(job.id).await {
            Ok(has_notifiable_delivery) => has_notifiable_delivery,
            Err(inspection_error) => {
                match self
                    .repo
                    .defer_eh_job_upload(
                        job.id,
                        expected_started_at,
                        defer_delay_secs as i64,
                        self.config.send_archive,
                    )
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => info!(
                        "Shared EH gallery upload {} changed before eligibility-inspection defer",
                        job.id
                    ),
                    Err(defer_error) => error!(
                        "Failed to defer shared EH gallery upload {} after eligibility inspection error: {:#}",
                        job.id, defer_error
                    ),
                }
                return Err(inspection_error);
            }
        };
        if !has_notifiable_delivery {
            info!(
                "Deferring shared EH gallery upload {} because no active Telegraph destination is notifiable",
                job.id
            );
            if !self
                .repo
                .defer_eh_job_upload(
                    job.id,
                    expected_started_at,
                    defer_delay_secs as i64,
                    self.config.send_archive,
                )
                .await?
            {
                info!(
                    "Skipping stale shared EH gallery upload {} before provider work",
                    job.id
                );
            }
            return Ok(());
        }

        if let Err(error) = self.process(&job).await {
            error!("Upload failed for shared EH job {}: {:#}", job.id, error);
            let failure_outcome = match self
                .repo
                .record_eh_job_upload_failure(
                    job.id,
                    expected_started_at,
                    &format!("{error:#}"),
                    self.config.max_retry_count,
                    self.config.send_archive,
                )
                .await
            {
                Ok(outcome) => outcome,
                Err(persistence_error) => {
                    error!(
                        "Failed to persist upload failure for shared EH job {}: {:#}",
                        job.id, persistence_error
                    );
                    match self
                        .repo
                        .defer_eh_job_upload(
                            job.id,
                            expected_started_at,
                            defer_delay_secs as i64,
                            self.config.send_archive,
                        )
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => info!(
                            "Shared EH gallery upload {} changed before failure-persistence recovery",
                            job.id
                        ),
                        Err(release_error) => error!(
                            "Failed to release shared EH gallery upload {} after failure-persistence error: {:#}",
                            job.id, release_error
                        ),
                    }
                    return Err(persistence_error);
                }
            };
            match failure_outcome {
                EhJobUploadFailureOutcome::RetryScheduled(_) => {}
                EhJobUploadFailureOutcome::Stale => return Err(error),
                EhJobUploadFailureOutcome::Terminal { job: _, deliveries } => {
                    for delivery in deliveries {
                        match get_chat_if_should_notify(&self.repo, delivery.chat_id).await {
                            Ok(Some(_)) => {}
                            Ok(None) => {
                                info!(
                                    "Skipping terminal EH Telegraph failure notification for delivery {} in non-notifiable chat {}",
                                    delivery.delivery_id, delivery.chat_id
                                );
                                continue;
                            }
                            Err(eligibility_error) => {
                                error!(
                                    "Failed to check notification eligibility for terminal EH Telegraph failure delivery {} in chat {}: {:#}",
                                    delivery.delivery_id, delivery.chat_id, eligibility_error
                                );
                                continue;
                            }
                        }
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

    pub(super) async fn process(&self, job: &eh_gallery_jobs::Model) -> Result<()> {
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
                let media_cids = serialize_eh_gallery_media_cids(&entry_names, &url_pairs)?;
                self.create_telegraph_page_for_job(job, &url_pairs, media_cids.as_deref())
                    .await?;
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
                // ZIP paths use '/', independently of the host filesystem.
                let filename = file.name().rsplit('/').next().unwrap().to_string();

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
        let mut uploaded_entry_names = Vec::new();
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
            if urls.len() != 1 {
                anyhow::bail!(
                    "Image uploader returned {} URL pairs for one image entry {}",
                    urls.len(),
                    image.filename
                );
            }
            uploaded_entry_names.push(image.filename);
            all_url_pairs.push(urls.into_iter().next().expect("checked URL pair count"));
        }

        reader.await.context("spawn_blocking failed")??;

        if all_url_pairs.is_empty() {
            anyhow::bail!("No images uploaded by configured image uploader");
        }

        let media_cids = serialize_eh_gallery_media_cids(&uploaded_entry_names, &all_url_pairs)?;
        self.create_telegraph_page_for_job(job, &all_url_pairs, media_cids.as_deref())
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
        media_cids: Option<&str>,
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
                media_cids,
                self.config.send_archive,
            )
            .await?;

        match ensure_job_upload_state_aborted(job, self.abort_uploader.as_deref()).await {
            Ok(abort_permit) => remove_job_upload_state(job, abort_permit).await,
            Err(abort_error) => warn!(
                "Preserving completed shared EH upload state for job {} because Abort cleanup failed: {:#}",
                job.id, abort_error
            ),
        }

        Ok(())
    }
}
