use super::*;

impl Repo {
    /// Reopen a cleaned rewrite-retained job only when this waiting delivery
    /// still needs its archive. The source claim is intentionally deferred to
    /// the policy-aware download selector after this transaction commits.
    async fn recover_eh_job_for_missing_archive_in_txn(
        &self,
        txn: &DatabaseTransaction,
        delivery: &eh_download_queue::Model,
        job: &eh_gallery_jobs::Model,
        send_archive: bool,
    ) -> Result<bool> {
        if !send_archive
            || delivery.archive_sent_at.is_some()
            || job.status != JOB_STATUS_DOWNLOADED
            || job.zip_path.is_some()
            || job.cleanup_status != CLEANUP_STATUS_NONE
            || job.background_download_status.is_some()
            || job.telegraph_status == TELEGRAPH_STATUS_UPLOADING
        {
            return Ok(false);
        }

        let delivery_still_waiting_for_archive = Expr::exists(
            Query::select()
                .expr(Expr::value(1))
                .from(eh_download_queue::Entity)
                .and_where(eh_download_queue::Column::Id.eq(delivery.id))
                .and_where(eh_download_queue::Column::JobId.eq(job.id))
                .and_where(eh_download_queue::Column::Status.eq(STATUS_WAITING))
                .and_where(eh_download_queue::Column::ArchiveSentAt.is_null())
                .and_where(optional_eh_delivery_datetime_filter(
                    eh_download_queue::Column::StartedAt,
                    delivery.started_at,
                ))
                .and_where(optional_eh_delivery_datetime_filter(
                    eh_download_queue::Column::NextRetryAt,
                    delivery.next_retry_at,
                ))
                .to_owned(),
        );
        let recovered = eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                status: Set(JOB_STATUS_PENDING.to_string()),
                error: Set(None),
                retry_count: Set(0_i32),
                next_retry_at: Set(None),
                completed_at: Set(None),
                background_download_status: Set(None),
                background_download_started_at: Set(None),
                background_download_next_retry_at: Set(None),
                background_download_attempt_count: Set(0_i32),
                background_download_error: Set(None),
                ..Default::default()
            })
            .filter(eh_gallery_jobs::Column::Id.eq(job.id))
            .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADED))
            .filter(eh_gallery_jobs::Column::ZipPath.is_null())
            .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
            .filter(eh_gallery_jobs::Column::BackgroundDownloadStatus.is_null())
            .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(&job.telegraph_status))
            .filter(eh_gallery_jobs::Column::TelegraphRequired.eq(job.telegraph_required))
            .filter(eh_job_claim_generation_filter(job.started_at))
            .filter(delivery_still_waiting_for_archive)
            .exec(txn)
            .await
            .context("Failed to recover shared EH job with a missing archive")?;
        Ok(recovered.rows_affected == 1)
    }

    /// Claim the next due shared-gallery delivery ready to send or finalize.
    /// Legacy rows without a `job_id` are never claimed by this runtime lane.
    pub async fn get_next_eh_delivery_for_publish(
        &self,
        send_archive: bool,
    ) -> Result<Option<EhDeliveryClaim>> {
        for _ in 0..SQLITE_TRANSACTION_ATTEMPTS {
            match self
                .get_next_eh_delivery_for_publish_once(send_archive)
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
        self.get_next_eh_delivery_for_publish_once(send_archive)
            .await
    }

    async fn get_next_eh_delivery_for_publish_once(
        &self,
        send_archive: bool,
    ) -> Result<Option<EhDeliveryClaim>> {
        let now = Local::now().naive_local();
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH delivery publish claim transaction")?;
        let result = async {
            let mut committed_state_transition = false;
            if send_archive {
                let missing_archive_candidates = eh_download_queue::Entity::find()
                    .find_also_related(eh_gallery_jobs::Entity)
                    .filter(eh_download_queue::Column::Status.eq(STATUS_WAITING))
                    .filter(eh_download_queue::Column::JobId.is_not_null())
                    .filter(eh_download_queue::Column::ArchiveSentAt.is_null())
                    .filter(eh_gallery_jobs::Column::Status.eq(JOB_STATUS_DOWNLOADED))
                    .filter(eh_gallery_jobs::Column::ZipPath.is_null())
                    .filter(eh_gallery_jobs::Column::CleanupStatus.eq(CLEANUP_STATUS_NONE))
                    .filter(eh_gallery_jobs::Column::BackgroundDownloadStatus.is_null())
                    .filter(eh_gallery_jobs::Column::TelegraphStatus.ne(TELEGRAPH_STATUS_UPLOADING))
                    .filter(
                        eh_download_queue::Column::NextRetryAt
                            .is_null()
                            .or(eh_download_queue::Column::NextRetryAt.lte(now)),
                    )
                    .order_by(eh_download_queue::Column::CreatedAt, Order::Asc)
                    .order_by(eh_download_queue::Column::Id, Order::Asc)
                    .limit(PUBLISH_MAINTENANCE_BATCH_SIZE)
                    .all(&txn)
                    .await
                    .context("Failed to fetch missing-archive shared EH deliveries for publish")?;
                for (delivery, job) in missing_archive_candidates {
                    let Some(job) = job else {
                        continue;
                    };
                    if self
                        .recover_eh_job_for_missing_archive_in_txn(
                            &txn,
                            &delivery,
                            &job,
                            send_archive,
                        )
                        .await?
                    {
                        committed_state_transition = true;
                    }
                }
            }

            let Some((delivery, Some(job))) = eh_download_queue::Entity::find()
                .find_also_related(eh_gallery_jobs::Entity)
                .filter(eh_download_queue::Column::Status.eq(STATUS_WAITING))
                .filter(eh_download_queue::Column::JobId.is_not_null())
                .filter(eh_chat_has_no_publishing_delivery_filter())
                .filter(
                    eh_download_queue::Column::NextRetryAt
                        .is_null()
                        .or(eh_download_queue::Column::NextRetryAt.lte(now)),
                )
                .filter(eh_delivery_ready_for_publish_filter(send_archive))
                .order_by(eh_download_queue::Column::CreatedAt, Order::Asc)
                .order_by(eh_download_queue::Column::Id, Order::Asc)
                .one(&txn)
                .await
                .context("Failed to fetch next ready shared EH delivery for publish")?
            else {
                return Ok((None, committed_state_transition));
            };
            debug_assert!(eh_delivery_is_ready_for_publish(
                &delivery,
                &job,
                send_archive
            ));

            let generation = next_claim_generation(now, delivery.started_at)?;
            let claimed = eh_download_queue::Entity::update_many()
                .set(eh_download_queue::ActiveModel {
                    status: Set(STATUS_PUBLISHING.to_string()),
                    started_at: Set(Some(generation)),
                    next_retry_at: Set(None),
                    ..Default::default()
                })
                .filter(eh_download_queue::Column::Id.eq(delivery.id))
                .filter(eh_download_queue::Column::JobId.eq(job.id))
                .filter(eh_download_queue::Column::Status.eq(STATUS_WAITING))
                .filter(claim_generation_filter(delivery.started_at))
                .filter(eh_chat_has_no_publishing_delivery_filter())
                .filter(
                    eh_download_queue::Column::NextRetryAt
                        .is_null()
                        .or(eh_download_queue::Column::NextRetryAt.lte(now)),
                )
                .exec(&txn)
                .await
                .context("Failed to atomically claim shared EH delivery for publish")?;
            if claimed.rows_affected == 0 {
                return Ok((None, committed_state_transition));
            }

            let claimed_delivery = eh_download_queue::Entity::find()
                .filter(eh_download_queue::Column::Id.eq(delivery.id))
                .filter(eh_download_queue::Column::JobId.eq(job.id))
                .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
                .filter(eh_download_queue::Column::StartedAt.eq(generation))
                .one(&txn)
                .await
                .context("Failed to reread shared EH delivery publish claim")?
                .context("Shared EH delivery changed before publish claim readback")?;
            let claimed_job = eh_gallery_jobs::Entity::find_by_id(job.id)
                .one(&txn)
                .await
                .context("Failed to reread shared EH job for delivery claim")?
                .context("Shared EH job changed before delivery claim readback")?;
            Ok((
                Some(EhDeliveryClaim {
                    delivery: claimed_delivery,
                    job: claimed_job,
                }),
                committed_state_transition,
            ))
        }
        .await;

        match result {
            Ok((Some(claim), _)) => {
                txn.commit()
                    .await
                    .context("Failed to commit shared EH delivery publish claim transaction")?;
                Ok(Some(claim))
            }
            Ok((None, true)) => txn
                .commit()
                .await
                .context("Failed to commit shared EH delivery state transition transaction")
                .map(|()| None),
            Ok((None, false)) => {
                txn.rollback().await.context(
                    "Failed to roll back empty shared EH delivery publish claim transaction",
                )?;
                Ok(None)
            }
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back shared EH delivery publish claim transaction")?;
                Err(error)
            }
        }
    }

    /// Recover abandoned publish claims without erasing their send progress or
    /// generation. The supervisor excludes every locally running task, including
    /// tasks waiting for a chat lock, and cancels overdue tasks before releasing
    /// their claims. Never use the startup reset while workers are running.
    pub async fn reset_stale_eh_publish_deliveries(
        &self,
        cutoff: DateTime,
        active_delivery_ids: &[i32],
    ) -> Result<u64> {
        let reset = eh_download_queue::Entity::update_many()
            .set(eh_download_queue::ActiveModel {
                status: Set(STATUS_WAITING.to_string()),
                next_retry_at: Set(None),
                error: Set(Some("Recovered abandoned EH publish claim".to_string())),
                ..Default::default()
            })
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .filter(
                eh_download_queue::Column::StartedAt
                    .is_null()
                    .or(eh_download_queue::Column::StartedAt.lte(cutoff)),
            )
            .filter(eh_download_queue::Column::Id.is_not_in(active_delivery_ids.iter().copied()))
            .exec(&self.db)
            .await
            .context("Failed to recover abandoned EH publish claims")?;
        Ok(reset.rows_affected)
    }

    /// Re-read a currently publishing delivery with its shared job. Publish
    /// workers call this after acquiring the keyed chat lock so cancellation,
    /// marker progress, and job readiness are observed as one fresh snapshot.
    pub async fn get_eh_delivery_publish_claim(
        &self,
        expected: &eh_download_queue::Model,
        send_archive: bool,
    ) -> Result<Option<EhDeliveryClaim>> {
        let delivery_id = expected.id;
        let Some((delivery, job)) = eh_download_queue::Entity::find()
            .find_also_related(eh_gallery_jobs::Entity)
            .filter(eh_download_queue::Column::Id.eq(delivery_id))
            .filter(claim_generation_filter(expected.started_at))
            .filter(job_id_filter(expected.job_id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .one(&self.db)
            .await
            .context("Failed to reread shared EH publishing delivery")?
        else {
            return Ok(None);
        };
        let Some(job) = job else {
            return Ok(None);
        };
        if !eh_delivery_is_ready_for_publish(&delivery, &job, send_archive) {
            eh_download_queue::Entity::update_many()
                .set(eh_download_queue::ActiveModel {
                    status: Set(STATUS_WAITING.to_string()),
                    next_retry_at: Set(None),
                    ..Default::default()
                })
                .filter(eh_download_queue::Column::Id.eq(delivery.id))
                .filter(claim_generation_filter(expected.started_at))
                .filter(eh_download_queue::Column::JobId.eq(job.id))
                .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
                .exec(&self.db)
                .await
                .context("Failed to release unready shared EH publishing delivery")?;
            return Ok(None);
        }
        Ok(Some(EhDeliveryClaim { delivery, job }))
    }

    /// Check whether a claimed delivery still has a live owner. Subscription
    /// ownership uses the existing cancellation-safe check; direct deliveries
    /// remain active while their expected claim status survives.
    pub async fn eh_delivery_is_active(
        &self,
        delivery_id: i32,
        expected_status: &str,
        send_archive: bool,
    ) -> Result<bool> {
        self.eh_download_is_active(delivery_id, expected_status, send_archive)
            .await
    }

    /// Release a claimed delivery without burning a retry. A positive delay is
    /// required so a tick that is refilling workers cannot reclaim it forever.
    pub async fn defer_eh_delivery_publish(
        &self,
        expected: &eh_download_queue::Model,
        delay_secs: i64,
    ) -> Result<()> {
        let delivery_id = expected.id;
        let retry_at = Local::now().naive_local() + chrono::Duration::seconds(delay_secs.max(1));
        let result = eh_download_queue::Entity::update_many()
            .set(eh_download_queue::ActiveModel {
                status: Set(STATUS_WAITING.to_string()),
                next_retry_at: Set(Some(retry_at)),
                ..Default::default()
            })
            .filter(eh_download_queue::Column::Id.eq(delivery_id))
            .filter(claim_generation_filter(expected.started_at))
            .filter(job_id_filter(expected.job_id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .exec(&self.db)
            .await
            .context("Failed to defer shared EH delivery publish")?;
        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot defer shared EH delivery {}: publishing claim changed",
                delivery_id
            );
        }
        Ok(())
    }

    /// Retry only the failed chat delivery. Shared download/upload/page state
    /// remains on its job and is intentionally untouched.
    pub async fn schedule_eh_delivery_retry(
        &self,
        expected: &eh_download_queue::Model,
        error: &str,
        max_retry_count: u8,
        send_archive: bool,
    ) -> Result<(eh_download_queue::Model, bool)> {
        for _ in 0..SQLITE_TRANSACTION_ATTEMPTS {
            match self
                .schedule_eh_delivery_retry_once(expected, error, max_retry_count, send_archive)
                .await
            {
                Ok(outcome) => return Ok(outcome),
                Err(err) => {
                    if !is_retryable_sqlite_lock_error(&err) {
                        return Err(err);
                    }
                }
            }
        }
        self.schedule_eh_delivery_retry_once(expected, error, max_retry_count, send_archive)
            .await
    }

    async fn schedule_eh_delivery_retry_once(
        &self,
        expected: &eh_download_queue::Model,
        error: &str,
        max_retry_count: u8,
        send_archive: bool,
    ) -> Result<(eh_download_queue::Model, bool)> {
        let delivery_id = expected.id;
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH delivery retry transaction")?;
        let result: Result<(eh_download_queue::Model, bool)> = async {
            let delivery = eh_download_queue::Entity::find_by_id(delivery_id)
                .one(&txn)
                .await
                .context("Failed to fetch shared EH delivery for retry")?
                .context("Shared EH delivery disappeared before retry")?;
            let retry_count = delivery
                .retry_count
                .checked_add(1)
                .context("Shared EH delivery retry count overflow")?;
            let terminal = retry_count > i32::from(max_retry_count);
            let now = Local::now().naive_local();
            let mut update = eh_download_queue::Entity::update_many()
                .set(eh_download_queue::ActiveModel {
                    status: Set((if terminal {
                        STATUS_FAILED
                    } else {
                        STATUS_WAITING
                    })
                    .to_string()),
                    error: Set(Some(error.to_string())),
                    retry_count: Set(retry_count),
                    ..Default::default()
                })
                .filter(eh_download_queue::Column::Id.eq(delivery_id))
                .filter(claim_generation_filter(expected.started_at))
                .filter(job_id_filter(expected.job_id))
                .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING));
            if terminal {
                update = update.set(eh_download_queue::ActiveModel {
                    completed_at: Set(Some(now)),
                    next_retry_at: Set(None),
                    ..Default::default()
                });
            } else {
                let retry_at = now
                    .checked_add_signed(chrono::Duration::seconds(Self::backoff_delay_secs(
                        retry_count,
                    )))
                    .context("Shared EH delivery retry deadline overflow")?;
                update = update.col_expr(
                    eh_download_queue::Column::NextRetryAt,
                    Expr::value(Some(retry_at)),
                );
            }
            let result = update
                .exec(&txn)
                .await
                .context("Failed to schedule shared EH delivery retry")?;
            if result.rows_affected != 1 {
                anyhow::bail!(
                    "Cannot retry shared EH delivery {}: publishing claim changed",
                    delivery_id
                );
            }
            if terminal {
                if let Some(job_id) = delivery.job_id {
                    self.recompute_eh_job_telegraph_requirement_in_txn(&txn, job_id)
                        .await?;
                    self.evaluate_eh_job_liveness_in_txn(&txn, job_id, send_archive)
                        .await?;
                }
            }
            let updated = eh_download_queue::Entity::find_by_id(delivery_id)
                .one(&txn)
                .await
                .context("Failed to reread shared EH delivery after retry")?
                .context("Shared EH delivery disappeared after retry")?;
            Ok((updated, terminal))
        }
        .await;
        match result {
            Ok(outcome) => txn
                .commit()
                .await
                .context("Failed to commit shared EH delivery retry transaction")
                .map(|()| outcome),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back shared EH delivery retry transaction")?;
                Err(error)
            }
        }
    }

    /// Persist the archive marker for one delivery immediately after Telegram
    /// accepts its document. The marker is deliberately delivery-local.
    pub async fn mark_eh_archive_delivery_sent(
        &self,
        expected: &eh_download_queue::Model,
    ) -> Result<()> {
        for _ in 0..SQLITE_TRANSACTION_ATTEMPTS {
            match self.mark_eh_archive_delivery_sent_once(expected).await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    if !is_retryable_sqlite_lock_error(&error) {
                        return Err(error);
                    }
                }
            }
        }
        self.mark_eh_archive_delivery_sent_once(expected).await
    }

    async fn mark_eh_archive_delivery_sent_once(
        &self,
        expected: &eh_download_queue::Model,
    ) -> Result<()> {
        let delivery_id = expected.id;
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH archive sent-marker transaction")?;
        let result: Result<()> = async {
            let delivery = eh_download_queue::Entity::find_by_id(delivery_id)
                .one(&txn)
                .await
                .context("Failed to read shared EH delivery for archive sent marker")?
                .context("Shared EH delivery disappeared before archive sent marker")?;
            let sent_at = Local::now().naive_local();
            let marked = eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::ArchiveSentAt,
                    Expr::value(Some(sent_at)),
                )
                .filter(eh_download_queue::Column::Id.eq(delivery_id))
                .filter(claim_generation_filter(expected.started_at))
                .filter(job_id_filter(expected.job_id))
                .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
                .filter(eh_download_queue::Column::ArchiveSentAt.is_null())
                .exec(&txn)
                .await
                .context("Failed to mark shared EH archive delivery sent")?;
            if marked.rows_affected != 1 {
                anyhow::bail!(
                    "Cannot mark archive sent for shared EH delivery {}: publishing claim changed",
                    delivery_id
                );
            }
            record_eh_push_in_txn(
                &txn,
                delivery.chat_id,
                delivery.gid,
                EhPushSurface::Archive,
                sent_at,
            )
            .await
        }
        .await;
        match result {
            Ok(()) => txn
                .commit()
                .await
                .context("Failed to commit shared EH archive sent-marker transaction"),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back shared EH archive sent-marker transaction")?;
                Err(error)
            }
        }
    }

    /// Finish only this delivery after all of its enabled/requested surfaces
    /// have durable sent markers.
    pub async fn mark_eh_delivery_done(
        &self,
        expected: &eh_download_queue::Model,
        send_archive: bool,
    ) -> Result<eh_download_queue::Model> {
        for _ in 0..SQLITE_TRANSACTION_ATTEMPTS {
            match self
                .mark_eh_delivery_done_once(expected, send_archive)
                .await
            {
                Ok(delivery) => return Ok(delivery),
                Err(error) => {
                    if !is_retryable_sqlite_lock_error(&error) {
                        return Err(error);
                    }
                }
            }
        }
        self.mark_eh_delivery_done_once(expected, send_archive)
            .await
    }

    async fn mark_eh_delivery_done_once(
        &self,
        expected: &eh_download_queue::Model,
        send_archive: bool,
    ) -> Result<eh_download_queue::Model> {
        let delivery_id = expected.id;
        let expected_job_id = expected
            .job_id
            .context("EH delivery completion requires a shared job")?;
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin shared EH delivery completion transaction")?;
        let result: Result<eh_download_queue::Model> = async {
            let updated = eh_download_queue::Entity::update_many()
                .set(eh_download_queue::ActiveModel {
                    status: Set(STATUS_DONE.to_string()),
                    completed_at: Set(Some(Local::now().naive_local())),
                    next_retry_at: Set(None),
                    error: Set(None),
                    ..Default::default()
                })
                .filter(eh_download_queue::Column::Id.eq(delivery_id))
                .filter(claim_generation_filter(expected.started_at))
                .filter(eh_download_queue::Column::JobId.eq(expected_job_id))
                .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
                .exec(&txn)
                .await
                .context("Failed to mark shared EH delivery done")?;
            if updated.rows_affected != 1 {
                anyhow::bail!(
                    "Cannot complete shared EH delivery {}: publishing claim changed",
                    delivery_id
                );
            }
            self.recompute_eh_job_telegraph_requirement_in_txn(&txn, expected_job_id)
                .await?;
            self.evaluate_eh_job_liveness_in_txn(&txn, expected_job_id, send_archive)
                .await?;
            eh_download_queue::Entity::find_by_id(delivery_id)
                .one(&txn)
                .await
                .context("Failed to reread completed shared EH delivery")?
                .filter(|delivery| delivery.job_id == Some(expected_job_id))
                .context("Shared EH delivery disappeared or changed jobs after completion")
        }
        .await;
        match result {
            Ok(delivery) => txn
                .commit()
                .await
                .context("Failed to commit shared EH delivery completion transaction")
                .map(|()| delivery),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back shared EH delivery completion transaction")?;
                Err(error)
            }
        }
    }
}
