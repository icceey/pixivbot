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
                        ColumnDef::new(EhDownloadQueue::GpCost)
                            .big_integer()
                            .not_null()
                            .default(0),
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
                    .drop_column(EhDownloadQueue::GpCost)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum EhDownloadQueue {
    Table,
    GpCost,
}
