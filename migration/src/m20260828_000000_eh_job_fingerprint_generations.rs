use sea_orm::{ConnectionTrait, DbBackend, Statement, TransactionTrait};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(Clone, Copy)]
enum JobUniqueness {
    FingerprintGenerations,
    LegacyVariant,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        rebuild_in_transaction(manager, JobUniqueness::FingerprintGenerations).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        ensure_legacy_variant_uniqueness(manager).await?;
        rebuild_in_transaction(manager, JobUniqueness::LegacyVariant).await
    }
}

async fn rebuild_in_transaction(
    manager: &SchemaManager<'_>,
    uniqueness: JobUniqueness,
) -> Result<(), DbErr> {
    if manager.get_connection().get_database_backend() != DbBackend::Sqlite {
        return Err(DbErr::Migration(
            "EH job fingerprint generations require SQLite table-rebuild semantics".to_owned(),
        ));
    }

    let transaction = manager.get_connection().begin().await?;
    {
        let transaction_manager = SchemaManager::new(&transaction);
        rebuild_eh_gallery_jobs(&transaction_manager, uniqueness).await?;
    }
    transaction.commit().await?;
    Ok(())
}

async fn ensure_legacy_variant_uniqueness(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    if manager.get_connection().get_database_backend() != DbBackend::Sqlite {
        return Err(DbErr::Migration(
            "EH job fingerprint generations require SQLite table-rebuild semantics".to_owned(),
        ));
    }

    let duplicate = manager
        .get_connection()
        .query_one(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT EXISTS(\
                 SELECT 1 FROM eh_gallery_jobs \
                 GROUP BY gid, token, download_mode, resolution \
                 HAVING COUNT(*) > 1\
             ) AS present"
                .to_owned(),
        ))
        .await?
        .expect("SELECT EXISTS returns one row")
        .try_get::<bool>("", "present")?;
    if duplicate {
        return Err(DbErr::Migration(
            "cannot downgrade EH job fingerprint generations: multiple source fingerprints exist for one gallery variant"
                .to_owned(),
        ));
    }
    Ok(())
}

async fn rebuild_eh_gallery_jobs(
    manager: &SchemaManager<'_>,
    uniqueness: JobUniqueness,
) -> Result<(), DbErr> {
    let connection = manager.get_connection();
    connection
        .execute_unprepared(
            "CREATE TEMP TABLE eh_gallery_jobs_fingerprint_backup AS \
             SELECT * FROM eh_gallery_jobs",
        )
        .await?;
    connection
        .execute_unprepared(
            "CREATE TEMP TABLE eh_gallery_jobs_sequence_backup AS \
             SELECT seq FROM sqlite_sequence WHERE name = 'eh_gallery_jobs'",
        )
        .await?;
    connection
        .execute_unprepared(
            "CREATE TEMP TABLE eh_download_queue_job_id_backup AS \
             SELECT id, job_id FROM eh_download_queue WHERE job_id IS NOT NULL",
        )
        .await?;
    connection
        .execute_unprepared(
            "CREATE TEMP TABLE eh_gp_spend_attempts_job_id_backup AS \
             SELECT id, job_id FROM eh_gp_spend_attempts WHERE job_id IS NOT NULL",
        )
        .await?;
    connection
        .execute_unprepared(
            "CREATE TEMP TABLE eh_download_completions_job_id_backup AS \
             SELECT id, job_id FROM eh_download_completions WHERE job_id IS NOT NULL",
        )
        .await?;

    manager
        .drop_table(Table::drop().table(EhGalleryJobs::Table).to_owned())
        .await?;
    create_eh_gallery_jobs_table(manager).await?;

    connection
        .execute_unprepared(
            "INSERT INTO eh_gallery_jobs (\
                 id, gid, token, download_mode, resolution, title, status, telegraph_status, \
                 telegraph_required, file_size, gp_cost, zip_path, telegraph_url, error, \
                 retry_count, next_retry_at, cleanup_status, cleanup_started_at, cleanup_error, \
                 cleanup_next_retry_at, created_at, started_at, completed_at, \
                 background_download_status, background_download_started_at, \
                 background_download_next_retry_at, background_download_attempt_count, \
                 background_download_error, telegraph_rewrite_data, telegraph_rewrite_status, \
                 telegraph_rewrite_after, telegraph_rewrite_started_at, \
                 telegraph_rewrite_next_retry_at, telegraph_rewrite_retry_count, \
                 telegraph_rewrite_error, telegraph_rewritten_at, source_fingerprint\
             ) SELECT \
                 id, gid, token, download_mode, resolution, title, status, telegraph_status, \
                 telegraph_required, file_size, gp_cost, zip_path, telegraph_url, error, \
                 retry_count, next_retry_at, cleanup_status, cleanup_started_at, cleanup_error, \
                 cleanup_next_retry_at, created_at, started_at, completed_at, \
                 background_download_status, background_download_started_at, \
                 background_download_next_retry_at, background_download_attempt_count, \
                 background_download_error, telegraph_rewrite_data, telegraph_rewrite_status, \
                 telegraph_rewrite_after, telegraph_rewrite_started_at, \
                 telegraph_rewrite_next_retry_at, telegraph_rewrite_retry_count, \
                 telegraph_rewrite_error, telegraph_rewritten_at, source_fingerprint \
             FROM eh_gallery_jobs_fingerprint_backup",
        )
        .await?;
    restore_autoincrement_sequence(connection).await?;

    for statement in [
        "UPDATE eh_download_queue \
         SET job_id = (SELECT backup.job_id FROM eh_download_queue_job_id_backup AS backup \
                       WHERE backup.id = eh_download_queue.id) \
         WHERE id IN (SELECT id FROM eh_download_queue_job_id_backup)",
        "UPDATE eh_gp_spend_attempts \
         SET job_id = (SELECT backup.job_id FROM eh_gp_spend_attempts_job_id_backup AS backup \
                       WHERE backup.id = eh_gp_spend_attempts.id) \
         WHERE id IN (SELECT id FROM eh_gp_spend_attempts_job_id_backup)",
        "UPDATE eh_download_completions \
         SET job_id = (SELECT backup.job_id FROM eh_download_completions_job_id_backup AS backup \
                       WHERE backup.id = eh_download_completions.id) \
         WHERE id IN (SELECT id FROM eh_download_completions_job_id_backup)",
    ] {
        connection.execute_unprepared(statement).await?;
    }

    for table in [
        "eh_gallery_jobs_fingerprint_backup",
        "eh_gallery_jobs_sequence_backup",
        "eh_download_queue_job_id_backup",
        "eh_gp_spend_attempts_job_id_backup",
        "eh_download_completions_job_id_backup",
    ] {
        connection
            .execute_unprepared(&format!("DROP TABLE {table}"))
            .await?;
    }

    create_job_indexes(manager, uniqueness).await?;
    let foreign_key_violations = connection
        .query_all(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA foreign_key_check".to_owned(),
        ))
        .await?;
    if !foreign_key_violations.is_empty() {
        return Err(DbErr::Migration(
            "EH job fingerprint-generation rebuild failed foreign-key validation".to_owned(),
        ));
    }
    Ok(())
}

async fn restore_autoincrement_sequence<C>(connection: &C) -> Result<(), DbErr>
where
    C: ConnectionTrait,
{
    connection
        .execute_unprepared(
            "UPDATE sqlite_sequence \
             SET seq = MAX(seq, (SELECT seq FROM eh_gallery_jobs_sequence_backup)) \
             WHERE name = 'eh_gallery_jobs' \
               AND EXISTS (SELECT 1 FROM eh_gallery_jobs_sequence_backup)",
        )
        .await?;
    connection
        .execute_unprepared(
            "INSERT INTO sqlite_sequence (name, seq) \
             SELECT 'eh_gallery_jobs', seq FROM eh_gallery_jobs_sequence_backup \
             WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'eh_gallery_jobs')",
        )
        .await?;
    Ok(())
}

async fn create_eh_gallery_jobs_table(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    manager
        .create_table(
            Table::create()
                .table(EhGalleryJobs::Table)
                .col(
                    ColumnDef::new(EhGalleryJobs::Id)
                        .integer()
                        .not_null()
                        .auto_increment()
                        .primary_key(),
                )
                .col(ColumnDef::new(EhGalleryJobs::Gid).big_integer().not_null())
                .col(ColumnDef::new(EhGalleryJobs::Token).string().not_null())
                .col(
                    ColumnDef::new(EhGalleryJobs::DownloadMode)
                        .string()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::Resolution)
                        .string()
                        .not_null()
                        .default(""),
                )
                .col(ColumnDef::new(EhGalleryJobs::Title).string().not_null())
                .col(
                    ColumnDef::new(EhGalleryJobs::Status)
                        .string()
                        .not_null()
                        .default("pending"),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::TelegraphStatus)
                        .string()
                        .not_null()
                        .default("not_required"),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::TelegraphRequired)
                        .boolean()
                        .not_null()
                        .default(false),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::FileSize)
                        .big_integer()
                        .not_null()
                        .default(0),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::GpCost)
                        .big_integer()
                        .not_null()
                        .default(0),
                )
                .col(ColumnDef::new(EhGalleryJobs::ZipPath).text().null())
                .col(ColumnDef::new(EhGalleryJobs::TelegraphUrl).text().null())
                .col(ColumnDef::new(EhGalleryJobs::Error).text().null())
                .col(
                    ColumnDef::new(EhGalleryJobs::RetryCount)
                        .integer()
                        .not_null()
                        .default(0),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::NextRetryAt)
                        .timestamp()
                        .null(),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::CleanupStatus)
                        .string()
                        .not_null()
                        .default("none"),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::CleanupStartedAt)
                        .timestamp()
                        .null(),
                )
                .col(ColumnDef::new(EhGalleryJobs::CleanupError).text().null())
                .col(
                    ColumnDef::new(EhGalleryJobs::CleanupNextRetryAt)
                        .timestamp()
                        .null(),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::CreatedAt)
                        .timestamp()
                        .not_null()
                        .default(Expr::current_timestamp()),
                )
                .col(ColumnDef::new(EhGalleryJobs::StartedAt).timestamp().null())
                .col(
                    ColumnDef::new(EhGalleryJobs::CompletedAt)
                        .timestamp()
                        .null(),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::BackgroundDownloadStatus)
                        .string()
                        .null(),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::BackgroundDownloadStartedAt)
                        .timestamp()
                        .null(),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::BackgroundDownloadNextRetryAt)
                        .timestamp()
                        .null(),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::BackgroundDownloadAttemptCount)
                        .integer()
                        .not_null()
                        .default(0),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::BackgroundDownloadError)
                        .text()
                        .null(),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::TelegraphRewriteData)
                        .text()
                        .null(),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::TelegraphRewriteStatus)
                        .string()
                        .null(),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::TelegraphRewriteAfter)
                        .timestamp()
                        .null(),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::TelegraphRewriteStartedAt)
                        .timestamp()
                        .null(),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::TelegraphRewriteNextRetryAt)
                        .timestamp()
                        .null(),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::TelegraphRewriteRetryCount)
                        .integer()
                        .not_null()
                        .default(0),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::TelegraphRewriteError)
                        .text()
                        .null(),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::TelegraphRewrittenAt)
                        .timestamp()
                        .null(),
                )
                .col(
                    ColumnDef::new(EhGalleryJobs::SourceFingerprint)
                        .text()
                        .null(),
                )
                .to_owned(),
        )
        .await
}

async fn create_job_indexes(
    manager: &SchemaManager<'_>,
    uniqueness: JobUniqueness,
) -> Result<(), DbErr> {
    for index in [
        Index::create()
            .name("idx_eh_gallery_jobs_status_retry")
            .table(EhGalleryJobs::Table)
            .col(EhGalleryJobs::Status)
            .col(EhGalleryJobs::NextRetryAt)
            .to_owned(),
        Index::create()
            .name("idx_eh_gallery_jobs_telegraph_retry")
            .table(EhGalleryJobs::Table)
            .col(EhGalleryJobs::TelegraphStatus)
            .col(EhGalleryJobs::NextRetryAt)
            .to_owned(),
        Index::create()
            .name("idx_eh_gallery_jobs_cleanup_retry")
            .table(EhGalleryJobs::Table)
            .col(EhGalleryJobs::CleanupStatus)
            .col(EhGalleryJobs::CleanupNextRetryAt)
            .to_owned(),
        Index::create()
            .name("idx_eh_gallery_jobs_background_status")
            .table(EhGalleryJobs::Table)
            .col(EhGalleryJobs::BackgroundDownloadStatus)
            .col(EhGalleryJobs::BackgroundDownloadNextRetryAt)
            .to_owned(),
        Index::create()
            .name("idx_eh_gallery_jobs_rewrite_status")
            .table(EhGalleryJobs::Table)
            .col(EhGalleryJobs::TelegraphRewriteStatus)
            .col(EhGalleryJobs::TelegraphRewriteNextRetryAt)
            .to_owned(),
        Index::create()
            .name("idx_eh_gallery_jobs_completed_at")
            .table(EhGalleryJobs::Table)
            .col(EhGalleryJobs::CompletedAt)
            .to_owned(),
    ] {
        manager.create_index(index).await?;
    }

    let uniqueness_sql = match uniqueness {
        JobUniqueness::FingerprintGenerations => [
            "CREATE UNIQUE INDEX uq_eh_gallery_jobs_known_fingerprint \
             ON eh_gallery_jobs(gid, token, download_mode, resolution, source_fingerprint) \
             WHERE source_fingerprint IS NOT NULL",
            "CREATE UNIQUE INDEX uq_eh_gallery_jobs_unknown_fingerprint \
             ON eh_gallery_jobs(gid, token, download_mode, resolution) \
             WHERE source_fingerprint IS NULL",
        ],
        JobUniqueness::LegacyVariant => [
            "CREATE UNIQUE INDEX uq_eh_gallery_jobs_variant \
             ON eh_gallery_jobs(gid, token, download_mode, resolution)",
            "",
        ],
    };
    for statement in uniqueness_sql
        .into_iter()
        .filter(|statement| !statement.is_empty())
    {
        manager
            .get_connection()
            .execute_unprepared(statement)
            .await?;
    }
    Ok(())
}

#[derive(DeriveIden)]
enum EhGalleryJobs {
    Table,
    Id,
    Gid,
    Token,
    DownloadMode,
    Resolution,
    Title,
    Status,
    TelegraphStatus,
    TelegraphRequired,
    FileSize,
    GpCost,
    ZipPath,
    TelegraphUrl,
    Error,
    RetryCount,
    NextRetryAt,
    CleanupStatus,
    CleanupStartedAt,
    CleanupError,
    CleanupNextRetryAt,
    CreatedAt,
    StartedAt,
    CompletedAt,
    BackgroundDownloadStatus,
    BackgroundDownloadStartedAt,
    BackgroundDownloadNextRetryAt,
    BackgroundDownloadAttemptCount,
    BackgroundDownloadError,
    TelegraphRewriteData,
    TelegraphRewriteStatus,
    TelegraphRewriteAfter,
    TelegraphRewriteStartedAt,
    TelegraphRewriteNextRetryAt,
    TelegraphRewriteRetryCount,
    TelegraphRewriteError,
    TelegraphRewrittenAt,
    SourceFingerprint,
}
