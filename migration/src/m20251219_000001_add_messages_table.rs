use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Create messages table to store bot-sent messages
        // This enables reply-based operations like "unsub this"
        manager
            .create_table(
                Table::create()
                    .table(Messages::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Messages::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Messages::ChatId).big_integer().not_null())
                    .col(ColumnDef::new(Messages::MessageId).integer().not_null())
                    .col(
                        ColumnDef::new(Messages::SubscriptionId)
                            .integer()
                            .not_null(),
                    )
                    .col(ColumnDef::new(Messages::IllustId).big_unsigned())
                    .col(
                        ColumnDef::new(Messages::CreatedAt)
                            .timestamp()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_messages_subscription")
                            .from(Messages::Table, Messages::SubscriptionId)
                            .to(Subscriptions::Table, Subscriptions::Id)
                            .on_delete(ForeignKeyAction::Cascade)
                            .on_update(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // Create index on (chat_id, message_id) for quick lookup when user replies
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_messages_chat_message")
                    .table(Messages::Table)
                    .col(Messages::ChatId)
                    .col(Messages::MessageId)
                    .unique()
                    .to_owned(),
            )
            .await?;

        // Create index on subscription_id for finding messages by subscription
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_messages_subscription")
                    .table(Messages::Table)
                    .col(Messages::SubscriptionId)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Messages::Table).to_owned())
            .await?;
        Ok(())
    }
}

#[derive(DeriveIden)]
enum Messages {
    Table,
    Id,
    ChatId,
    MessageId,
    SubscriptionId,
    IllustId,
    CreatedAt,
}

#[derive(DeriveIden)]
enum Subscriptions {
    Table,
    Id,
}
