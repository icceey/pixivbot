use super::*;

impl Repo {
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
