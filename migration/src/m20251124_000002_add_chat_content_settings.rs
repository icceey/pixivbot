use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Add blur_sensitive_tags column to chats table (default true)
        manager
            .alter_table(
                Table::alter()
                    .table(Chats::Table)
                    .add_column(
                        ColumnDef::new(Chats::BlurSensitiveTags)
                            .boolean()
                            .not_null()
                            .default(true)
                    )
                    .to_owned(),
            )
            .await?;

        // Add excluded_tags column to chats table (JSON array)
        manager
            .alter_table(
                Table::alter()
                    .table(Chats::Table)
                    .add_column(ColumnDef::new(Chats::ExcludedTags).json())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Remove excluded_tags column
        manager
            .alter_table(
                Table::alter()
                    .table(Chats::Table)
                    .drop_column(Chats::ExcludedTags)
                    .to_owned(),
            )
            .await?;

        // Remove blur_sensitive_tags column
        manager
            .alter_table(
                Table::alter()
                    .table(Chats::Table)
                    .drop_column(Chats::BlurSensitiveTags)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum Chats {
    Table,
    BlurSensitiveTags,
    ExcludedTags,
}
