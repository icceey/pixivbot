use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Remove duplicate rows before adding unique index.
        // Keep the highest-progress entry per (chat_id, gid) and delete the rest.
        // Priority: publishing > uploaded > uploading > downloaded > downloading > pending > done > failed.
        // Tie-break by newest COALESCE(completed_at, started_at, created_at), then highest id.
        manager
            .get_connection()
            .execute_unprepared(
                "DELETE FROM eh_download_queue \
                 WHERE id NOT IN ( \
                     SELECT id FROM ( \
                         SELECT id, \
                                ROW_NUMBER() OVER ( \
                                    PARTITION BY chat_id, gid \
                                    ORDER BY \
                                      CASE status \
                                        WHEN 'publishing' THEN 1 \
                                        WHEN 'uploaded' THEN 2 \
                                        WHEN 'uploading' THEN 3 \
                                        WHEN 'downloaded' THEN 4 \
                                        WHEN 'downloading' THEN 5 \
                                        WHEN 'pending' THEN 6 \
                                        WHEN 'done' THEN 7 \
                                        WHEN 'failed' THEN 8 \
                                        ELSE 9 \
                                      END, \
                                      COALESCE(completed_at, started_at, created_at) DESC, \
                                      id DESC \
                                ) AS rn \
                         FROM eh_download_queue \
                     ) ranked \
                     WHERE rn = 1 \
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
