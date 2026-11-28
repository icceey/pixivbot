use anyhow::{Context, Result};
use sea_orm::DatabaseConnection;
use std::collections::HashMap;
use std::sync::Weak;
use tokio::sync::Mutex;

mod chats;
pub mod collection_jobs;
pub mod eh_download_completions;
pub mod eh_download_queue;
pub mod eh_gallery_jobs;
pub mod eh_gallery_push_ledger;
pub mod eh_gallery_results;
pub mod eh_gp_spend_attempts;
mod messages;
mod pixiv_push;
pub mod pixiv_tag;
mod stats;
mod subscriptions;
mod tasks;
mod users;

pub struct Repo {
    db: DatabaseConnection,
    pixiv_push_locks: Mutex<HashMap<i64, Weak<Mutex<()>>>>,
}

impl Repo {
    pub fn new(db: DatabaseConnection) -> Self {
        Self {
            db,
            pixiv_push_locks: Mutex::new(HashMap::new()),
        }
    }

    pub async fn ping(&self) -> Result<()> {
        self.db.ping().await.context("Database ping failed")
    }

    /// Get a reference to the underlying DB connection (for tests).
    #[cfg(test)]
    pub(crate) fn db(&self) -> &DatabaseConnection {
        &self.db
    }
}

/// Shared test helpers for repo unit tests.
#[cfg(test)]
pub mod tests_helpers {
    use super::Repo;
    use anyhow::Result;
    use migration::MigratorTrait;
    use sea_orm::{ConnectionTrait, Database, DatabaseConnection};

    pub async fn setup_test_db() -> Result<Repo> {
        let db = Database::connect("sqlite::memory:").await?;
        db.execute_unprepared("PRAGMA foreign_keys = ON").await?;
        create_schema(&db).await?;
        Ok(Repo::new(db))
    }

    /// Apply the real migration chain on an arbitrary SQLite connection so
    /// tests exercise the same schema and indexes as production.
    pub async fn create_schema(db: &DatabaseConnection) -> Result<()> {
        migration::Migrator::up(db, None).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::tests_helpers::setup_test_db;
    use crate::db::types::Tags;

    #[tokio::test]
    async fn test_migrate_chat_success() {
        let repo = setup_test_db().await.unwrap();

        let old_chat_id = -888888;
        let new_chat_id = -1009999999999;

        repo.upsert_chat(
            old_chat_id,
            "group".to_string(),
            Some("Test Group".to_string()),
            true,
            Tags::from(vec!["nsfw".to_string()]),
        )
        .await
        .unwrap();

        let task = repo
            .get_or_create_task(
                crate::db::types::TaskType::Author,
                "12345".to_string(),
                Some("TestAuthor".to_string()),
            )
            .await
            .unwrap();

        let sub = repo
            .upsert_subscription(old_chat_id, task.id, crate::db::types::TagFilter::default())
            .await
            .unwrap();

        repo.save_message(old_chat_id, 12345, sub.id, Some(67890))
            .await
            .unwrap();

        repo.migrate_chat(old_chat_id, new_chat_id).await.unwrap();
        repo.migrate_chat(old_chat_id, new_chat_id).await.unwrap();

        let old_chat = repo.get_chat(old_chat_id).await.unwrap();
        assert!(old_chat.is_none());

        let new_chat = repo.get_chat(new_chat_id).await.unwrap().unwrap();
        assert_eq!(new_chat.id, new_chat_id);
        assert!(new_chat.enabled);
        assert_eq!(new_chat.title, Some("Test Group".to_string()));
        assert_eq!(new_chat.r#type, "group");

        let subs = repo.list_subscriptions_by_chat(new_chat_id).await.unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].0.chat_id, new_chat_id);

        let old_subs = repo.list_subscriptions_by_chat(old_chat_id).await.unwrap();
        assert_eq!(old_subs.len(), 0);
        let (message, related) = repo
            .get_message_with_subscription(new_chat_id, 12345)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(message.subscription_id, sub.id);
        assert_eq!(related.unwrap().0.id, sub.id);
    }

    #[tokio::test]
    async fn test_migrate_chat_with_preexisting_new_chat() {
        let repo = setup_test_db().await.unwrap();

        let old_chat_id = -888888;
        let new_chat_id = -1009999999999;

        repo.upsert_chat(
            old_chat_id,
            "group".to_string(),
            Some("Old Group".to_string()),
            true,
            Tags::default(),
        )
        .await
        .unwrap();

        repo.upsert_chat(
            new_chat_id,
            "supergroup".to_string(),
            Some("New Supergroup".to_string()),
            false,
            Tags::default(),
        )
        .await
        .unwrap();

        repo.save_pixiv_push_progress(
            old_chat_id,
            1,
            chrono::Utc::now().timestamp(),
            &[0, 1],
            false,
            None,
        )
        .await
        .unwrap();
        repo.save_pixiv_push_progress(
            new_chat_id,
            1,
            chrono::Utc::now().timestamp(),
            &[1, 2],
            false,
            None,
        )
        .await
        .unwrap();
        repo.save_pixiv_push_progress(
            old_chat_id,
            2,
            chrono::Utc::now().timestamp(),
            &[0],
            false,
            None,
        )
        .await
        .unwrap();
        repo.save_pixiv_push_progress(
            new_chat_id,
            2,
            chrono::Utc::now().timestamp(),
            &[],
            true,
            None,
        )
        .await
        .unwrap();
        repo.migrate_chat(old_chat_id, new_chat_id).await.unwrap();

        assert_eq!(
            repo.pixiv_push_progress(new_chat_id, 1)
                .await
                .unwrap()
                .sent_pages,
            vec![0, 1, 2]
        );
        assert!(repo.pixiv_push_progress(new_chat_id, 2).await.unwrap().done);
        let new_chat = repo.get_chat(new_chat_id).await.unwrap().unwrap();
        assert!(new_chat.enabled);
        assert_eq!(new_chat.title, Some("Old Group".to_string()));
    }
}

#[cfg(test)]
mod eh_integration_tests;
