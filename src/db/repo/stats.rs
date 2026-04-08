use super::Repo;
use crate::db::entities::{subscriptions, tasks, users};
use crate::db::types::UserRole;
use anyhow::{Context, Result};
use sea_orm::{
    ConnectionTrait, EntityTrait, FromQueryResult, PaginatorTrait, QueryFilter, Statement,
};

impl Repo {
    pub async fn count_admin_users(&self) -> Result<u64> {
        users::Entity::find()
            .filter(users::Column::Role.is_in([UserRole::Admin, UserRole::Owner]))
            .count(&self.db)
            .await
            .context("Failed to count admin users")
    }

    pub async fn count_enabled_chats(&self) -> Result<u64> {
        #[derive(FromQueryResult)]
        struct CountResult {
            count: i64,
        }

        let result: Option<CountResult> =
            CountResult::find_by_statement(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                r#"
                SELECT COUNT(DISTINCT c.id) as count 
                FROM chats c
                LEFT JOIN users u ON c.id = u.id
                WHERE c.enabled = true OR u.role IN ('admin', 'owner')
            "#,
                [],
            ))
            .one(&self.db)
            .await
            .context("Failed to count enabled chats")?;

        Ok(result.map(|r| r.count as u64).unwrap_or(0))
    }

    pub async fn count_all_subscriptions(&self) -> Result<u64> {
        subscriptions::Entity::find()
            .count(&self.db)
            .await
            .context("Failed to count all subscriptions")
    }

    pub async fn count_all_tasks(&self) -> Result<u64> {
        tasks::Entity::find()
            .count(&self.db)
            .await
            .context("Failed to count all tasks")
    }
}
