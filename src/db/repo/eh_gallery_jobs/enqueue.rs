use super::*;

async fn try_apply_cached_eh_result_in_txn(
    txn: &DatabaseTransaction,
    job_id: i32,
    send_archive: bool,
) -> Result<bool> {
    let Some(job) = eh_gallery_jobs::Entity::find_by_id(job_id)
        .one(txn)
        .await
        .context("Failed to select shared EH gallery job for cached-result reuse")?
    else {
        return Ok(false);
    };
    let Some(fingerprint) = job.source_fingerprint.as_deref() else {
        return Ok(false);
    };
    let variant = EhGalleryVariant {
        download_mode: job.download_mode.clone(),
        resolution: job.resolution.clone(),
    };
    let Some(cached_result) =
        find_eh_gallery_result_in_txn(txn, job.gid, &job.token, &variant).await?
    else {
        return Ok(false);
    };
    if cached_result.source_fingerprint != fingerprint
        || job.cleanup_status != CLEANUP_STATUS_NONE
        || job.telegraph_status == TELEGRAPH_STATUS_UPLOADING
    {
        return Ok(false);
    }

    let has_archive_demand =
        send_archive && has_active_eh_archive_delivery_in_txn(txn, job.id).await?;
    let make_zipless_ready = !has_archive_demand
        && job.status == JOB_STATUS_PENDING
        && job.background_download_status.is_none();
    let now = Local::now().naive_local();
    let mut update = eh_gallery_jobs::Entity::update_many()
        .set(eh_gallery_jobs::ActiveModel {
            telegraph_status: Set(TELEGRAPH_STATUS_READY.to_string()),
            telegraph_url: Set(Some(cached_result.telegraph_url)),
            telegraph_rewrite_data: Set(cached_result.telegraph_rewrite_data),
            telegraph_rewrite_status: Set(None),
            telegraph_rewrite_after: Set(None),
            telegraph_rewrite_started_at: Set(None),
            telegraph_rewrite_next_retry_at: Set(None),
            telegraph_rewrite_retry_count: Set(0_i32),
            telegraph_rewrite_error: Set(None),
            telegraph_rewritten_at: Set(None),
            ..Default::default()
        })
        .filter(eh_gallery_jobs::Column::Id.eq(job.id))
        .filter(eh_gallery_jobs::Column::Status.eq(&job.status))
        .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(&job.telegraph_status))
        .filter(eh_gallery_jobs::Column::SourceFingerprint.eq(fingerprint))
        .filter(eh_gallery_jobs::Column::CleanupStatus.eq(&job.cleanup_status))
        .filter(nullable_eq(
            eh_gallery_jobs::Column::StartedAt,
            job.started_at,
        ))
        .filter(nullable_eq(
            eh_gallery_jobs::Column::CleanupStartedAt,
            job.cleanup_started_at,
        ))
        .filter(nullable_eq(
            eh_gallery_jobs::Column::BackgroundDownloadStatus,
            job.background_download_status.as_deref(),
        ))
        .filter(nullable_eq(
            eh_gallery_jobs::Column::BackgroundDownloadStartedAt,
            job.background_download_started_at,
        ))
        .filter(nullable_eq(
            eh_gallery_jobs::Column::TelegraphRewriteData,
            job.telegraph_rewrite_data.as_deref(),
        ))
        .filter(nullable_eq(
            eh_gallery_jobs::Column::TelegraphRewriteStatus,
            job.telegraph_rewrite_status.as_deref(),
        ))
        .filter(nullable_eq(
            eh_gallery_jobs::Column::TelegraphRewriteAfter,
            job.telegraph_rewrite_after,
        ))
        .filter(nullable_eq(
            eh_gallery_jobs::Column::TelegraphRewriteStartedAt,
            job.telegraph_rewrite_started_at,
        ))
        .filter(nullable_eq(
            eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt,
            job.telegraph_rewrite_next_retry_at,
        ))
        .filter(
            eh_gallery_jobs::Column::TelegraphRewriteRetryCount
                .eq(job.telegraph_rewrite_retry_count),
        )
        .filter(nullable_eq(
            eh_gallery_jobs::Column::TelegraphRewriteError,
            job.telegraph_rewrite_error.as_deref(),
        ))
        .filter(nullable_eq(
            eh_gallery_jobs::Column::TelegraphRewrittenAt,
            job.telegraph_rewritten_at,
        ));
    if make_zipless_ready {
        update = update.set(eh_gallery_jobs::ActiveModel {
            status: Set(JOB_STATUS_DOWNLOADED.to_string()),
            zip_path: Set(None),
            file_size: Set(0_i64),
            gp_cost: Set(0_i64),
            completed_at: Set(Some(now)),
            ..Default::default()
        });
    }
    let updated = update
        .exec(txn)
        .await
        .context("Failed to apply cached EH gallery result")?;
    Ok(updated.rows_affected == 1)
}

impl Repo {
    /// Enqueue a direct EH request and atomically bind its delivery to the
    /// canonical shared-gallery job for the requested variant.
    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_eh_download(
        &self,
        chat_id: i64,
        gid: i64,
        token: &str,
        title: &str,
        telegraph: bool,
        source: &str,
        variant: &EhGalleryVariant,
        fingerprint: Option<&str>,
        send_archive: bool,
    ) -> Result<Option<eh_download_queue::Model>> {
        self.enqueue_eh_download_request(EhEnqueueRequest {
            chat_id,
            gid,
            token,
            title,
            fingerprint: fingerprint.map(str::to_owned),
            telegraph,
            send_archive,
            source,
            subscription_id: None,
            variant,
        })
        .await
    }

    /// Enqueue a scheduler-created delivery and retain subscription ownership
    /// while binding it to the requested shared-gallery job variant.
    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_eh_subscription_download(
        &self,
        chat_id: i64,
        subscription_id: i32,
        gid: i64,
        token: &str,
        title: &str,
        telegraph: bool,
        variant: &EhGalleryVariant,
        fingerprint: Option<&str>,
        send_archive: bool,
    ) -> Result<Option<eh_download_queue::Model>> {
        self.enqueue_eh_download_request(EhEnqueueRequest {
            chat_id,
            gid,
            token,
            title,
            fingerprint: fingerprint.map(str::to_owned),
            telegraph,
            send_archive,
            source: SOURCE_SUBSCRIPTION,
            subscription_id: Some(subscription_id),
            variant,
        })
        .await
    }

    async fn enqueue_eh_download_request(
        &self,
        req: EhEnqueueRequest<'_>,
    ) -> Result<Option<eh_download_queue::Model>> {
        // Keep enqueue in the same per-chat critical section as cancellation
        // and publish. In particular, a new request cannot rebind a delivery
        // after a publisher has read it but before it commits its markers.
        let _chat_guard = EH_CHAT_LOCKS.lock_chat(req.chat_id).await;
        for attempt in 0..MAX_ENQUEUE_TRANSACTION_ATTEMPTS {
            let txn = match self.db.begin().await {
                Ok(txn) => txn,
                Err(error) => {
                    if is_retryable_enqueue_db_error(&error)
                        && attempt + 1 < MAX_ENQUEUE_TRANSACTION_ATTEMPTS
                    {
                        continue;
                    }
                    return Err(error).context("Failed to begin shared EH enqueue transaction");
                }
            };
            match self.enqueue_eh_download_in_txn(&txn, &req).await {
                Ok(Some(delivery_id)) => match txn.commit().await {
                    Ok(()) => {
                        return eh_download_queue::Entity::find_by_id(delivery_id)
                            .one(&self.db)
                            .await
                            .context("Failed to reread shared EH delivery after enqueue")?
                            .context("Shared EH delivery disappeared after enqueue commit")
                            .map(Some);
                    }
                    Err(error) => {
                        if is_retryable_enqueue_db_error(&error)
                            && attempt + 1 < MAX_ENQUEUE_TRANSACTION_ATTEMPTS
                        {
                            continue;
                        }
                        return Err(error)
                            .context("Failed to commit shared EH enqueue transaction");
                    }
                },
                Ok(None) => match txn.commit().await {
                    Ok(()) => return Ok(None),
                    Err(error) => {
                        if is_retryable_enqueue_db_error(&error)
                            && attempt + 1 < MAX_ENQUEUE_TRANSACTION_ATTEMPTS
                        {
                            continue;
                        }
                        return Err(error)
                            .context("Failed to commit shared EH enqueue transaction");
                    }
                },
                Err(error) => {
                    let should_retry = is_retryable_enqueue_error(&error);
                    let rollback = txn.rollback().await;
                    if let Err(rollback_error) = rollback {
                        return Err(rollback_error)
                            .context("Failed to roll back shared EH enqueue transaction");
                    }
                    if should_retry && attempt + 1 < MAX_ENQUEUE_TRANSACTION_ATTEMPTS {
                        continue;
                    }
                    return Err(error).context("Failed to enqueue shared EH gallery delivery");
                }
            }
        }

        unreachable!("shared EH enqueue retry loop always returns")
    }

    async fn enqueue_eh_download_in_txn(
        &self,
        txn: &DatabaseTransaction,
        req: &EhEnqueueRequest<'_>,
    ) -> Result<Option<i32>> {
        let push_ledger = if req.source == SOURCE_SUBSCRIPTION {
            eh_gallery_push_ledger::Entity::find()
                .filter(eh_gallery_push_ledger::Column::ChatId.eq(req.chat_id))
                .filter(eh_gallery_push_ledger::Column::Gid.eq(req.gid))
                .one(txn)
                .await
                .context("Failed to select EH subscription push ledger")?
        } else {
            None
        };
        let premarked_archive_sent_at = if req.source == SOURCE_SUBSCRIPTION {
            push_ledger
                .as_ref()
                .and_then(|ledger| ledger.archive_sent_at)
        } else {
            None
        };
        let premarked_telegraph_sent_at = if req.source == SOURCE_SUBSCRIPTION && req.telegraph {
            push_ledger
                .as_ref()
                .and_then(|ledger| ledger.telegraph_sent_at)
        } else {
            None
        };
        if (!req.send_archive || premarked_archive_sent_at.is_some())
            && (!req.telegraph || premarked_telegraph_sent_at.is_some())
        {
            return Ok(None);
        }

        let EhGalleryJobResolution {
            job,
            started_new_generation,
        } = self.get_or_create_eh_gallery_job_in_txn(txn, req).await?;
        let EhDeliveryResolution { old_job_id } = self
            .upsert_eh_delivery_in_txn(
                txn,
                req,
                job.id,
                premarked_archive_sent_at,
                premarked_telegraph_sent_at,
            )
            .await?;

        if req.telegraph && premarked_telegraph_sent_at.is_none() && started_new_generation {
            try_apply_cached_eh_result_in_txn(txn, job.id, req.send_archive).await?;
        }

        self.recompute_eh_job_telegraph_requirement_with_policy_in_txn(
            txn,
            job.id,
            req.send_archive,
        )
        .await?;
        if let Some(old_job_id) = old_job_id.filter(|old_job_id| *old_job_id != job.id) {
            self.recompute_eh_job_telegraph_requirement_with_policy_in_txn(
                txn,
                old_job_id,
                req.send_archive,
            )
            .await?;
            retire_consumerless_eh_job_in_txn(txn, old_job_id).await?;
        }

        // A marker-bearing delivery may remain on its previous job.  Retire a
        // just-created/reused target job if it ended this transaction without a
        // consumer rather than leaving a claimable orphan behind.
        retire_consumerless_eh_job_in_txn(txn, job.id).await?;

        eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::ChatId.eq(req.chat_id))
            .filter(eh_download_queue::Column::Gid.eq(req.gid))
            .one(txn)
            .await
            .context("Failed to reread shared EH delivery in transaction")?
            .map(|delivery| delivery.id)
            .context("Shared EH delivery disappeared in enqueue transaction")
            .map(Some)
    }

    async fn get_or_create_eh_gallery_job_in_txn(
        &self,
        txn: &DatabaseTransaction,
        req: &EhEnqueueRequest<'_>,
    ) -> Result<EhGalleryJobResolution> {
        anyhow::ensure!(
            matches!(
                req.variant.download_mode.as_str(),
                DOWNLOAD_MODE_ARCHIVE | DOWNLOAD_MODE_IMAGES | DOWNLOAD_MODE_LEGACY
            ),
            "Unsupported shared EH gallery download mode '{}'",
            req.variant.download_mode
        );
        let existing = eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::Gid.eq(req.gid))
            .filter(eh_gallery_jobs::Column::Token.eq(req.token))
            .filter(eh_gallery_jobs::Column::DownloadMode.eq(&req.variant.download_mode))
            .filter(eh_gallery_jobs::Column::Resolution.eq(&req.variant.resolution))
            .filter(nullable_eq(
                eh_gallery_jobs::Column::SourceFingerprint,
                req.fingerprint.as_deref(),
            ))
            .one(txn)
            .await
            .context("Failed to select shared EH gallery job")?;

        let (mut job, mut started_new_generation) = if let Some(job) = existing {
            (job, false)
        } else {
            let source_generation =
                next_eh_gallery_source_generation_in_txn(txn, req.gid, req.token, req.variant)
                    .await?;
            (
                eh_gallery_jobs::ActiveModel {
                    gid: Set(req.gid),
                    token: Set(req.token.to_string()),
                    download_mode: Set(req.variant.download_mode.clone()),
                    resolution: Set(req.variant.resolution.clone()),
                    source_fingerprint: Set(req.fingerprint.clone()),
                    source_generation: Set(source_generation),
                    title: Set(req.title.to_string()),
                    status: Set(JOB_STATUS_PENDING.to_string()),
                    telegraph_status: Set(TELEGRAPH_STATUS_NOT_REQUIRED.to_string()),
                    telegraph_required: Set(false),
                    cleanup_status: Set(CLEANUP_STATUS_NONE.to_string()),
                    created_at: Set(Local::now().naive_local()),
                    ..Default::default()
                }
                .insert(txn)
                .await
                .context("Failed to insert shared EH gallery job")?,
                true,
            )
        };

        let has_active_delivery = has_active_eh_delivery_in_txn(txn, job.id).await?;
        let clean_for_reactivation = job.cleanup_status == CLEANUP_STATUS_NONE;
        let should_reactivate = clean_for_reactivation
            && !has_active_delivery
            && matches!(job.status.as_str(), JOB_STATUS_RETIRED | JOB_STATUS_FAILED);
        let should_update_title = !req.title.is_empty() && job.title != req.title;

        if should_reactivate {
            let source_generation =
                next_eh_gallery_source_generation_in_txn(txn, req.gid, req.token, req.variant)
                    .await?;
            reset_eh_gallery_job_generation_in_txn(
                txn,
                job.id,
                req.title,
                req.fingerprint.as_deref(),
                source_generation,
            )
            .await?;
            job = eh_gallery_jobs::Entity::find_by_id(job.id)
                .one(txn)
                .await
                .context("Failed to reread reactivated shared EH gallery job")?
                .context("Reactivated shared EH gallery job disappeared")?;
            started_new_generation = true;
        } else if should_update_title {
            eh_gallery_jobs::Entity::update_many()
                .col_expr(
                    eh_gallery_jobs::Column::Title,
                    Expr::value(req.title.to_string()),
                )
                .filter(eh_gallery_jobs::Column::Id.eq(job.id))
                .exec(txn)
                .await
                .context("Failed to update shared EH gallery job title")?;
            job.title = req.title.to_string();
        }

        Ok(EhGalleryJobResolution {
            job,
            started_new_generation,
        })
    }

    async fn upsert_eh_delivery_in_txn(
        &self,
        txn: &DatabaseTransaction,
        req: &EhEnqueueRequest<'_>,
        requested_job_id: i32,
        premarked_archive_sent_at: Option<DateTime>,
        premarked_telegraph_sent_at: Option<DateTime>,
    ) -> Result<EhDeliveryResolution> {
        let existing = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::ChatId.eq(req.chat_id))
            .filter(eh_download_queue::Column::Gid.eq(req.gid))
            .one(txn)
            .await
            .context("Failed to select shared EH delivery")?;

        let Some(existing) = existing else {
            eh_download_queue::ActiveModel {
                job_id: Set(Some(requested_job_id)),
                chat_id: Set(req.chat_id),
                gid: Set(req.gid),
                token: Set(req.token.to_string()),
                title: Set(req.title.to_string()),
                telegraph: Set(req.telegraph),
                source: Set(req.source.to_string()),
                subscription_ids: Set(req.subscription_id.map(|id| id.to_string())),
                telegraph_subscription_ids: Set(if req.telegraph {
                    req.subscription_id.map(|id| id.to_string())
                } else {
                    None
                }),
                status: Set(DELIVERY_STATUS_WAITING.to_string()),
                created_at: Set(Local::now().naive_local()),
                archive_sent_at: Set(premarked_archive_sent_at),
                telegraph_sent_at: Set(premarked_telegraph_sent_at),
                ..Default::default()
            }
            .insert(txn)
            .await
            .context("Failed to insert shared EH delivery")?;
            return Ok(EhDeliveryResolution { old_job_id: None });
        };

        let terminal = is_terminal_delivery_status(&existing.status);
        let (
            merged_source,
            merged_subscription_ids,
            merged_telegraph_subscription_ids,
            merged_telegraph,
        ) = if terminal {
            // A terminal record is history, not an owner of the next
            // delivery wave. Start that wave from the request alone.
            (
                req.source,
                req.subscription_id.map(|id| id.to_string()),
                if req.telegraph {
                    req.subscription_id.map(|id| id.to_string())
                } else {
                    None
                },
                req.telegraph,
            )
        } else {
            let merged_source = if existing.source == SOURCE_DIRECT || req.source == SOURCE_DIRECT {
                SOURCE_DIRECT
            } else {
                SOURCE_SUBSCRIPTION
            };
            let merged_subscription_ids = if merged_source == SOURCE_DIRECT {
                None
            } else {
                merge_subscription_ids(existing.subscription_ids.as_deref(), req.subscription_id)
            };
            let merged_telegraph_subscription_ids = if merged_source == SOURCE_DIRECT {
                None
            } else {
                merge_telegraph_subscription_ids(
                    existing.telegraph_subscription_ids.as_deref(),
                    req.subscription_id,
                    req.telegraph,
                )
            };
            let merged_telegraph = if merged_source == SOURCE_SUBSCRIPTION {
                merged_telegraph_subscription_ids.is_some()
            } else {
                existing.telegraph || req.telegraph
            };
            (
                merged_source,
                merged_subscription_ids,
                merged_telegraph_subscription_ids,
                merged_telegraph,
            )
        };
        let marker_bearing =
            existing.archive_sent_at.is_some() || existing.telegraph_sent_at.is_some();
        let direct_marker_bearing_rebind = !terminal
            && req.source == SOURCE_DIRECT
            && existing.job_id != Some(requested_job_id)
            && marker_bearing;
        let subscription_fingerprint_generation_rebind = if !terminal
            && req.source == SOURCE_SUBSCRIPTION
            && existing.job_id != Some(requested_job_id)
        {
            if let Some(existing_job_id) = existing.job_id {
                let existing_job = eh_gallery_jobs::Entity::find_by_id(existing_job_id)
                    .one(txn)
                    .await
                    .context("Failed to select existing shared EH subscription delivery job")?
                    .context("Shared EH subscription delivery referenced a missing job")?;
                let requested_job = eh_gallery_jobs::Entity::find_by_id(requested_job_id)
                    .one(txn)
                    .await
                    .context("Failed to select requested shared EH subscription delivery job")?
                    .context("Requested shared EH subscription delivery job disappeared")?;
                existing_job.gid == requested_job.gid
                    && existing_job.token == requested_job.token
                    && existing_job.download_mode == requested_job.download_mode
                    && existing_job.resolution == requested_job.resolution
                    && existing_job.source_fingerprint != requested_job.source_fingerprint
            } else {
                false
            }
        } else {
            false
        };

        if existing.status == DELIVERY_STATUS_PUBLISHING
            && (existing.job_id == Some(requested_job_id)
                || (marker_bearing
                    && !direct_marker_bearing_rebind
                    && !subscription_fingerprint_generation_rebind))
        {
            // A publisher already bound to the requested job, or one with a
            // persisted send marker, owns this delivery's binding and local
            // state. Merge only owner demand; never replace its job, status,
            // metadata, sent markers, or generation fields.
            let updated = eh_download_queue::Entity::update_many()
                .set(eh_download_queue::ActiveModel {
                    telegraph: Set(merged_telegraph),
                    source: Set(merged_source.to_string()),
                    subscription_ids: Set(merged_subscription_ids),
                    telegraph_subscription_ids: Set(merged_telegraph_subscription_ids),
                    ..Default::default()
                })
                .filter(eh_download_queue::Column::Id.eq(existing.id))
                .filter(eh_download_queue::Column::Status.eq(DELIVERY_STATUS_PUBLISHING))
                .filter(eh_download_queue::Column::Telegraph.eq(existing.telegraph))
                .filter(eh_download_queue::Column::Source.eq(&existing.source))
                .filter(nullable_eq(
                    eh_download_queue::Column::SubscriptionIds,
                    existing.subscription_ids.as_deref(),
                ))
                .filter(nullable_eq(
                    eh_download_queue::Column::TelegraphSubscriptionIds,
                    existing.telegraph_subscription_ids.as_deref(),
                ))
                .exec(txn)
                .await
                .context("Failed to merge shared EH publishing delivery owner")?;
            if updated.rows_affected != 1 {
                anyhow::bail!("Shared EH delivery changed concurrently during enqueue")
            }
            return Ok(EhDeliveryResolution {
                old_job_id: existing.job_id,
            });
        }

        if existing.status == DELIVERY_STATUS_PUBLISHING
            && !marker_bearing
            && !subscription_fingerprint_generation_rebind
        {
            // `enqueue_eh_download_request` and `process_claimed` share the
            // keyed chat lock. A markerless publishing claim therefore has not
            // entered the send critical section yet, so a different requested
            // job may replace the stale binding.
            let updated = eh_download_queue::Entity::update_many()
                .set(eh_download_queue::ActiveModel {
                    job_id: Set(Some(requested_job_id)),
                    token: Set(req.token.to_string()),
                    title: Set(req.title.to_string()),
                    telegraph: Set(merged_telegraph),
                    source: Set(merged_source.to_string()),
                    subscription_ids: Set(merged_subscription_ids),
                    telegraph_subscription_ids: Set(merged_telegraph_subscription_ids),
                    status: Set(DELIVERY_STATUS_WAITING.to_string()),
                    file_size: Set(0_i64),
                    gp_cost: Set(0_i64),
                    error: Set(None),
                    retry_count: Set(0_i32),
                    // Keep the last claim generation to fence delayed publishers across waves.
                    completed_at: Set(None),
                    zip_path: Set(None),
                    telegraph_url: Set(None),
                    next_retry_at: Set(None),
                    archive_sent_at: Set(None),
                    telegraph_sent_at: Set(None),
                    ..Default::default()
                })
                .filter(eh_download_queue::Column::Id.eq(existing.id))
                .filter(eh_download_queue::Column::Status.eq(DELIVERY_STATUS_PUBLISHING))
                .filter(nullable_eq(
                    eh_download_queue::Column::JobId,
                    existing.job_id,
                ))
                .filter(eh_download_queue::Column::Telegraph.eq(existing.telegraph))
                .filter(eh_download_queue::Column::Source.eq(&existing.source))
                .filter(nullable_eq(
                    eh_download_queue::Column::SubscriptionIds,
                    existing.subscription_ids.as_deref(),
                ))
                .filter(nullable_eq(
                    eh_download_queue::Column::TelegraphSubscriptionIds,
                    existing.telegraph_subscription_ids.as_deref(),
                ))
                .filter(eh_download_queue::Column::ArchiveSentAt.is_null())
                .filter(eh_download_queue::Column::TelegraphSentAt.is_null())
                .exec(txn)
                .await
                .context("Failed to rebind markerless shared EH publishing delivery")?;
            if updated.rows_affected != 1 {
                anyhow::bail!("Shared EH delivery changed concurrently during enqueue")
            }
            return Ok(EhDeliveryResolution {
                old_job_id: existing.job_id,
            });
        }

        if terminal || direct_marker_bearing_rebind || subscription_fingerprint_generation_rebind {
            // Keep `created_at` stable: delivery ordering remains attached to
            // the chat/gid record. Retain the claim generation for fencing,
            // while other wave-local fields start clean.
            let reset_telegraph = if subscription_fingerprint_generation_rebind {
                merged_telegraph
            } else {
                req.telegraph
            };
            let reset_source = if subscription_fingerprint_generation_rebind {
                merged_source
            } else {
                req.source
            };
            let reset_subscription_ids = if subscription_fingerprint_generation_rebind {
                merged_subscription_ids.clone()
            } else {
                req.subscription_id.map(|id| id.to_string())
            };
            let reset_telegraph_subscription_ids = if subscription_fingerprint_generation_rebind {
                merged_telegraph_subscription_ids.clone()
            } else if req.telegraph {
                req.subscription_id.map(|id| id.to_string())
            } else {
                None
            };
            let reset_archive_sent_at = if subscription_fingerprint_generation_rebind {
                existing.archive_sent_at
            } else {
                premarked_archive_sent_at
            };
            let reset_telegraph_sent_at = if subscription_fingerprint_generation_rebind {
                existing.telegraph_sent_at
            } else {
                premarked_telegraph_sent_at
            };
            let mut update = eh_download_queue::Entity::update_many()
                .set(eh_download_queue::ActiveModel {
                    job_id: Set(Some(requested_job_id)),
                    token: Set(req.token.to_string()),
                    title: Set(req.title.to_string()),
                    telegraph: Set(reset_telegraph),
                    source: Set(reset_source.to_string()),
                    subscription_ids: Set(reset_subscription_ids),
                    telegraph_subscription_ids: Set(reset_telegraph_subscription_ids),
                    status: Set(DELIVERY_STATUS_WAITING.to_string()),
                    file_size: Set(0_i64),
                    gp_cost: Set(0_i64),
                    error: Set(None),
                    retry_count: Set(0_i32),
                    // Keep the last claim generation to fence delayed publishers across waves.
                    completed_at: Set(None),
                    zip_path: Set(None),
                    telegraph_url: Set(None),
                    next_retry_at: Set(None),
                    archive_sent_at: Set(reset_archive_sent_at),
                    telegraph_sent_at: Set(reset_telegraph_sent_at),
                    background_download_status: Set(None),
                    background_download_started_at: Set(None),
                    background_download_next_retry_at: Set(None),
                    background_download_attempt_count: Set(0_i32),
                    background_download_error: Set(None),
                    telegraph_rewrite_data: Set(None),
                    telegraph_rewrite_status: Set(None),
                    telegraph_rewrite_after: Set(None),
                    telegraph_rewrite_started_at: Set(None),
                    telegraph_rewrite_next_retry_at: Set(None),
                    telegraph_rewrite_retry_count: Set(0_i32),
                    telegraph_rewrite_error: Set(None),
                    telegraph_rewritten_at: Set(None),
                    ..Default::default()
                })
                .filter(eh_download_queue::Column::Id.eq(existing.id))
                .filter(eh_download_queue::Column::Status.eq(&existing.status))
                .filter(nullable_eq(
                    eh_download_queue::Column::JobId,
                    existing.job_id,
                ));
            if !terminal {
                update = update
                    .filter(eh_download_queue::Column::Telegraph.eq(existing.telegraph))
                    .filter(eh_download_queue::Column::Source.eq(&existing.source))
                    .filter(nullable_eq(
                        eh_download_queue::Column::SubscriptionIds,
                        existing.subscription_ids.as_deref(),
                    ))
                    .filter(nullable_eq(
                        eh_download_queue::Column::TelegraphSubscriptionIds,
                        existing.telegraph_subscription_ids.as_deref(),
                    ))
                    .filter(nullable_eq(
                        eh_download_queue::Column::ArchiveSentAt,
                        existing.archive_sent_at,
                    ))
                    .filter(nullable_eq(
                        eh_download_queue::Column::TelegraphSentAt,
                        existing.telegraph_sent_at,
                    ));
            }
            let updated = update
                .exec(txn)
                .await
                .context("Failed to reset shared EH delivery wave")?;
            if updated.rows_affected != 1 {
                anyhow::bail!("Shared EH delivery changed concurrently during enqueue")
            }
            return Ok(EhDeliveryResolution {
                old_job_id: existing.job_id,
            });
        }

        let target_job_id = if marker_bearing {
            existing.job_id
        } else {
            Some(requested_job_id)
        };
        let target_status = if marker_bearing {
            existing.status.clone()
        } else {
            DELIVERY_STATUS_WAITING.to_string()
        };

        let mut update = eh_download_queue::Entity::update_many()
            .set(eh_download_queue::ActiveModel {
                job_id: Set(target_job_id),
                token: Set(req.token.to_string()),
                title: Set(req.title.to_string()),
                telegraph: Set(merged_telegraph),
                source: Set(merged_source.to_string()),
                subscription_ids: Set(merged_subscription_ids),
                telegraph_subscription_ids: Set(merged_telegraph_subscription_ids),
                status: Set(target_status.to_string()),
                ..Default::default()
            })
            .filter(eh_download_queue::Column::Id.eq(existing.id))
            .filter(eh_download_queue::Column::Status.eq(&existing.status))
            .filter(eh_download_queue::Column::Telegraph.eq(existing.telegraph))
            .filter(eh_download_queue::Column::Source.eq(&existing.source))
            .filter(nullable_eq(
                eh_download_queue::Column::JobId,
                existing.job_id,
            ));
        update = update.filter(nullable_eq(
            eh_download_queue::Column::SubscriptionIds,
            existing.subscription_ids.as_deref(),
        ));
        update = update.filter(nullable_eq(
            eh_download_queue::Column::TelegraphSubscriptionIds,
            existing.telegraph_subscription_ids.as_deref(),
        ));
        update = update.filter(nullable_eq(
            eh_download_queue::Column::ArchiveSentAt,
            existing.archive_sent_at,
        ));
        update = update.filter(nullable_eq(
            eh_download_queue::Column::TelegraphSentAt,
            existing.telegraph_sent_at,
        ));

        let updated = update
            .exec(txn)
            .await
            .context("Failed to merge shared EH delivery")?;
        if updated.rows_affected != 1 {
            anyhow::bail!("Shared EH delivery changed concurrently during enqueue")
        }
        Ok(EhDeliveryResolution {
            old_job_id: existing.job_id,
        })
    }

    pub(crate) async fn recompute_eh_job_telegraph_requirement_in_txn(
        &self,
        txn: &DatabaseTransaction,
        job_id: i32,
    ) -> Result<()> {
        self.recompute_eh_job_telegraph_requirement_with_policy_in_txn(txn, job_id, true)
            .await
    }

    pub(super) async fn recompute_eh_job_telegraph_requirement_with_policy_in_txn(
        &self,
        txn: &DatabaseTransaction,
        job_id: i32,
        send_archive: bool,
    ) -> Result<()> {
        let deliveries = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::JobId.eq(job_id))
            .all(txn)
            .await
            .context("Failed to select shared EH job deliveries for Telegraph requirement")?;
        let telegraph_required = deliveries.iter().any(|delivery| {
            delivery.telegraph
                && delivery.telegraph_sent_at.is_none()
                && is_active_delivery_status(&delivery.status)
        });
        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(txn)
            .await
            .context("Failed to select shared EH job for Telegraph requirement")?
            .context("Shared EH job disappeared while recomputing Telegraph requirement")?;
        let should_try_cached_result = telegraph_required
            && !job.telegraph_required
            && job.telegraph_status == TELEGRAPH_STATUS_NOT_REQUIRED;
        if should_try_cached_result
            && try_apply_cached_eh_result_in_txn(txn, job_id, send_archive).await?
        {
            let updated = eh_gallery_jobs::Entity::update_many()
                .col_expr(
                    eh_gallery_jobs::Column::TelegraphRequired,
                    Expr::value(true),
                )
                .filter(eh_gallery_jobs::Column::Id.eq(job_id))
                .filter(eh_gallery_jobs::Column::TelegraphRequired.eq(false))
                .filter(eh_gallery_jobs::Column::TelegraphStatus.eq(TELEGRAPH_STATUS_READY))
                .exec(txn)
                .await
                .context("Failed to record cached shared EH Telegraph demand")?;
            if updated.rows_affected != 1 {
                anyhow::bail!(
                    "Shared EH job {} changed concurrently while applying cached Telegraph result",
                    job_id
                );
            }
            return Ok(());
        }
        let start_fresh_upload_wave = telegraph_required
            && job.status == JOB_STATUS_DOWNLOADED
            && job.telegraph_status == TELEGRAPH_STATUS_FAILED;
        let telegraph_status = if telegraph_required {
            match job.telegraph_status.as_str() {
                TELEGRAPH_STATUS_NOT_REQUIRED | TELEGRAPH_STATUS_FAILED
                    if job.status == JOB_STATUS_DOWNLOADED =>
                {
                    TELEGRAPH_STATUS_PENDING
                }
                TELEGRAPH_STATUS_PENDING | TELEGRAPH_STATUS_UPLOADING | TELEGRAPH_STATUS_READY => {
                    &job.telegraph_status
                }
                _ => &job.telegraph_status,
            }
        } else if job.telegraph_status == TELEGRAPH_STATUS_PENDING {
            TELEGRAPH_STATUS_NOT_REQUIRED
        } else {
            &job.telegraph_status
        };

        let mut update = eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                telegraph_required: Set(telegraph_required),
                telegraph_status: Set(telegraph_status.to_string()),
                ..Default::default()
            })
            .filter(eh_gallery_jobs::Column::Id.eq(job_id));
        if start_fresh_upload_wave {
            update = update.set(eh_gallery_jobs::ActiveModel {
                telegraph_url: Set(None),
                error: Set(None),
                retry_count: Set(0_i32),
                next_retry_at: Set(None),
                ..Default::default()
            });
        }
        update
            .exec(txn)
            .await
            .context("Failed to update shared EH job Telegraph requirement")?;
        Ok(())
    }
}
