use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Add zip_path column
        manager
            .alter_table(
                Table::alter()
                    .table(EhDownloadQueue::Table)
                    .add_column(ColumnDef::new(EhDownloadQueue::ZipPath).text().null())
                    .to_owned(),
            )
            .await?;

        // Add telegraph_url column
        manager
            .alter_table(
                Table::alter()
                    .table(EhDownloadQueue::Table)
                    .add_column(ColumnDef::new(EhDownloadQueue::TelegraphUrl).text().null())
                    .to_owned(),
            )
            .await?;

        // Add next_retry_at column
        manager
            .alter_table(
                Table::alter()
                    .table(EhDownloadQueue::Table)
                    .add_column(
                        ColumnDef::new(EhDownloadQueue::NextRetryAt)
                            .timestamp()
                            .null(),
                    )
                    .to_owned(),
            )
            .await?;

        // Composite index for worker queries: (status, next_retry_at)
        manager
            .create_index(
                Index::create()
                    .name("idx_eh_download_queue_status_retry")
                    .table(EhDownloadQueue::Table)
                    .col(EhDownloadQueue::Status)
                    .col(EhDownloadQueue::NextRetryAt)
                    .to_owned(),
            )
            .await?;

        // Migrate stale data: any 'downloading' entries → 'pending'
        manager
            .get_connection()
            .execute_unprepared(
                "UPDATE eh_download_queue SET status = 'pending', started_at = NULL WHERE status = 'downloading'",
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                Index::drop()
                    .name("idx_eh_download_queue_status_retry")
                    .to_owned(),
            )
            .await?;

        for col in [
            EhDownloadQueue::NextRetryAt,
            EhDownloadQueue::TelegraphUrl,
            EhDownloadQueue::ZipPath,
        ] {
            manager
                .alter_table(
                    Table::alter()
                        .table(EhDownloadQueue::Table)
                        .drop_column(col)
                        .to_owned(),
                )
                .await?;
        }

        Ok(())
    }
}

#[derive(DeriveIden)]
enum EhDownloadQueue {
    Table,
    Status,
    ZipPath,
    TelegraphUrl,
    NextRetryAt,
}
