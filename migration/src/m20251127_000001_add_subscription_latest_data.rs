use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Add latest_data column to subscriptions table
        // This stores per-subscription push state (e.g., pushed_ids for ranking, latest_illust_id for author)
        manager
            .alter_table(
                Table::alter()
                    .table(Subscriptions::Table)
                    .add_column(ColumnDef::new(Subscriptions::LatestData).json())
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Subscriptions::Table)
                    .drop_column(Subscriptions::LatestData)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }
}

#[derive(DeriveIden)]
enum Subscriptions {
    Table,
    LatestData,
}
