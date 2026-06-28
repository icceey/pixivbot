use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Remove duplicate rows before adding unique index.
        // Keep the oldest entry per (chat_id, gid) and delete the rest.
        manager
            .get_connection()
            .execute_unprepared(
                "DELETE FROM eh_download_queue WHERE id NOT IN ( \
                    SELECT MIN(id) FROM eh_download_queue GROUP BY chat_id, gid \
                )",
            )
            .await?;

        // Unique index: one queue entry per (chat_id, gid).
        // If a user re-issues /edl for the same gallery, the existing entry is reused.
        manager
            .create_index(
                Index::create()
                    .name("idx_eh_download_queue_chat_gid_unique")
                    .table(EhDownloadQueue::Table)
                    .col(EhDownloadQueue::ChatId)
                    .col(EhDownloadQueue::Gid)
                    .unique()
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                Index::drop()
                    .name("idx_eh_download_queue_chat_gid_unique")
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum EhDownloadQueue {
    Table,
    ChatId,
    Gid,
}
