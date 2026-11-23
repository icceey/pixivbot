use sea_orm::{ConnectionTrait, TransactionTrait};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let txn = manager.get_connection().begin().await?;
        txn.execute_unprepared(
            "CREATE TABLE collection_jobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                chat_id INTEGER NOT NULL,
                user_id INTEGER NOT NULL,
                source TEXT NOT NULL,
                telegraph BOOLEAN NOT NULL,
                generation INTEGER NOT NULL DEFAULT 1,
                delivered INTEGER NOT NULL DEFAULT 0,
                cursor TEXT,
                status TEXT NOT NULL DEFAULT 'queued'
                    CHECK (status IN ('queued', 'preparing', 'publishing', 'waiting')),
                queue_order INTEGER NOT NULL UNIQUE
            );
            CREATE INDEX idx_collection_jobs_status_order ON collection_jobs(status, queue_order);",
        )
        .await?;
        txn.commit().await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE collection_jobs")
            .await?;
        Ok(())
    }
}
