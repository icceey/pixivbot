use sea_orm::{ConnectionTrait, TransactionTrait};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let txn = manager.get_connection().begin().await?;
        txn.execute_unprepared(
            "ALTER TABLE collection_jobs ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE collection_jobs ADD COLUMN next_retry_at INTEGER NULL;",
        )
        .await?;
        txn.commit().await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let txn = manager.get_connection().begin().await?;
        txn.execute_unprepared(
            "ALTER TABLE collection_jobs DROP COLUMN next_retry_at;
             ALTER TABLE collection_jobs DROP COLUMN retry_count;",
        )
        .await?;
        txn.commit().await
    }
}
