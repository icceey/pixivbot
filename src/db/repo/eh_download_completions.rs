use super::Repo;
use crate::db::entities::eh_download_completions;
use anyhow::{Context, Result};
use chrono::{Duration, Local};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, QuerySelect, Set,
};

/// Append one completed shared-download generation inside its owning transaction.
/// This remains transaction-scoped so later shared stages can preserve the same
/// append-only accounting guarantee.
pub(crate) async fn append_eh_download_completion_in_txn(
    txn: &DatabaseTransaction,
    job_id: i32,
    gid: i64,
    file_size: i64,
    created_at: sea_orm::prelude::DateTime,
) -> Result<eh_download_completions::Model> {
    anyhow::ensure!(
        file_size >= 0,
        "EH download completion file size must be non-negative"
    );
    eh_download_completions::ActiveModel {
        job_id: Set(Some(job_id)),
        gid: Set(gid),
        file_size: Set(file_size),
        created_at: Set(created_at),
        ..Default::default()
    }
    .insert(txn)
    .await
    .context("Failed to append EH download completion")
}

impl Repo {
    /// Sum append-only download completions in the rolling rate-limit window.
    pub async fn get_eh_downloaded_bytes_in_window(&self, hours: i64) -> Result<i64> {
        let hours = hours.max(1);
        let duration = Duration::try_hours(hours)
            .context("EH download completion window hours exceed Chrono duration range")?;
        let cutoff = Local::now()
            .naive_local()
            .checked_sub_signed(duration)
            .context(
                "EH download completion window cutoff is outside the supported datetime range",
            )?;
        let total = eh_download_completions::Entity::find()
            .select_only()
            .column_as(eh_download_completions::Column::FileSize.sum(), "total")
            .filter(eh_download_completions::Column::CreatedAt.gte(cutoff))
            .into_tuple::<Option<i64>>()
            .one(&self.db)
            .await
            .context("Failed to sum EH download completions in window")?
            .flatten()
            .unwrap_or(0);
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use crate::db::entities::{eh_download_completions, eh_download_queue, eh_gallery_jobs};
    use crate::db::repo::eh_gallery_jobs::{
        EhGalleryVariant, CLEANUP_STATUS_NONE, DELIVERY_STATUS_DONE, JOB_STATUS_RETIRED,
    };
    use crate::db::repo::tests_helpers;
    use chrono::Local;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

    #[tokio::test]
    async fn completion_ledger_empty_window_is_zero() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        assert_eq!(repo.get_eh_downloaded_bytes_in_window(24).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn completion_ledger_counts_both_generations_after_clean_retired_job_reactivation() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let first_delivery = repo
            .enqueue_eh_download(1, 410, "token", "First", false, "direct", &variant)
            .await
            .unwrap();
        let first_job = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        let first_started_at = first_job.started_at.unwrap();
        repo.mark_eh_job_downloaded(first_job.id, first_started_at, 100, "/tmp/first.zip", 0)
            .await
            .unwrap();

        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                sea_orm::sea_query::Expr::value(DELIVERY_STATUS_DONE),
            )
            .filter(eh_download_queue::Column::Id.eq(first_delivery.id))
            .exec(repo.db())
            .await
            .unwrap();
        eh_gallery_jobs::Entity::update_many()
            .col_expr(
                eh_gallery_jobs::Column::Status,
                sea_orm::sea_query::Expr::value(JOB_STATUS_RETIRED),
            )
            .col_expr(
                eh_gallery_jobs::Column::CleanupStatus,
                sea_orm::sea_query::Expr::value(CLEANUP_STATUS_NONE),
            )
            .filter(eh_gallery_jobs::Column::Id.eq(first_job.id))
            .exec(repo.db())
            .await
            .unwrap();

        let second_delivery = repo
            .enqueue_eh_download(1, 410, "token", "Second", false, "direct", &variant)
            .await
            .unwrap();
        assert_eq!(second_delivery.job_id, Some(first_job.id));
        let second_job = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
        assert_eq!(second_job.id, first_job.id);
        repo.mark_eh_job_downloaded(
            second_job.id,
            second_job.started_at.unwrap(),
            250,
            "/tmp/second.zip",
            0,
        )
        .await
        .unwrap();

        let completions = eh_download_completions::Entity::find()
            .order_by_asc(eh_download_completions::Column::Id)
            .all(repo.db())
            .await
            .unwrap();
        assert_eq!(
            completions
                .iter()
                .map(|completion| completion.file_size)
                .collect::<Vec<_>>(),
            vec![100, 250]
        );
        assert!(completions
            .iter()
            .all(|completion| completion.job_id == Some(first_job.id)));
        assert_eq!(
            repo.get_eh_downloaded_bytes_in_window(24).await.unwrap(),
            350
        );
        assert!(completions
            .iter()
            .all(|completion| completion.created_at <= Local::now().naive_local()));
    }
}
