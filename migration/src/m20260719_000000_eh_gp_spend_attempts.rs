use sea_orm::{ConnectionTrait, DbBackend, TransactionTrait};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_connection().get_database_backend() == DbBackend::Sqlite {
            let transaction = manager.get_connection().begin().await?;
            {
                let transaction_manager = SchemaManager::new(&transaction);
                create_ledger_table_index_and_backfill(&transaction_manager).await?;
            }
            transaction.commit().await?;
            Ok(())
        } else {
            create_ledger_table_index_and_backfill(manager).await
        }
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(EhGpSpendAttempts::Table).to_owned())
            .await
    }
}

async fn create_ledger_table_index_and_backfill(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    manager
        .create_table(
            Table::create()
                .table(EhGpSpendAttempts::Table)
                .col(
                    ColumnDef::new(EhGpSpendAttempts::Id)
                        .integer()
                        .not_null()
                        .auto_increment()
                        .primary_key(),
                )
                .col(ColumnDef::new(EhGpSpendAttempts::QueueId).integer().null())
                .col(
                    ColumnDef::new(EhGpSpendAttempts::Gid)
                        .big_integer()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(EhGpSpendAttempts::GpCost)
                        .big_integer()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(EhGpSpendAttempts::CreatedAt)
                        .timestamp()
                        .not_null()
                        .default(Expr::current_timestamp()),
                )
                .check(Expr::col(EhGpSpendAttempts::GpCost).gt(0))
                .foreign_key(
                    ForeignKey::create()
                        .name("fk_eh_gp_spend_attempts_queue")
                        .from(EhGpSpendAttempts::Table, EhGpSpendAttempts::QueueId)
                        .to(EhDownloadQueue::Table, EhDownloadQueue::Id)
                        .on_delete(ForeignKeyAction::SetNull),
                )
                .to_owned(),
        )
        .await?;

    manager
        .create_index(
            Index::create()
                .name("idx_eh_gp_spend_attempts_created_at")
                .table(EhGpSpendAttempts::Table)
                .col(EhGpSpendAttempts::CreatedAt)
                .to_owned(),
        )
        .await?;

    manager
        .get_connection()
        .execute_unprepared(
            "INSERT INTO eh_gp_spend_attempts (queue_id, gid, gp_cost, created_at) \
             SELECT id, gid, gp_cost, completed_at \
             FROM eh_download_queue \
             WHERE gp_cost > 0 AND completed_at IS NOT NULL",
        )
        .await?;

    Ok(())
}

#[derive(DeriveIden)]
enum EhDownloadQueue {
    Table,
    Id,
}

#[derive(DeriveIden)]
enum EhGpSpendAttempts {
    Table,
    Id,
    QueueId,
    Gid,
    GpCost,
    CreatedAt,
}
