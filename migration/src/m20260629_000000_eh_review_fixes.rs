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
                        ColumnDef::new(EhDownloadQueue::ArchiveSentAt)
                            .timestamp()
                            .null(),
                    )
                    .add_column(
                        ColumnDef::new(EhDownloadQueue::TelegraphSentAt)
                            .timestamp()
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
                    .drop_column(EhDownloadQueue::TelegraphSentAt)
                    .drop_column(EhDownloadQueue::ArchiveSentAt)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum EhDownloadQueue {
    Table,
    ArchiveSentAt,
    TelegraphSentAt,
}
