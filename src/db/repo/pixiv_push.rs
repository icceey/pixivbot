use super::Repo;
use anyhow::Result;
use chrono::Local;
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

    /// Commit acknowledged pages and the batch's reply mapping together while
    /// holding lock_pixiv_push. Queue/cursor updates may safely lag this commit.
    pub async fn save_pixiv_push_progress(
        &self,
        chat_id: i64,
        illust_id: u64,
        sent_pages: &[usize],
        done: bool,
        message: Option<(i32, i32)>,
    ) -> Result<()> {
        let txn = self.db.begin().await?;
        txn.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO pixiv_push_history(chat_id, illust_id, sent_pages, done)
             VALUES (?, ?, ?, ?) ON CONFLICT(chat_id, illust_id) DO UPDATE SET
             sent_pages=excluded.sent_pages, done=excluded.done WHERE pixiv_push_history.done=0",
            [
                chat_id.into(),
                i64::try_from(illust_id)?.into(),
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
}
