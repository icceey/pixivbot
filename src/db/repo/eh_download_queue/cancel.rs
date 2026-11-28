use super::*;

impl Repo {
    /// Cancel delivery ownership created for a subscription that has just been
    /// removed. Direct deliveries and other subscription owners are left
    /// untouched.
    pub async fn cancel_eh_subscription_queue_entries(
        &self,
        subscription_id: i32,
        send_archive: bool,
    ) -> Result<u64> {
        let chat_ids = self
            .find_eh_subscription_delivery_rows(subscription_id)
            .await?
            .into_iter()
            .map(|row| row.chat_id)
            .collect::<Vec<_>>();
        let _guards = EH_CHAT_LOCKS.lock_chats(&chat_ids).await;
        self.cancel_eh_subscription_queue_entries_under_chat_locks(subscription_id, send_archive)
            .await
    }

    /// Delete an EH subscription and cancel/prune its deliveries while holding
    /// that subscription's chat lock.
    pub async fn delete_eh_subscription_and_cancel_queue(
        &self,
        subscription_id: i32,
        send_archive: bool,
    ) -> Result<()> {
        for _ in 0..SQLITE_TRANSACTION_ATTEMPTS {
            match self
                .delete_eh_subscription_and_cancel_queue_once(subscription_id, send_archive)
                .await
            {
                Ok(()) => return Ok(()),
                Err(error) => {
                    if !is_retryable_sqlite_lock_error(&error) {
                        return Err(error);
                    }
                }
            }
        }
        self.delete_eh_subscription_and_cancel_queue_once(subscription_id, send_archive)
            .await
    }

    async fn delete_eh_subscription_and_cancel_queue_once(
        &self,
        subscription_id: i32,
        send_archive: bool,
    ) -> Result<()> {
        let subscription = subscriptions::Entity::find_by_id(subscription_id)
            .one(&self.db)
            .await
            .context("Failed to load EH subscription before cancellation")?
            .context("EH subscription disappeared before cancellation")?;
        let _guard = EH_CHAT_LOCKS.lock_chat(subscription.chat_id).await;
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin EH subscription deletion transaction")?;
        let result: Result<()> = async {
            let deleted = subscriptions::Entity::delete_by_id(subscription_id)
                .exec(&txn)
                .await
                .context("Failed to delete EH subscription")?;
            anyhow::ensure!(
                deleted.rows_affected == 1,
                "EH subscription disappeared during cancellation"
            );
            self.cancel_eh_subscription_queue_entries_in_txn(&txn, subscription_id, send_archive)
                .await?;
            Ok(())
        }
        .await;
        match result {
            Ok(()) => txn
                .commit()
                .await
                .context("Failed to commit EH subscription deletion transaction"),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back EH subscription deletion transaction")?;
                Err(error)
            }
        }
    }

    async fn find_eh_subscription_delivery_rows(
        &self,
        subscription_id: i32,
    ) -> Result<Vec<eh_download_queue::Model>> {
        let rows = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Source.eq(SOURCE_SUBSCRIPTION))
            .filter(eh_download_queue::Column::SubscriptionIds.is_not_null())
            .filter(eh_download_queue::Column::Status.is_in([
                STATUS_WAITING,
                STATUS_PENDING,
                STATUS_DOWNLOADING,
                STATUS_DOWNLOADED,
                STATUS_UPLOADING,
                STATUS_UPLOADED,
                STATUS_PUBLISHING,
                STATUS_DONE,
                STATUS_FAILED,
                STATUS_CANCELED,
            ]))
            .all(&self.db)
            .await
            .context("Failed to cancel eh subscription queue entries")?;
        Ok(rows
            .into_iter()
            .filter(|row| {
                parse_subscription_ids(row.subscription_ids.as_deref()).contains(&subscription_id)
            })
            .collect())
    }

    async fn find_eh_subscription_delivery_rows_in_txn(
        &self,
        txn: &sea_orm::DatabaseTransaction,
        subscription_id: i32,
    ) -> Result<Vec<eh_download_queue::Model>> {
        let rows = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Source.eq(SOURCE_SUBSCRIPTION))
            .filter(eh_download_queue::Column::SubscriptionIds.is_not_null())
            .filter(eh_download_queue::Column::Status.is_in([
                STATUS_WAITING,
                STATUS_PENDING,
                STATUS_DOWNLOADING,
                STATUS_DOWNLOADED,
                STATUS_UPLOADING,
                STATUS_UPLOADED,
                STATUS_PUBLISHING,
                STATUS_DONE,
                STATUS_FAILED,
                STATUS_CANCELED,
            ]))
            .all(txn)
            .await
            .context("Failed to select EH subscription deliveries for cancellation")?;
        Ok(rows
            .into_iter()
            .filter(|row| {
                parse_subscription_ids(row.subscription_ids.as_deref()).contains(&subscription_id)
            })
            .collect())
    }

    async fn cancel_eh_subscription_queue_entries_under_chat_locks(
        &self,
        subscription_id: i32,
        send_archive: bool,
    ) -> Result<u64> {
        for _ in 0..SQLITE_TRANSACTION_ATTEMPTS {
            match self
                .cancel_eh_subscription_queue_entries_under_chat_locks_once(
                    subscription_id,
                    send_archive,
                )
                .await
            {
                Ok(changed) => return Ok(changed),
                Err(error) => {
                    if !is_retryable_sqlite_lock_error(&error) {
                        return Err(error);
                    }
                }
            }
        }
        self.cancel_eh_subscription_queue_entries_under_chat_locks_once(
            subscription_id,
            send_archive,
        )
        .await
    }

    async fn cancel_eh_subscription_queue_entries_under_chat_locks_once(
        &self,
        subscription_id: i32,
        send_archive: bool,
    ) -> Result<u64> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin EH subscription cancellation transaction")?;
        let result = self
            .cancel_eh_subscription_queue_entries_in_txn(&txn, subscription_id, send_archive)
            .await;
        match result {
            Ok(changed) => txn
                .commit()
                .await
                .context("Failed to commit EH subscription cancellation transaction")
                .map(|()| changed),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back EH subscription cancellation transaction")?;
                Err(error)
            }
        }
    }

    async fn cancel_eh_subscription_queue_entries_in_txn(
        &self,
        txn: &sea_orm::DatabaseTransaction,
        subscription_id: i32,
        send_archive: bool,
    ) -> Result<u64> {
        // Re-query after the caller has acquired every affected chat lock so a
        // send/cancel operation for the same chat observes one order.
        let active_rows = self
            .find_eh_subscription_delivery_rows_in_txn(txn, subscription_id)
            .await?;
        let mut changed = 0u64;
        let mut affected_job_ids = BTreeSet::new();
        for row in active_rows {
            let job_id = row.job_id;
            let row_changed = self
                .remove_subscription_owner_from_eh_row_in_txn(txn, row, subscription_id)
                .await?;
            changed += row_changed;
            if row_changed == 1 {
                if let Some(job_id) = job_id {
                    affected_job_ids.insert(job_id);
                }
            }
        }
        for job_id in affected_job_ids {
            self.recompute_eh_job_telegraph_requirement_in_txn(txn, job_id)
                .await?;
            self.evaluate_eh_job_liveness_in_txn(txn, job_id, send_archive)
                .await?;
        }
        Ok(changed)
    }

    async fn remove_subscription_owner_from_eh_row_in_txn(
        &self,
        txn: &sea_orm::DatabaseTransaction,
        mut row: eh_download_queue::Model,
        subscription_id: i32,
    ) -> Result<u64> {
        const MAX_RETRIES: usize = 3;
        for attempt in 0..MAX_RETRIES {
            if row.source != SOURCE_SUBSCRIPTION
                || !is_cancelable_subscription_queue_status(&row.status)
            {
                return Ok(0);
            }

            let mut ids = parse_subscription_ids(row.subscription_ids.as_deref());
            if !ids.remove(&subscription_id) {
                return Ok(0);
            }
            let remaining_ids = format_subscription_ids(&ids);
            let mut telegraph_ids =
                parse_subscription_ids(row.telegraph_subscription_ids.as_deref());
            telegraph_ids.remove(&subscription_id);
            let remaining_telegraph_ids = format_subscription_ids(&telegraph_ids);
            let canceled = remaining_ids.is_none();
            let telegraph_still_required = remaining_telegraph_ids.is_some();
            let new_status = if canceled && is_active_subscription_queue_status(&row.status) {
                STATUS_CANCELED.to_string()
            } else {
                row.status.clone()
            };
            let result = eh_download_queue::Entity::update_many()
                .set(eh_download_queue::ActiveModel {
                    subscription_ids: Set(remaining_ids),
                    telegraph_subscription_ids: Set(remaining_telegraph_ids),
                    status: Set(new_status.to_string()),
                    telegraph: Set(telegraph_still_required),
                    ..Default::default()
                })
                .filter(eh_download_queue::Column::Id.eq(row.id))
                .filter(eh_download_queue::Column::Status.eq(&row.status))
                .filter(eh_download_queue::Column::Source.eq(SOURCE_SUBSCRIPTION))
                .filter(job_id_filter(row.job_id))
                .filter(eh_download_queue::Column::SubscriptionIds.eq(row.subscription_ids.clone()))
                .filter(eh_download_queue::Column::Telegraph.eq(row.telegraph))
                .filter(telegraph_subscription_ids_filter(
                    row.telegraph_subscription_ids.clone(),
                ))
                .exec(txn)
                .await
                .context("Failed to cancel eh subscription queue entry")?;
            if result.rows_affected == 1 {
                return Ok(1);
            }

            if attempt + 1 == MAX_RETRIES {
                anyhow::bail!(
                    "Failed to cancel EH subscription queue entry {} after {} attempts: row changed too frequently",
                    row.id,
                    MAX_RETRIES
                );
            }

            match eh_download_queue::Entity::find_by_id(row.id)
                .one(txn)
                .await?
            {
                Some(fresh) => row = fresh,
                None => return Ok(0),
            }
        }
        Ok(0)
    }

    /// True if a claimed queue row is still active and has not been canceled
    /// after its originating subscription was removed.
    pub async fn eh_download_is_active(
        &self,
        id: i32,
        expected_status: &str,
        send_archive: bool,
    ) -> Result<bool> {
        let row = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(expected_status))
            .one(&self.db)
            .await
            .context("Failed to check eh download activity")?;
        let Some(row) = row else {
            return Ok(false);
        };
        if row.source != SOURCE_SUBSCRIPTION {
            return Ok(true);
        }
        self.eh_download_has_live_owner_or_cancel(row.id, expected_status, send_archive)
            .await
    }

    async fn eh_download_has_live_owner_or_cancel(
        &self,
        id: i32,
        expected_status: &str,
        send_archive: bool,
    ) -> Result<bool> {
        for _ in 0..SQLITE_TRANSACTION_ATTEMPTS {
            match self
                .eh_download_has_live_owner_or_cancel_once(id, expected_status, send_archive)
                .await
            {
                Ok(active) => return Ok(active),
                Err(error) => {
                    if !is_retryable_sqlite_lock_error(&error) {
                        return Err(error);
                    }
                }
            }
        }
        self.eh_download_has_live_owner_or_cancel_once(id, expected_status, send_archive)
            .await
    }

    async fn eh_download_has_live_owner_or_cancel_once(
        &self,
        id: i32,
        expected_status: &str,
        send_archive: bool,
    ) -> Result<bool> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin inactive EH download soft-cancel transaction")?;
        let result: Result<bool> = async {
            let Some(mut row) = eh_download_queue::Entity::find()
                .filter(eh_download_queue::Column::Id.eq(id))
                .filter(eh_download_queue::Column::Status.eq(expected_status))
                .one(&txn)
                .await
                .context("Failed to re-read EH download activity in transaction")?
            else {
                return Ok(false);
            };
            const MAX_RETRIES: usize = 3;
            for attempt in 0..MAX_RETRIES {
                if row.status != expected_status {
                    return Ok(false);
                }
                if row.source != SOURCE_SUBSCRIPTION {
                    return Ok(true);
                }
                let ids = parse_subscription_ids(row.subscription_ids.as_deref());
                let alive = if ids.is_empty() {
                    false
                } else {
                    subscriptions::Entity::find()
                        .filter(subscriptions::Column::Id.is_in(ids.iter().copied()))
                        .count(&txn)
                        .await
                        .context("Failed to check EH subscription owners")?
                        > 0
                };
                if alive {
                    return Ok(true);
                }

                let result = eh_download_queue::Entity::update_many()
                    .set(eh_download_queue::ActiveModel {
    status: Set(STATUS_CANCELED.to_string()),
    telegraph: Set(false),
    subscription_ids: Set(None),
    telegraph_subscription_ids: Set(None),
    telegraph_url: Set(None),
    archive_sent_at: Set(None),
    telegraph_sent_at: Set(None),
    telegraph_rewrite_data: Set(None),
    telegraph_rewrite_status: Set(None),
    telegraph_rewrite_after: Set(None),
    telegraph_rewrite_started_at: Set(None),
    telegraph_rewrite_next_retry_at: Set(None),
    telegraph_rewrite_retry_count: Set(0),
    telegraph_rewrite_error: Set(None),
    telegraph_rewritten_at: Set(None),
    ..Default::default()
})
                    .filter(eh_download_queue::Column::Id.eq(row.id))
                    .filter(eh_download_queue::Column::Status.eq(&row.status))
                    .filter(eh_download_queue::Column::Source.eq(SOURCE_SUBSCRIPTION))
                    .filter(eh_download_queue::Column::Telegraph.eq(row.telegraph))
                    .filter(subscription_ids_filter(row.subscription_ids.clone()))
                    .filter(telegraph_subscription_ids_filter(
                        row.telegraph_subscription_ids.clone(),
                    ))
                    .exec(&txn)
                    .await
                    .context("Failed to soft-cancel inactive EH download")?;
                if result.rows_affected == 1 {
                    if let Some(job_id) = row.job_id {
                        self.recompute_eh_job_telegraph_requirement_in_txn(&txn, job_id)
                            .await?;
                        self.evaluate_eh_job_liveness_in_txn(&txn, job_id, send_archive)
                            .await?;
                    }
                    return Ok(false);
                }

                if attempt + 1 == MAX_RETRIES {
                    anyhow::bail!(
                        "Failed to soft-cancel inactive EH download {} after {} attempts: row changed too frequently",
                        row.id,
                        MAX_RETRIES
                    );
                }

                match eh_download_queue::Entity::find_by_id(row.id).one(&txn).await? {
                    Some(fresh) => row = fresh,
                    None => return Ok(false),
                }
            }
            Ok(false)
        }
        .await;
        match result {
            Ok(active) => txn
                .commit()
                .await
                .context("Failed to commit inactive EH download soft-cancel transaction")
                .map(|()| active),
            Err(error) => {
                txn.rollback()
                    .await
                    .context("Failed to roll back inactive EH download soft-cancel transaction")?;
                Err(error)
            }
        }
    }
}
