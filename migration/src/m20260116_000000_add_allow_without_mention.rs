//! Adds `allow_without_mention` column to `chats` table.
//!
//! This setting allows commands to be processed without @mention in group chats.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Chats::Table)
                    .add_column(
                        ColumnDef::new(Chats::AllowWithoutMention)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Chats::Table)
                    .drop_column(Chats::AllowWithoutMention)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum Chats {
    Table,
    AllowWithoutMention,
}
