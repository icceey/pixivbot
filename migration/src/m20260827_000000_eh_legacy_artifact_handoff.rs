use sea_orm::{ConnectionTrait, DbBackend, TransactionTrait};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_connection().get_database_backend() == DbBackend::Sqlite {
            let transaction = manager.get_connection().begin().await?;
            let transaction_manager = SchemaManager::new(&transaction);
            add_legacy_handoff_column_and_backfill(&transaction_manager).await?;
            transaction.commit().await?;
            Ok(())
        } else {
            add_legacy_handoff_column_and_backfill(manager).await
        }
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager
            .has_column("eh_gallery_jobs", "legacy_artifact_handoff")
            .await?
        {
            manager
                .alter_table(
                    Table::alter()
                        .table(EhGalleryJobs::Table)
                        .drop_column(EhGalleryJobs::LegacyArtifactHandoff)
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }
}

async fn add_legacy_handoff_column_and_backfill(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let column_was_missing = !manager
        .has_column("eh_gallery_jobs", "legacy_artifact_handoff")
        .await?;
    if column_was_missing {
        manager
            .alter_table(
                Table::alter()
                    .table(EhGalleryJobs::Table)
                    .add_column(
                        ColumnDef::new(EhGalleryJobs::LegacyArtifactHandoff)
                            .string()
                            .null(),
                    )
                    .to_owned(),
            )
            .await?;

        // A database which already applied the old shared/fingerprint
        // migrations never ran the marker backfill in m20260824. Its old
        // queue claim evidence has already been reset, so a single clean
        // legacy variant is the only safe owner; multiple variants must
        // preserve the old family and block workers until it disappears.
        manager
            .get_connection()
            .execute_unprepared(
                "UPDATE eh_gallery_jobs AS job \
                        SET legacy_artifact_handoff = CASE \
                            WHEN (\
                                SELECT COUNT(*) \
                                  FROM eh_gallery_jobs AS candidate \
                                 WHERE candidate.gid = job.gid \
                                   AND candidate.token = job.token \
                                   AND candidate.download_mode = 'legacy' \
                                   AND candidate.status = 'pending' \
                                   AND candidate.cleanup_status = 'none' \
                                   AND candidate.zip_path IS NULL \
                                   AND candidate.file_size = 0 \
                                   AND candidate.completed_at IS NULL\
                            ) = 1 THEN 'pending' \
                            ELSE 'conflict' \
                        END \
                      WHERE job.download_mode = 'legacy' \
                        AND job.status = 'pending' \
                        AND job.cleanup_status = 'none' \
                        AND job.zip_path IS NULL \
                        AND job.file_size = 0 \
                        AND job.completed_at IS NULL \
                        AND job.legacy_artifact_handoff IS NULL",
            )
            .await?;
    }
    Ok(())
}

#[derive(DeriveIden)]
enum EhGalleryJobs {
    Table,
    LegacyArtifactHandoff,
}
