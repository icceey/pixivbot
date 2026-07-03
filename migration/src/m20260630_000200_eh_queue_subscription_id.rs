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
                        ColumnDef::new(EhDownloadQueue::SubscriptionIds)
                            .string()
                            .null(),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .alter_table(
                Table::alter()
                    .table(EhDownloadQueue::Table)
                    .add_column(
                        ColumnDef::new(EhDownloadQueue::TelegraphSubscriptionIds)
                            .string()
                            .null(),
                    )
                    .to_owned(),
            )
            .await?;

        // Existing active subscription queue rows predate owner tracking, so
        // they cannot be safely associated with a specific subscription after
        // `/eunsub`. Cancel them deterministically rather than publishing work
        // the user may have just unsubscribed from.
        manager
            .get_connection()
            .execute_unprepared(
                "UPDATE eh_download_queue \
                     SET status = 'canceled', \
                         telegraph = FALSE, \
                         telegraph_subscription_ids = NULL, \
                         telegraph_url = NULL, \
                     archive_sent_at = NULL, \
                     telegraph_sent_at = NULL \
                 WHERE source = 'subscription' \
                   AND subscription_ids IS NULL \
                   AND status IN ('pending','downloading','downloaded','uploading','uploaded','publishing','done','failed','canceled')",
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(EhDownloadQueue::Table)
                    .drop_column(EhDownloadQueue::TelegraphSubscriptionIds)
                    .to_owned(),
            )
            .await?;

        manager
            .alter_table(
                Table::alter()
                    .table(EhDownloadQueue::Table)
                    .drop_column(EhDownloadQueue::SubscriptionIds)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum EhDownloadQueue {
    Table,
    SubscriptionIds,
    TelegraphSubscriptionIds,
}
