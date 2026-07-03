use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Add eh_filter column to subscriptions table
        manager
            .alter_table(
                Table::alter()
                    .table(Subscriptions::Table)
                    .add_column(ColumnDef::new(Subscriptions::EhFilter).json().null())
                    .to_owned(),
            )
            .await?;

        // Create eh_download_queue table
        manager
            .create_table(
                Table::create()
                    .table(EhDownloadQueue::Table)
                    .col(
                        ColumnDef::new(EhDownloadQueue::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(EhDownloadQueue::ChatId)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(EhDownloadQueue::Gid)
                            .big_unsigned()
                            .not_null(),
                    )
                    .col(ColumnDef::new(EhDownloadQueue::Token).string().not_null())
                    .col(ColumnDef::new(EhDownloadQueue::Title).string().not_null())
                    .col(
                        ColumnDef::new(EhDownloadQueue::Telegraph)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(
                        ColumnDef::new(EhDownloadQueue::Source)
                            .string_len(20)
                            .not_null()
                            .default("subscription"),
                    )
                    .col(
                        ColumnDef::new(EhDownloadQueue::Status)
                            .string_len(20)
                            .not_null()
                            .default("pending"),
                    )
                    .col(
                        ColumnDef::new(EhDownloadQueue::FileSize)
                            .big_unsigned()
                            .default(0),
                    )
                    .col(ColumnDef::new(EhDownloadQueue::Error).text().null())
                    .col(
                        ColumnDef::new(EhDownloadQueue::RetryCount)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(EhDownloadQueue::CreatedAt)
                            .timestamp()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .col(
                        ColumnDef::new(EhDownloadQueue::StartedAt)
                            .timestamp()
                            .null(),
                    )
                    .col(
                        ColumnDef::new(EhDownloadQueue::CompletedAt)
                            .timestamp()
                            .null(),
                    )
                    .to_owned(),
            )
            .await?;

        // Index on status for efficient pending query
        manager
            .create_index(
                Index::create()
                    .name("idx_eh_download_queue_status")
                    .table(EhDownloadQueue::Table)
                    .col(EhDownloadQueue::Status)
                    .to_owned(),
            )
            .await?;

        // Index on completed_at for rate-limit window query
        manager
            .create_index(
                Index::create()
                    .name("idx_eh_download_queue_completed_at")
                    .table(EhDownloadQueue::Table)
                    .col(EhDownloadQueue::CompletedAt)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Drop eh_download_queue table
        manager
            .drop_table(Table::drop().table(EhDownloadQueue::Table).to_owned())
            .await?;

        // Drop eh_filter column from subscriptions
        manager
            .alter_table(
                Table::alter()
                    .table(Subscriptions::Table)
                    .drop_column(Subscriptions::EhFilter)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum Subscriptions {
    Table,
    EhFilter,
}

#[derive(DeriveIden)]
enum EhDownloadQueue {
    Table,
    Id,
    ChatId,
    Gid,
    Token,
    Title,
    Telegraph,
    Source,
    Status,
    FileSize,
    Error,
    RetryCount,
    CreatedAt,
    StartedAt,
    CompletedAt,
}
