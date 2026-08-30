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
                create_result_reuse_and_push_ledger_schema(&transaction_manager).await?;
            }
            transaction.commit().await?;
            Ok(())
        } else {
            create_result_reuse_and_push_ledger_schema(manager).await
        }
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(EhGalleryPushLedger::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(EhGalleryResults::Table).to_owned())
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(EhGalleryJobs::Table)
                    .drop_column(EhGalleryJobs::SourceFingerprint)
                    .to_owned(),
            )
            .await
    }
}

async fn create_result_reuse_and_push_ledger_schema(
    manager: &SchemaManager<'_>,
) -> Result<(), DbErr> {
    manager
        .get_connection()
        .execute_unprepared("ALTER TABLE eh_gallery_jobs ADD COLUMN source_fingerprint TEXT NULL")
        .await?;

    manager
        .create_table(
            Table::create()
                .table(EhGalleryResults::Table)
                .col(
                    ColumnDef::new(EhGalleryResults::Id)
                        .integer()
                        .not_null()
                        .auto_increment()
                        .primary_key(),
                )
                .col(
                    ColumnDef::new(EhGalleryResults::Gid)
                        .big_integer()
                        .not_null(),
                )
                .col(ColumnDef::new(EhGalleryResults::Token).string().not_null())
                .col(
                    ColumnDef::new(EhGalleryResults::DownloadMode)
                        .string()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(EhGalleryResults::Resolution)
                        .string()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(EhGalleryResults::SourceFingerprint)
                        .string()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(EhGalleryResults::TelegraphUrl)
                        .text()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(EhGalleryResults::TelegraphRewriteData)
                        .text()
                        .null(),
                )
                .col(ColumnDef::new(EhGalleryResults::MediaCids).text().null())
                .col(
                    ColumnDef::new(EhGalleryResults::CreatedAt)
                        .timestamp()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(EhGalleryResults::UpdatedAt)
                        .timestamp()
                        .not_null(),
                )
                .index(
                    Index::create()
                        .name("uq_eh_gallery_results_variant")
                        .col(EhGalleryResults::Gid)
                        .col(EhGalleryResults::Token)
                        .col(EhGalleryResults::DownloadMode)
                        .col(EhGalleryResults::Resolution)
                        .unique(),
                )
                .to_owned(),
        )
        .await?;

    manager
        .create_table(
            Table::create()
                .table(EhGalleryPushLedger::Table)
                .col(
                    ColumnDef::new(EhGalleryPushLedger::Id)
                        .integer()
                        .not_null()
                        .auto_increment()
                        .primary_key(),
                )
                .col(
                    ColumnDef::new(EhGalleryPushLedger::ChatId)
                        .big_integer()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(EhGalleryPushLedger::Gid)
                        .big_integer()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(EhGalleryPushLedger::ArchiveSentAt)
                        .timestamp()
                        .null(),
                )
                .col(
                    ColumnDef::new(EhGalleryPushLedger::TelegraphSentAt)
                        .timestamp()
                        .null(),
                )
                .col(
                    ColumnDef::new(EhGalleryPushLedger::UpdatedAt)
                        .timestamp()
                        .not_null(),
                )
                .index(
                    Index::create()
                        .name("uq_eh_gallery_push_ledger_chat_gid")
                        .col(EhGalleryPushLedger::ChatId)
                        .col(EhGalleryPushLedger::Gid)
                        .unique(),
                )
                .to_owned(),
        )
        .await
}

#[derive(DeriveIden)]
enum EhGalleryJobs {
    Table,
    SourceFingerprint,
}

#[derive(DeriveIden)]
enum EhGalleryResults {
    Table,
    Id,
    Gid,
    Token,
    DownloadMode,
    Resolution,
    SourceFingerprint,
    TelegraphUrl,
    TelegraphRewriteData,
    MediaCids,
    CreatedAt,
    UpdatedAt,
}

#[derive(DeriveIden)]
enum EhGalleryPushLedger {
    Table,
    Id,
    ChatId,
    Gid,
    ArchiveSentAt,
    TelegraphSentAt,
    UpdatedAt,
}
