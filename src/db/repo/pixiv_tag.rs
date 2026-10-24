use super::Repo;
use crate::db::entities::subscriptions;
use crate::db::types::TagFilter;
use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate};
use pixiv_client::Illust;
use sea_orm::{
    sea_query::OnConflict, ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, FromQueryResult,
    QueryFilter, Set, Statement, TransactionTrait,
};

#[derive(Debug, FromQueryResult)]
pub struct TagSettings {
    pub min_bookmarks: i64,
    pub max_pages: Option<i64>,
}

#[derive(Debug, Clone, FromQueryResult)]
pub struct TagScan {
    pub cursor_date: NaiveDate,
    pub cursor_offset: i64,
    pub cursor_time: Option<i64>,
    pub complete: bool,
}

#[derive(Debug, FromQueryResult)]
pub struct TagCandidate {
    pub illust: String,
    pub created_at: i64,
}

#[derive(Debug, FromQueryResult)]
pub struct TagDelivery {
    pub subscription_id: i32,
    pub illust_id: i64,
    pub illust: String,
    pub attempts: i64,
}

impl Repo {
    pub async fn upsert_pixiv_tag_subscription(
        &self,
        chat_id: i64,
        task_id: i32,
        filter_tags: TagFilter,
        settings: &TagSettings,
    ) -> Result<()> {
        let txn = self.db.begin().await?;
        subscriptions::Entity::insert(subscriptions::ActiveModel {
            chat_id: Set(chat_id),
            task_id: Set(task_id),
            filter_tags: Set(filter_tags),
            created_at: Set(Local::now().naive_local()),
            ..Default::default()
        })
        .on_conflict(
            OnConflict::columns([subscriptions::Column::ChatId, subscriptions::Column::TaskId])
                .update_column(subscriptions::Column::FilterTags)
                .to_owned(),
        )
        .exec(&txn)
        .await?;
        let sub = subscriptions::Entity::find()
            .filter(subscriptions::Column::ChatId.eq(chat_id))
            .filter(subscriptions::Column::TaskId.eq(task_id))
            .one(&txn)
            .await?
            .context("Tag subscription disappeared")?;
        txn.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO pixiv_tag_settings(subscription_id, min_bookmarks, max_pages)
             VALUES (?, ?, ?) ON CONFLICT(subscription_id) DO UPDATE SET
             min_bookmarks=excluded.min_bookmarks, max_pages=excluded.max_pages",
            [
                sub.id.into(),
                settings.min_bookmarks.into(),
                settings.max_pages.into(),
            ],
        ))
        .await?;
        txn.commit().await?;
        Ok(())
    }

    pub async fn pixiv_tag_settings(&self, sub_id: i32) -> Result<Option<TagSettings>> {
        Ok(
            TagSettings::find_by_statement(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT min_bookmarks, max_pages FROM pixiv_tag_settings WHERE subscription_id=?",
                [sub_id.into()],
            ))
            .one(&self.db)
            .await?,
        )
    }

    pub async fn pixiv_tag_scan(
        &self,
        task_id: i32,
        oldest_date: NaiveDate,
        window_days: u16,
    ) -> Result<TagScan> {
        self.db
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO pixiv_tag_scans(task_id, cursor_date, window_days)
             VALUES (?, ?, ?) ON CONFLICT(task_id) DO UPDATE SET
             cursor_date=excluded.cursor_date, cursor_offset=0, cursor_time=NULL,
             complete=0,
             window_days=excluded.window_days
             WHERE pixiv_tag_scans.complete=1 OR pixiv_tag_scans.cursor_date<excluded.cursor_date
                OR pixiv_tag_scans.window_days<>excluded.window_days",
                [
                    task_id.into(),
                    oldest_date.into(),
                    i32::from(window_days).into(),
                ],
            ))
            .await?;
        TagScan::find_by_statement(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT cursor_date, cursor_offset, cursor_time, complete
             FROM pixiv_tag_scans WHERE task_id=?",
            [task_id.into()],
        ))
        .one(&self.db)
        .await?
        .context("Tag scan disappeared")
    }

    /// Candidate snapshots and the page checkpoint commit together. A failed page
    /// can be replayed; delivery insertion is idempotent and happens before this.
    pub async fn save_pixiv_tag_page(
        &self,
        task_id: i32,
        illusts: &[(Illust, i64)],
        scan: &TagScan,
    ) -> Result<()> {
        let txn = self.db.begin().await?;
        for (illust, created_at) in illusts {
            txn.execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO pixiv_tag_candidates(task_id, illust_id, illust, created_at)
                 VALUES (?, ?, ?, ?) ON CONFLICT(task_id, illust_id) DO UPDATE SET
                 illust=excluded.illust, created_at=excluded.created_at",
                [
                    task_id.into(),
                    i64::try_from(illust.id)?.into(),
                    serde_json::to_string(illust)?.into(),
                    (*created_at).into(),
                ],
            ))
            .await?;
        }
        txn.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE pixiv_tag_scans SET cursor_date=?, cursor_offset=?, cursor_time=?,
                 complete=? WHERE task_id=?",
            [
                scan.cursor_date.into(),
                scan.cursor_offset.into(),
                scan.cursor_time.into(),
                scan.complete.into(),
                task_id.into(),
            ],
        ))
        .await?;
        txn.commit().await?;
        Ok(())
    }

    pub async fn pixiv_tag_candidates(&self, task_id: i32) -> Result<Vec<TagCandidate>> {
        Ok(
            TagCandidate::find_by_statement(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT c.illust, c.created_at FROM pixiv_tag_candidates c
             WHERE c.task_id=? AND EXISTS (
                 SELECT 1 FROM subscriptions s WHERE s.task_id=c.task_id
                 AND NOT EXISTS (SELECT 1 FROM pixiv_push_history h
                     WHERE h.chat_id=s.chat_id AND h.illust_id=c.illust_id AND h.done=1)
                 AND NOT EXISTS (SELECT 1 FROM pixiv_tag_deliveries d
                     WHERE d.subscription_id=s.id AND d.illust_id=c.illust_id))",
                [task_id.into()],
            ))
            .all(&self.db)
            .await?,
        )
    }

    pub async fn prune_pixiv_tag_candidates(&self, task_id: i32, before: i64) -> Result<()> {
        self.db
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "DELETE FROM pixiv_tag_candidates WHERE task_id=? AND created_at<?",
                [task_id.into(), before.into()],
            ))
            .await?;
        Ok(())
    }

    pub async fn enqueue_pixiv_tag_delivery(
        &self,
        sub_id: i32,
        illust: &Illust,
        now: i64,
    ) -> Result<()> {
        let published_at = DateTime::parse_from_rfc3339(&illust.create_date)?.timestamp();
        self.db.execute(Statement::from_sql_and_values(DbBackend::Sqlite,
            "INSERT INTO pixiv_tag_deliveries(subscription_id, illust_id, illust, next_attempt_at, published_at)
             SELECT s.id, ?, ?, ?, ? FROM subscriptions s WHERE s.id=?
             AND NOT EXISTS (SELECT 1 FROM pixiv_push_history h
                 WHERE h.chat_id=s.chat_id AND h.illust_id=? AND h.done=1)
             ON CONFLICT(subscription_id, illust_id) DO NOTHING",
            [i64::try_from(illust.id)?.into(), serde_json::to_string(illust)?.into(), now.into(), published_at.into(),
             sub_id.into(), i64::try_from(illust.id)?.into()],
        )).await?;
        Ok(())
    }

    pub async fn pixiv_tag_deliveries_due(
        &self,
        sub_id: i32,
        now: i64,
    ) -> Result<Vec<TagDelivery>> {
        Ok(TagDelivery::find_by_statement(Statement::from_sql_and_values(DbBackend::Sqlite,
            "SELECT subscription_id, illust_id, illust, attempts
             FROM pixiv_tag_deliveries WHERE subscription_id=? AND done=0 AND next_attempt_at<=?
             ORDER BY next_attempt_at, json_extract(illust, '$.total_bookmarks') DESC, illust_id LIMIT 5",
            [sub_id.into(), now.into()],
        )).all(&self.db).await?)
    }

    pub async fn pixiv_tag_subscriptions_due(&self, now: i64) -> Result<Vec<subscriptions::Model>> {
        Ok(
            subscriptions::Model::find_by_statement(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT s.* FROM subscriptions s WHERE EXISTS (
                SELECT 1 FROM pixiv_tag_deliveries d WHERE d.subscription_id=s.id
                AND d.done=0 AND d.next_attempt_at<=?) ORDER BY s.id",
                [now.into()],
            ))
            .all(&self.db)
            .await?,
        )
    }

    /// Update the queue after the sender has saved shared progress and messages.
    pub async fn save_pixiv_tag_delivery(
        &self,
        delivery: &TagDelivery,
        done: bool,
        next_attempt_at: i64,
    ) -> Result<()> {
        self.db
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "UPDATE pixiv_tag_deliveries SET attempts=?, done=?, next_attempt_at=?,
             illust=CASE WHEN ? THEN '{}' ELSE illust END
             WHERE subscription_id=? AND illust_id=? AND done=0",
                [
                    delivery.attempts.into(),
                    done.into(),
                    next_attempt_at.into(),
                    done.into(),
                    delivery.subscription_id.into(),
                    delivery.illust_id.into(),
                ],
            ))
            .await?;
        Ok(())
    }
}
