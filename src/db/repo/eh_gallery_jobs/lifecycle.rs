use super::*;

impl Repo {
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
            .set(eh_gallery_jobs::ActiveModel {
                cleanup_status: Set(CLEANUP_STATUS_RUNNING.to_string()),
                cleanup_started_at: Set(Some(generation)),
                cleanup_next_retry_at: Set(None),
                ..Default::default()
            })
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(&job.cleanup_status))
            .filter(nullable_eq(
                eh_gallery_jobs::Column::CleanupStartedAt,
                job.cleanup_started_at,
            ))
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
            .set(eh_gallery_jobs::ActiveModel {
                cleanup_status: Set(CLEANUP_STATUS_FAILED.to_string()),
                cleanup_error: Set(Some(error.to_string())),
                cleanup_next_retry_at: Set(Some(retry_at)),
                ..Default::default()
            })
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_RUNNING))
            .filter(eh_gallery_jobs::Column::CleanupStartedAt.eq(expected_cleanup_started_at))
            .exec(&self.db)
            .await
            .context("Failed to record shared EH artifact cleanup failure")?;
        Ok(failed.rows_affected == 1)
    }

    /// Finalize a removed artifact family only for its matching cleanup generation.
    /// Delivery ownership is rechecked by the final update in this transaction.
    pub async fn finalize_eh_job_cleanup(
        &self,
        job_id: i32,
        expected_cleanup_started_at: DateTime,
        send_archive: bool,
    ) -> Result<Option<EhCleanupFinalizeOutcome>> {
        for _ in 0..SQLITE_TRANSACTION_ATTEMPTS {
            match self
                .finalize_eh_job_cleanup_once(job_id, expected_cleanup_started_at, send_archive)
                .await
            {
                Ok(outcome) => return Ok(outcome),
                Err(error) => {
                    if !is_retryable_sqlite_lock_error(&error) {
                        return Err(error);
                    }
                }
            }
        }
        self.finalize_eh_job_cleanup_once(job_id, expected_cleanup_started_at, send_archive)
            .await
    }

    async fn finalize_eh_job_cleanup_once(
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

            let mut changes = eh_gallery_jobs::ActiveModel {
                cleanup_status: Set(CLEANUP_STATUS_NONE.to_string()),
                cleanup_error: Set(None),
                cleanup_next_retry_at: Set(None),
                zip_path: Set(None),
                file_size: Set(0),
                gp_cost: Set(0),
                ..Default::default()
            };
            // A consumerless rewrite retains its current execution state. Other
            // outcomes finish the old source attempt before retirement or reuse.
            if has_active_delivery || !rewrite_in_progress {
                changes.error = Set(None);
                changes.retry_count = Set(0);
                changes.next_retry_at = Set(None);
                changes.completed_at = Set(None);
                changes.background_download_status = Set(None);
                changes.background_download_started_at = Set(None);
                changes.background_download_next_retry_at = Set(None);
                changes.background_download_attempt_count = Set(0);
                changes.background_download_error = Set(None);
            }
            // Keep a usable page and its rewrite lease when the next source
            // attempt can reuse them. Final retirement discards both.
            if (has_active_delivery && source_work_required && !has_usable_ready_telegraph)
                || (!has_active_delivery && !rewrite_in_progress)
            {
                changes.telegraph_status = Set(TELEGRAPH_STATUS_NOT_REQUIRED.to_string());
                changes.telegraph_url = Set(None);
                changes.telegraph_rewrite_data = Set(None);
                changes.telegraph_rewrite_status = Set(None);
                changes.telegraph_rewrite_after = Set(None);
                changes.telegraph_rewrite_started_at = Set(None);
                changes.telegraph_rewrite_next_retry_at = Set(None);
                changes.telegraph_rewrite_retry_count = Set(0);
                changes.telegraph_rewrite_error = Set(None);
                changes.telegraph_rewritten_at = Set(None);
            }
            let outcome = if has_active_delivery {
                if source_work_required {
                    changes.status = Set(JOB_STATUS_PENDING.to_string());
                    EhCleanupFinalizeOutcome::ReactivatedPending
                } else {
                    changes.status = Set(JOB_STATUS_DOWNLOADED.to_string());
                    EhCleanupFinalizeOutcome::FinalizedWithoutSourceWork
                }
            } else if rewrite_in_progress {
                EhCleanupFinalizeOutcome::RetainedForRewrite
            } else {
                changes.status = Set(JOB_STATUS_RETIRED.to_string());
                changes.telegraph_required = Set(false);
                EhCleanupFinalizeOutcome::CleanRetired
            };
            let no_active_delivery = no_active_eh_delivery_filter(job_id);
            let finalized = eh_gallery_jobs::Entity::update_many()
                .set(changes)
                .filter(eh_gallery_jobs::Column::Id.eq(job_id))
                .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_RUNNING))
                .filter(eh_gallery_jobs::Column::CleanupStartedAt.eq(expected_cleanup_started_at))
                .filter(if has_active_delivery {
                    no_active_delivery.not()
                } else {
                    no_active_delivery
                })
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
                    .set(eh_gallery_jobs::ActiveModel {
                        telegraph_status: Set((if telegraph_required {
                            TELEGRAPH_STATUS_PENDING
                        } else {
                            TELEGRAPH_STATUS_NOT_REQUIRED
                        })
                        .to_string()),
                        telegraph_required: Set(telegraph_required),
                        ..Default::default()
                    })
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
                .set(eh_gallery_jobs::ActiveModel {
                    background_download_status: Set(Some(BACKGROUND_STATUS_PENDING.to_string())),
                    background_download_started_at: Set(None),
                    ..Default::default()
                })
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
                .set(eh_gallery_jobs::ActiveModel {
                    cleanup_status: Set(CLEANUP_STATUS_PENDING.to_string()),
                    cleanup_next_retry_at: Set(None),
                    ..Default::default()
                })
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
            let download_still_needs_zip = download_in_flight
                || (job.status == JOB_STATUS_PENDING
                    && !(job.telegraph_status == TELEGRAPH_STATUS_READY
                        && job.telegraph_url.is_some())
                    && deliveries.iter().any(|delivery| {
                        is_active_delivery_status(&delivery.status)
                            && delivery.telegraph
                            && delivery.telegraph_sent_at.is_none()
                    }));
            let remove_archive_family = job.zip_path.is_some()
                && !archive_still_needed
                && !upload_still_needs_zip
                && !download_still_needs_zip;
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
                .set(eh_gallery_jobs::ActiveModel {
                    status: Set(status.to_string()),
                    cleanup_status: Set(cleanup_status.to_string()),
                    ..Default::default()
                })
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
            update = update.filter(nullable_eq(
                eh_gallery_jobs::Column::StartedAt,
                job.started_at,
            ));
            update = update.filter(nullable_eq(
                eh_gallery_jobs::Column::ZipPath,
                job.zip_path.as_deref(),
            ));
            update = update.filter(nullable_eq(
                eh_gallery_jobs::Column::TelegraphRewriteStatus,
                job.telegraph_rewrite_status.as_deref(),
            ));
            update = update.filter(nullable_eq(
                eh_gallery_jobs::Column::TelegraphRewriteData,
                job.telegraph_rewrite_data.as_deref(),
            ));
            update = update.filter(nullable_eq(
                eh_gallery_jobs::Column::TelegraphRewrittenAt,
                job.telegraph_rewritten_at,
            ));
            if retire {
                update = update.filter(no_active_eh_delivery_filter(job_id));
            }

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
            update = clear_background_download_state(update)
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
                .filter(nullable_eq(
                    eh_gallery_jobs::Column::TelegraphRewriteStatus,
                    job.telegraph_rewrite_status.as_deref(),
                ))
                .filter(nullable_eq(
                    eh_gallery_jobs::Column::TelegraphRewriteData,
                    job.telegraph_rewrite_data.as_deref(),
                ))
                .filter(nullable_eq(
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
        .set(eh_gallery_jobs::ActiveModel {
            status: Set(status.to_string()),
            cleanup_status: Set(cleanup_status.to_string()),
            ..Default::default()
        })
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
