use sea_orm::{ConnectionTrait, DbBackend, TransactionTrait};
use sea_orm_migration::prelude::*;

const JOB_GENERATION_INDEX: &str = "uq_eh_gallery_jobs_variant_source_generation";

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        ensure_sqlite(manager)?;
        let transaction = manager.get_connection().begin().await?;
        transaction
            .execute_unprepared(
                "ALTER TABLE eh_gallery_jobs \
                 ADD COLUMN source_generation INTEGER NOT NULL DEFAULT 0",
            )
            .await?;
        transaction
            .execute_unprepared(
                "ALTER TABLE eh_gallery_results \
                 ADD COLUMN source_generation INTEGER NOT NULL DEFAULT 0",
            )
            .await?;
        transaction
            .execute_unprepared(
                "WITH ranked_jobs AS ( \
                     SELECT id, ROW_NUMBER() OVER ( \
                         PARTITION BY gid, token, download_mode, resolution \
                         ORDER BY created_at, id \
                     ) AS generation \
                     FROM eh_gallery_jobs \
                 ) \
                 UPDATE eh_gallery_jobs \
                 SET source_generation = ( \
                     SELECT generation FROM ranked_jobs \
                     WHERE ranked_jobs.id = eh_gallery_jobs.id \
                 )",
            )
            .await?;
        transaction
            .execute_unprepared(
                "UPDATE eh_gallery_results \
                 SET source_generation = COALESCE(( \
                     SELECT MAX(jobs.source_generation) \
                     FROM eh_gallery_jobs AS jobs \
                     WHERE jobs.gid = eh_gallery_results.gid \
                       AND jobs.token = eh_gallery_results.token \
                       AND jobs.download_mode = eh_gallery_results.download_mode \
                       AND jobs.resolution = eh_gallery_results.resolution \
                       AND jobs.source_fingerprint = eh_gallery_results.source_fingerprint \
                 ), 0)",
            )
            .await?;
        transaction
            .execute_unprepared(
                "CREATE UNIQUE INDEX uq_eh_gallery_jobs_variant_source_generation \
                 ON eh_gallery_jobs(gid, token, download_mode, resolution, source_generation)",
            )
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        ensure_sqlite(manager)?;
        let transaction = manager.get_connection().begin().await?;
        transaction
            .execute_unprepared(&format!("DROP INDEX {JOB_GENERATION_INDEX}"))
            .await?;
        transaction
            .execute_unprepared("ALTER TABLE eh_gallery_results DROP COLUMN source_generation")
            .await?;
        transaction
            .execute_unprepared("ALTER TABLE eh_gallery_jobs DROP COLUMN source_generation")
            .await?;
        transaction.commit().await?;
        Ok(())
    }
}

fn ensure_sqlite(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    if manager.get_connection().get_database_backend() == DbBackend::Sqlite {
        Ok(())
    } else {
        Err(DbErr::Migration(
            "EH result generation ordering requires SQLite".to_owned(),
        ))
    }
}
