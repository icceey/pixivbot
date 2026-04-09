use anyhow::{Context, Result};
use sea_orm::DatabaseConnection;

mod chats;
mod messages;
mod stats;
mod subscriptions;
mod tasks;
mod users;

pub struct Repo {
    db: DatabaseConnection,
}

impl Repo {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn ping(&self) -> Result<()> {
        self.db.ping().await.context("Database ping failed")
    }
}

#[cfg(test)]
mod tests {
    use super::Repo;
    use crate::db::types::{Tags, UserRole};
    use anyhow::Result;
    use sea_orm::{ConnectionTrait, Database, DbBackend, Statement};

    async fn setup_test_db() -> Result<Repo> {
        let db = Database::connect("sqlite::memory:").await?;

        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            r#"
            CREATE TABLE users (
                id INTEGER PRIMARY KEY NOT NULL,
                username TEXT,
                role TEXT NOT NULL DEFAULT 'user',
                created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        ))
        .await?;

        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            r#"
            CREATE TABLE chats (
                id INTEGER PRIMARY KEY NOT NULL,
                type TEXT NOT NULL,
                title TEXT,
                enabled BOOLEAN NOT NULL DEFAULT 0,
                blur_sensitive_tags BOOLEAN NOT NULL DEFAULT 1,
                excluded_tags TEXT NOT NULL DEFAULT '[]',
                sensitive_tags TEXT NOT NULL DEFAULT '[]',
                created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                allow_without_mention BOOLEAN NOT NULL DEFAULT 0
            )
            "#,
        ))
        .await?;

        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            r#"
            CREATE TABLE tasks (
                id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                type TEXT NOT NULL,
                value TEXT NOT NULL,
                author_name TEXT,
                next_poll_at TIMESTAMP NOT NULL,
                last_polled_at TIMESTAMP,
                UNIQUE(type, value)
            )
            "#,
        ))
        .await?;

        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            r#"
            CREATE TABLE subscriptions (
                id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                chat_id INTEGER NOT NULL,
                task_id INTEGER NOT NULL,
                filter_tags TEXT NOT NULL DEFAULT '{"include":[],"exclude":[]}',
                latest_data TEXT,
                created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (chat_id) REFERENCES chats(id) ON DELETE CASCADE ON UPDATE CASCADE,
                FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE ON UPDATE CASCADE,
                UNIQUE(chat_id, task_id)
            )
            "#,
        ))
        .await?;

        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            r#"
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                chat_id INTEGER NOT NULL,
                message_id INTEGER NOT NULL,
                subscription_id INTEGER NOT NULL,
                illust_id INTEGER,
                created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        ))
        .await?;

        Ok(Repo::new(db))
    }

    #[tokio::test]
    async fn test_migrate_chat_success() {
        let repo = setup_test_db().await.unwrap();

        let old_chat_id = -888888;
        let new_chat_id = -1009999999999;

        let chat = repo
            .upsert_chat(
                old_chat_id,
                "group".to_string(),
                Some("Test Group".to_string()),
                true,
                Tags::from(vec!["nsfw".to_string()]),
            )
            .await
            .unwrap();

        assert_eq!(chat.id, old_chat_id);
        assert!(chat.enabled);
        assert_eq!(chat.title, Some("Test Group".to_string()));

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

        assert_eq!(sub.chat_id, old_chat_id);

        repo.save_message(old_chat_id, 12345, sub.id, Some(67890))
            .await
            .unwrap();

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
    }

    #[tokio::test]
    async fn test_migrate_chat_idempotent() {
        let repo = setup_test_db().await.unwrap();

        let old_chat_id = -888888;
        let new_chat_id = -1009999999999;

        repo.upsert_chat(
            old_chat_id,
            "group".to_string(),
            Some("Test Group".to_string()),
            true,
            Tags::default(),
        )
        .await
        .unwrap();

        repo.migrate_chat(old_chat_id, new_chat_id).await.unwrap();

        let result = repo.migrate_chat(old_chat_id, new_chat_id).await;
        assert!(result.is_ok());

        let new_chat = repo.get_chat(new_chat_id).await.unwrap();
        assert!(new_chat.is_some());
    }

    #[tokio::test]
    async fn test_migrate_chat_old_not_found() {
        let repo = setup_test_db().await.unwrap();

        let old_chat_id = -888888;
        let new_chat_id = -1009999999999;

        let result = repo.migrate_chat(old_chat_id, new_chat_id).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
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

        repo.migrate_chat(old_chat_id, new_chat_id).await.unwrap();

        let new_chat = repo.get_chat(new_chat_id).await.unwrap().unwrap();
        assert!(new_chat.enabled);
        assert_eq!(new_chat.title, Some("Old Group".to_string()));
    }

    #[tokio::test]
    async fn test_has_owner_empty_database() {
        let repo = setup_test_db().await.unwrap();

        let has_owner = repo.has_owner().await.unwrap();
        assert!(!has_owner);
    }

    #[tokio::test]
    async fn test_has_owner_only_non_owner_users() {
        let repo = setup_test_db().await.unwrap();

        repo.upsert_user(12345, Some("user1".to_string()), UserRole::User)
            .await
            .unwrap();

        repo.upsert_user(67890, Some("admin1".to_string()), UserRole::Admin)
            .await
            .unwrap();

        let has_owner = repo.has_owner().await.unwrap();
        assert!(!has_owner);
    }

    #[tokio::test]
    async fn test_has_owner_with_owner() {
        let repo = setup_test_db().await.unwrap();

        repo.upsert_user(12345, Some("user1".to_string()), UserRole::User)
            .await
            .unwrap();

        repo.upsert_user(99999, Some("owner1".to_string()), UserRole::Owner)
            .await
            .unwrap();

        let has_owner = repo.has_owner().await.unwrap();
        assert!(has_owner);
    }

    #[tokio::test]
    async fn test_has_owner_multiple_owners() {
        let repo = setup_test_db().await.unwrap();

        repo.upsert_user(11111, Some("owner1".to_string()), UserRole::Owner)
            .await
            .unwrap();

        repo.upsert_user(22222, Some("owner2".to_string()), UserRole::Owner)
            .await
            .unwrap();

        let has_owner = repo.has_owner().await.unwrap();
        assert!(has_owner);
    }

    #[tokio::test]
    async fn test_create_user_with_auto_owner_first_user() {
        let repo = setup_test_db().await.unwrap();

        let user1 = repo
            .create_user_with_auto_owner(11111, Some("user1".to_string()))
            .await
            .unwrap();

        assert_eq!(user1.id, 11111);
        assert_eq!(user1.role, UserRole::Owner);

        let user2 = repo
            .create_user_with_auto_owner(22222, Some("user2".to_string()))
            .await
            .unwrap();

        assert_eq!(user2.id, 22222);
        assert_eq!(user2.role, UserRole::User);
    }

    #[tokio::test]
    async fn test_create_user_with_auto_owner_concurrent() {
        use std::sync::Arc;
        use tokio::task::JoinSet;

        let repo = Arc::new(setup_test_db().await.unwrap());

        let mut set = JoinSet::new();
        for i in 1..=10 {
            let repo_clone = Arc::clone(&repo);
            set.spawn(async move {
                repo_clone
                    .create_user_with_auto_owner(i, Some(format!("user{}", i)))
                    .await
            });
        }

        let mut owners = 0;
        let mut users = 0;
        while let Some(result) = set.join_next().await {
            let user = result.unwrap().unwrap();
            match user.role {
                UserRole::Owner => owners += 1,
                UserRole::User => users += 1,
                _ => {}
            }
        }

        assert_eq!(owners, 1, "Exactly one owner should be created");
        assert_eq!(users, 9, "Nine regular users should be created");
    }

    #[tokio::test]
    async fn test_create_user_with_auto_owner_preserves_existing_owner() {
        let repo = setup_test_db().await.unwrap();

        let owner = repo
            .upsert_user(11111, Some("owner".to_string()), UserRole::Owner)
            .await
            .unwrap();
        assert_eq!(owner.role, UserRole::Owner);

        let user = repo
            .upsert_user(22222, Some("user".to_string()), UserRole::User)
            .await
            .unwrap();
        assert_eq!(user.role, UserRole::User);

        let owner_updated = repo
            .create_user_with_auto_owner(11111, Some("owner_updated".to_string()))
            .await
            .unwrap();

        assert_eq!(owner_updated.id, 11111);
        assert_eq!(owner_updated.role, UserRole::Owner);
        assert_eq!(owner_updated.username, Some("owner_updated".to_string()));
    }
}
