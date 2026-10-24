use sea_orm::{ConnectionTrait, TransactionTrait};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let txn = manager.get_connection().begin().await?;
        txn.execute_unprepared(
            "ALTER TABLE pixiv_tag_scans ADD COLUMN cursor_time INTEGER;
             ALTER TABLE pixiv_tag_scans DROP COLUMN pages_scanned;
             ALTER TABLE pixiv_tag_scans DROP COLUMN total_pages;
             UPDATE tasks SET next_poll_at=datetime('now', 'localtime')
             WHERE type='pixiv_tag' AND next_poll_at>datetime('now', 'localtime');",
        )
        .await?;
        txn.commit().await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let txn = manager.get_connection().begin().await?;
        txn.execute_unprepared(
            "ALTER TABLE pixiv_tag_scans ADD COLUMN pages_scanned INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE pixiv_tag_scans ADD COLUMN total_pages INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE pixiv_tag_scans DROP COLUMN cursor_time;",
        )
        .await?;
        txn.commit().await
    }
}
