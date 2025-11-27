use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Remove latest_data column from tasks table
        // This field is no longer used - push state is now stored in subscriptions.latest_data
        manager
            .alter_table(
                Table::alter()
                    .table(Tasks::Table)
                    .drop_column(Tasks::LatestData)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Re-add latest_data column to tasks table
        manager
            .alter_table(
                Table::alter()
                    .table(Tasks::Table)
                    .add_column(ColumnDef::new(Tasks::LatestData).json())
                    .to_owned(),
            )
            .await?;

        Ok(())
    }
}

#[derive(DeriveIden)]
enum Tasks {
    Table,
    LatestData,
}
