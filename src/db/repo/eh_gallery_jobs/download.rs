use super::*;

impl Repo {
    /// Claim the next job in the selected queue without changing its persisted lane rules.
    pub async fn claim_eh_download_job(
        &self,
        queue: EhDownloadQueue,
        send_archive: bool,
    ) -> Result<Option<eh_gallery_jobs::Model>> {
        self.claim_eh_download_job_at(queue, Local::now().naive_local(), send_archive)
            .await
    }

    async fn claim_eh_download_job_at(
        &self,
        queue: EhDownloadQueue,
        now: DateTime,
        send_archive: bool,
    ) -> Result<Option<eh_gallery_jobs::Model>> {
        let retry_column = match queue {
            EhDownloadQueue::Main => eh_gallery_jobs::Column::NextRetryAt,
            EhDownloadQueue::Background => eh_gallery_jobs::Column::BackgroundDownloadNextRetryAt,
        };
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
            .filter(eh_gallery_jobs::Column::LegacyArtifactHandoff.is_null())
            .filter(match queue {
                EhDownloadQueue::Main => {
                    eh_gallery_jobs::Column::BackgroundDownloadStatus.is_null()
                }
                EhDownloadQueue::Background => {
                    eh_gallery_jobs::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_PENDING)
                }
            })
            .filter(eh_job_has_configured_source_work_candidate_filter(
                send_archive,
            ))
            .filter(retry_column.is_null().or(retry_column.lte(now)));
        QueryTrait::query(&mut query)
            .order_by_expr(recent_priority, Order::Asc)
            .order_by_expr(recent_created_at, Order::Asc)
            .order_by_expr(recent_id, Order::Asc)
            .order_by_expr(old_created_at, Order::Desc)
            .order_by_expr(old_id, Order::Desc);
        let Some(job) = query.one(&self.db).await.context(match queue {
            EhDownloadQueue::Main => "Failed to fetch next shared EH gallery job for download",
            EhDownloadQueue::Background => {
                "Failed to fetch next shared EH gallery background download"
            }
        })?
        else {
            return Ok(None);
        };

        self.claim_eh_download_job_from_snapshot_at(queue, &job, now, send_archive)
            .await
    }

    async fn claim_eh_download_job_from_snapshot_at(
        &self,
        queue: EhDownloadQueue,
        job: &eh_gallery_jobs::Model,
        now: DateTime,
        send_archive: bool,
    ) -> Result<Option<eh_gallery_jobs::Model>> {
        let generation = next_job_claim_generation(now, job.started_at)?;
        let (retry_column, mut update) = match queue {
            EhDownloadQueue::Main => (
                eh_gallery_jobs::Column::NextRetryAt,
                eh_gallery_jobs::Entity::update_many().set(eh_gallery_jobs::ActiveModel {
                    status: Set(JOB_STATUS_DOWNLOADING.to_string()),
                    next_retry_at: Set(None),
                    ..Default::default()
                }),
            ),
            EhDownloadQueue::Background => {
                let lease_started_at = next_job_claim_generation(now, None)?;
                (
                    eh_gallery_jobs::Column::BackgroundDownloadNextRetryAt,
                    eh_gallery_jobs::Entity::update_many().set(eh_gallery_jobs::ActiveModel {
                        background_download_status: Set(Some(
                            BACKGROUND_STATUS_RUNNING.to_string(),
                        )),
                        background_download_started_at: Set(Some(lease_started_at)),
                        ..Default::default()
                    }),
                )
            }
        };
        update = update.col_expr(eh_gallery_jobs::Column::StartedAt, Expr::value(generation));
        let result = update
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_PENDING))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(eh_gallery_jobs::Column::LegacyArtifactHandoff.is_null())
            .filter(match queue {
                EhDownloadQueue::Main => {
                    eh_gallery_jobs::Column::BackgroundDownloadStatus.is_null()
                }
                EhDownloadQueue::Background => {
                    eh_gallery_jobs::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_PENDING)
                }
            })
            .filter(eh_job_has_configured_source_work_filter(
                job.id,
                send_archive,
            ))
            .filter(nullable_eq(
                eh_gallery_jobs::Column::StartedAt,
                job.started_at,
            ))
            .filter(
                sea_orm::Condition::any()
                    .add(retry_column.is_null())
                    .add(retry_column.lte(now)),
            )
            .exec(&self.db)
            .await
            .context(match queue {
                EhDownloadQueue::Main => "Failed to atomically claim shared EH gallery job",
                EhDownloadQueue::Background => {
                    "Failed to atomically claim shared EH gallery background download"
                }
            })?;
        if result.rows_affected == 0 {
            return Ok(None);
        }

        eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .filter(eh_gallery_jobs::Column::Status.eq(match queue {
                EhDownloadQueue::Main => JOB_STATUS_DOWNLOADING,
                EhDownloadQueue::Background => JOB_STATUS_PENDING,
            }))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(match queue {
                EhDownloadQueue::Main => {
                    eh_gallery_jobs::Column::BackgroundDownloadStatus.is_null()
                }
                EhDownloadQueue::Background => {
                    eh_gallery_jobs::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING)
                }
            })
            .filter(eh_gallery_jobs::Column::StartedAt.eq(generation))
            .one(&self.db)
            .await
            .context(match queue {
                EhDownloadQueue::Main => "Failed to reread shared EH gallery download claim",
                EhDownloadQueue::Background => {
                    "Failed to reread shared EH gallery background claim"
                }
            })
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
            .filter(eh_gallery_jobs::Column::LegacyArtifactHandoff.is_null())
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

    /// Move migration-marked legacy archive families into their single shared
    /// job-specific family before workers or orphan cleanup can touch them.
    ///
    /// The durable marker is first changed from `pending` to `moving`; both
    /// source and target families remain cleanup-owned until the final DB
    /// update. Ambiguous `conflict` families are moved aside before their jobs
    /// are released for fresh work. This makes interrupted renames restart-safe
    /// without allowing a worker to claim an ambiguous legacy family.
    pub async fn handoff_legacy_eh_archive_artifacts(
        &self,
        cache_dir: &std::path::Path,
    ) -> Result<u64> {
        let jobs = eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::LegacyArtifactHandoff.is_not_null())
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_PENDING))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .order_by(eh_gallery_jobs::Column::Id, Order::Asc)
            .all(&self.db)
            .await
            .context("Failed to fetch legacy EH archive handoff jobs")?;
        let mut completed = 0;

        for job in jobs {
            let Some(state) = job.legacy_artifact_handoff.as_deref() else {
                continue;
            };
            let source =
                ArchiveArtifacts::new(legacy_eh_gallery_job_artifact_path(cache_dir, &job));
            let target = ArchiveArtifacts::new(eh_gallery_job_artifact_path(cache_dir, &job));
            let source_has_members = archive_artifacts_have_members(&source).await?;
            let target_has_members = archive_artifacts_have_members(&target).await?;

            if state == LEGACY_ARTIFACT_HANDOFF_CONFLICT {
                if source_has_members {
                    let quarantine_dir = cache_dir.join("legacy-conflicts");
                    tokio::fs::create_dir_all(&quarantine_dir)
                        .await
                        .context("Failed to create legacy EH archive quarantine")?;
                    let source_name = source
                        .final_zip()
                        .file_name()
                        .context("Legacy EH archive source has no file name")?;
                    let quarantine = ArchiveArtifacts::new(quarantine_dir.join(source_name));
                    if let Err(error) =
                        move_archive_artifacts_without_overwrite(&source, &quarantine).await
                    {
                        tracing::warn!(
                            job_id = job.id,
                            gid = job.gid,
                            error = %error,
                            "Failed to quarantine ambiguous legacy EH archive family"
                        );
                        continue;
                    }
                    if archive_artifacts_have_members(&source).await? {
                        tracing::warn!(
                            job_id = job.id,
                            gid = job.gid,
                            "Ambiguous legacy EH archive source remains after quarantine"
                        );
                        continue;
                    }
                    tracing::warn!(
                        job_id = job.id,
                        gid = job.gid,
                        path = %quarantine.final_zip().display(),
                        "Quarantined ambiguous legacy EH archive family"
                    );
                }
                if self
                    .finish_legacy_eh_archive_handoff(
                        job.id,
                        LEGACY_ARTIFACT_HANDOFF_CONFLICT,
                        None,
                    )
                    .await?
                {
                    completed += 1;
                }
                continue;
            }

            if state == LEGACY_ARTIFACT_HANDOFF_PENDING {
                if source_has_members && target_has_members {
                    tracing::warn!(
                        job_id = job.id,
                        gid = job.gid,
                        "Preserving conflicting legacy and shared EH archive families"
                    );
                    continue;
                }
                if !source_has_members {
                    if target_has_members {
                        tracing::warn!(
                            job_id = job.id,
                            gid = job.gid,
                            "Preserving unexpected shared EH archive family without its legacy source"
                        );
                        continue;
                    }
                    if self
                        .finish_legacy_eh_archive_handoff(
                            job.id,
                            LEGACY_ARTIFACT_HANDOFF_PENDING,
                            None,
                        )
                        .await?
                    {
                        completed += 1;
                    }
                    continue;
                }

                let claimed = eh_gallery_jobs::Entity::update_many()
                    .col_expr(
                        eh_gallery_jobs::Column::LegacyArtifactHandoff,
                        Expr::value(Some(LEGACY_ARTIFACT_HANDOFF_MOVING.to_string())),
                    )
                    .filter(eh_gallery_jobs::Column::Id.eq(job.id))
                    .filter(
                        eh_gallery_jobs::Column::LegacyArtifactHandoff
                            .eq(LEGACY_ARTIFACT_HANDOFF_PENDING),
                    )
                    .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_PENDING))
                    .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
                    .exec(&self.db)
                    .await
                    .context("Failed to mark legacy EH archive handoff as moving")?;
                if claimed.rows_affected != 1 {
                    continue;
                }
            } else if state != LEGACY_ARTIFACT_HANDOFF_MOVING {
                tracing::warn!(
                    job_id = job.id,
                    gid = job.gid,
                    state,
                    "Preserving EH archive family with an unknown legacy handoff state"
                );
                continue;
            }

            if let Err(error) = move_archive_artifacts_without_overwrite(&source, &target).await {
                tracing::warn!(
                    job_id = job.id,
                    gid = job.gid,
                    error = %error,
                    "Failed to move legacy EH archive family; preserving both families"
                );
                continue;
            }
            if archive_artifacts_have_members(&source).await? {
                tracing::warn!(
                    job_id = job.id,
                    gid = job.gid,
                    "Legacy EH archive source remains after handoff; preserving both families"
                );
                continue;
            }

            let target_path = archive_artifacts_have_members(&target)
                .await?
                .then_some(target.final_zip());
            if self
                .finish_legacy_eh_archive_handoff(
                    job.id,
                    LEGACY_ARTIFACT_HANDOFF_MOVING,
                    target_path,
                )
                .await?
            {
                completed += 1;
            }
        }

        Ok(completed)
    }

    async fn finish_legacy_eh_archive_handoff(
        &self,
        job_id: i32,
        expected_state: &str,
        zip_path: Option<&std::path::Path>,
    ) -> Result<bool> {
        let zip_path = zip_path.map(|path| path.to_string_lossy().into_owned());
        let updated = eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                legacy_artifact_handoff: Set(None),
                zip_path: Set(zip_path),
                ..Default::default()
            })
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(eh_gallery_jobs::Column::LegacyArtifactHandoff.eq(expected_state))
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_PENDING))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .exec(&self.db)
            .await
            .context("Failed to finalize legacy EH archive handoff")?;
        Ok(updated.rows_affected == 1)
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
            .set(eh_gallery_jobs::ActiveModel {
                status: Set(JOB_STATUS_PENDING.to_string()),
                next_retry_at: Set(None),
                error: Set(Some(error.to_string())),
                background_download_status: Set(Some(BACKGROUND_STATUS_PENDING.to_string())),
                background_download_started_at: Set(None),
                background_download_next_retry_at: Set(Some(now)),
                background_download_attempt_count: Set(0_i32),
                background_download_error: Set(None),
                ..Default::default()
            })
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

    /// Return one expected background claim to its retry queue without
    /// consuming a retry attempt. Returns `false` when the claim generation is
    /// stale, leaving its successor untouched.
    pub async fn defer_eh_job_background_download(
        &self,
        job_id: i32,
        expected_started_at: DateTime,
        delay_secs: i64,
        reason: &str,
    ) -> Result<bool> {
        let now = Local::now().naive_local();
        let result = eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                background_download_status: Set(Some(BACKGROUND_STATUS_PENDING.to_string())),
                background_download_started_at: Set(None),
                background_download_next_retry_at: Set(Some(
                    now + chrono::Duration::seconds(delay_secs.max(1)),
                )),
                background_download_error: Set(Some(reason.to_string())),
                ..Default::default()
            })
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_PENDING))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(eh_gallery_jobs::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING))
            .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at))
            .exec(&self.db)
            .await
            .context("Failed to defer shared EH gallery background download")?;
        Ok(result.rows_affected == 1)
    }

    pub async fn release_eh_job_background_downloads_to_main_queue(&self) -> Result<u64> {
        let result = clear_background_download_state(eh_gallery_jobs::Entity::update_many())
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
                    .set(eh_gallery_jobs::ActiveModel {
                        status: Set(JOB_STATUS_FAILED.to_string()),
                        error: Set(Some("cached EH ZIP is missing".to_string())),
                        retry_count: Set(retry_count),
                        completed_at: Set(Some(now)),
                        next_retry_at: Set(None),
                        ..Default::default()
                    })
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
                .set(eh_gallery_jobs::ActiveModel {
                    status: Set(JOB_STATUS_PENDING.to_string()),
                    zip_path: Set(None),
                    file_size: Set(0_i64),
                    gp_cost: Set(0_i64),
                    completed_at: Set(None),
                    next_retry_at: Set(Some(retry_at)),
                    error: Set(Some("cached EH ZIP is missing".to_string())),
                    retry_count: Set(retry_count),
                    ..Default::default()
                })
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
                .set(eh_download_queue::ActiveModel {
                    status: Set(DELIVERY_STATUS_WAITING.to_string()),
                    next_retry_at: Set(None),
                    ..Default::default()
                })
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

    pub async fn defer_eh_job_download(&self, job_id: i32, delay_secs: i64) -> Result<()> {
        let result = eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                status: Set(JOB_STATUS_PENDING.to_string()),
                next_retry_at: Set(Some(
                    Local::now().naive_local() + chrono::Duration::seconds(delay_secs),
                )),
                ..Default::default()
            })
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
        queue: EhDownloadQueue,
        job_id: i32,
        expected_started_at: DateTime,
        error: &str,
        max_retry_count: u8,
    ) -> Result<(eh_gallery_jobs::Model, bool)> {
        let background = if queue == EhDownloadQueue::Background {
            " background"
        } else {
            ""
        };
        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&self.db)
            .await
            .with_context(|| {
                format!("Failed to fetch shared EH gallery job for{background} retry")
            })?
            .with_context(|| {
                format!("Shared EH gallery job disappeared before{background} retry")
            })?;
        let retry_count = match queue {
            EhDownloadQueue::Main => job
                .retry_count
                .checked_add(1)
                .context("Shared EH gallery job retry count overflow")?,
            EhDownloadQueue::Background => job
                .background_download_attempt_count
                .checked_add(1)
                .context("Shared EH gallery background attempt count overflow")?,
        };
        let permanent = match queue {
            EhDownloadQueue::Main => retry_count > i32::from(max_retry_count),
            EhDownloadQueue::Background => retry_count >= i32::from(max_retry_count),
        };
        if permanent {
            return Ok((
                self.fail_eh_job_download_claim(
                    queue,
                    job_id,
                    expected_started_at,
                    error,
                    retry_count,
                )
                .await?,
                true,
            ));
        }

        let delay = Self::backoff_delay_secs(retry_count);
        let now = Local::now().naive_local();
        let mut update = match queue {
            EhDownloadQueue::Main => {
                eh_gallery_jobs::Entity::update_many().set(eh_gallery_jobs::ActiveModel {
                    status: Set(JOB_STATUS_PENDING.to_string()),
                    next_retry_at: Set(Some(now + chrono::Duration::seconds(delay))),
                    error: Set(Some(error.to_string())),
                    retry_count: Set(retry_count),
                    ..Default::default()
                })
            }
            EhDownloadQueue::Background => {
                eh_gallery_jobs::Entity::update_many().set(eh_gallery_jobs::ActiveModel {
                    background_download_status: Set(Some(BACKGROUND_STATUS_PENDING.to_string())),
                    background_download_started_at: Set(None),
                    background_download_next_retry_at: Set(Some(
                        now + chrono::Duration::seconds(delay),
                    )),
                    background_download_error: Set(Some(error.to_string())),
                    background_download_attempt_count: Set(retry_count),
                    ..Default::default()
                })
            }
        };
        update = update
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at));
        update = match queue {
            EhDownloadQueue::Main => {
                update.filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADING))
            }
            EhDownloadQueue::Background => update
                .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_PENDING))
                .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
                .filter(
                    eh_gallery_jobs::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING),
                ),
        };
        let result = update.exec(&self.db).await.context(match queue {
            EhDownloadQueue::Main => "Failed to schedule shared EH gallery download retry",
            EhDownloadQueue::Background => "Failed to schedule shared EH gallery background retry",
        })?;
        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot retry shared EH gallery{background} job {}: claim changed concurrently",
                job_id
            );
        }
        let updated = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&self.db)
            .await
            .with_context(|| {
                format!("Failed to reread shared EH gallery job after{background} retry")
            })?
            .with_context(|| {
                format!("Shared EH gallery job disappeared after{background} retry")
            })?;
        Ok((updated, false))
    }

    pub async fn fail_eh_job_for_archive_policy(
        &self,
        queue: EhDownloadQueue,
        job: &eh_gallery_jobs::Model,
        error: &str,
    ) -> Result<eh_gallery_jobs::Model> {
        let started_at = job.started_at.context(match queue {
            EhDownloadQueue::Main => "Cannot fail shared EH gallery job for archive policy: missing download claim started_at",
            EhDownloadQueue::Background => "Cannot fail shared EH gallery background download for archive policy: missing claim started_at",
        })?;
        self.fail_eh_job_download_claim(queue, job.id, started_at, error, job.retry_count)
            .await
    }

    pub async fn mark_eh_job_downloaded(
        &self,
        queue: EhDownloadQueue,
        job_id: i32,
        expected_started_at: DateTime,
        file_size: i64,
        zip_path: &str,
        gp_cost: i64,
    ) -> Result<eh_gallery_jobs::Model> {
        let background = if queue == EhDownloadQueue::Background {
            " background"
        } else {
            ""
        };
        anyhow::ensure!(
            file_size >= 0,
            "Shared EH gallery{background} download file size must be non-negative"
        );
        let now = Local::now().naive_local();
        let txn = self.db.begin().await.with_context(|| {
            format!("Failed to begin shared EH gallery{background} completion transaction")
        })?;
        let telegraph_status: SimpleExpr = Expr::case(
            sea_orm::Condition::all()
                .add(eh_gallery_jobs::Column::TelegraphRequired.eq(true))
                .add(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_NOT_REQUIRED)),
            Expr::value(TELEGRAPH_STATUS_PENDING),
        )
        .finally(Expr::col(eh_gallery_jobs::Column::TelegraphStatus))
        .into();
        let mut update = eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                status: Set(JOB_STATUS_DOWNLOADED.to_string()),
                file_size: Set(file_size),
                gp_cost: Set(gp_cost),
                zip_path: Set(Some(zip_path.to_string())),
                completed_at: Set(Some(now)),
                error: Set(None),
                next_retry_at: Set(None),
                ..Default::default()
            })
            .col_expr(eh_gallery_jobs::Column::TelegraphStatus, telegraph_status)
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at));
        update = match queue {
            EhDownloadQueue::Main => {
                update.filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADING))
            }
            EhDownloadQueue::Background => clear_background_download_state(update)
                .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_PENDING))
                .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
                .filter(
                    eh_gallery_jobs::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING),
                ),
        };
        let updated = update.exec(&txn).await.context(match queue {
            EhDownloadQueue::Main => "Failed to mark shared EH gallery job downloaded",
            EhDownloadQueue::Background => {
                "Failed to mark shared EH gallery background download complete"
            }
        })?;
        if updated.rows_affected != 1 {
            txn.rollback().await?;
            anyhow::bail!(
                "Cannot mark shared EH gallery job {}{background} downloaded: claim changed concurrently",
                job_id
            );
        }
        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&txn)
            .await
            .with_context(|| format!("Failed to reread shared EH gallery job before{background} completion ledger append"))?
            .with_context(|| format!("Shared EH gallery job disappeared before{background} completion ledger append"))?;
        crate::db::repo::eh_download_completions::append_eh_download_completion_in_txn(
            &txn, job.id, job.gid, file_size, now,
        )
        .await?;
        self.evaluate_eh_job_liveness_in_txn(&txn, job_id, true)
            .await?;
        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&txn)
            .await
            .with_context(|| {
                format!("Failed to reread settled shared EH gallery{background} completion")
            })?
            .with_context(|| {
                format!("Shared EH gallery job disappeared after{background} completion settlement")
            })?;
        txn.commit().await.with_context(|| {
            format!("Failed to commit shared EH gallery{background} completion transaction")
        })?;
        Ok(job)
    }

    async fn fail_eh_job_download_claim(
        &self,
        queue: EhDownloadQueue,
        job_id: i32,
        expected_started_at: DateTime,
        error: &str,
        retry_count: i32,
    ) -> Result<eh_gallery_jobs::Model> {
        let background = if queue == EhDownloadQueue::Background {
            " background"
        } else {
            ""
        };
        let now = Local::now().naive_local();
        let txn = self.db.begin().await.with_context(|| {
            format!("Failed to begin shared EH gallery{background} failure transaction")
        })?;
        let mut update = eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                status: Set(JOB_STATUS_FAILED.to_string()),
                error: Set(Some(error.to_string())),
                completed_at: Set(Some(now)),
                next_retry_at: Set(None),
                ..Default::default()
            })
            .col_expr(
                eh_gallery_jobs::Column::CleanupStatus,
                cleanup_pending_when_zip_owned_expr(),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at));
        update = match queue {
            EhDownloadQueue::Main => update
                .col_expr(
                    eh_gallery_jobs::Column::RetryCount,
                    Expr::value(retry_count),
                )
                .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADING)),
            EhDownloadQueue::Background => clear_background_download_state(update)
                .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_PENDING))
                .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
                .filter(
                    eh_gallery_jobs::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING),
                ),
        };
        let updated = update
            .exec(&txn)
            .await
            .with_context(|| format!("Failed to fail shared EH gallery{background} job"))?;
        if updated.rows_affected != 1 {
            txn.rollback().await?;
            anyhow::bail!(
                "Cannot fail shared EH gallery{background} job {}: claim changed concurrently",
                job_id
            );
        }
        fail_active_eh_job_deliveries_in_txn(&txn, job_id).await?;
        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&txn)
            .await
            .with_context(|| format!("Failed to reread failed shared EH gallery{background} job"))?
            .with_context(|| {
                format!("Shared EH gallery job disappeared after{background} failure")
            })?;
        txn.commit().await.with_context(|| {
            format!("Failed to commit shared EH gallery{background} failure transaction")
        })?;
        Ok(job)
    }
}
