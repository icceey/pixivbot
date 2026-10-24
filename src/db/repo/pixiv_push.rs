use super::Repo;
use anyhow::Result;
use chrono::Local;
use pixiv_client::Illust;
use sea_orm::{ConnectionTrait, DbBackend, Statement, TransactionTrait};
use std::sync::Arc;
use tokio::sync::{Mutex, OwnedMutexGuard};

#[derive(Default)]
pub struct PixivPushProgress {
    pub sent_pages: Vec<usize>,
    pub done: bool,
}

impl Repo {
    /// Author and tag engines share this Repo. Hold the guard from the history
    /// read through sending and saving. Serializing a chat also keeps different
    /// works' batches from interleaving between these two engines.
    pub async fn lock_pixiv_push(&self, chat_id: i64) -> OwnedMutexGuard<()> {
        let mutex = {
            let mut locks = self.pixiv_push_locks.lock().await;
            locks.retain(|_, mutex| mutex.strong_count() > 0);
            let entry = locks.entry(chat_id).or_default();
            match entry.upgrade() {
                Some(mutex) => mutex,
                None => {
                    let mutex = Arc::new(Mutex::new(()));
                    *entry = Arc::downgrade(&mutex);
                    mutex
                }
            }
        };
        mutex.lock_owned().await
    }

    pub async fn pixiv_push_progress(
        &self,
        chat_id: i64,
        illust_id: u64,
    ) -> Result<PixivPushProgress> {
        let row = self
            .db
            .query_one(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT sent_pages, done FROM pixiv_push_history WHERE chat_id=? AND illust_id=?",
                [chat_id.into(), i64::try_from(illust_id)?.into()],
            ))
            .await?;
        match row {
            Some(row) => Ok(PixivPushProgress {
                sent_pages: serde_json::from_str(&row.try_get::<String>("", "sent_pages")?)?,
                done: row.try_get("", "done")?,
            }),
            None => Ok(PixivPushProgress::default()),
        }
    }

    /// Refresh work metadata before reading shared progress under the chat lock.
    pub async fn begin_pixiv_push(
        &self,
        chat_id: i64,
        illust: &Illust,
    ) -> Result<PixivPushProgress> {
        let published_at = chrono::DateTime::parse_from_rfc3339(&illust.create_date)?.timestamp();
        self.db
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO pixiv_push_history(chat_id, illust_id, published_at, author_id)
             VALUES (?, ?, ?, ?) ON CONFLICT(chat_id, illust_id) DO UPDATE SET
             published_at=excluded.published_at, author_id=excluded.author_id",
                [
                    chat_id.into(),
                    i64::try_from(illust.id)?.into(),
                    published_at.into(),
                    i64::try_from(illust.user.id)?.into(),
                ],
            ))
            .await?;
        self.pixiv_push_progress(chat_id, illust.id).await
    }

    /// Commit acknowledged pages and the batch's reply mapping together while
    /// holding lock_pixiv_push. Queue/cursor updates may safely lag this commit.
    pub async fn save_pixiv_push_progress(
        &self,
        chat_id: i64,
        illust_id: u64,
        published_at: i64,
        sent_pages: &[usize],
        done: bool,
        message: Option<(i32, i32)>,
    ) -> Result<()> {
        let txn = self.db.begin().await?;
        txn.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO pixiv_push_history(chat_id, illust_id, published_at, sent_pages, done)
             VALUES (?, ?, ?, ?, ?) ON CONFLICT(chat_id, illust_id) DO UPDATE SET
             published_at=excluded.published_at, sent_pages=excluded.sent_pages,
             done=excluded.done WHERE pixiv_push_history.done=0",
            [
                chat_id.into(),
                i64::try_from(illust_id)?.into(),
                published_at.into(),
                serde_json::to_string(if done { &[][..] } else { sent_pages })?.into(),
                done.into(),
            ],
        ))
        .await?;
        if let Some((subscription_id, message_id)) = message {
            txn.execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO messages(chat_id, message_id, subscription_id, illust_id, created_at)
                 VALUES (?, ?, ?, ?, ?) ON CONFLICT(chat_id, message_id) DO NOTHING",
                [
                    chat_id.into(),
                    message_id.into(),
                    subscription_id.into(),
                    i64::try_from(illust_id)?.into(),
                    Local::now().naive_local().into(),
                ],
            ))
            .await?;
        }
        txn.commit().await?;
        Ok(())
    }

    pub async fn prune_pixiv_push_history(&self, before: i64) -> Result<()> {
        let chats = self
            .db
            .query_all(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT chat_id FROM pixiv_push_history WHERE published_at<?1
             UNION SELECT s.chat_id FROM pixiv_tag_deliveries d
             JOIN subscriptions s ON s.id=d.subscription_id
             WHERE d.done=1 AND d.published_at<?1",
                [before.into()],
            ))
            .await?;
        for chat in chats {
            let chat_id: i64 = chat.try_get("", "chat_id")?;
            let _guard = self.lock_pixiv_push(chat_id).await;
            let txn = self.db.begin().await?;
            // Author progress may lag acknowledgements, including deduplicated
            // pushes originally sent by a tag subscription. Legacy history without
            // an author must wait for all potentially dependent author cursors.
            let pending = "WITH chat_subscriptions AS (
                SELECT s.id, s.latest_data, t.type, t.value FROM subscriptions s
                JOIN tasks t ON t.id=s.task_id WHERE s.chat_id=?1
             ), pending_works AS (
                SELECT d.illust_id FROM pixiv_tag_deliveries d
                JOIN chat_subscriptions s ON s.id=d.subscription_id WHERE d.done=0
                UNION SELECT json_extract(latest_data, '$.state.pending_illust.illust_id')
                FROM chat_subscriptions WHERE type='author'
                UNION SELECT h.illust_id FROM pixiv_push_history h
                JOIN chat_subscriptions s ON s.type='author'
                AND (h.author_id IS NULL OR CAST(s.value AS INTEGER)=h.author_id)
                WHERE h.chat_id=?1 AND h.illust_id>
                    COALESCE(json_extract(s.latest_data, '$.state.latest_illust_id'), 0)
             )";
            for delete in [
                "DELETE FROM pixiv_tag_deliveries AS d WHERE done=1 AND published_at<?2
                 AND subscription_id IN (SELECT id FROM chat_subscriptions)
                 AND NOT EXISTS (SELECT 1 FROM pending_works p WHERE p.illust_id=d.illust_id)",
                "DELETE FROM pixiv_push_history AS h WHERE chat_id=?1
                 AND published_at<?2
                 AND NOT EXISTS (SELECT 1 FROM pending_works p WHERE p.illust_id=h.illust_id)",
            ] {
                txn.execute(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    format!("{pending} {delete}"),
                    [chat_id.into(), before.into()],
                ))
                .await?;
            }
            txn.commit().await?;
        }
        Ok(())
    }
}
