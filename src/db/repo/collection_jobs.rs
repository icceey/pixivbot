use super::Repo;
use anyhow::{ensure, Result};
use sea_orm::{ConnectionTrait, DbBackend, FromQueryResult, Statement, TransactionTrait};

pub use crate::db::entities::collection_jobs::Model;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeResult {
    Queued,
    Publishing,
    Unavailable,
}

impl Repo {
    pub async fn enqueue_collection_job(
        &self,
        chat_id: i64,
        user_id: i64,
        source: String,
        telegraph: bool,
    ) -> Result<i64> {
        let result = self
            .db
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO collection_jobs (chat_id, user_id, source, telegraph, queue_order)
                 VALUES (?, ?, ?, ?, (SELECT COALESCE(MAX(queue_order), 0) + 1 FROM collection_jobs))",
                [chat_id.into(), user_id.into(), source.into(), telegraph.into()],
            ))
            .await?;
        Ok(result.last_insert_id() as i64)
    }

    pub async fn claim_collection_job(&self) -> Result<Option<Model>> {
        Ok(Model::find_by_statement(Statement::from_string(
            DbBackend::Sqlite,
            "UPDATE collection_jobs SET status = 'preparing'
             WHERE id = (
                SELECT id FROM collection_jobs WHERE status = 'queued' ORDER BY queue_order LIMIT 1
             )
             RETURNING id, chat_id, user_id, source, telegraph, generation, delivered, cursor",
        ))
        .one(&self.db)
        .await?)
    }

    pub async fn mark_collection_publishing(&self, id: i64) -> Result<()> {
        let result = self
            .db
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "UPDATE collection_jobs SET status = 'publishing' WHERE id = ? AND status = 'preparing'",
                [id.into()],
            ))
            .await?;
        ensure!(
            result.rows_affected() == 1,
            "Collection job {id} is not preparing"
        );
        Ok(())
    }

    pub async fn finish_collection_batch(
        &self,
        id: i64,
        delivered: i64,
        cursor: Option<String>,
    ) -> Result<()> {
        let statement = match cursor {
            Some(cursor) => Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "UPDATE collection_jobs SET delivered = ?, cursor = ?, status = 'waiting'
                 WHERE id = ? AND status = 'publishing'",
                [delivered.into(), cursor.into(), id.into()],
            ),
            None => Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "DELETE FROM collection_jobs WHERE id = ? AND status = 'publishing'",
                [id.into()],
            ),
        };
        let result = self.db.execute(statement).await?;
        ensure!(
            result.rows_affected() == 1,
            "Collection job {id} is not publishing"
        );
        Ok(())
    }

    pub async fn fail_collection_job(&self, id: i64) -> Result<()> {
        self.db
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "DELETE FROM collection_jobs WHERE id = ? AND status IN ('preparing', 'publishing')",
                [id.into()],
            ))
            .await?;
        Ok(())
    }

    pub async fn recover_collection_jobs(&self) -> Result<Vec<Model>> {
        let txn = self.db.begin().await?;
        let interrupted = Model::find_by_statement(Statement::from_string(
            DbBackend::Sqlite,
            "DELETE FROM collection_jobs WHERE status = 'publishing'
             RETURNING id, chat_id, user_id, source, telegraph, generation, delivered, cursor",
        ))
        .all(&txn)
        .await?;
        txn.execute_unprepared(
            "UPDATE collection_jobs SET status = 'queued' WHERE status = 'preparing'",
        )
        .await?;
        txn.commit().await?;
        Ok(interrupted)
    }

    pub async fn resume_collection_job(
        &self,
        id: i64,
        chat: i64,
        user: i64,
        generation: i64,
    ) -> Result<ResumeResult> {
        let txn = self.db.begin().await?;
        let values = [id.into(), chat.into(), user.into(), generation.into()];
        let queued = txn
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "UPDATE collection_jobs
                 SET status = 'queued', generation = generation + 1,
                     queue_order = (SELECT MAX(queue_order) + 1 FROM collection_jobs)
                 WHERE id = ? AND chat_id = ? AND user_id = ? AND generation = ? AND status = 'waiting'",
                values.clone(),
            ))
            .await?;
        let result = if queued.rows_affected() == 1 {
            ResumeResult::Queued
        } else if txn
            .query_one(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT 1 FROM collection_jobs
                 WHERE id = ? AND chat_id = ? AND user_id = ? AND generation = ? AND status = 'publishing'",
                values,
            ))
            .await?
            .is_some()
        {
            ResumeResult::Publishing
        } else {
            ResumeResult::Unavailable
        };
        txn.commit().await?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repo::tests_helpers::setup_test_db;
    use migration::MigratorTrait;
    use sea_orm::Database;

    #[tokio::test]
    async fn continuation_is_single_use_and_joins_the_back_of_the_queue() -> Result<()> {
        let repo = setup_test_db().await?;
        let id = repo
            .enqueue_collection_job(-42, 7, "first".into(), true)
            .await?;
        let (left, right) = tokio::join!(repo.claim_collection_job(), repo.claim_collection_job());
        let initial = match (left?, right?) {
            (Some(job), None) | (None, Some(job)) => job,
            other => panic!("expected one claim, got {other:?}"),
        };
        assert_eq!(initial.id, id);
        assert_eq!(initial.generation, 1);
        repo.mark_collection_publishing(id).await?;
        assert_eq!(
            repo.resume_collection_job(id, -42, 7, 1).await?,
            ResumeResult::Publishing
        );
        assert_eq!(
            repo.resume_collection_job(id, -42, 8, 1).await?,
            ResumeResult::Unavailable
        );
        assert_eq!(
            repo.resume_collection_job(id, -43, 7, 1).await?,
            ResumeResult::Unavailable
        );

        let cursor = r#"{"offset":12}"#;
        repo.finish_collection_batch(id, 12, Some(cursor.into()))
            .await?;
        let other = repo
            .enqueue_collection_job(-42, 8, "other".into(), false)
            .await?;
        for (chat, user, generation) in [(-43, 7, 1), (-42, 8, 1), (-42, 7, 2)] {
            assert_eq!(
                repo.resume_collection_job(id, chat, user, generation)
                    .await?,
                ResumeResult::Unavailable
            );
        }
        let (first, second) = tokio::join!(
            repo.resume_collection_job(id, -42, 7, 1),
            repo.resume_collection_job(id, -42, 7, 1),
        );
        assert!(matches!(
            (first?, second?),
            (ResumeResult::Queued, ResumeResult::Unavailable)
                | (ResumeResult::Unavailable, ResumeResult::Queued)
        ));
        assert_eq!(repo.claim_collection_job().await?.unwrap().id, other);
        repo.fail_collection_job(other).await?;
        let resumed = repo.claim_collection_job().await?.unwrap();
        assert_eq!(resumed.id, id);
        assert_eq!(resumed.generation, 2);
        assert_eq!(resumed.delivered, 12);
        assert_eq!(resumed.cursor.as_deref(), Some(cursor));
        assert_eq!(
            (resumed.source.as_str(), resumed.telegraph),
            ("first", true)
        );
        repo.mark_collection_publishing(id).await?;
        assert_eq!(
            repo.resume_collection_job(id, -42, 7, 1).await?,
            ResumeResult::Unavailable
        );
        repo.finish_collection_batch(id, 19, None).await?;
        assert!(repo
            .db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT id FROM collection_jobs"
            ))
            .await?
            .is_none());
        assert!(
            repo.enqueue_collection_job(-42, 7, "next".into(), false)
                .await?
                > id
        );
        Ok(())
    }

    #[tokio::test]
    async fn startup_requeues_preparing_without_replaying_publishing() -> Result<()> {
        let db = Database::connect("sqlite::memory:").await?;
        migration::Migrator::up(&db, None).await?;
        let repo = Repo::new(db);
        let interrupted = repo
            .enqueue_collection_job(-42, 7, "interrupted".into(), true)
            .await?;
        repo.claim_collection_job().await?.unwrap();
        repo.mark_collection_publishing(interrupted).await?;
        repo.finish_collection_batch(interrupted, 12, Some(r#"{"offset":12}"#.into()))
            .await?;
        assert_eq!(
            repo.resume_collection_job(interrupted, -42, 7, 1).await?,
            ResumeResult::Queued
        );
        let publishing = repo.claim_collection_job().await?.unwrap();
        repo.mark_collection_publishing(interrupted).await?;

        let waiting = repo
            .enqueue_collection_job(-42, 7, "waiting".into(), false)
            .await?;
        repo.claim_collection_job().await?.unwrap();
        repo.mark_collection_publishing(waiting).await?;
        repo.finish_collection_batch(waiting, 5, Some(r#"{"offset":5}"#.into()))
            .await?;
        let preparing = repo
            .enqueue_collection_job(-42, 8, "preparing".into(), false)
            .await?;
        assert_eq!(repo.claim_collection_job().await?.unwrap().id, preparing);

        assert_eq!(repo.recover_collection_jobs().await?, vec![publishing]);
        assert!(repo.recover_collection_jobs().await?.is_empty());
        assert_eq!(repo.claim_collection_job().await?.unwrap().id, preparing);
        assert!(repo.claim_collection_job().await?.is_none());
        assert_eq!(
            repo.resume_collection_job(interrupted, -42, 7, 2).await?,
            ResumeResult::Unavailable
        );
        assert_eq!(
            repo.resume_collection_job(waiting, -42, 7, 1).await?,
            ResumeResult::Queued
        );
        Ok(())
    }
}
