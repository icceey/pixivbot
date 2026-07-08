use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(EhDownloadQueue::Table)
                    .add_column(
                        ColumnDef::new(EhDownloadQueue::BackgroundDownloadStatus)
                            .string()
                            .null(),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(EhDownloadQueue::Table)
                    .add_column(
                        ColumnDef::new(EhDownloadQueue::BackgroundDownloadStartedAt)
                            .timestamp()
                            .null(),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(EhDownloadQueue::Table)
                    .add_column(
                        ColumnDef::new(EhDownloadQueue::BackgroundDownloadNextRetryAt)
                            .timestamp()
                            .null(),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(EhDownloadQueue::Table)
                    .add_column(
                        ColumnDef::new(EhDownloadQueue::BackgroundDownloadAttemptCount)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(EhDownloadQueue::Table)
                    .add_column(
                        ColumnDef::new(EhDownloadQueue::BackgroundDownloadError)
                            .string()
                            .null(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(EhDownloadQueue::Table)
                    .drop_column(EhDownloadQueue::BackgroundDownloadError)
                    .drop_column(EhDownloadQueue::BackgroundDownloadAttemptCount)
                    .drop_column(EhDownloadQueue::BackgroundDownloadNextRetryAt)
                    .drop_column(EhDownloadQueue::BackgroundDownloadStartedAt)
                    .drop_column(EhDownloadQueue::BackgroundDownloadStatus)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum EhDownloadQueue {
    Table,
    BackgroundDownloadStatus,
    BackgroundDownloadStartedAt,
    BackgroundDownloadNextRetryAt,
    BackgroundDownloadAttemptCount,
    BackgroundDownloadError,
}
