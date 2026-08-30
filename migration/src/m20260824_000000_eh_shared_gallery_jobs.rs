use sea_orm::{ConnectionTrait, DbBackend, FromQueryResult, Statement, TransactionTrait};
use sea_orm_migration::prelude::*;
use std::collections::BTreeMap;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(FromQueryResult)]
struct LegacyTelegraphRewrite {
    job_id: i32,
    rewrite_data: String,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_connection().get_database_backend() == DbBackend::Sqlite {
            let transaction = manager.get_connection().begin().await?;
            {
                let transaction_manager = SchemaManager::new(&transaction);
                create_shared_gallery_job_schema_and_backfill(&transaction_manager).await?;
            }
            transaction.commit().await?;
            Ok(())
        } else {
            create_shared_gallery_job_schema_and_backfill(manager).await
        }
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for index in [
            "idx_eh_download_queue_job_id",
            "idx_eh_gp_spend_attempts_job_id",
            "idx_eh_download_completions_created_at",
            "idx_eh_download_completions_job_id",
            "idx_eh_gallery_jobs_status_retry",
            "idx_eh_gallery_jobs_telegraph_retry",
            "idx_eh_gallery_jobs_cleanup_retry",
            "idx_eh_gallery_jobs_background_status",
            "idx_eh_gallery_jobs_rewrite_status",
            "idx_eh_gallery_jobs_completed_at",
        ] {
            manager
                .drop_index(Index::drop().name(index).to_owned())
                .await?;
        }

        manager
            .drop_table(Table::drop().table(EhDownloadCompletions::Table).to_owned())
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(EhGpSpendAttempts::Table)
                    .drop_column(EhGpSpendAttempts::JobId)
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(EhDownloadQueue::Table)
                    .drop_column(EhDownloadQueue::JobId)
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(Table::drop().table(EhGalleryJobs::Table).to_owned())
            .await
    }
}

async fn create_shared_gallery_job_schema_and_backfill(
    manager: &SchemaManager<'_>,
) -> Result<(), DbErr> {
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
                .col(
                    ColumnDef::new(EhGalleryJobs::LegacyArtifactHandoff)
                        .string()
                        .null(),
                )
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
                .index(
                    Index::create()
                        .name("uq_eh_gallery_jobs_variant")
                        .col(EhGalleryJobs::Gid)
                        .col(EhGalleryJobs::Token)
                        .col(EhGalleryJobs::DownloadMode)
                        .col(EhGalleryJobs::Resolution)
                        .unique(),
                )
                .to_owned(),
        )
        .await?;

    manager
        .create_table(
            Table::create()
                .table(EhDownloadCompletions::Table)
                .col(
                    ColumnDef::new(EhDownloadCompletions::Id)
                        .integer()
                        .not_null()
                        .auto_increment()
                        .primary_key(),
                )
                .col(
                    ColumnDef::new(EhDownloadCompletions::JobId)
                        .integer()
                        .null(),
                )
                .col(
                    ColumnDef::new(EhDownloadCompletions::Gid)
                        .big_integer()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(EhDownloadCompletions::FileSize)
                        .big_integer()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(EhDownloadCompletions::CreatedAt)
                        .timestamp()
                        .not_null()
                        .default(Expr::current_timestamp()),
                )
                .foreign_key(
                    ForeignKey::create()
                        .name("fk_eh_download_completions_job")
                        .from(EhDownloadCompletions::Table, EhDownloadCompletions::JobId)
                        .to(EhGalleryJobs::Table, EhGalleryJobs::Id)
                        .on_delete(ForeignKeyAction::SetNull),
                )
                .to_owned(),
        )
        .await?;

    manager
        .get_connection()
        .execute_unprepared(
            "ALTER TABLE eh_download_queue ADD COLUMN job_id INTEGER REFERENCES eh_gallery_jobs(id) ON DELETE SET NULL",
        )
        .await?;
    manager
        .get_connection()
        .execute_unprepared(
            "ALTER TABLE eh_gp_spend_attempts ADD COLUMN job_id INTEGER REFERENCES eh_gallery_jobs(id) ON DELETE SET NULL",
        )
        .await?;

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
        Index::create()
            .name("idx_eh_download_completions_created_at")
            .table(EhDownloadCompletions::Table)
            .col(EhDownloadCompletions::CreatedAt)
            .to_owned(),
        Index::create()
            .name("idx_eh_download_completions_job_id")
            .table(EhDownloadCompletions::Table)
            .col(EhDownloadCompletions::JobId)
            .to_owned(),
        Index::create()
            .name("idx_eh_download_queue_job_id")
            .table(EhDownloadQueue::Table)
            .col(EhDownloadQueue::JobId)
            .to_owned(),
        Index::create()
            .name("idx_eh_gp_spend_attempts_job_id")
            .table(EhGpSpendAttempts::Table)
            .col(EhGpSpendAttempts::JobId)
            .to_owned(),
    ] {
        manager.create_index(index).await?;
    }

    manager
        .get_connection()
        .execute_unprepared(
            "INSERT INTO eh_gallery_jobs (\
                gid, token, download_mode, resolution, title, telegraph_status, \
                telegraph_required, created_at\
             ) \
             SELECT grouped.gid, \
                    grouped.token, \
                    'legacy', \
                    grouped.resolution, \
                    (SELECT queue.title \
                       FROM eh_download_queue AS queue \
                      WHERE queue.gid = grouped.gid \
                        AND queue.token = grouped.token \
                        AND (CASE WHEN queue.source = 'direct' THEN 'direct' ELSE 'subscription' END) = grouped.resolution \
                         AND (queue.status IN ('pending', 'downloading', 'downloaded', 'uploading', 'uploaded', 'publishing') \
                              OR (queue.status IN ('done', 'failed', 'canceled') \
                                  AND queue.telegraph_sent_at IS NOT NULL \
                                  AND queue.telegraph_url IS NOT NULL \
                                  AND queue.telegraph_rewrite_data IS NOT NULL \
                                  AND queue.telegraph_rewrite_status IN ('pending', 'rewriting') \
                                  AND queue.telegraph_rewritten_at IS NULL)) \
                      ORDER BY queue.id \
                      LIMIT 1), \
                    CASE WHEN grouped.telegraph_required = 1 THEN 'pending' ELSE 'not_required' END, \
                    grouped.telegraph_required, \
                    grouped.created_at \
               FROM (\
                    SELECT gid, \
                           token, \
                           CASE WHEN source = 'direct' THEN 'direct' ELSE 'subscription' END AS resolution, \
                           MAX(CASE WHEN telegraph = TRUE AND telegraph_sent_at IS NULL THEN 1 ELSE 0 END) AS telegraph_required, \
                           MIN(created_at) AS created_at \
                      FROM eh_download_queue \
                      WHERE status IN ('pending', 'downloading', 'downloaded', 'uploading', 'uploaded', 'publishing') \
                         OR (status IN ('done', 'failed', 'canceled') \
                             AND telegraph_sent_at IS NOT NULL \
                             AND telegraph_url IS NOT NULL \
                             AND telegraph_rewrite_data IS NOT NULL \
                             AND telegraph_rewrite_status IN ('pending', 'rewriting') \
                             AND telegraph_rewritten_at IS NULL) \
                     GROUP BY gid, token, CASE WHEN source = 'direct' THEN 'direct' ELSE 'subscription' END\
               ) AS grouped",
        )
        .await?;
    manager
        .get_connection()
        .execute_unprepared(
            "UPDATE eh_download_queue AS queue \
                SET job_id = (\
                    SELECT job.id \
                      FROM eh_gallery_jobs AS job \
                     WHERE job.gid = queue.gid \
                       AND job.token = queue.token \
                       AND job.download_mode = 'legacy' \
                       AND job.resolution = CASE WHEN queue.source = 'direct' THEN 'direct' ELSE 'subscription' END\
                ) \
               WHERE queue.status IN ('pending', 'downloading', 'downloaded', 'uploading', 'uploaded', 'publishing') \
                  OR (queue.status IN ('done', 'failed', 'canceled') \
                      AND queue.telegraph_sent_at IS NOT NULL \
                      AND queue.telegraph_url IS NOT NULL \
                      AND queue.telegraph_rewrite_data IS NOT NULL \
                      AND queue.telegraph_rewrite_status IN ('pending', 'rewriting') \
                      AND queue.telegraph_rewritten_at IS NULL)",
        )
        .await?;
    // Preserve already-completed download work: the most advanced active
    // delivery row (download complete with a persisted ZIP, lowest id for
    // stability) hands its archive result to the new shared job so the paid
    // archive POST/GP is never repeated and orphan cleanup keeps seeing the
    // artifact family as owned. Must run before the compatibility clearing
    // UPDATE below strips the legacy result columns.
    manager
        .get_connection()
        .execute_unprepared(
            "UPDATE eh_gallery_jobs AS job \
                 SET status = 'downloaded', \
                     zip_path = (\
                         SELECT winner.zip_path \
                           FROM eh_download_queue AS winner \
                          WHERE winner.job_id = job.id \
                            AND winner.status IN ('downloaded', 'uploading', 'uploaded', 'publishing') \
                            AND winner.zip_path IS NOT NULL \
                          ORDER BY winner.id \
                          LIMIT 1\
                     ), \
                     file_size = (\
                         SELECT winner.file_size \
                           FROM eh_download_queue AS winner \
                          WHERE winner.job_id = job.id \
                            AND winner.status IN ('downloaded', 'uploading', 'uploaded', 'publishing') \
                            AND winner.zip_path IS NOT NULL \
                          ORDER BY winner.id \
                          LIMIT 1\
                     ), \
                     gp_cost = (\
                         SELECT winner.gp_cost \
                           FROM eh_download_queue AS winner \
                          WHERE winner.job_id = job.id \
                            AND winner.status IN ('downloaded', 'uploading', 'uploaded', 'publishing') \
                            AND winner.zip_path IS NOT NULL \
                          ORDER BY winner.id \
                          LIMIT 1\
                     ), \
                     completed_at = (\
                         SELECT winner.completed_at \
                           FROM eh_download_queue AS winner \
                          WHERE winner.job_id = job.id \
                            AND winner.status IN ('downloaded', 'uploading', 'uploaded', 'publishing') \
                            AND winner.zip_path IS NOT NULL \
                          ORDER BY winner.id \
                          LIMIT 1\
                     ) \
               WHERE job.status = 'pending' \
                 AND job.download_mode = 'legacy' \
                 AND EXISTS (\
                     SELECT 1 \
                       FROM eh_download_queue AS winner \
                      WHERE winner.job_id = job.id \
                        AND winner.status IN ('downloaded', 'uploading', 'uploaded', 'publishing') \
                        AND winner.zip_path IS NOT NULL\
                 )",
        )
        .await?;
    // Preserve already-completed Telegraph work: the earliest unfinished
    // rewrite-bearing delivery, then the earliest other page URL, hands its
    // URL to the shared job so the upload is never repeated. Every unfinished
    // rewrite payload is migrated below as one batch, so collapsing deliveries
    // onto a shared job cannot strand any previously sent page.
    // Terminal deliveries only upgrade a `not_required` job when they carry an
    // unfinished rewrite; an already-sent terminal page never creates unsent
    // Telegraph demand.
    manager
        .get_connection()
        .execute_unprepared(
            "UPDATE eh_gallery_jobs AS job \
                 SET telegraph_status = 'ready', \
                     telegraph_url = (\
                         SELECT winner.telegraph_url \
                           FROM eh_download_queue AS winner \
                           WHERE winner.job_id = job.id \
                             AND winner.telegraph_url IS NOT NULL \
                           ORDER BY CASE WHEN winner.telegraph_rewrite_data IS NOT NULL \
                                               AND winner.telegraph_rewrite_status IN ('pending', 'rewriting') \
                                               AND winner.telegraph_rewritten_at IS NULL \
                                         THEN 0 ELSE 1 END, winner.id \
                           LIMIT 1\
                     ) \
                WHERE job.download_mode = 'legacy' \
                  AND EXISTS (\
                      SELECT 1 \
                        FROM eh_download_queue AS winner \
                       WHERE winner.job_id = job.id \
                         AND winner.telegraph_url IS NOT NULL\
                  ) \
                  AND (job.telegraph_status = 'pending' \
                       OR EXISTS (\
                           SELECT 1 \
                             FROM eh_download_queue AS winner \
                            WHERE winner.job_id = job.id \
                              AND winner.telegraph_url IS NOT NULL \
                              AND winner.telegraph_rewrite_data IS NOT NULL \
                              AND winner.telegraph_rewrite_status IN ('pending', 'rewriting') \
                              AND winner.telegraph_rewritten_at IS NULL\
                        ))",
        )
        .await?;
    migrate_pending_telegraph_rewrites(manager.get_connection()).await?;
    manager
        .get_connection()
        .execute_unprepared(
            "INSERT INTO eh_download_completions (job_id, gid, file_size, created_at) \
             SELECT CASE \
                        WHEN status IN ('pending', 'downloading', 'downloaded', 'uploading', 'uploaded', 'publishing') THEN job_id \
                        ELSE NULL \
                    END, \
                    gid, \
                    file_size, \
                    completed_at \
             FROM eh_download_queue \
              WHERE file_size > 0 AND completed_at IS NOT NULL",
        )
        .await?;
    // Active legacy deliveries can leave a resumable `gid_token.zip` family,
    // but the shared job's deterministic family is job-specific. Simultaneous
    // active claims, or multiple never-claimed variants, cannot prove which
    // resolution owns that family, so mark every candidate conflict-blocked.
    // Otherwise select the most recent concrete legacy claim before
    // compatibility state is cleared. The runtime startup handoff derives the
    // configured cache path and moves the files.
    manager
        .get_connection()
        .execute_unprepared(
            "UPDATE eh_gallery_jobs AS job \
                SET legacy_artifact_handoff = 'conflict' \
              WHERE job.download_mode = 'legacy' \
                AND job.status = 'pending' \
                AND (\
                    1 < (\
                        SELECT COUNT(DISTINCT active.job_id) \
                          FROM eh_download_queue AS active \
                         WHERE active.gid = job.gid \
                           AND active.token = job.token \
                           AND active.job_id IS NOT NULL \
                           AND (active.status = 'downloading' \
                                OR active.background_download_status = 'running')\
                    ) \
                    OR (\
                        NOT EXISTS (\
                            SELECT 1 \
                              FROM eh_download_queue AS claimed \
                             WHERE claimed.gid = job.gid \
                               AND claimed.token = job.token \
                               AND claimed.job_id IS NOT NULL \
                               AND (claimed.status = 'downloading' \
                                    OR claimed.background_download_status = 'running' \
                                    OR (claimed.status = 'pending' AND claimed.started_at IS NOT NULL) \
                                    OR (claimed.background_download_status = 'pending' \
                                        AND claimed.background_download_started_at IS NOT NULL))\
                        ) \
                        AND 1 < (\
                            SELECT COUNT(DISTINCT candidate.job_id) \
                              FROM eh_download_queue AS candidate \
                             WHERE candidate.gid = job.gid \
                               AND candidate.token = job.token \
                               AND candidate.job_id IS NOT NULL \
                               AND (candidate.status IN ('pending', 'downloading') \
                                    OR candidate.background_download_status IN ('pending', 'running'))\
                        )\
                    )\
                )",
        )
        .await?;
    manager
        .get_connection()
        .execute_unprepared(
            "UPDATE eh_gallery_jobs AS job \
                SET legacy_artifact_handoff = 'pending' \
              WHERE job.download_mode = 'legacy' \
                AND job.status = 'pending' \
                AND job.legacy_artifact_handoff IS NULL \
                AND job.id = (\
                    SELECT candidate.job_id \
                      FROM eh_download_queue AS candidate \
                     WHERE candidate.gid = job.gid \
                       AND candidate.token = job.token \
                       AND candidate.job_id IS NOT NULL \
                       AND (candidate.status IN ('pending', 'downloading') \
                            OR candidate.background_download_status IN ('pending', 'running')) \
                     ORDER BY CASE \
                                  WHEN candidate.status = 'downloading' \
                                       OR candidate.background_download_status = 'running' THEN 0 \
                                  WHEN (candidate.status = 'pending' \
                                        AND candidate.started_at IS NOT NULL) \
                                       OR (candidate.background_download_status = 'pending' \
                                           AND candidate.background_download_started_at IS NOT NULL) THEN 1 \
                                  ELSE 2 \
                              END, \
                              CASE \
                                  WHEN candidate.background_download_status IN ('running', 'pending') \
                                      THEN COALESCE(candidate.background_download_started_at, candidate.started_at) \
                                  ELSE candidate.started_at \
                              END DESC, \
                              candidate.id DESC \
                     LIMIT 1\
                )",
        )
        .await?;
    manager
        .get_connection()
        .execute_unprepared(
            "UPDATE eh_download_queue \
                SET status = 'waiting', \
                    file_size = 0, \
                    gp_cost = 0, \
                    error = NULL, \
                    retry_count = 0, \
                    started_at = NULL, \
                    completed_at = NULL, \
                    zip_path = NULL, \
                    telegraph_url = NULL, \
                    next_retry_at = NULL, \
                    background_download_status = NULL, \
                    background_download_started_at = NULL, \
                    background_download_next_retry_at = NULL, \
                    background_download_attempt_count = 0, \
                    background_download_error = NULL, \
                    telegraph_rewrite_data = NULL, \
                    telegraph_rewrite_status = NULL, \
                    telegraph_rewrite_after = NULL, \
                    telegraph_rewrite_started_at = NULL, \
                    telegraph_rewrite_next_retry_at = NULL, \
                    telegraph_rewrite_retry_count = 0, \
                    telegraph_rewrite_error = NULL, \
                    telegraph_rewritten_at = NULL \
              WHERE status IN ('pending', 'downloading', 'downloaded', 'uploading', 'uploaded', 'publishing')",
        )
        .await?;

    Ok(())
}

async fn migrate_pending_telegraph_rewrites(
    connection: &SchemaManagerConnection<'_>,
) -> Result<(), DbErr> {
    let backend = connection.get_database_backend();
    let rows = LegacyTelegraphRewrite::find_by_statement(Statement::from_string(
        backend,
        "SELECT job_id, telegraph_rewrite_data AS rewrite_data \
           FROM eh_download_queue \
          WHERE job_id IS NOT NULL \
            AND telegraph_url IS NOT NULL \
            AND telegraph_rewrite_data IS NOT NULL \
            AND telegraph_rewrite_status IN ('pending', 'rewriting') \
            AND telegraph_rewritten_at IS NULL \
          ORDER BY job_id, id"
            .to_owned(),
    ))
    .all(connection)
    .await?;

    let mut grouped = BTreeMap::<i32, Vec<serde_json::Value>>::new();
    for row in rows {
        let payload = serde_json::from_str(&row.rewrite_data).map_err(|error| {
            DbErr::Migration(format!(
                "invalid legacy Telegraph rewrite payload for job {}: {error}",
                row.job_id
            ))
        })?;
        grouped.entry(row.job_id).or_default().push(payload);
    }
    let batches = grouped
        .into_iter()
        .map(|(job_id, rewrites)| {
            serde_json::to_string(&rewrites)
                .map(|batch| (job_id, batch))
                .map_err(|error| {
                    DbErr::Migration(format!(
                        "failed to serialize Telegraph rewrite batch for job {job_id}: {error}"
                    ))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;

    for (job_id, batch) in batches {
        connection
            .execute(Statement::from_sql_and_values(
                backend,
                "UPDATE eh_gallery_jobs AS job \
                    SET telegraph_rewrite_data = ?, \
                        telegraph_rewrite_status = 'pending', \
                        telegraph_rewrite_after = (\
                            SELECT MAX(source.telegraph_rewrite_after) \
                              FROM eh_download_queue AS source \
                             WHERE source.job_id = job.id \
                               AND source.telegraph_rewrite_data IS NOT NULL \
                               AND source.telegraph_rewrite_status IN ('pending', 'rewriting') \
                               AND source.telegraph_rewritten_at IS NULL\
                        ), \
                        telegraph_rewrite_started_at = NULL, \
                        telegraph_rewrite_next_retry_at = (\
                            SELECT MAX(source.telegraph_rewrite_next_retry_at) \
                              FROM eh_download_queue AS source \
                             WHERE source.job_id = job.id \
                               AND source.telegraph_rewrite_data IS NOT NULL \
                               AND source.telegraph_rewrite_status IN ('pending', 'rewriting') \
                               AND source.telegraph_rewritten_at IS NULL\
                        ), \
                        telegraph_rewrite_retry_count = (\
                            SELECT MAX(source.telegraph_rewrite_retry_count) \
                              FROM eh_download_queue AS source \
                             WHERE source.job_id = job.id \
                               AND source.telegraph_rewrite_data IS NOT NULL \
                               AND source.telegraph_rewrite_status IN ('pending', 'rewriting') \
                               AND source.telegraph_rewritten_at IS NULL\
                        ), \
                        telegraph_rewrite_error = (\
                            SELECT source.telegraph_rewrite_error \
                              FROM eh_download_queue AS source \
                             WHERE source.job_id = job.id \
                               AND source.telegraph_rewrite_data IS NOT NULL \
                               AND source.telegraph_rewrite_status IN ('pending', 'rewriting') \
                               AND source.telegraph_rewritten_at IS NULL \
                             ORDER BY source.telegraph_rewrite_retry_count DESC, source.id \
                             LIMIT 1\
                        ), \
                        telegraph_rewritten_at = NULL \
                  WHERE job.id = ?",
                [batch.into(), job_id.into()],
            ))
            .await?;
    }

    connection
        .execute_unprepared(
            "UPDATE eh_download_queue \
                SET telegraph_rewrite_data = NULL, \
                    telegraph_rewrite_status = NULL, \
                    telegraph_rewrite_after = NULL, \
                    telegraph_rewrite_started_at = NULL, \
                    telegraph_rewrite_next_retry_at = NULL, \
                    telegraph_rewrite_retry_count = 0, \
                    telegraph_rewrite_error = NULL, \
                    telegraph_rewritten_at = NULL \
              WHERE job_id IS NOT NULL \
                AND telegraph_rewrite_data IS NOT NULL \
                AND telegraph_rewrite_status IN ('pending', 'rewriting') \
                AND telegraph_rewritten_at IS NULL",
        )
        .await?;

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
    LegacyArtifactHandoff,
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
}

#[derive(DeriveIden)]
enum EhDownloadCompletions {
    Table,
    Id,
    JobId,
    Gid,
    FileSize,
    CreatedAt,
}

#[derive(DeriveIden)]
enum EhDownloadQueue {
    Table,
    JobId,
}

#[derive(DeriveIden)]
enum EhGpSpendAttempts {
    Table,
    JobId,
}
