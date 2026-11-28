use super::Repo;
use crate::db::entities::eh_gp_spend_attempts;
use anyhow::{Context, Result};
use chrono::{Duration, Local};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

impl Repo {
    /// Record one GP charge attempt owned by a shared-gallery download job.
    /// Each reservation represents a source archive POST, never a delivery.
    pub async fn append_eh_job_gp_spend_attempt(
        &self,
        job_id: i32,
        gid: i64,
        gp_cost: i64,
    ) -> Result<eh_gp_spend_attempts::Model> {
        if gp_cost <= 0 {
            anyhow::bail!("EH GP spend attempt cost must be positive, got {gp_cost}");
        }

        eh_gp_spend_attempts::ActiveModel {
            job_id: Set(Some(job_id)),
            queue_id: Set(None),
            gid: Set(gid),
            gp_cost: Set(gp_cost),
            created_at: Set(Local::now().naive_local()),
            ..Default::default()
        }
        .insert(&self.db)
        .await
        .context("Failed to append shared EH job GP spend attempt")
    }

    /// Get total GP charged in the last `window_hours` from the append-only ledger.
    pub async fn get_eh_gp_cost_in_window(&self, window_hours: u64) -> Result<i64> {
        let window_hours = i64::try_from(window_hours)
            .context("EH GP spend window hours exceed the supported range")?;
        let duration = Duration::try_hours(window_hours)
            .context("EH GP spend window hours exceed Chrono duration range")?;
        let cutoff = Local::now()
            .naive_local()
            .checked_sub_signed(duration)
            .context("EH GP spend window cutoff is outside the supported datetime range")?;

        let attempts = eh_gp_spend_attempts::Entity::find()
            .filter(eh_gp_spend_attempts::Column::CreatedAt.gte(cutoff))
            .all(&self.db)
            .await
            .context("Failed to fetch EH GP spend attempts in window")?;

        attempts.into_iter().try_fold(0_i64, |total, attempt| {
            total.checked_add(attempt.gp_cost).ok_or_else(|| {
                anyhow::anyhow!("EH GP spend total overflow while summing attempts in window")
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests_helpers::setup_test_db;
    use crate::db::entities::{eh_download_queue, eh_gallery_jobs, eh_gp_spend_attempts};
    use crate::db::repo::eh_gallery_jobs::EhGalleryVariant;
    use anyhow::Result;
    use chrono::Local;
    use sea_orm::{ActiveModelTrait, EntityTrait, Set};

    #[tokio::test]
    async fn job_gp_ledger_keeps_repeated_charges_after_job_deletion() -> Result<()> {
        let repo = setup_test_db().await?;
        let queue = repo
            .enqueue_eh_download(
                1,
                102,
                "token",
                "title",
                false,
                "direct",
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await?
            .expect("delivery should be enqueued");

        let first = repo
            .append_eh_job_gp_spend_attempt(queue.job_id.unwrap(), queue.gid, 218)
            .await?;
        let second = repo
            .append_eh_job_gp_spend_attempt(queue.job_id.unwrap(), queue.gid, 218)
            .await?;

        assert_ne!(first.id, second.id);
        assert_eq!(first.job_id, queue.job_id);
        assert!(first.queue_id.is_none());
        eh_gallery_jobs::Entity::delete_by_id(queue.job_id.unwrap())
            .exec(repo.db())
            .await?;
        let rows = eh_gp_spend_attempts::Entity::find().all(repo.db()).await?;
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|attempt| attempt.job_id.is_none()));
        assert_eq!(repo.get_eh_gp_cost_in_window(24).await?, 436);
        Ok(())
    }

    #[tokio::test]
    async fn get_eh_gp_cost_in_window_reads_only_the_ledger() -> Result<()> {
        let repo = setup_test_db().await?;
        let queue = repo
            .enqueue_eh_download(
                1,
                106,
                "token",
                "title",
                false,
                "direct",
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await?
            .expect("delivery should be enqueued");
        let job_id = queue.job_id.unwrap();
        let mut queue: eh_download_queue::ActiveModel = queue.into();
        queue.gp_cost = Set(218);
        queue.completed_at = Set(Some(Local::now().naive_local()));
        queue.update(repo.db()).await?;

        assert_eq!(repo.get_eh_gp_cost_in_window(24).await?, 0);

        repo.append_eh_job_gp_spend_attempt(job_id, 106, 7).await?;

        assert_eq!(repo.get_eh_gp_cost_in_window(24).await?, 7);
        Ok(())
    }
}
