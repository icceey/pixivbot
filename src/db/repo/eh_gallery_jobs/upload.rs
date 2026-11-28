use super::*;

impl Repo {
    /// Claim one downloaded shared-gallery job for its single Telegraph upload.
    pub async fn get_next_eh_job_for_upload(&self) -> Result<Option<eh_gallery_jobs::Model>> {
        let now = Local::now().naive_local();
        let telegraph_demand = eh_gallery_jobs::Column::Id.in_subquery(
            Query::select()
                .column(eh_download_queue::Column::JobId)
                .from(eh_download_queue::Entity)
                .and_where(eh_active_delivery_status_filter())
                .and_where(eh_pending_telegraph_delivery_filter())
                .to_owned(),
        );
        let job = eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADED))
            .filter(telegraph_demand.clone())
            .filter(
                eh_gallery_jobs::Column::TelegraphStatus
                    .is_in([TELEGRAPH_STATUS_NOT_REQUIRED, TELEGRAPH_STATUS_PENDING]),
            )
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(
                eh_gallery_jobs::Column::NextRetryAt
                    .is_null()
                    .or(eh_gallery_jobs::Column::NextRetryAt.lte(now)),
            )
            .order_by(eh_gallery_jobs::Column::CreatedAt, Order::Asc)
            .order_by(eh_gallery_jobs::Column::Id, Order::Asc)
            .one(&self.db)
            .await
            .context("Failed to fetch next shared EH gallery job for upload")?;
        let Some(job) = job else {
            return Ok(None);
        };

        let generation = next_job_claim_generation(now, job.started_at)?;
        let claimed = eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                telegraph_required: Set(true),
                telegraph_status: Set(TELEGRAPH_STATUS_UPLOADING.to_string()),
                started_at: Set(Some(generation)),
                next_retry_at: Set(None),
                ..Default::default()
            })
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADED))
            .filter(telegraph_demand)
            .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(&job.telegraph_status))
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(nullable_eq(
                eh_gallery_jobs::Column::StartedAt,
                job.started_at,
            ))
            .filter(
                sea_orm::Condition::any()
                    .add(eh_gallery_jobs::Column::NextRetryAt.is_null())
                    .add(eh_gallery_jobs::Column::NextRetryAt.lte(now)),
            )
            .exec(&self.db)
            .await
            .context("Failed to atomically claim shared EH gallery upload")?;
        if claimed.rows_affected == 0 {
            return Ok(None);
        }

        // Do not re-check telegraph_required here. Cancellation after the CAS is
        // allowed to remove the final consumer, but it must not revoke this
        // generation or delete its multipart state while the upload is running.
        eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_UPLOADING))
            .filter(eh_gallery_jobs::Column::StartedAt.eq(generation))
            .one(&self.db)
            .await
            .context("Failed to reread shared EH gallery upload claim")
    }

    /// Persist the single Telegraph page and optional delayed-rewrite payload
    /// owned by a shared gallery job.
    pub async fn mark_eh_job_telegraph_ready(
        &self,
        job_id: i32,
        expected_started_at: DateTime,
        telegraph_url: &str,
        rewrite_data_json: Option<&str>,
        media_cids: Option<&str>,
        send_archive: bool,
    ) -> Result<eh_gallery_jobs::Model> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH Telegraph completion transaction")?;
        let result: Result<eh_gallery_jobs::Model> = async {
            let updated = eh_gallery_jobs::Entity::update_many()
                .set(eh_gallery_jobs::ActiveModel {
    telegraph_status: Set(TELEGRAPH_STATUS_READY.to_string()),
    telegraph_url: Set(Some(telegraph_url.to_string())),
    error: Set(None),
    next_retry_at: Set(None),
    telegraph_rewrite_data: Set(rewrite_data_json.map(str::to_string)),
    telegraph_rewrite_status: Set(None),
    telegraph_rewrite_after: Set(None),
    telegraph_rewrite_started_at: Set(None),
    telegraph_rewrite_next_retry_at: Set(None),
    telegraph_rewrite_retry_count: Set(0_i32),
    telegraph_rewrite_error: Set(None),
    telegraph_rewritten_at: Set(None),
    ..Default::default()
})
                .filter(eh_gallery_jobs::Column::Id.eq(job_id))
                .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_UPLOADING))
                .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at))
                .exec(&txn)
                .await
                .context("Failed to mark shared EH gallery Telegraph page ready")?;
            if updated.rows_affected != 1 {
                anyhow::bail!(
                    "Cannot mark shared EH gallery job {} Telegraph-ready: upload claim changed concurrently",
                    job_id
                );
            }

            let completed_job = eh_gallery_jobs::Entity::find_by_id(job_id)
                .one(&txn)
                .await
                .context("Failed to read shared EH gallery job for result caching")?
                .context("Shared EH gallery job disappeared after Telegraph completion")?;
            if let (Some(fingerprint), Some(media_cids)) =
                (completed_job.source_fingerprint.as_deref(), media_cids)
            {
                let variant = EhGalleryVariant {
                    download_mode: completed_job.download_mode.clone(),
                    resolution: completed_job.resolution.clone(),
                };
                upsert_eh_gallery_result_in_txn(
                    &txn,
                    completed_job.gid,
                    &completed_job.token,
                    &variant,
                    fingerprint,
                    completed_job.source_generation,
                    telegraph_url,
                    rewrite_data_json,
                    Some(media_cids),
                )
                .await?;
            }

            self.recompute_eh_job_telegraph_requirement_with_policy_in_txn(
                &txn,
                job_id,
                send_archive,
            )
                .await?;
            self.evaluate_eh_job_liveness_in_txn(&txn, job_id, send_archive)
                .await?;
            eh_gallery_jobs::Entity::find_by_id(job_id)
                .one(&txn)
                .await
                .context("Failed to reread settled shared EH gallery Telegraph completion")?
                .context("Shared EH gallery job disappeared after Telegraph completion")
        }
        .await;
        match result {
            Ok(job) => txn
                .commit()
                .await
                .context("Failed to commit shared EH Telegraph completion transaction")
                .map(|()| job),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back shared EH Telegraph completion transaction")?;
                Err(error)
            }
        }
    }

    /// Persist one delivery's sent marker and schedule the shared job's delayed
    /// Telegraph rewrite exactly once.
    pub async fn mark_eh_telegraph_delivery_sent(
        &self,
        expected: &eh_download_queue::Model,
        rewrite_delay_secs: Option<i64>,
    ) -> Result<()> {
        let delivery_id = expected.id;
        let job_id = expected
            .job_id
            .context("EH Telegraph sent marker requires a shared job")?;
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin EH Telegraph sent-marker transaction")?;
        let result: Result<()> = async {
            let delivery = eh_download_queue::Entity::find()
                .filter(eh_download_queue::Column::Id.eq(delivery_id))
                .filter(claim_generation_filter(expected.started_at))
                .filter(eh_download_queue::Column::JobId.eq(job_id))
                .filter(eh_download_queue::Column::Telegraph.eq(true))
                .filter(eh_download_queue::Column::Status.eq(DELIVERY_STATUS_PUBLISHING))
                .one(&txn)
                .await
                .context("Failed to read shared EH Telegraph delivery for sent marker")?
                .context("Shared EH Telegraph delivery disappeared before sent marker")?;
            let now = Local::now().naive_local();
            let marked = eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::TelegraphSentAt,
                    Expr::value(Some(now)),
                )
                .filter(eh_download_queue::Column::Id.eq(delivery_id))
                .filter(claim_generation_filter(expected.started_at))
                .filter(eh_download_queue::Column::JobId.eq(job_id))
                .filter(eh_download_queue::Column::Telegraph.eq(true))
                .filter(eh_download_queue::Column::Status.eq(DELIVERY_STATUS_PUBLISHING))
                .filter(eh_download_queue::Column::TelegraphSentAt.is_null())
                .exec(&txn)
                .await
                .context("Failed to mark shared EH Telegraph delivery sent")?;
            let (sent_at, chat_id, gid) = if marked.rows_affected == 1 {
                (now, delivery.chat_id, delivery.gid)
            } else {
                let already_marked = eh_download_queue::Entity::find()
                    .filter(eh_download_queue::Column::Id.eq(delivery_id))
                    .filter(claim_generation_filter(expected.started_at))
                    .filter(eh_download_queue::Column::JobId.eq(job_id))
                    .filter(eh_download_queue::Column::Telegraph.eq(true))
                    .filter(eh_download_queue::Column::Status.eq(DELIVERY_STATUS_PUBLISHING))
                    .filter(eh_download_queue::Column::TelegraphSentAt.is_not_null())
                    .one(&txn)
                    .await
                    .context("Failed to verify shared EH Telegraph sent marker")?;
                let Some(already_marked) = already_marked else {
                    anyhow::bail!(
                        "Cannot mark Telegraph sent for EH delivery {} on job {}: publishing claim changed",
                        delivery_id,
                        job_id
                    );
                };
                (
                    already_marked.telegraph_sent_at.context(
                        "Shared EH Telegraph sent marker disappeared during verification",
                    )?,
                    already_marked.chat_id,
                    already_marked.gid,
                )
            };
            record_eh_push_in_txn(
                &txn,
                chat_id,
                gid,
                EhPushSurface::Telegraph,
                sent_at,
            )
            .await?;

            if let Some(delay_secs) = rewrite_delay_secs {
                eh_gallery_jobs::Entity::update_many()
                    .set(eh_gallery_jobs::ActiveModel {
    telegraph_rewrite_status: Set(Some(TELEGRAPH_REWRITE_STATUS_PENDING.to_string())),
    telegraph_rewrite_after: Set(Some(
                            now.checked_add_signed(chrono::Duration::seconds(delay_secs))
                                .context("EH Telegraph rewrite deadline overflow")?,
                        )),
    telegraph_rewrite_next_retry_at: Set(None),
    telegraph_rewrite_retry_count: Set(0_i32),
    telegraph_rewrite_error: Set(None),
    ..Default::default()
})
                    .filter(eh_gallery_jobs::Column::Id.eq(job_id))
                    .filter(eh_gallery_jobs::Column::TelegraphRewriteData.is_not_null())
                    .filter(eh_gallery_jobs::Column::TelegraphRewriteStatus.is_null())
                    .filter(eh_gallery_jobs::Column::TelegraphRewriteAfter.is_null())
                    .filter(eh_gallery_jobs::Column::TelegraphRewriteStartedAt.is_null())
                    .filter(eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt.is_null())
                    .filter(eh_gallery_jobs::Column::TelegraphRewrittenAt.is_null())
                    .exec(&txn)
                    .await
                    .context("Failed to schedule shared EH Telegraph rewrite")?;
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => txn
                .commit()
                .await
                .context("Failed to commit EH Telegraph sent-marker transaction"),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back EH Telegraph sent-marker transaction")?;
                Err(error)
            }
        }
    }

    /// Claim one due shared Telegraph rewrite. The claim generation advances
    /// monotonically even after retry or stale-claim recovery.
    pub async fn get_next_eh_job_for_telegraph_rewrite(
        &self,
    ) -> Result<Option<eh_gallery_jobs::Model>> {
        let now = Local::now().naive_local();
        let job = eh_gallery_jobs::Entity::find()
            .filter(
                eh_gallery_jobs::Column::TelegraphRewriteStatus
                    .eq(TELEGRAPH_REWRITE_STATUS_PENDING),
            )
            .filter(eh_gallery_jobs::Column::TelegraphRewriteData.is_not_null())
            .filter(eh_gallery_jobs::Column::TelegraphRewrittenAt.is_null())
            .filter(
                sea_orm::Condition::any()
                    .add(eh_gallery_jobs::Column::TelegraphRewriteAfter.is_null())
                    .add(eh_gallery_jobs::Column::TelegraphRewriteAfter.lte(now)),
            )
            .filter(
                sea_orm::Condition::any()
                    .add(eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt.is_null())
                    .add(eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt.lte(now)),
            )
            .order_by(eh_gallery_jobs::Column::TelegraphRewriteAfter, Order::Asc)
            .one(&self.db)
            .await
            .context("Failed to fetch next shared EH Telegraph rewrite")?;
        let Some(job) = job else {
            return Ok(None);
        };

        let generation = next_job_claim_generation(now, job.telegraph_rewrite_started_at)?;
        let mut update = eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                telegraph_rewrite_status: Set(Some(TELEGRAPH_REWRITE_STATUS_REWRITING.to_string())),
                telegraph_rewrite_started_at: Set(Some(generation)),
                telegraph_rewrite_next_retry_at: Set(None),
                ..Default::default()
            })
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .filter(
                eh_gallery_jobs::Column::TelegraphRewriteStatus
                    .eq(TELEGRAPH_REWRITE_STATUS_PENDING),
            )
            .filter(eh_gallery_jobs::Column::TelegraphRewriteData.is_not_null())
            .filter(eh_gallery_jobs::Column::TelegraphRewrittenAt.is_null())
            .filter(
                sea_orm::Condition::any()
                    .add(eh_gallery_jobs::Column::TelegraphRewriteAfter.is_null())
                    .add(eh_gallery_jobs::Column::TelegraphRewriteAfter.lte(now)),
            )
            .filter(
                sea_orm::Condition::any()
                    .add(eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt.is_null())
                    .add(eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt.lte(now)),
            );
        update = update.filter(nullable_eq(
            eh_gallery_jobs::Column::TelegraphRewriteStartedAt,
            job.telegraph_rewrite_started_at,
        ));
        let claimed = update
            .exec(&self.db)
            .await
            .context("Failed to atomically claim shared EH Telegraph rewrite")?;
        if claimed.rows_affected == 0 {
            return Ok(None);
        }

        eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .filter(
                eh_gallery_jobs::Column::TelegraphRewriteStatus
                    .eq(TELEGRAPH_REWRITE_STATUS_REWRITING),
            )
            .filter(eh_gallery_jobs::Column::TelegraphRewriteStartedAt.eq(generation))
            .one(&self.db)
            .await
            .context("Failed to reread shared EH Telegraph rewrite claim")
    }

    /// Complete a shared Telegraph rewrite only when the caller still owns the
    /// claimed generation. Payload removal is deferred to liveness evaluation.
    pub async fn mark_eh_job_telegraph_rewritten(
        &self,
        job_id: i32,
        expected_started_at: DateTime,
    ) -> Result<bool> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH Telegraph rewrite completion transaction")?;
        let now = Local::now().naive_local();
        let result: Result<bool> = async {
            let Some(job) = eh_gallery_jobs::Entity::find_by_id(job_id)
                .filter(
                    eh_gallery_jobs::Column::TelegraphRewriteStatus
                        .eq(TELEGRAPH_REWRITE_STATUS_REWRITING),
                )
                .filter(eh_gallery_jobs::Column::TelegraphRewriteStartedAt.eq(expected_started_at))
                .one(&txn)
                .await
                .context("Failed to select shared EH Telegraph rewrite completion")?
            else {
                return Ok(false);
            };
            let updated = eh_gallery_jobs::Entity::update_many()
                .set(eh_gallery_jobs::ActiveModel {
                    telegraph_rewrite_status: Set(None),
                    telegraph_rewrite_after: Set(None),
                    telegraph_rewrite_next_retry_at: Set(None),
                    telegraph_rewrite_retry_count: Set(0_i32),
                    telegraph_rewrite_error: Set(None),
                    telegraph_rewritten_at: Set(Some(now)),
                    ..Default::default()
                })
                .filter(eh_gallery_jobs::Column::Id.eq(job_id))
                .filter(
                    eh_gallery_jobs::Column::TelegraphRewriteStatus
                        .eq(TELEGRAPH_REWRITE_STATUS_REWRITING),
                )
                .filter(eh_gallery_jobs::Column::TelegraphRewriteStartedAt.eq(expected_started_at))
                .exec(&txn)
                .await
                .context("Failed to complete shared EH Telegraph rewrite")?;
            if updated.rows_affected != 1 {
                return Ok(false);
            }
            clear_rewritten_eh_gallery_result_in_txn(&txn, &job).await?;
            Ok(true)
        }
        .await;
        match result {
            Ok(true) => txn
                .commit()
                .await
                .context("Failed to commit shared EH Telegraph rewrite completion")
                .map(|()| true),
            Ok(false) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back stale shared EH Telegraph rewrite completion")?;
                Ok(false)
            }
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back shared EH Telegraph rewrite completion")?;
                Err(error)
            }
        }
    }

    /// Retry a claimed shared Telegraph rewrite. Returns true only when the
    /// matching generation becomes terminally failed; retries and stale calls
    /// both return false and retain the rewrite payload.
    pub async fn schedule_eh_job_telegraph_rewrite_retry(
        &self,
        job_id: i32,
        expected_started_at: DateTime,
        error: &str,
        max_retry_count: u8,
    ) -> Result<bool> {
        let Some(job) = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&self.db)
            .await
            .context("Failed to fetch shared EH Telegraph rewrite for retry")?
        else {
            return Ok(false);
        };
        if job.telegraph_rewrite_status.as_deref() != Some(TELEGRAPH_REWRITE_STATUS_REWRITING)
            || job.telegraph_rewrite_started_at != Some(expected_started_at)
            || job.telegraph_rewrite_data.is_none()
            || job.telegraph_rewritten_at.is_some()
        {
            return Ok(false);
        }
        let retry_count = job
            .telegraph_rewrite_retry_count
            .checked_add(1)
            .context("Shared EH Telegraph rewrite retry count overflow")?;
        let terminal = retry_count > i32::from(max_retry_count);
        let now = Local::now().naive_local();

        let mut update = eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                telegraph_rewrite_status: Set(Some(
                    if terminal {
                        TELEGRAPH_REWRITE_STATUS_FAILED
                    } else {
                        TELEGRAPH_REWRITE_STATUS_PENDING
                    }
                    .to_string(),
                )),
                telegraph_rewrite_next_retry_at: Set(if terminal {
                    None
                } else {
                    Some(
                        now.checked_add_signed(chrono::Duration::seconds(
                            Self::backoff_delay_secs(retry_count),
                        ))
                        .context("Shared EH Telegraph rewrite retry deadline overflow")?,
                    )
                }),
                telegraph_rewrite_retry_count: Set(retry_count),
                telegraph_rewrite_error: Set(Some(error.to_string())),
                ..Default::default()
            })
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(
                eh_gallery_jobs::Column::TelegraphRewriteStatus
                    .eq(TELEGRAPH_REWRITE_STATUS_REWRITING),
            )
            .filter(eh_gallery_jobs::Column::TelegraphRewriteData.is_not_null())
            .filter(eh_gallery_jobs::Column::TelegraphRewriteStartedAt.eq(expected_started_at));
        if terminal {
            update = update.filter(eh_gallery_jobs::Column::TelegraphRewrittenAt.is_null());
        }
        let updated = update
            .exec(&self.db)
            .await
            .context("Failed to persist shared EH Telegraph rewrite retry")?;
        Ok(terminal && updated.rows_affected == 1)
    }

    /// Record one failed shared Telegraph upload generation. Terminal failure
    /// returns only deliveries newly transitioned by this transaction, so a
    /// repeated or stale caller cannot fan out duplicate notifications.
    pub async fn record_eh_job_upload_failure(
        &self,
        job_id: i32,
        expected_started_at: DateTime,
        error: &str,
        max_retry_count: u8,
        send_archive: bool,
    ) -> Result<EhJobUploadFailureOutcome> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH gallery upload failure transaction")?;
        let Some(job) = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&txn)
            .await
            .context("Failed to select shared EH gallery job for upload failure")?
        else {
            txn.rollback().await?;
            return Ok(EhJobUploadFailureOutcome::Stale);
        };
        if job.telegraph_status != TELEGRAPH_STATUS_UPLOADING
            || job.started_at != Some(expected_started_at)
        {
            txn.rollback().await?;
            return Ok(EhJobUploadFailureOutcome::Stale);
        }

        let retry_count = job
            .retry_count
            .checked_add(1)
            .context("Shared EH gallery upload retry count overflow")?;
        if retry_count <= i32::from(max_retry_count) {
            let retry_at = Local::now().naive_local()
                + chrono::Duration::seconds(Self::backoff_delay_secs(retry_count));
            let updated = eh_gallery_jobs::Entity::update_many()
                .set(eh_gallery_jobs::ActiveModel {
                    telegraph_status: Set(TELEGRAPH_STATUS_PENDING.to_string()),
                    error: Set(Some(error.to_string())),
                    retry_count: Set(retry_count),
                    next_retry_at: Set(Some(retry_at)),
                    ..Default::default()
                })
                .filter(eh_gallery_jobs::Column::Id.eq(job_id))
                .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_UPLOADING))
                .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at))
                .exec(&txn)
                .await
                .context("Failed to schedule shared EH gallery upload retry")?;
            if updated.rows_affected != 1 {
                txn.rollback().await?;
                return Ok(EhJobUploadFailureOutcome::Stale);
            }
            self.recompute_eh_job_telegraph_requirement_with_policy_in_txn(
                &txn,
                job_id,
                send_archive,
            )
            .await?;
            self.evaluate_eh_job_liveness_in_txn(&txn, job_id, send_archive)
                .await?;
            let job = eh_gallery_jobs::Entity::find_by_id(job_id)
                .one(&txn)
                .await
                .context("Failed to reread shared EH gallery upload retry")?
                .context("Shared EH gallery job disappeared after upload retry")?;
            txn.commit()
                .await
                .context("Failed to commit shared EH gallery upload retry")?;
            return Ok(EhJobUploadFailureOutcome::RetryScheduled(job));
        }

        let failed = eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                telegraph_status: Set(TELEGRAPH_STATUS_FAILED.to_string()),
                error: Set(Some(error.to_string())),
                retry_count: Set(retry_count),
                next_retry_at: Set(None),
                ..Default::default()
            })
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_UPLOADING))
            .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at))
            .exec(&txn)
            .await
            .context("Failed to mark shared EH gallery upload terminally failed")?;
        if failed.rows_affected != 1 {
            txn.rollback().await?;
            return Ok(EhJobUploadFailureOutcome::Stale);
        }

        let candidates = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::JobId.eq(job_id))
            .filter(eh_download_queue::Column::Telegraph.eq(true))
            .filter(eh_download_queue::Column::TelegraphSentAt.is_null())
            .filter(
                eh_download_queue::Column::Status
                    .is_in([DELIVERY_STATUS_WAITING, DELIVERY_STATUS_PUBLISHING]),
            )
            .order_by(eh_download_queue::Column::Id, Order::Asc)
            .all(&txn)
            .await
            .context("Failed to select Telegraph deliveries for terminal upload failure")?;
        let now = Local::now().naive_local();
        let mut deliveries = Vec::with_capacity(candidates.len());
        for delivery in candidates {
            let mut transition = eh_download_queue::Entity::update_many()
                .filter(eh_download_queue::Column::Id.eq(delivery.id))
                .filter(eh_download_queue::Column::JobId.eq(job_id))
                .filter(eh_download_queue::Column::Telegraph.eq(true))
                .filter(eh_download_queue::Column::TelegraphSentAt.is_null())
                .filter(eh_download_queue::Column::Status.eq(&delivery.status));
            if send_archive {
                transition = transition.set(eh_download_queue::ActiveModel {
                    telegraph: Set(false),
                    telegraph_subscription_ids: Set(None),
                    status: Set((if delivery.archive_sent_at.is_some() {
                        DELIVERY_STATUS_DONE
                    } else {
                        DELIVERY_STATUS_WAITING
                    })
                    .to_string()),
                    next_retry_at: Set(None),
                    ..Default::default()
                });
                if delivery.archive_sent_at.is_some() {
                    transition = transition.col_expr(
                        eh_download_queue::Column::CompletedAt,
                        Expr::value(Some(now)),
                    );
                } else {
                    transition = transition.set(eh_download_queue::ActiveModel {
                        completed_at: Set(None),
                        ..Default::default()
                    });
                }
            } else {
                transition = transition.set(eh_download_queue::ActiveModel {
                    status: Set(DELIVERY_STATUS_FAILED.to_string()),
                    completed_at: Set(Some(now)),
                    next_retry_at: Set(None),
                    ..Default::default()
                });
            }
            let transitioned = transition
                .filter(nullable_eq(
                    eh_download_queue::Column::ArchiveSentAt,
                    delivery.archive_sent_at,
                ))
                .exec(&txn)
                .await
                .context(
                    "Failed to transition one Telegraph delivery after terminal upload failure",
                )?;
            if transitioned.rows_affected == 1 {
                deliveries.push(EhFailedTelegraphDelivery {
                    delivery_id: delivery.id,
                    chat_id: delivery.chat_id,
                    title: delivery.title,
                });
            }
        }
        self.recompute_eh_job_telegraph_requirement_with_policy_in_txn(&txn, job_id, send_archive)
            .await?;
        self.evaluate_eh_job_liveness_in_txn(&txn, job_id, send_archive)
            .await?;

        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(&txn)
            .await
            .context("Failed to reread terminally failed shared EH gallery upload")?
            .context("Shared EH gallery job disappeared after terminal upload failure")?;
        txn.commit()
            .await
            .context("Failed to commit terminal shared EH gallery upload failure")?;
        Ok(EhJobUploadFailureOutcome::Terminal { job, deliveries })
    }

    /// Release a claimed upload when no active destination can currently
    /// receive Telegraph. This is deliberately not a provider retry: it keeps
    /// the upload generation, retry budget, error, and artifact ownership.
    pub async fn defer_eh_job_upload(
        &self,
        job_id: i32,
        expected_started_at: DateTime,
        delay_secs: i64,
        send_archive: bool,
    ) -> Result<bool> {
        let retry_at = Local::now()
            .naive_local()
            .checked_add_signed(chrono::Duration::seconds(delay_secs.max(1)))
            .context("Shared EH gallery upload defer deadline overflow")?;
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH gallery upload defer transaction")?;
        let result: Result<bool> = async {
            let deferred = eh_gallery_jobs::Entity::update_many()
                .set(eh_gallery_jobs::ActiveModel {
                    telegraph_status: Set(TELEGRAPH_STATUS_PENDING.to_string()),
                    next_retry_at: Set(Some(retry_at)),
                    ..Default::default()
                })
                .filter(eh_gallery_jobs::Column::Id.eq(job_id))
                .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_UPLOADING))
                .filter(eh_gallery_jobs::Column::StartedAt.eq(expected_started_at))
                .exec(&txn)
                .await
                .context("Failed to defer shared EH gallery upload")?;
            if deferred.rows_affected != 1 {
                return Ok(false);
            }

            self.recompute_eh_job_telegraph_requirement_with_policy_in_txn(
                &txn,
                job_id,
                send_archive,
            )
            .await?;
            self.evaluate_eh_job_liveness_in_txn(&txn, job_id, send_archive)
                .await?;
            Ok(true)
        }
        .await;
        match result {
            Ok(deferred) => txn
                .commit()
                .await
                .context("Failed to commit shared EH gallery upload defer transaction")
                .map(|()| deferred),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back shared EH gallery upload defer transaction")?;
                Err(error)
            }
        }
    }

    /// When no Telegraph client exists, stop only job-level uploads that have
    /// not claimed an upload generation.  Ready links and all rewrite payloads
    /// are intentionally left untouched.
    pub async fn disable_eh_telegraph_for_unuploaded_jobs(&self) -> Result<u64> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH Telegraph disable transaction")?;
        let result: Result<u64> = async {
            let jobs = eh_gallery_jobs::Entity::find()
                .filter(eh_gallery_jobs::Column::TelegraphRequired.eq(true))
                .filter(
                    eh_gallery_jobs::Column::TelegraphStatus
                        .is_in([TELEGRAPH_STATUS_NOT_REQUIRED, TELEGRAPH_STATUS_PENDING]),
                )
                .order_by(eh_gallery_jobs::Column::Id, Order::Asc)
                .all(&txn)
                .await
                .context("Failed to select unclaimed shared EH Telegraph jobs")?;
            let mut changed = 0;
            for job in jobs {
                let deliveries = eh_download_queue::Entity::update_many()
                    .set(eh_download_queue::ActiveModel {
                        telegraph: Set(false),
                        telegraph_subscription_ids: Set(None),
                        ..Default::default()
                    })
                    .filter(eh_download_queue::Column::JobId.eq(job.id))
                    .filter(eh_download_queue::Column::Telegraph.eq(true))
                    .filter(eh_download_queue::Column::TelegraphSentAt.is_null())
                    .filter(
                        eh_download_queue::Column::Status
                            .is_in([DELIVERY_STATUS_WAITING, DELIVERY_STATUS_PUBLISHING]),
                    )
                    .exec(&txn)
                    .await
                    .context("Failed to remove unclaimed shared EH Telegraph delivery intent")?;
                if deliveries.rows_affected == 0 {
                    continue;
                }
                self.recompute_eh_job_telegraph_requirement_with_policy_in_txn(&txn, job.id, true)
                    .await?;
                changed += 1;
            }
            Ok(changed)
        }
        .await;
        match result {
            Ok(changed) => txn
                .commit()
                .await
                .context("Failed to commit shared EH Telegraph disable transaction")
                .map(|()| changed),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back shared EH Telegraph disable transaction")?;
                Err(error)
            }
        }
    }
}
