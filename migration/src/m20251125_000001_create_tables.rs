use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Create users table
        manager
            .create_table(
                Table::create()
                    .table(Users::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Users::Id)
                            .big_integer()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Users::Username).string())
                    .col(
                        ColumnDef::new(Users::Role)
                            .string()
                            .not_null()
                            .default("user"),
                    )
                    .col(
                        ColumnDef::new(Users::CreatedAt)
                            .timestamp()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;

        // Create chats table
        manager
            .create_table(
                Table::create()
                    .table(Chats::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Chats::Id)
                            .big_integer()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Chats::Type).string().not_null())
                    .col(ColumnDef::new(Chats::Title).string())
                    .col(
                        ColumnDef::new(Chats::Enabled)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(
                        ColumnDef::new(Chats::BlurSensitiveTags)
                            .boolean()
                            .not_null()
                            .default(true),
                    )
                    .col(ColumnDef::new(Chats::ExcludedTags).json())
                    .col(
                        ColumnDef::new(Chats::CreatedAt)
                            .timestamp()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;

        // Create tasks table (without interval_sec)
        manager
            .create_table(
                Table::create()
                    .table(Tasks::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Tasks::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Tasks::Type).string().not_null())
                    .col(ColumnDef::new(Tasks::Value).string().not_null())
                    .col(ColumnDef::new(Tasks::AuthorName).string())
                    .col(
                        ColumnDef::new(Tasks::NextPollAt)
                            .timestamp()
                            .not_null(),
                    )
                    .col(ColumnDef::new(Tasks::LastPolledAt).timestamp())
                    .col(ColumnDef::new(Tasks::LatestData).json())
                    .col(ColumnDef::new(Tasks::CreatedBy).big_integer().not_null())
                    .col(ColumnDef::new(Tasks::UpdatedBy).big_integer().not_null())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_tasks_created_by")
                            .from(Tasks::Table, Tasks::CreatedBy)
                            .to(Users::Table, Users::Id)
                            .on_delete(ForeignKeyAction::NoAction)
                            .on_update(ForeignKeyAction::NoAction),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_tasks_updated_by")
                            .from(Tasks::Table, Tasks::UpdatedBy)
                            .to(Users::Table, Users::Id)
                            .on_delete(ForeignKeyAction::NoAction)
                            .on_update(ForeignKeyAction::NoAction),
                    )
                    .to_owned(),
            )
            .await?;

        // Create unique index on tasks (type, value)
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_tasks_type_value")
                    .table(Tasks::Table)
                    .col(Tasks::Type)
                    .col(Tasks::Value)
                    .unique()
                    .to_owned(),
            )
            .await?;

        // Create index on tasks.next_poll_at for scheduler queries
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_tasks_next_poll_at")
                    .table(Tasks::Table)
                    .col(Tasks::NextPollAt)
                    .to_owned(),
            )
            .await?;

        // Create subscriptions table
        manager
            .create_table(
                Table::create()
                    .table(Subscriptions::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Subscriptions::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Subscriptions::ChatId).big_integer().not_null())
                    .col(ColumnDef::new(Subscriptions::TaskId).integer().not_null())
                    .col(ColumnDef::new(Subscriptions::FilterTags).json())
                    .col(
                        ColumnDef::new(Subscriptions::CreatedAt)
                            .timestamp()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_subscriptions_chat")
                            .from(Subscriptions::Table, Subscriptions::ChatId)
                            .to(Chats::Table, Chats::Id)
                            .on_delete(ForeignKeyAction::Cascade)
                            .on_update(ForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_subscriptions_task")
                            .from(Subscriptions::Table, Subscriptions::TaskId)
                            .to(Tasks::Table, Tasks::Id)
                            .on_delete(ForeignKeyAction::Cascade)
                            .on_update(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // Create unique index on subscriptions (chat_id, task_id)
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_subscriptions_chat_task")
                    .table(Subscriptions::Table)
                    .col(Subscriptions::ChatId)
                    .col(Subscriptions::TaskId)
                    .unique()
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Subscriptions::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Tasks::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Chats::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Users::Table).to_owned())
            .await?;
        Ok(())
    }
}

#[derive(DeriveIden)]
enum Users {
    Table,
    Id,
    Username,
    Role,
    CreatedAt,
}

#[derive(DeriveIden)]
enum Chats {
    Table,
    Id,
    Type,
    Title,
    Enabled,
    BlurSensitiveTags,
    ExcludedTags,
    CreatedAt,
}

#[derive(DeriveIden)]
enum Tasks {
    Table,
    Id,
    Type,
    Value,
    AuthorName,
    NextPollAt,
    LastPolledAt,
    LatestData,
    CreatedBy,
    UpdatedBy,
}

#[derive(DeriveIden)]
enum Subscriptions {
    Table,
    Id,
    ChatId,
    TaskId,
    FilterTags,
    CreatedAt,
}
