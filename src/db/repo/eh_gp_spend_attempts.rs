use super::Repo;
use crate::db::entities::eh_gp_spend_attempts;
use anyhow::{Context, Result};
use chrono::{Duration, Local};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

impl Repo {
    /// Record one GP charge attempt owned by a shared-gallery download job.
    /// Each reservation represents a source archive POST, never a delivery.
    pub async fn append_eh_job_gp_spend_attempt(
        &self,
        job_id: i32,
        gid: i64,
        gp_cost: i64,
    ) -> Result<eh_gp_spend_attempts::Model> {
        if gp_cost <= 0 {
            anyhow::bail!("EH GP spend attempt cost must be positive, got {gp_cost}");
        }

        eh_gp_spend_attempts::ActiveModel {
            job_id: Set(Some(job_id)),
            queue_id: Set(None),
            gid: Set(gid),
            gp_cost: Set(gp_cost),
            created_at: Set(Local::now().naive_local()),
            ..Default::default()
        }
        .insert(&self.db)
        .await
        .context("Failed to append shared EH job GP spend attempt")
    }

    /// Record a GP charge attempt for an EH archive download.
    ///
    /// Every call inserts a distinct ledger row, even for the same queue entry.
    pub async fn append_eh_gp_spend_attempt(
        &self,
        queue_id: i32,
        gid: i64,
        gp_cost: i64,
    ) -> Result<eh_gp_spend_attempts::Model> {
        if gp_cost <= 0 {
            anyhow::bail!("EH GP spend attempt cost must be positive, got {gp_cost}");
        }

        eh_gp_spend_attempts::ActiveModel {
            queue_id: Set(Some(queue_id)),
            gid: Set(gid),
            gp_cost: Set(gp_cost),
            created_at: Set(Local::now().naive_local()),
            ..Default::default()
        }
        .insert(&self.db)
        .await
        .context("Failed to append EH GP spend attempt")
    }

    /// Get total GP charged in the last `window_hours` from the append-only ledger.
    pub async fn get_eh_gp_cost_in_window(&self, window_hours: u64) -> Result<i64> {
        let window_hours = i64::try_from(window_hours)
            .context("EH GP spend window hours exceed the supported range")?;
        let duration = Duration::try_hours(window_hours)
            .context("EH GP spend window hours exceed Chrono duration range")?;
        let cutoff = Local::now()
            .naive_local()
            .checked_sub_signed(duration)
            .context("EH GP spend window cutoff is outside the supported datetime range")?;

        let attempts = eh_gp_spend_attempts::Entity::find()
            .filter(eh_gp_spend_attempts::Column::CreatedAt.gte(cutoff))
            .all(&self.db)
            .await
            .context("Failed to fetch EH GP spend attempts in window")?;

        attempts.into_iter().try_fold(0_i64, |total, attempt| {
            total.checked_add(attempt.gp_cost).ok_or_else(|| {
                anyhow::anyhow!("EH GP spend total overflow while summing attempts in window")
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests_helpers::setup_test_db;
    use crate::db::entities::{
        eh_download_completions, eh_download_queue, eh_gallery_jobs, eh_gp_spend_attempts,
    };
    use crate::db::repo::eh_gallery_jobs::{
        eh_gallery_job_artifact_path, LEGACY_ARTIFACT_HANDOFF_CONFLICT,
        LEGACY_ARTIFACT_HANDOFF_MOVING, LEGACY_ARTIFACT_HANDOFF_PENDING,
    };
    use crate::db::repo::Repo;
    use anyhow::{bail, Result};
    use chrono::{Duration, Local};
    use eh_client::ArchiveArtifacts;
    use migration::{MigrationTrait, Migrator, MigratorTrait, SchemaManager};
    use sea_orm::{
        ActiveModelTrait, ColumnTrait, ConnectionTrait, Database, DatabaseConnection, DbBackend,
        EntityTrait, QueryFilter, Set, Statement,
    };

    const TABLE: &str = "eh_gp_spend_attempts";
    const CREATED_AT_INDEX: &str = "idx_eh_gp_spend_attempts_created_at";
    const MIGRATION_NAME: &str = "m20260719_000000_eh_gp_spend_attempts";
    const SHARED_JOBS_MIGRATION_NAME: &str = "m20260824_000000_eh_shared_gallery_jobs";
    const REUSE_LEDGER_MIGRATION_NAME: &str = "m20260826_000000_eh_result_reuse_and_push_ledger";
    const LEGACY_ARTIFACT_HANDOFF_COMPAT_MIGRATION_NAME: &str =
        "m20260827_000000_eh_legacy_artifact_handoff";
    const FINGERPRINT_GENERATIONS_MIGRATION_NAME: &str =
        "m20260828_000000_eh_job_fingerprint_generations";
    const JOB_REBUILD_INDEXES: [&str; 6] = [
        "idx_eh_gallery_jobs_status_retry",
        "idx_eh_gallery_jobs_telegraph_retry",
        "idx_eh_gallery_jobs_cleanup_retry",
        "idx_eh_gallery_jobs_background_status",
        "idx_eh_gallery_jobs_rewrite_status",
        "idx_eh_gallery_jobs_completed_at",
    ];
    const KNOWN_FINGERPRINT_INDEX: &str = "uq_eh_gallery_jobs_known_fingerprint";
    const UNKNOWN_FINGERPRINT_INDEX: &str = "uq_eh_gallery_jobs_unknown_fingerprint";
    const LEGACY_VARIANT_INDEX: &str = "uq_eh_gallery_jobs_variant";

    async fn new_db() -> Result<DatabaseConnection> {
        let db = Database::connect("sqlite::memory:").await?;
        db.execute_unprepared("PRAGMA foreign_keys = ON").await?;
        Ok(db)
    }

    async fn create_legacy_queue_table(db: &DatabaseConnection) -> Result<()> {
        db.execute_unprepared(
            "CREATE TABLE eh_download_queue (\
                id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, \
                gid INTEGER NOT NULL, \
                gp_cost INTEGER NOT NULL DEFAULT 0, \
                completed_at TIMESTAMP\
            )",
        )
        .await?;
        Ok(())
    }

    fn target_migration() -> Result<Box<dyn MigrationTrait>> {
        Migrator::migrations()
            .into_iter()
            .find(|migration| migration.name() == MIGRATION_NAME)
            .ok_or_else(|| anyhow::anyhow!("migration {MIGRATION_NAME} is not registered"))
    }

    async fn migrate_up(db: &DatabaseConnection) -> Result<()> {
        target_migration()?.up(&SchemaManager::new(db)).await?;
        Ok(())
    }

    fn shared_jobs_target_migration() -> Result<Box<dyn MigrationTrait>> {
        Migrator::migrations()
            .into_iter()
            .find(|migration| migration.name() == SHARED_JOBS_MIGRATION_NAME)
            .ok_or_else(|| {
                anyhow::anyhow!("migration {SHARED_JOBS_MIGRATION_NAME} is not registered")
            })
    }

    async fn migrate_shared_jobs_up(db: &DatabaseConnection) -> Result<()> {
        shared_jobs_target_migration()?
            .up(&SchemaManager::new(db))
            .await?;
        Ok(())
    }

    fn reuse_ledger_target_migration() -> Result<Box<dyn MigrationTrait>> {
        Migrator::migrations()
            .into_iter()
            .find(|migration| migration.name() == REUSE_LEDGER_MIGRATION_NAME)
            .ok_or_else(|| {
                anyhow::anyhow!("migration {REUSE_LEDGER_MIGRATION_NAME} is not registered")
            })
    }

    async fn migrate_reuse_ledger_up(db: &DatabaseConnection) -> Result<()> {
        migrate_shared_jobs_up(db).await?;
        reuse_ledger_target_migration()?
            .up(&SchemaManager::new(db))
            .await?;
        Ok(())
    }

    fn legacy_artifact_handoff_compat_target_migration() -> Result<Box<dyn MigrationTrait>> {
        Migrator::migrations()
            .into_iter()
            .find(|migration| migration.name() == LEGACY_ARTIFACT_HANDOFF_COMPAT_MIGRATION_NAME)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "migration {LEGACY_ARTIFACT_HANDOFF_COMPAT_MIGRATION_NAME} is not registered"
                )
            })
    }

    async fn migrate_legacy_artifact_handoff_compat_up(db: &DatabaseConnection) -> Result<()> {
        legacy_artifact_handoff_compat_target_migration()?
            .up(&SchemaManager::new(db))
            .await?;
        Ok(())
    }

    async fn migrate_legacy_artifact_handoff_compat_down(db: &DatabaseConnection) -> Result<()> {
        legacy_artifact_handoff_compat_target_migration()?
            .down(&SchemaManager::new(db))
            .await?;
        Ok(())
    }

    fn fingerprint_generations_target_migration() -> Result<Box<dyn MigrationTrait>> {
        Migrator::migrations()
            .into_iter()
            .find(|migration| migration.name() == FINGERPRINT_GENERATIONS_MIGRATION_NAME)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "migration {FINGERPRINT_GENERATIONS_MIGRATION_NAME} is not registered"
                )
            })
    }

    async fn migrate_fingerprint_generations_up(db: &DatabaseConnection) -> Result<()> {
        migrate_reuse_ledger_up(db).await?;
        migrate_legacy_artifact_handoff_compat_up(db).await?;
        migrate_fingerprint_generations_from_reuse_schema_up(db).await
    }

    async fn migrate_fingerprint_generations_from_reuse_schema_up(
        db: &DatabaseConnection,
    ) -> Result<()> {
        fingerprint_generations_target_migration()?
            .up(&SchemaManager::new(db))
            .await?;
        Ok(())
    }

    async fn migrate_fingerprint_generations_down(db: &DatabaseConnection) -> Result<()> {
        fingerprint_generations_target_migration()?
            .down(&SchemaManager::new(db))
            .await?;
        Ok(())
    }

    async fn create_legacy_shared_jobs_tables(db: &DatabaseConnection) -> Result<()> {
        db.execute_unprepared(
            "CREATE TABLE eh_download_queue (\
                id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, \
                chat_id INTEGER NOT NULL, \
                gid INTEGER NOT NULL, \
                token TEXT NOT NULL, \
                title TEXT NOT NULL, \
                telegraph BOOLEAN NOT NULL DEFAULT 0, \
                source TEXT NOT NULL DEFAULT 'subscription', \
                subscription_ids TEXT, \
                telegraph_subscription_ids TEXT, \
                status TEXT NOT NULL DEFAULT 'pending', \
                file_size INTEGER NOT NULL DEFAULT 0, \
                gp_cost INTEGER NOT NULL DEFAULT 0, \
                error TEXT, \
                retry_count INTEGER NOT NULL DEFAULT 0, \
                created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, \
                started_at TIMESTAMP, \
                completed_at TIMESTAMP, \
                zip_path TEXT, \
                telegraph_url TEXT, \
                next_retry_at TIMESTAMP, \
                archive_sent_at TIMESTAMP, \
                telegraph_sent_at TIMESTAMP, \
                background_download_status TEXT, \
                background_download_started_at TIMESTAMP, \
                background_download_next_retry_at TIMESTAMP, \
                background_download_attempt_count INTEGER NOT NULL DEFAULT 0, \
                background_download_error TEXT, \
                telegraph_rewrite_data TEXT, \
                telegraph_rewrite_status TEXT, \
                telegraph_rewrite_after TIMESTAMP, \
                telegraph_rewrite_started_at TIMESTAMP, \
                telegraph_rewrite_next_retry_at TIMESTAMP, \
                telegraph_rewrite_retry_count INTEGER NOT NULL DEFAULT 0, \
                telegraph_rewrite_error TEXT, \
                telegraph_rewritten_at TIMESTAMP, \
                UNIQUE(chat_id, gid)\
            )",
        )
        .await?;
        db.execute_unprepared(
            "CREATE TABLE eh_gp_spend_attempts (\
                id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, \
                queue_id INTEGER, \
                gid INTEGER NOT NULL, \
                gp_cost INTEGER NOT NULL CHECK (gp_cost > 0), \
                created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, \
                FOREIGN KEY (queue_id) REFERENCES eh_download_queue(id) ON DELETE SET NULL\
            )",
        )
        .await?;
        Ok(())
    }

    async fn migration_table_exists(db: &DatabaseConnection) -> Result<bool> {
        let row = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                format!(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '{TABLE}') AS present"
                ),
            ))
            .await?
            .expect("SELECT EXISTS returns one row");
        Ok(row.try_get("", "present")?)
    }

    async fn migration_created_at_index_exists(db: &DatabaseConnection) -> Result<bool> {
        let row = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                format!(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = '{CREATED_AT_INDEX}') AS present"
                ),
            ))
            .await?
            .expect("SELECT EXISTS returns one row");
        Ok(row.try_get("", "present")?)
    }

    async fn sqlite_master_entry_exists(
        db: &DatabaseConnection,
        object_type: &str,
        name: &str,
    ) -> Result<bool> {
        let row = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                format!(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = '{object_type}' AND name = '{name}') AS present"
                ),
            ))
            .await?
            .expect("SELECT EXISTS returns one row");
        Ok(row.try_get("", "present")?)
    }

    async fn table_has_column(
        db: &DatabaseConnection,
        table: &str,
        expected_column: &str,
    ) -> Result<bool> {
        let columns = db
            .query_all(Statement::from_string(
                DbBackend::Sqlite,
                format!("PRAGMA table_info({table})"),
            ))
            .await?;
        Ok(columns.iter().any(|column| {
            column.try_get::<String>("", "name").ok().as_deref() == Some(expected_column)
        }))
    }

    async fn table_difference_count(
        db: &DatabaseConnection,
        left_table: &str,
        right_table: &str,
    ) -> Result<i64> {
        let row = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                format!(
                    "SELECT COUNT(*) AS count FROM (SELECT * FROM {left_table} EXCEPT SELECT * FROM {right_table})"
                ),
            ))
            .await?
            .expect("table difference query returns one row");
        Ok(row.try_get("", "count")?)
    }

    async fn assert_same_table_rows(
        db: &DatabaseConnection,
        expected_table: &str,
        actual_table: &str,
    ) -> Result<()> {
        assert_eq!(
            table_difference_count(db, expected_table, actual_table).await?,
            0,
            "{expected_table} contains rows missing from {actual_table}"
        );
        assert_eq!(
            table_difference_count(db, actual_table, expected_table).await?,
            0,
            "{actual_table} contains rows missing from {expected_table}"
        );
        Ok(())
    }

    async fn snapshot_job_rebuild_data(db: &DatabaseConnection) -> Result<()> {
        for (snapshot_table, source) in [
            ("expected_eh_gallery_jobs", "SELECT * FROM eh_gallery_jobs"),
            (
                "expected_eh_download_queue_job_ids",
                "SELECT id, job_id FROM eh_download_queue WHERE job_id IS NOT NULL",
            ),
            (
                "expected_eh_gp_spend_attempts_job_ids",
                "SELECT id, job_id FROM eh_gp_spend_attempts WHERE job_id IS NOT NULL",
            ),
            (
                "expected_eh_download_completions_job_ids",
                "SELECT id, job_id FROM eh_download_completions WHERE job_id IS NOT NULL",
            ),
        ] {
            db.execute_unprepared(&format!("CREATE TEMP TABLE {snapshot_table} AS {source}"))
                .await?;
        }
        Ok(())
    }

    async fn assert_job_rebuild_data_is_preserved(db: &DatabaseConnection) -> Result<()> {
        for (expected_table, actual_table) in [
            ("expected_eh_gallery_jobs", "eh_gallery_jobs"),
            (
                "expected_eh_download_queue_job_ids",
                "(SELECT id, job_id FROM eh_download_queue WHERE job_id IS NOT NULL)",
            ),
            (
                "expected_eh_gp_spend_attempts_job_ids",
                "(SELECT id, job_id FROM eh_gp_spend_attempts WHERE job_id IS NOT NULL)",
            ),
            (
                "expected_eh_download_completions_job_ids",
                "(SELECT id, job_id FROM eh_download_completions WHERE job_id IS NOT NULL)",
            ),
        ] {
            assert_same_table_rows(db, expected_table, actual_table).await?;
        }
        Ok(())
    }

    async fn assert_foreign_key_check_is_clean(db: &DatabaseConnection) -> Result<()> {
        let violations = db
            .query_all(Statement::from_string(
                DbBackend::Sqlite,
                "PRAGMA foreign_key_check".to_owned(),
            ))
            .await?;
        assert!(
            violations.is_empty(),
            "foreign_key_check reported {} violation(s)",
            violations.len()
        );
        Ok(())
    }

    async fn seed_fingerprint_generation_fixture(db: &DatabaseConnection) -> Result<i64> {
        migrate_reuse_ledger_up(db).await?;
        db.execute_unprepared(
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
             ) VALUES (\
                410, 7001, 'fixture-token', 'archive', '1280x', 'Complete fixture', 'downloaded', 'ready', \
                1, 4096, 23, '/tmp/fixture.zip', 'https://fixture.example/page', 'fixture error', \
                7, '2026-08-28 01:02:03', 'pending', '2026-08-28 02:03:04', 'cleanup error', \
                '2026-08-28 03:04:05', '2026-08-28 04:05:06', '2026-08-28 05:06:07', '2026-08-28 06:07:08', \
                'running', '2026-08-28 07:08:09', '2026-08-28 08:09:10', 11, 'background error', \
                'rewrite data', 'pending', '2026-08-28 09:10:11', '2026-08-28 10:11:12', \
                '2026-08-28 11:12:13', 13, 'rewrite error', '2026-08-28 12:13:14', 'fixture-fingerprint'\
             )",
        )
        .await?;
        db.execute_unprepared(
            "INSERT INTO eh_download_queue (\
                id, job_id, chat_id, gid, token, title, telegraph, source, status\
             ) VALUES (601, 410, 61, 7001, 'fixture-token', 'Fixture delivery', 0, 'direct', 'waiting')",
        )
        .await?;
        db.execute_unprepared(
            "INSERT INTO eh_gp_spend_attempts (id, job_id, queue_id, gid, gp_cost, created_at) \
             VALUES (701, 410, 601, 7001, 23, '2026-08-28 13:14:15')",
        )
        .await?;
        db.execute_unprepared(
            "INSERT INTO eh_download_completions (id, job_id, gid, file_size, created_at) \
             VALUES (801, 410, 7001, 4096, '2026-08-28 14:15:16')",
        )
        .await?;
        snapshot_job_rebuild_data(db).await?;

        let row = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT MAX(id) AS max_id FROM eh_gallery_jobs".to_owned(),
            ))
            .await?
            .expect("maximum job ID query returns one row");
        Ok(row.try_get("", "max_id")?)
    }

    #[tokio::test]
    async fn migration_creates_ledger_table_and_created_at_index() -> Result<()> {
        let db = new_db().await?;
        create_legacy_queue_table(&db).await?;

        migrate_up(&db).await?;

        assert!(migration_table_exists(&db).await?);
        assert!(migration_created_at_index_exists(&db).await?);
        Ok(())
    }

    #[tokio::test]
    async fn migration_reuse_tables_and_fingerprint_column() -> Result<()> {
        let db = new_db().await?;
        create_legacy_shared_jobs_tables(&db).await?;
        db.execute_unprepared(
            "INSERT INTO eh_download_queue (chat_id, gid, token, title, source, status) \
             VALUES (1, 101, 'token-101', 'Legacy gallery', 'subscription', 'pending')",
        )
        .await?;

        migrate_reuse_ledger_up(&db).await?;

        assert!(
            sqlite_master_entry_exists(&db, "table", "eh_gallery_results").await?,
            "result reuse table must be created"
        );
        assert!(
            sqlite_master_entry_exists(&db, "table", "eh_gallery_push_ledger").await?,
            "push ledger table must be created"
        );
        let migrated_job = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT source_fingerprint FROM eh_gallery_jobs WHERE gid = 101".to_owned(),
            ))
            .await?
            .expect("legacy gallery job must be migrated");
        assert_eq!(
            migrated_job.try_get::<Option<String>>("", "source_fingerprint")?,
            None
        );

        db.execute_unprepared(
            "INSERT INTO eh_gallery_results (\
                gid, token, download_mode, resolution, source_fingerprint, telegraph_url, created_at, updated_at\
             ) VALUES (101, 'token-101', 'archive', '1280x', 'fingerprint', 'https://telegra.ph/page', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
        )
        .await?;
        assert!(
            db.execute_unprepared(
                "INSERT INTO eh_gallery_results (\
                    gid, token, download_mode, resolution, source_fingerprint, telegraph_url, created_at, updated_at\
                 ) VALUES (101, 'token-101', 'archive', '1280x', 'fingerprint', 'https://telegra.ph/page', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)"
            )
            .await
            .is_err(),
            "duplicate gallery result variants must violate their unique constraint"
        );

        db.execute_unprepared(
            "INSERT INTO eh_gallery_push_ledger (chat_id, gid, updated_at) \
             VALUES (1, 101, CURRENT_TIMESTAMP)",
        )
        .await?;
        assert!(
            db.execute_unprepared(
                "INSERT INTO eh_gallery_push_ledger (chat_id, gid, updated_at) \
                 VALUES (1, 101, CURRENT_TIMESTAMP)"
            )
            .await
            .is_err(),
            "duplicate chat/gallery ledger rows must violate their unique constraint"
        );
        Ok(())
    }

    #[tokio::test]
    async fn migration_isolates_known_fingerprint_generations_and_deduplicates_null() -> Result<()>
    {
        let db = new_db().await?;
        create_legacy_shared_jobs_tables(&db).await?;
        migrate_reuse_ledger_up(&db).await?;

        db.execute_unprepared(
            "INSERT INTO eh_gallery_jobs (gid, token, download_mode, resolution, source_fingerprint, title) \
             VALUES (501, 'token', 'archive', '1280x', 'fingerprint-a', 'A')",
        )
        .await?;
        let legacy_error = db
            .execute_unprepared(
                "INSERT INTO eh_gallery_jobs (gid, token, download_mode, resolution, source_fingerprint, title) \
                 VALUES (501, 'token', 'archive', '1280x', 'fingerprint-b', 'B')",
            )
            .await
            .expect_err("legacy variant uniqueness must reject a second known fingerprint");
        assert!(
            legacy_error
                .to_string()
                .contains("UNIQUE constraint failed"),
            "unexpected legacy uniqueness error: {legacy_error:#}"
        );

        migrate_fingerprint_generations_from_reuse_schema_up(&db).await?;
        db.execute_unprepared(
            "INSERT INTO eh_gallery_jobs (gid, token, download_mode, resolution, source_fingerprint, title) VALUES \
                (501, 'token', 'archive', '1280x', 'fingerprint-b', 'B'), \
                (502, 'token', 'archive', '1280x', NULL, 'unknown')",
        )
        .await?;
        assert!(
            db.execute_unprepared(
                "INSERT INTO eh_gallery_jobs (gid, token, download_mode, resolution, source_fingerprint, title) \
                 VALUES (501, 'token', 'archive', '1280x', 'fingerprint-a', 'duplicate A')"
            )
            .await
            .is_err(),
            "the same known fingerprint must be unique within a variant"
        );
        assert!(
            db.execute_unprepared(
                "INSERT INTO eh_gallery_jobs (gid, token, download_mode, resolution, source_fingerprint, title) \
                 VALUES (502, 'token', 'archive', '1280x', NULL, 'duplicate unknown')"
            )
            .await
            .is_err(),
            "a variant must have at most one unknown fingerprint bucket"
        );
        Ok(())
    }

    #[tokio::test]
    async fn migration_fingerprint_generations_up_preserves_jobs_references_and_sequence(
    ) -> Result<()> {
        let db = new_db().await?;
        create_legacy_shared_jobs_tables(&db).await?;
        let prior_max_id = seed_fingerprint_generation_fixture(&db).await?;

        migrate_fingerprint_generations_from_reuse_schema_up(&db).await?;

        assert_job_rebuild_data_is_preserved(&db).await?;
        assert_foreign_key_check_is_clean(&db).await?;
        let fingerprint = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT source_fingerprint FROM eh_gallery_jobs WHERE id = 410".to_owned(),
            ))
            .await?
            .expect("fixture job survives the rebuild");
        assert_eq!(
            fingerprint.try_get::<Option<String>>("", "source_fingerprint")?,
            Some("fixture-fingerprint".to_owned())
        );

        db.execute_unprepared(
            "INSERT INTO eh_gallery_jobs (gid, token, download_mode, resolution, title) \
             VALUES (7002, 'new-token', 'archive', '1280x', 'New job')",
        )
        .await?;
        let row = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT id FROM eh_gallery_jobs WHERE gid = 7002".to_owned(),
            ))
            .await?
            .expect("new job is inserted");
        assert!(
            row.try_get::<i64>("", "id")? > prior_max_id,
            "the rebuilt table must retain its AUTOINCREMENT sequence"
        );
        Ok(())
    }

    #[tokio::test]
    async fn migration_compat_backfills_post_fingerprint_legacy_handoffs_and_preserves_references(
    ) -> Result<()> {
        let db = new_db().await?;
        create_legacy_shared_jobs_tables(&db).await?;
        migrate_fingerprint_generations_up(&db).await?;
        db.execute_unprepared("ALTER TABLE eh_gallery_jobs DROP COLUMN legacy_artifact_handoff")
            .await?;
        assert!(
            !table_has_column(&db, "eh_gallery_jobs", "legacy_artifact_handoff").await?,
            "fixture must simulate the already applied old shared/fingerprint migrations"
        );
        db.execute_unprepared(
            "INSERT INTO eh_gallery_jobs (\
                 id, gid, token, download_mode, resolution, source_fingerprint, title, \
                 status, file_size, zip_path, completed_at\
              ) VALUES \
                 (701, 9701, 'compat-token', 'archive', '1280x', 'fingerprint', 'Completed archive job', \
                  'downloaded', 4096, '/existing/archive.zip', '2026-08-27 01:02:03'), \
                 (702, 9702, 'single-token', 'legacy', 'subscription', NULL, 'Single legacy job', \
                  'pending', 0, NULL, NULL), \
                 (703, 9703, 'ambiguous-token', 'legacy', 'direct', NULL, 'Ambiguous direct legacy job', \
                  'pending', 0, NULL, NULL), \
                 (704, 9703, 'ambiguous-token', 'legacy', 'subscription', NULL, 'Ambiguous subscription legacy job', \
                  'pending', 0, NULL, NULL)",
        )
        .await?;
        db.execute_unprepared(
            "INSERT INTO eh_download_queue (\
                 id, job_id, chat_id, gid, token, title, telegraph, source, status\
              ) VALUES \
                 (801, 701, 80, 9701, 'compat-token', 'Completed archive delivery', 0, 'direct', 'waiting'), \
                 (802, 702, 81, 9702, 'single-token', 'Single legacy delivery', 0, 'subscription', 'waiting'), \
                 (803, 703, 82, 9703, 'ambiguous-token', 'Ambiguous direct delivery', 0, 'direct', 'waiting'), \
                 (804, 704, 83, 9703, 'ambiguous-token', 'Ambiguous subscription delivery', 0, 'subscription', 'waiting')",
        )
        .await?;
        db.execute_unprepared(
            "INSERT INTO eh_gp_spend_attempts (id, job_id, queue_id, gid, gp_cost, created_at) \
             VALUES (901, 701, 801, 9701, 7, '2026-08-27 01:02:03')",
        )
        .await?;
        db.execute_unprepared(
            "INSERT INTO eh_download_completions (id, job_id, gid, file_size, created_at) \
             VALUES (1001, 701, 9701, 4096, '2026-08-27 01:02:03')",
        )
        .await?;

        migrate_legacy_artifact_handoff_compat_up(&db).await?;
        migrate_legacy_artifact_handoff_compat_up(&db).await?;
        assert!(table_has_column(&db, "eh_gallery_jobs", "legacy_artifact_handoff").await?);

        let jobs = eh_gallery_jobs::Entity::find().all(&db).await?;
        let by_id = |id| {
            jobs.iter()
                .find(|job| job.id == id)
                .expect("compatibility fixture job must survive")
        };
        assert_eq!(
            by_id(702).legacy_artifact_handoff.as_deref(),
            Some(LEGACY_ARTIFACT_HANDOFF_PENDING),
            "the only legacy variant must be handed off before cleanup"
        );
        for id in [703, 704] {
            assert_eq!(
                by_id(id).legacy_artifact_handoff.as_deref(),
                Some(LEGACY_ARTIFACT_HANDOFF_CONFLICT),
                "unproven legacy variants must fail closed instead of choosing a resolution"
            );
        }
        assert_eq!(
            by_id(701).legacy_artifact_handoff,
            None,
            "non-legacy archive jobs must not gain migration handoff state"
        );

        let job = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT id, gid, token, source_fingerprint, status, zip_path \
                 FROM eh_gallery_jobs WHERE id = 701"
                    .to_owned(),
            ))
            .await?
            .expect("pre-compatibility job must survive the rebuild");
        assert_eq!(job.try_get::<i64>("", "gid")?, 9701);
        assert_eq!(job.try_get::<String>("", "token")?, "compat-token");
        assert_eq!(
            job.try_get::<Option<String>>("", "source_fingerprint")?,
            Some("fingerprint".to_owned())
        );
        assert_eq!(job.try_get::<String>("", "status")?, "downloaded");
        assert_eq!(
            job.try_get::<Option<String>>("", "zip_path")?,
            Some("/existing/archive.zip".to_owned())
        );
        for (table, id) in [
            ("eh_download_queue", 801),
            ("eh_gp_spend_attempts", 901),
            ("eh_download_completions", 1001),
        ] {
            let row = db
                .query_one(Statement::from_string(
                    DbBackend::Sqlite,
                    format!("SELECT job_id FROM {table} WHERE id = {id}"),
                ))
                .await?
                .expect("pre-compatibility reference must survive the rebuild");
            assert_eq!(row.try_get::<i64>("", "job_id")?, 701);
        }
        assert_foreign_key_check_is_clean(&db).await?;

        let repo = Repo::new(db.clone());
        let temp = tempfile::tempdir()?;
        let cache_dir = temp.path().join("eh_cache");
        std::fs::create_dir_all(&cache_dir)?;
        let single_source = ArchiveArtifacts::new(cache_dir.join("9702_single-token.zip"));
        std::fs::write(single_source.assembly_scratch(), b"single partial")?;
        std::fs::create_dir_all(single_source.parts_dir().join("nested"))?;
        std::fs::write(
            single_source.parts_dir().join("manifest.json"),
            b"single manifest",
        )?;
        std::fs::write(
            single_source.parts_dir().join("nested/part-0001"),
            b"single part",
        )?;
        let ambiguous_source = ArchiveArtifacts::new(cache_dir.join("9703_ambiguous-token.zip"));
        std::fs::write(ambiguous_source.assembly_scratch(), b"ambiguous partial")?;

        repo.reset_stale_eh_shared_work(60, 60).await?;
        repo.reconcile_eh_shared_job_liveness(true).await?;
        assert_eq!(
            repo.handoff_legacy_eh_archive_artifacts(&cache_dir).await?,
            1
        );
        repo.cleanup_eh_cache_orphans(&cache_dir, None).await?;

        let single_job = eh_gallery_jobs::Entity::find_by_id(702)
            .one(repo.db())
            .await?
            .expect("single legacy job must survive handoff");
        let single_target =
            ArchiveArtifacts::new(eh_gallery_job_artifact_path(&cache_dir, &single_job));
        assert_eq!(single_job.legacy_artifact_handoff, None);
        assert!(!single_source.assembly_scratch().exists());
        assert!(!single_source.parts_dir().exists());
        assert_eq!(
            std::fs::read(single_target.assembly_scratch())?,
            b"single partial"
        );
        assert!(single_target.parts_dir().join("nested/part-0001").exists());
        assert!(
            ambiguous_source.assembly_scratch().exists(),
            "ambiguous old source must remain cleanup-owned"
        );

        let claimed = repo
            .get_next_eh_job_for_download()
            .await?
            .expect("the single adopted job must be claimable");
        assert_eq!(claimed.id, 702);
        assert!(
            repo.get_next_eh_job_for_download().await?.is_none(),
            "conflict-marked jobs must remain blocked while their old source exists"
        );
        std::fs::remove_file(ambiguous_source.assembly_scratch())?;
        assert_eq!(
            repo.handoff_legacy_eh_archive_artifacts(&cache_dir).await?,
            2
        );
        repo.cleanup_eh_cache_orphans(&cache_dir, None).await?;
        let ambiguous_jobs = eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::Id.is_in([703, 704]))
            .all(repo.db())
            .await?;
        assert!(ambiguous_jobs
            .iter()
            .all(|job| job.legacy_artifact_handoff.is_none()));
        assert!(
            repo.get_next_eh_job_for_download().await?.is_some(),
            "missing ambiguous source must release jobs for ordinary shared work"
        );

        migrate_fingerprint_generations_down(&db).await?;
        migrate_legacy_artifact_handoff_compat_down(&db).await?;
        assert!(
            !table_has_column(&db, "eh_gallery_jobs", "legacy_artifact_handoff").await?,
            "compatibility migration down must restore the pre-compatibility schema"
        );
        assert_foreign_key_check_is_clean(&db).await?;
        Ok(())
    }

    #[tokio::test]
    async fn migration_fingerprint_generations_creates_expected_indexes() -> Result<()> {
        let db = new_db().await?;
        create_legacy_shared_jobs_tables(&db).await?;

        migrate_fingerprint_generations_up(&db).await?;

        for index in JOB_REBUILD_INDEXES {
            assert!(
                sqlite_master_entry_exists(&db, "index", index).await?,
                "{index} must be recreated"
            );
        }
        assert!(
            sqlite_master_entry_exists(&db, "index", KNOWN_FINGERPRINT_INDEX).await?,
            "known-fingerprint partial unique index must exist"
        );
        assert!(
            sqlite_master_entry_exists(&db, "index", UNKNOWN_FINGERPRINT_INDEX).await?,
            "unknown-fingerprint partial unique index must exist"
        );
        assert!(
            !sqlite_master_entry_exists(&db, "index", LEGACY_VARIANT_INDEX).await?,
            "legacy variant unique index must be removed"
        );
        Ok(())
    }

    #[tokio::test]
    async fn migration_fingerprint_generations_down_restores_legacy_variant_without_data_loss(
    ) -> Result<()> {
        let db = new_db().await?;
        create_legacy_shared_jobs_tables(&db).await?;
        seed_fingerprint_generation_fixture(&db).await?;
        migrate_fingerprint_generations_from_reuse_schema_up(&db).await?;

        migrate_fingerprint_generations_down(&db).await?;

        assert_job_rebuild_data_is_preserved(&db).await?;
        assert_foreign_key_check_is_clean(&db).await?;
        assert!(
            sqlite_master_entry_exists(&db, "index", LEGACY_VARIANT_INDEX).await?,
            "compatible down migration must restore the legacy variant index"
        );
        assert!(
            db.execute_unprepared(
                "INSERT INTO eh_gallery_jobs (\
                    gid, token, download_mode, resolution, source_fingerprint, title\
                 ) VALUES (7001, 'fixture-token', 'archive', '1280x', 'other-fingerprint', 'duplicate variant')"
            )
            .await
            .is_err(),
            "legacy variant uniqueness must reject a different fingerprint for the same variant"
        );
        Ok(())
    }

    #[tokio::test]
    async fn migration_fingerprint_generations_down_rejects_duplicate_generations_without_changes(
    ) -> Result<()> {
        let db = new_db().await?;
        create_legacy_shared_jobs_tables(&db).await?;
        migrate_fingerprint_generations_up(&db).await?;
        db.execute_unprepared(
            "INSERT INTO eh_gallery_jobs (gid, token, download_mode, resolution, source_fingerprint, title) VALUES \
                (8001, 'duplicate-token', 'archive', '1280x', 'fingerprint-a', 'Generation A'), \
                (8001, 'duplicate-token', 'archive', '1280x', 'fingerprint-b', 'Generation B')",
        )
        .await?;
        db.execute_unprepared(
            "CREATE TEMP TABLE expected_eh_gallery_jobs AS SELECT * FROM eh_gallery_jobs",
        )
        .await?;

        let error = migrate_fingerprint_generations_down(&db)
            .await
            .expect_err("down migration must reject multiple generations for one variant");
        assert!(error.to_string().contains("multiple source fingerprints"));

        assert_same_table_rows(&db, "expected_eh_gallery_jobs", "eh_gallery_jobs").await?;
        assert!(
            sqlite_master_entry_exists(&db, "index", KNOWN_FINGERPRINT_INDEX).await?,
            "failed down migration must keep the known-fingerprint index"
        );
        assert!(
            sqlite_master_entry_exists(&db, "index", UNKNOWN_FINGERPRINT_INDEX).await?,
            "failed down migration must keep the unknown-fingerprint index"
        );
        assert!(
            !sqlite_master_entry_exists(&db, "index", LEGACY_VARIANT_INDEX).await?,
            "failed down migration must not create the legacy variant index"
        );
        assert_foreign_key_check_is_clean(&db).await?;
        Ok(())
    }

    #[tokio::test]
    async fn migration_rolls_back_table_and_index_when_backfill_fails() -> Result<()> {
        let db = new_db().await?;
        db.execute_unprepared(
            "CREATE TABLE eh_download_queue (\
                id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, \
                gp_cost INTEGER NOT NULL DEFAULT 0, \
                completed_at TIMESTAMP\
            )",
        )
        .await?;

        let error = migrate_up(&db)
            .await
            .expect_err("missing gid must make the backfill fail after DDL");
        assert!(error.to_string().contains("gid"));
        assert!(
            !migration_table_exists(&db).await?,
            "failed SQLite migration must roll back the created table"
        );
        assert!(
            !migration_created_at_index_exists(&db).await?,
            "failed SQLite migration must roll back the created index"
        );

        db.execute_unprepared("DROP TABLE eh_download_queue")
            .await?;
        create_legacy_queue_table(&db).await?;
        migrate_up(&db).await?;

        assert!(migration_table_exists(&db).await?);
        assert!(migration_created_at_index_exists(&db).await?);
        Ok(())
    }

    #[tokio::test]
    async fn migration_preserves_pending_rewrite_from_completed_delivery() -> Result<()> {
        let db = new_db().await?;
        create_legacy_shared_jobs_tables(&db).await?;
        db.execute_unprepared(
            "INSERT INTO eh_download_queue (\
                id, chat_id, gid, token, title, telegraph, source, status, created_at, \
                telegraph_url, telegraph_sent_at, telegraph_rewrite_data, \
                telegraph_rewrite_status, telegraph_rewrite_after, \
                telegraph_rewrite_next_retry_at, telegraph_rewrite_retry_count, \
                telegraph_rewrite_error, telegraph_rewritten_at\
             ) VALUES \
                (1, 10, 150, 'token-150', 'Completed rewrite delivery', 1, 'subscription', 'done', '2026-08-01 00:00:00', 'https://old.example/completed', '2026-08-01 00:30:00', 'rewrite payload', 'pending', '1970-01-01 01:00:00', '1970-01-01 02:00:00', 5, 'previous rewrite error', NULL), \
                (2, 20, 150, 'token-150', 'Active sibling delivery', 0, 'subscription', 'pending', '2026-08-02 00:00:00', NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL, NULL), \
                (3, 30, 151, 'token-151', 'Earlier ordinary page', 0, 'subscription', 'uploading', '2026-08-03 00:00:00', 'https://old.example/ordinary', '2026-08-03 00:30:00', NULL, NULL, NULL, NULL, 0, NULL, NULL), \
                (4, 40, 151, 'token-151', 'Terminal rewrite winner', 1, 'subscription', 'done', '2026-08-04 00:00:00', 'https://old.example/rewrite', '2026-08-04 00:30:00', 'winner payload', 'pending', '2026-08-04 01:00:00', NULL, 2, 'winner retry error', NULL)",
        )
        .await?;

        migrate_shared_jobs_up(&db).await?;

        let job_count = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM eh_gallery_jobs WHERE gid = 150".to_owned(),
            ))
            .await?
            .expect("job count query returns one row");
        assert_eq!(
            job_count.try_get::<i64>("", "count")?,
            1,
            "an active delivery and terminal rewrite share one variant job"
        );
        let job = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT id, title, telegraph_required, telegraph_status, telegraph_url, telegraph_rewrite_data, telegraph_rewrite_status, telegraph_rewrite_after, telegraph_rewrite_started_at, telegraph_rewrite_next_retry_at, telegraph_rewrite_retry_count, telegraph_rewrite_error, telegraph_rewritten_at FROM eh_gallery_jobs WHERE gid = 150".to_owned(),
            ))
            .await?
            .expect("terminal rewrite and active sibling share one job");
        let job_id = job.try_get::<i64>("", "id")?;
        assert_eq!(
            job.try_get::<String>("", "title")?,
            "Completed rewrite delivery",
            "a terminal rewrite-bearing row remains a title candidate"
        );
        assert!(
            !job.try_get::<bool>("", "telegraph_required")?,
            "already-sent terminal work must not create unsent Telegraph demand"
        );
        assert_eq!(job.try_get::<String>("", "telegraph_status")?, "ready");
        assert_eq!(
            job.try_get::<Option<String>>("", "telegraph_url")?,
            Some("https://old.example/completed".to_owned())
        );
        assert_eq!(
            job.try_get::<Option<String>>("", "telegraph_rewrite_data")?,
            Some("rewrite payload".to_owned())
        );
        assert_eq!(
            job.try_get::<Option<String>>("", "telegraph_rewrite_status")?,
            Some("pending".to_owned())
        );
        assert_eq!(
            job.try_get::<Option<String>>("", "telegraph_rewrite_after")?,
            Some("1970-01-01 01:00:00".to_owned())
        );
        assert_eq!(
            job.try_get::<Option<String>>("", "telegraph_rewrite_started_at")?,
            None
        );
        assert_eq!(
            job.try_get::<Option<String>>("", "telegraph_rewrite_next_retry_at")?,
            Some("1970-01-01 02:00:00".to_owned())
        );
        assert_eq!(job.try_get::<i64>("", "telegraph_rewrite_retry_count")?, 5);
        assert_eq!(
            job.try_get::<Option<String>>("", "telegraph_rewrite_error")?,
            Some("previous rewrite error".to_owned())
        );
        assert_eq!(
            job.try_get::<Option<String>>("", "telegraph_rewritten_at")?,
            None
        );

        let rewrite_claimable = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT id FROM eh_gallery_jobs \
                 WHERE gid = 150 \
                   AND telegraph_rewrite_status = 'pending' \
                   AND telegraph_rewrite_data IS NOT NULL \
                   AND telegraph_rewritten_at IS NULL \
                   AND (telegraph_rewrite_after IS NULL OR telegraph_rewrite_after <= CURRENT_TIMESTAMP) \
                   AND (telegraph_rewrite_next_retry_at IS NULL OR telegraph_rewrite_next_retry_at <= CURRENT_TIMESTAMP)".to_owned(),
            ))
            .await?
            .expect("migrated rewrite must satisfy the job rewrite lane predicate");
        assert_eq!(rewrite_claimable.try_get::<i64>("", "id")?, job_id);

        let rewrite_winner = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT telegraph_required, telegraph_status, telegraph_url, telegraph_rewrite_data, telegraph_rewrite_status FROM eh_gallery_jobs WHERE gid = 151".to_owned(),
            ))
            .await?
            .expect("ordinary and terminal pages share one job");
        assert!(
            !rewrite_winner.try_get::<bool>("", "telegraph_required")?,
            "a sent terminal page does not create Telegraph demand"
        );
        assert_eq!(
            rewrite_winner.try_get::<String>("", "telegraph_status")?,
            "ready"
        );
        assert_eq!(
            rewrite_winner.try_get::<Option<String>>("", "telegraph_url")?,
            Some("https://old.example/rewrite".to_owned()),
            "unfinished rewrite work must win over an earlier ordinary page"
        );
        assert_eq!(
            rewrite_winner.try_get::<Option<String>>("", "telegraph_rewrite_data")?,
            Some("winner payload".to_owned())
        );
        assert_eq!(
            rewrite_winner.try_get::<Option<String>>("", "telegraph_rewrite_status")?,
            Some("pending".to_owned())
        );

        let deliveries = db
            .query_all(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT id, job_id, status, telegraph_url, telegraph_sent_at, telegraph_rewrite_data, telegraph_rewrite_status FROM eh_download_queue WHERE id IN (1, 2) ORDER BY id".to_owned(),
            ))
            .await?;
        assert_eq!(deliveries.len(), 2);
        assert_eq!(
            deliveries[0].try_get::<Option<i64>>("", "job_id")?,
            Some(job_id)
        );
        assert_eq!(deliveries[0].try_get::<String>("", "status")?, "done");
        assert_eq!(
            deliveries[0].try_get::<Option<String>>("", "telegraph_url")?,
            Some("https://old.example/completed".to_owned())
        );
        assert_eq!(
            deliveries[0].try_get::<Option<String>>("", "telegraph_sent_at")?,
            Some("2026-08-01 00:30:00".to_owned())
        );
        assert_eq!(
            deliveries[0].try_get::<Option<String>>("", "telegraph_rewrite_data")?,
            Some("rewrite payload".to_owned())
        );
        assert_eq!(
            deliveries[0].try_get::<Option<String>>("", "telegraph_rewrite_status")?,
            Some("pending".to_owned())
        );
        assert_eq!(
            deliveries[1].try_get::<Option<i64>>("", "job_id")?,
            Some(job_id)
        );
        assert_eq!(deliveries[1].try_get::<String>("", "status")?, "waiting");
        Ok(())
    }

    #[tokio::test]
    async fn migration_groups_active_legacy_variants_and_leaves_terminal_history_unbound(
    ) -> Result<()> {
        let db = new_db().await?;
        create_legacy_shared_jobs_tables(&db).await?;
        db.execute_unprepared(
            "INSERT INTO eh_download_queue (\
                id, chat_id, gid, token, title, telegraph, source, subscription_ids, \
                telegraph_subscription_ids, status, file_size, gp_cost, error, retry_count, \
                created_at, started_at, completed_at, zip_path, telegraph_url, next_retry_at, \
                archive_sent_at, telegraph_sent_at, background_download_status, \
                background_download_started_at, background_download_next_retry_at, \
                background_download_attempt_count, background_download_error, \
                telegraph_rewrite_data, telegraph_rewrite_status, telegraph_rewrite_after, \
                telegraph_rewrite_started_at, telegraph_rewrite_next_retry_at, \
                telegraph_rewrite_retry_count, telegraph_rewrite_error, telegraph_rewritten_at\
             ) VALUES \
                (1, 10, 101, 'token-101', 'Subscription first', 0, 'subscription', '1', NULL, 'pending', 0, 0, NULL, 0, '2026-08-01 00:00:00', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL, NULL), \
                (2, 20, 101, 'token-101', 'Subscription second', 1, 'subscription', '2', '2', 'uploading', 123, 456, 'old error', 3, '2026-08-02 00:00:00', '2026-08-02 01:00:00', '2026-08-02 02:00:00', '/old/subscription.zip', 'https://old.example/subscription', '2026-08-02 03:00:00', '2026-08-02 04:00:00', NULL, 'running', '2026-08-02 05:00:00', '2026-08-02 06:00:00', 2, 'background error', 'rewrite data', 'rewriting', '2026-08-02 07:00:00', '2026-08-02 08:00:00', '2026-08-02 09:00:00', 4, 'rewrite error', '2026-08-02 10:00:00'), \
                (3, 30, 101, 'token-101', 'Direct first', 1, 'direct', NULL, NULL, 'downloaded', 10, 20, 'direct error', 1, '2026-08-03 00:00:00', '2026-08-03 01:00:00', '2026-08-03 02:00:00', '/old/direct.zip', 'https://old.example/direct', '2026-08-03 03:00:00', NULL, '2026-08-03 04:00:00', 'pending', '2026-08-03 05:00:00', '2026-08-03 06:00:00', 1, 'direct background error', NULL, NULL, NULL, NULL, NULL, 0, NULL, NULL), \
                (4, 40, 102, 'token-102', 'Other gallery', 0, 'subscription', '3', NULL, 'publishing', 0, 0, NULL, 0, '2026-08-04 00:00:00', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL, NULL), \
                (5, 50, 101, 'token-101', 'Terminal history', 0, 'subscription', '4', NULL, 'done', 999, 0, 'terminal error', 9, '2026-08-05 00:00:00', '2026-08-05 01:00:00', '2026-08-05 02:00:00', '/terminal.zip', NULL, '2026-08-05 03:00:00', '2026-08-05 04:00:00', '2026-08-05 05:00:00', 'failed', '2026-08-05 06:00:00', '2026-08-05 07:00:00', 8, 'terminal background error', 'terminal rewrite', 'failed', '2026-08-05 08:00:00', '2026-08-05 09:00:00', '2026-08-05 10:00:00', 7, 'terminal rewrite error', '2026-08-05 11:00:00')",
        )
        .await?;

        migrate_shared_jobs_up(&db).await?;

        let jobs = db
            .query_all(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT gid, token, download_mode, resolution, title, status, telegraph_required, telegraph_status, created_at, zip_path, file_size, gp_cost, completed_at, telegraph_url, telegraph_rewrite_data, telegraph_rewrite_status, telegraph_rewrite_after, telegraph_rewrite_started_at, telegraph_rewrite_next_retry_at, telegraph_rewrite_retry_count, telegraph_rewrite_error, telegraph_rewritten_at FROM eh_gallery_jobs ORDER BY gid, resolution".to_owned(),
            ))
            .await?;
        assert_eq!(jobs.len(), 3);
        assert_eq!(jobs[0].try_get::<i64>("", "gid")?, 101);
        assert_eq!(jobs[0].try_get::<String>("", "download_mode")?, "legacy");
        assert_eq!(jobs[0].try_get::<String>("", "resolution")?, "direct");
        assert_eq!(jobs[0].try_get::<String>("", "title")?, "Direct first");
        assert!(!jobs[0].try_get::<bool>("", "telegraph_required")?);
        assert_eq!(
            jobs[0].try_get::<String>("", "telegraph_status")?,
            "not_required"
        );
        assert_eq!(jobs[1].try_get::<i64>("", "gid")?, 101);
        assert_eq!(jobs[1].try_get::<String>("", "resolution")?, "subscription");
        assert_eq!(
            jobs[1].try_get::<String>("", "title")?,
            "Subscription first"
        );
        assert!(jobs[1].try_get::<bool>("", "telegraph_required")?);
        assert_eq!(
            jobs[1].try_get::<String>("", "telegraph_status")?,
            "ready",
            "job with a pre-existing Telegraph page must not re-upload it"
        );
        assert_eq!(jobs[2].try_get::<i64>("", "gid")?, 102);
        assert_eq!(jobs[2].try_get::<String>("", "resolution")?, "subscription");

        // Gallery 101 subscription group: row 2 (`uploading`) carries the most
        // advanced completed work, so its download result and Telegraph page
        // must be preserved on the shared job instead of being redownloaded.
        assert_eq!(
            jobs[1].try_get::<String>("", "status")?,
            "downloaded",
            "subscription job must inherit the most advanced row's download result"
        );
        assert_eq!(
            jobs[1].try_get::<Option<String>>("", "zip_path")?,
            Some("/old/subscription.zip".to_owned())
        );
        assert_eq!(jobs[1].try_get::<i64>("", "file_size")?, 123);
        assert_eq!(jobs[1].try_get::<i64>("", "gp_cost")?, 456);
        assert_eq!(
            jobs[1].try_get::<Option<String>>("", "completed_at")?,
            Some("2026-08-02 02:00:00".to_owned())
        );
        assert_eq!(
            jobs[1].try_get::<String>("", "telegraph_status")?,
            "ready",
            "subscription job must inherit the existing Telegraph page"
        );
        assert_eq!(
            jobs[1].try_get::<Option<String>>("", "telegraph_url")?,
            Some("https://old.example/subscription".to_owned())
        );
        assert_eq!(
            jobs[1].try_get::<Option<String>>("", "telegraph_rewrite_data")?,
            Some("rewrite data".to_owned())
        );
        assert_eq!(
            jobs[1].try_get::<Option<String>>("", "telegraph_rewrite_status")?,
            Some("rewriting".to_owned())
        );
        assert_eq!(
            jobs[1].try_get::<Option<String>>("", "telegraph_rewrite_after")?,
            Some("2026-08-02 07:00:00".to_owned())
        );
        assert_eq!(
            jobs[1].try_get::<Option<String>>("", "telegraph_rewrite_started_at")?,
            Some("2026-08-02 08:00:00".to_owned())
        );
        assert_eq!(
            jobs[1].try_get::<Option<String>>("", "telegraph_rewrite_next_retry_at")?,
            Some("2026-08-02 09:00:00".to_owned())
        );
        assert_eq!(
            jobs[1].try_get::<i64>("", "telegraph_rewrite_retry_count")?,
            4
        );
        assert_eq!(
            jobs[1].try_get::<Option<String>>("", "telegraph_rewrite_error")?,
            Some("rewrite error".to_owned())
        );
        assert_eq!(
            jobs[1].try_get::<Option<String>>("", "telegraph_rewritten_at")?,
            Some("2026-08-02 10:00:00".to_owned())
        );

        // Gallery 101 direct group: row 3 (`downloaded`) completed its download,
        // but Telegraph was never demanded unsent for this variant, so the
        // direct job stays `not_required` with no URL.
        assert_eq!(
            jobs[0].try_get::<String>("", "status")?,
            "downloaded",
            "direct job must inherit the downloaded row's archive"
        );
        assert_eq!(
            jobs[0].try_get::<Option<String>>("", "zip_path")?,
            Some("/old/direct.zip".to_owned())
        );
        assert_eq!(jobs[0].try_get::<i64>("", "file_size")?, 10);
        assert_eq!(jobs[0].try_get::<i64>("", "gp_cost")?, 20);
        assert_eq!(
            jobs[0].try_get::<Option<String>>("", "completed_at")?,
            Some("2026-08-03 02:00:00".to_owned())
        );
        assert_eq!(
            jobs[0].try_get::<Option<String>>("", "telegraph_url")?,
            None,
            "not_required jobs must not gain a Telegraph URL from siblings"
        );

        // Gallery 102 subscription group: row 4 (`publishing`) has no zip
        // artifact, so its shared job stays `pending` with no download fields.
        assert_eq!(
            jobs[2].try_get::<String>("", "status")?,
            "pending",
            "job without a download-complete row must stay pending"
        );
        assert_eq!(jobs[2].try_get::<Option<String>>("", "zip_path")?, None);
        assert_eq!(jobs[2].try_get::<i64>("", "file_size")?, 0);
        assert_eq!(jobs[2].try_get::<i64>("", "gp_cost")?, 0);
        assert_eq!(jobs[2].try_get::<Option<String>>("", "completed_at")?, None);

        let active = db
            .query_all(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT id, job_id, status, started_at, completed_at, error, retry_count, next_retry_at, file_size, gp_cost, zip_path, telegraph_url, background_download_status, background_download_started_at, background_download_next_retry_at, background_download_attempt_count, background_download_error, telegraph_rewrite_data, telegraph_rewrite_status, telegraph_rewrite_after, telegraph_rewrite_started_at, telegraph_rewrite_next_retry_at, telegraph_rewrite_retry_count, telegraph_rewrite_error, telegraph_rewritten_at, subscription_ids, telegraph_subscription_ids, archive_sent_at, telegraph_sent_at FROM eh_download_queue WHERE id IN (1, 2, 3, 4) ORDER BY id".to_owned(),
            ))
            .await?;
        assert_eq!(active.len(), 4);
        for row in &active {
            assert!(row.try_get::<Option<i64>>("", "job_id")?.is_some());
            assert_eq!(row.try_get::<String>("", "status")?, "waiting");
            assert_eq!(row.try_get::<Option<String>>("", "started_at")?, None);
            assert_eq!(row.try_get::<Option<String>>("", "completed_at")?, None);
            assert_eq!(row.try_get::<Option<String>>("", "error")?, None);
            assert_eq!(row.try_get::<i64>("", "retry_count")?, 0);
            assert_eq!(row.try_get::<Option<String>>("", "next_retry_at")?, None);
            assert_eq!(row.try_get::<i64>("", "file_size")?, 0);
            assert_eq!(row.try_get::<i64>("", "gp_cost")?, 0);
            assert_eq!(row.try_get::<Option<String>>("", "zip_path")?, None);
            assert_eq!(row.try_get::<Option<String>>("", "telegraph_url")?, None);
            assert_eq!(
                row.try_get::<Option<String>>("", "background_download_status")?,
                None
            );
            assert_eq!(
                row.try_get::<Option<String>>("", "background_download_started_at")?,
                None
            );
            assert_eq!(
                row.try_get::<Option<String>>("", "background_download_next_retry_at")?,
                None
            );
            assert_eq!(
                row.try_get::<i64>("", "background_download_attempt_count")?,
                0
            );
            assert_eq!(
                row.try_get::<Option<String>>("", "background_download_error")?,
                None
            );
            assert_eq!(
                row.try_get::<Option<String>>("", "telegraph_rewrite_data")?,
                None
            );
            assert_eq!(
                row.try_get::<Option<String>>("", "telegraph_rewrite_status")?,
                None
            );
            assert_eq!(
                row.try_get::<Option<String>>("", "telegraph_rewrite_after")?,
                None
            );
            assert_eq!(
                row.try_get::<Option<String>>("", "telegraph_rewrite_started_at")?,
                None
            );
            assert_eq!(
                row.try_get::<Option<String>>("", "telegraph_rewrite_next_retry_at")?,
                None
            );
            assert_eq!(row.try_get::<i64>("", "telegraph_rewrite_retry_count")?, 0);
            assert_eq!(
                row.try_get::<Option<String>>("", "telegraph_rewrite_error")?,
                None
            );
            assert_eq!(
                row.try_get::<Option<String>>("", "telegraph_rewritten_at")?,
                None
            );
        }
        assert_eq!(
            active[1].try_get::<Option<String>>("", "subscription_ids")?,
            Some("2".to_owned())
        );
        assert_eq!(
            active[1].try_get::<Option<String>>("", "telegraph_subscription_ids")?,
            Some("2".to_owned())
        );
        assert_eq!(
            active[1].try_get::<Option<String>>("", "archive_sent_at")?,
            Some("2026-08-02 04:00:00".to_owned())
        );
        assert_eq!(
            active[2].try_get::<Option<String>>("", "telegraph_sent_at")?,
            Some("2026-08-03 04:00:00".to_owned())
        );

        let terminal = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT job_id, status, file_size, error FROM eh_download_queue WHERE id = 5"
                    .to_owned(),
            ))
            .await?
            .expect("terminal history row exists");
        assert_eq!(terminal.try_get::<Option<i64>>("", "job_id")?, None);
        assert_eq!(terminal.try_get::<String>("", "status")?, "done");
        assert_eq!(terminal.try_get::<i64>("", "file_size")?, 999);
        assert_eq!(
            terminal.try_get::<Option<String>>("", "error")?,
            Some("terminal error".to_owned())
        );
        Ok(())
    }

    #[tokio::test]
    async fn migration_startup_handoff_moves_crash_left_legacy_partial_before_cleanup() -> Result<()>
    {
        let db = new_db().await?;
        create_legacy_shared_jobs_tables(&db).await?;
        db.execute_unprepared(
            "INSERT INTO eh_download_queue (\
                 id, chat_id, gid, token, title, source, status, created_at\
             ) VALUES (1, 10, 912, 'legacy-token', 'Crash left partial', 'subscription', \
                       'downloading', '2026-08-24 00:00:00')",
        )
        .await?;
        migrate_fingerprint_generations_up(&db).await?;

        let repo = Repo::new(db);
        let temp = tempfile::tempdir()?;
        let cache_dir = temp.path().join("eh_cache");
        std::fs::create_dir_all(&cache_dir)?;
        let legacy = ArchiveArtifacts::new(cache_dir.join("912_legacy-token.zip"));
        std::fs::write(legacy.assembly_scratch(), b"partial")?;
        std::fs::create_dir_all(legacy.parts_dir().join("nested"))?;
        std::fs::write(legacy.parts_dir().join("manifest.json"), b"manifest")?;
        std::fs::write(legacy.parts_dir().join("nested/part-0001"), b"part")?;

        repo.reset_stale_eh_shared_work(60, 60).await?;
        repo.reconcile_eh_shared_job_liveness(true).await?;
        assert_eq!(
            repo.handoff_legacy_eh_archive_artifacts(&cache_dir).await?,
            1
        );
        repo.cleanup_eh_cache_orphans(&cache_dir, None).await?;

        let job = eh_gallery_jobs::Entity::find()
            .one(repo.db())
            .await?
            .expect("migration must create one shared job");
        let target = ArchiveArtifacts::new(eh_gallery_job_artifact_path(&cache_dir, &job));
        assert!(
            !legacy.assembly_scratch().exists() && !legacy.parts_dir().exists(),
            "the old family must be gone only after it is handed off"
        );
        assert!(
            target.assembly_scratch().exists() && target.parts_dir().exists(),
            "the job-specific family must retain every resumable member through orphan cleanup"
        );
        assert_eq!(job.legacy_artifact_handoff, None);
        assert_eq!(
            job.zip_path.as_deref(),
            Some(target.final_zip().to_string_lossy().as_ref())
        );

        let claimed = repo
            .get_next_eh_job_for_download()
            .await?
            .expect("the adopted job must be claimable by a shared worker");
        assert_eq!(claimed.id, job.id);
        assert!(
            repo.persist_eh_job_archive_artifact_ownership(
                claimed.id,
                claimed.started_at.expect("claim must have a generation"),
                &target.final_zip().to_string_lossy(),
                false,
            )
            .await?,
            "the shared worker must accept the migrated target family before making a provider request"
        );
        Ok(())
    }

    #[tokio::test]
    async fn migration_handoff_assigns_one_background_owner_and_is_idempotent() -> Result<()> {
        let db = new_db().await?;
        create_legacy_shared_jobs_tables(&db).await?;
        db.execute_unprepared(
            "INSERT INTO eh_download_queue (\
                 id, chat_id, gid, token, title, source, status, background_download_status, created_at\
             ) VALUES \
                 (1, 10, 913, 'shared-token', 'Direct pending', 'direct', 'pending', 'pending', '2026-08-24 00:00:00'), \
                 (2, 20, 913, 'shared-token', 'Background running', 'subscription', 'pending', 'running', '2026-08-24 00:01:00')",
        )
        .await?;
        migrate_fingerprint_generations_up(&db).await?;

        let repo = Repo::new(db);
        let jobs = eh_gallery_jobs::Entity::find().all(repo.db()).await?;
        let marked: Vec<_> = jobs
            .iter()
            .filter(|job| {
                job.legacy_artifact_handoff.as_deref() == Some(LEGACY_ARTIFACT_HANDOFF_PENDING)
            })
            .collect();
        assert_eq!(marked.len(), 1, "only one shared job may own an old family");
        assert_eq!(marked[0].resolution, "subscription");

        let temp = tempfile::tempdir()?;
        let cache_dir = temp.path().join("eh_cache");
        std::fs::create_dir_all(&cache_dir)?;
        let legacy = ArchiveArtifacts::new(cache_dir.join("913_shared-token.zip"));
        std::fs::write(legacy.assembly_scratch(), b"background partial")?;
        std::fs::create_dir_all(legacy.parts_dir())?;
        std::fs::write(legacy.parts_dir().join("manifest.json"), b"manifest")?;

        repo.reset_stale_eh_shared_work(60, 60).await?;
        repo.reconcile_eh_shared_job_liveness(true).await?;
        assert_eq!(
            repo.handoff_legacy_eh_archive_artifacts(&cache_dir).await?,
            1
        );
        assert_eq!(
            repo.handoff_legacy_eh_archive_artifacts(&cache_dir).await?,
            0,
            "a completed handoff must be safe to repeat after a restart"
        );
        repo.cleanup_eh_cache_orphans(&cache_dir, None).await?;

        let target = ArchiveArtifacts::new(eh_gallery_job_artifact_path(&cache_dir, marked[0]));
        assert!(!legacy.assembly_scratch().exists());
        assert!(!legacy.parts_dir().exists());
        assert!(target.assembly_scratch().exists());
        assert!(target.parts_dir().exists());
        Ok(())
    }

    #[tokio::test]
    async fn migration_handoff_prefers_the_latest_normal_and_background_claims() -> Result<()> {
        let db = new_db().await?;
        create_legacy_shared_jobs_tables(&db).await?;
        db.execute_unprepared(
            "INSERT INTO eh_download_queue (\
                 id, chat_id, gid, token, title, source, status, started_at, \
                 background_download_status, background_download_started_at, created_at\
             ) VALUES \
                 (1, 10, 916, 'normal-claim-token', 'Earlier direct claim', 'direct', 'pending', \
                  '2026-08-24 00:01:00', NULL, NULL, '2026-08-24 00:00:00'), \
                 (2, 20, 916, 'normal-claim-token', 'Later subscription claim', 'subscription', 'pending', \
                  '2026-08-24 00:02:00', NULL, NULL, '2026-08-24 00:00:00'), \
                 (3, 30, 917, 'background-claim-token', 'Earlier direct background claim', 'direct', 'pending', \
                  NULL, 'pending', '2026-08-24 00:03:00', '2026-08-24 00:00:00'), \
                 (4, 40, 917, 'background-claim-token', 'Later subscription background claim', 'subscription', 'pending', \
                  NULL, 'pending', '2026-08-24 00:04:00', '2026-08-24 00:00:00')",
        )
        .await?;
        migrate_fingerprint_generations_up(&db).await?;

        for gid in [916, 917] {
            let jobs = eh_gallery_jobs::Entity::find()
                .filter(eh_gallery_jobs::Column::Gid.eq(gid))
                .all(&db)
                .await?;
            let marked: Vec<_> = jobs
                .iter()
                .filter(|job| {
                    job.legacy_artifact_handoff.as_deref() == Some(LEGACY_ARTIFACT_HANDOFF_PENDING)
                })
                .collect();
            assert_eq!(marked.len(), 1, "gid {gid} must have one handoff owner");
            assert_eq!(
                marked[0].resolution, "subscription",
                "gid {gid} must retain the resume identity from the most recently claimed row"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn migration_handoff_conflicting_active_claims_preserve_source_and_unblock_when_absent(
    ) -> Result<()> {
        let db = new_db().await?;
        create_legacy_shared_jobs_tables(&db).await?;
        db.execute_unprepared(
            "INSERT INTO eh_download_queue (\
                 id, chat_id, gid, token, title, source, status, started_at, created_at\
             ) VALUES \
                 (1, 10, 918, 'ambiguous-token', 'Direct active claim', 'direct', 'downloading', \
                  '2026-08-24 00:01:00', '2026-08-24 00:00:00'), \
                 (2, 20, 918, 'ambiguous-token', 'Subscription active claim', 'subscription', 'downloading', \
                  '2026-08-24 00:02:00', '2026-08-24 00:00:00')",
        )
        .await?;
        migrate_fingerprint_generations_up(&db).await?;

        let repo = Repo::new(db);
        let jobs = eh_gallery_jobs::Entity::find().all(repo.db()).await?;
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().all(|job| {
            job.legacy_artifact_handoff.as_deref() == Some(LEGACY_ARTIFACT_HANDOFF_CONFLICT)
        }));

        let temp = tempfile::tempdir()?;
        let cache_dir = temp.path().join("eh_cache");
        std::fs::create_dir_all(&cache_dir)?;
        let legacy = ArchiveArtifacts::new(cache_dir.join("918_ambiguous-token.zip"));
        std::fs::write(legacy.assembly_scratch(), b"ambiguous partial")?;
        let target = ArchiveArtifacts::new(eh_gallery_job_artifact_path(&cache_dir, &jobs[0]));
        std::fs::write(target.assembly_scratch(), b"ambiguous target partial")?;

        repo.reset_stale_eh_shared_work(60, 60).await?;
        repo.reconcile_eh_shared_job_liveness(true).await?;
        assert_eq!(
            repo.handoff_legacy_eh_archive_artifacts(&cache_dir).await?,
            0
        );
        repo.cleanup_eh_cache_orphans(&cache_dir, None).await?;
        assert!(legacy.assembly_scratch().exists());
        assert!(
            target.assembly_scratch().exists(),
            "conflict state must retain a target family until source ownership is resolved"
        );
        assert!(
            repo.get_next_eh_job_for_download().await?.is_none(),
            "all ambiguous owners must block shared workers while the source remains"
        );

        std::fs::remove_file(legacy.assembly_scratch())?;
        assert_eq!(
            repo.handoff_legacy_eh_archive_artifacts(&cache_dir).await?,
            2
        );
        repo.cleanup_eh_cache_orphans(&cache_dir, None).await?;
        let unblocked = eh_gallery_jobs::Entity::find().all(repo.db()).await?;
        assert!(
            unblocked
                .iter()
                .all(|job| job.legacy_artifact_handoff.is_none()),
            "a missing source must not strand conflict-marked jobs"
        );
        assert!(
            repo.get_next_eh_job_for_download().await?.is_some(),
            "workers may resume ordinary work after the ambiguous source is gone"
        );
        Ok(())
    }

    #[tokio::test]
    async fn migration_handoff_clears_a_benign_pending_marker_when_source_is_absent() -> Result<()>
    {
        let db = new_db().await?;
        create_legacy_shared_jobs_tables(&db).await?;
        db.execute_unprepared(
            "INSERT INTO eh_download_queue (\
                 id, chat_id, gid, token, title, source, status, started_at, created_at\
             ) VALUES (1, 10, 919, 'missing-source-token', 'Missing source', 'subscription', \
                       'downloading', '2026-08-24 00:01:00', '2026-08-24 00:00:00')",
        )
        .await?;
        migrate_fingerprint_generations_up(&db).await?;

        let repo = Repo::new(db);
        let temp = tempfile::tempdir()?;
        let cache_dir = temp.path().join("eh_cache");
        std::fs::create_dir_all(&cache_dir)?;
        repo.reset_stale_eh_shared_work(60, 60).await?;
        repo.reconcile_eh_shared_job_liveness(true).await?;
        assert_eq!(
            repo.handoff_legacy_eh_archive_artifacts(&cache_dir).await?,
            1
        );
        repo.cleanup_eh_cache_orphans(&cache_dir, None).await?;

        let job = eh_gallery_jobs::Entity::find()
            .one(repo.db())
            .await?
            .expect("migration must create one shared job");
        assert_eq!(job.legacy_artifact_handoff, None);
        assert!(
            repo.get_next_eh_job_for_download().await?.is_some(),
            "a harmless missing legacy family must not block a shared worker"
        );
        Ok(())
    }

    #[tokio::test]
    async fn migration_handoff_conflict_preserves_both_families_and_blocks_workers() -> Result<()> {
        let db = new_db().await?;
        create_legacy_shared_jobs_tables(&db).await?;
        db.execute_unprepared(
            "INSERT INTO eh_download_queue (\
                 id, chat_id, gid, token, title, source, status, created_at\
             ) VALUES (1, 10, 914, 'conflict-token', 'Conflicting family', 'subscription', \
                       'downloading', '2026-08-24 00:00:00')",
        )
        .await?;
        migrate_fingerprint_generations_up(&db).await?;

        let repo = Repo::new(db);
        let job = eh_gallery_jobs::Entity::find()
            .one(repo.db())
            .await?
            .expect("migration must create one shared job");
        let temp = tempfile::tempdir()?;
        let cache_dir = temp.path().join("eh_cache");
        std::fs::create_dir_all(&cache_dir)?;
        let legacy = ArchiveArtifacts::new(cache_dir.join("914_conflict-token.zip"));
        std::fs::write(legacy.assembly_scratch(), b"legacy partial")?;
        let target = ArchiveArtifacts::new(eh_gallery_job_artifact_path(&cache_dir, &job));
        std::fs::write(target.assembly_scratch(), b"unexpected target partial")?;

        repo.reset_stale_eh_shared_work(60, 60).await?;
        repo.reconcile_eh_shared_job_liveness(true).await?;
        assert_eq!(
            repo.handoff_legacy_eh_archive_artifacts(&cache_dir).await?,
            0
        );
        assert_eq!(
            repo.handoff_legacy_eh_archive_artifacts(&cache_dir).await?,
            0,
            "a conflicting handoff must remain fail-closed across repeated startups"
        );
        repo.cleanup_eh_cache_orphans(&cache_dir, None).await?;

        let updated = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await?
            .expect("job must survive the conflict");
        assert_eq!(
            updated.legacy_artifact_handoff.as_deref(),
            Some(LEGACY_ARTIFACT_HANDOFF_PENDING)
        );
        assert!(legacy.assembly_scratch().exists());
        assert!(target.assembly_scratch().exists());
        assert!(
            repo.get_next_eh_job_for_download().await?.is_none(),
            "a worker must not create a second download while source ownership is ambiguous"
        );
        Ok(())
    }

    #[tokio::test]
    async fn migration_handoff_resumes_a_partial_rename_without_cleanup_loss() -> Result<()> {
        let db = new_db().await?;
        create_legacy_shared_jobs_tables(&db).await?;
        db.execute_unprepared(
            "INSERT INTO eh_download_queue (\
                 id, chat_id, gid, token, title, source, status, created_at\
             ) VALUES (1, 10, 915, 'moving-token', 'Interrupted handoff', 'subscription', \
                       'downloading', '2026-08-24 00:00:00')",
        )
        .await?;
        migrate_fingerprint_generations_up(&db).await?;

        let repo = Repo::new(db);
        let job = eh_gallery_jobs::Entity::find()
            .one(repo.db())
            .await?
            .expect("migration must create one shared job");
        repo.db()
            .execute_unprepared(&format!(
                "UPDATE eh_gallery_jobs SET legacy_artifact_handoff = '{LEGACY_ARTIFACT_HANDOFF_MOVING}' WHERE id = {}",
                job.id
            ))
            .await?;
        let temp = tempfile::tempdir()?;
        let cache_dir = temp.path().join("eh_cache");
        std::fs::create_dir_all(&cache_dir)?;
        let legacy = ArchiveArtifacts::new(cache_dir.join("915_moving-token.zip"));
        std::fs::write(legacy.assembly_scratch(), b"source partial")?;
        let target = ArchiveArtifacts::new(eh_gallery_job_artifact_path(&cache_dir, &job));
        std::fs::create_dir_all(target.parts_dir())?;
        std::fs::write(target.parts_dir().join("manifest.json"), b"moved first")?;

        repo.reset_stale_eh_shared_work(60, 60).await?;
        repo.reconcile_eh_shared_job_liveness(true).await?;
        assert_eq!(
            repo.handoff_legacy_eh_archive_artifacts(&cache_dir).await?,
            1
        );
        repo.cleanup_eh_cache_orphans(&cache_dir, None).await?;

        let updated = eh_gallery_jobs::Entity::find_by_id(job.id)
            .one(repo.db())
            .await?
            .expect("job must survive the partial handoff");
        assert_eq!(updated.legacy_artifact_handoff, None);
        assert!(!legacy.assembly_scratch().exists());
        assert!(target.assembly_scratch().exists());
        assert!(target.parts_dir().join("manifest.json").exists());
        Ok(())
    }

    #[tokio::test]
    async fn migration_backfills_completed_work_from_most_advanced_active_row() -> Result<()> {
        let db = new_db().await?;
        create_legacy_shared_jobs_tables(&db).await?;
        db.execute_unprepared(
            "INSERT INTO eh_download_queue (\
                id, chat_id, gid, token, title, telegraph, source, subscription_ids, \
                telegraph_subscription_ids, status, file_size, gp_cost, error, retry_count, \
                created_at, started_at, completed_at, zip_path, telegraph_url, next_retry_at, \
                archive_sent_at, telegraph_sent_at, background_download_status, \
                background_download_started_at, background_download_next_retry_at, \
                background_download_attempt_count, background_download_error, \
                telegraph_rewrite_data, telegraph_rewrite_status, telegraph_rewrite_after, \
                telegraph_rewrite_started_at, telegraph_rewrite_next_retry_at, \
                telegraph_rewrite_retry_count, telegraph_rewrite_error, telegraph_rewritten_at\
             ) VALUES \
                (1, 10, 301, 'token-301', 'Earlier pending row', 0, 'subscription', '1', NULL, 'pending', 0, 0, NULL, 0, '2026-08-01 00:00:00', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL, NULL), \
                (2, 20, 301, 'token-301', 'Later downloaded row', 0, 'subscription', '2', NULL, 'downloaded', 500, 70, NULL, 0, '2026-08-02 00:00:00', '2026-08-02 01:00:00', '2026-08-02 02:00:00', '/new/subscription.zip', NULL, NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL, NULL), \
                (3, 30, 302, 'token-302', 'Uploading row', 1, 'subscription', '3', '3', 'uploading', 80, 0, NULL, 0, '2026-08-03 00:00:00', '2026-08-03 01:00:00', '2026-08-03 02:00:00', '/new/uploading.zip', 'https://new.example/uploading', NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL, 'rw', 'pending', '2026-08-03 03:00:00', NULL, NULL, 0, NULL, NULL)",
        )
        .await?;

        migrate_shared_jobs_up(&db).await?;

        let jobs = db
            .query_all(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT gid, status, zip_path, file_size, gp_cost, completed_at, telegraph_status, telegraph_url, telegraph_rewrite_data, telegraph_rewrite_status, telegraph_rewrite_after, telegraph_rewrite_started_at, telegraph_rewrite_next_retry_at, telegraph_rewrite_retry_count, telegraph_rewrite_error, telegraph_rewritten_at FROM eh_gallery_jobs ORDER BY gid".to_owned(),
            ))
            .await?;
        assert_eq!(jobs.len(), 2);

        // Group 301: the later `downloaded` row wins over the earlier
        // `pending` row, so its completed archive backfills the shared job.
        assert_eq!(jobs[0].try_get::<i64>("", "gid")?, 301);
        assert_eq!(jobs[0].try_get::<String>("", "status")?, "downloaded");
        assert_eq!(
            jobs[0].try_get::<Option<String>>("", "zip_path")?,
            Some("/new/subscription.zip".to_owned())
        );
        assert_eq!(jobs[0].try_get::<i64>("", "file_size")?, 500);
        assert_eq!(jobs[0].try_get::<i64>("", "gp_cost")?, 70);
        assert_eq!(
            jobs[0].try_get::<Option<String>>("", "completed_at")?,
            Some("2026-08-02 02:00:00".to_owned())
        );
        assert_eq!(
            jobs[0].try_get::<String>("", "telegraph_status")?,
            "not_required",
            "group without unsent Telegraph demand stays not_required"
        );
        assert_eq!(
            jobs[0].try_get::<Option<String>>("", "telegraph_url")?,
            None
        );

        // Group 302: the `uploading` row has a ZIP on disk and a ready
        // Telegraph page with a resumable pending rewrite, so the shared job
        // becomes `downloaded` + `ready` and the rewrite stays claimable.
        assert_eq!(jobs[1].try_get::<i64>("", "gid")?, 302);
        assert_eq!(jobs[1].try_get::<String>("", "status")?, "downloaded");
        assert_eq!(
            jobs[1].try_get::<Option<String>>("", "zip_path")?,
            Some("/new/uploading.zip".to_owned())
        );
        assert_eq!(jobs[1].try_get::<i64>("", "file_size")?, 80);
        assert_eq!(jobs[1].try_get::<String>("", "telegraph_status")?, "ready");
        assert_eq!(
            jobs[1].try_get::<Option<String>>("", "telegraph_url")?,
            Some("https://new.example/uploading".to_owned())
        );
        assert_eq!(
            jobs[1].try_get::<Option<String>>("", "telegraph_rewrite_data")?,
            Some("rw".to_owned())
        );
        assert_eq!(
            jobs[1].try_get::<Option<String>>("", "telegraph_rewrite_status")?,
            Some("pending".to_owned())
        );
        assert_eq!(
            jobs[1].try_get::<Option<String>>("", "telegraph_rewrite_after")?,
            Some("2026-08-03 03:00:00".to_owned())
        );
        assert_eq!(
            jobs[1].try_get::<Option<String>>("", "telegraph_rewrite_started_at")?,
            None
        );
        assert_eq!(
            jobs[1].try_get::<Option<String>>("", "telegraph_rewrite_next_retry_at")?,
            None
        );
        assert_eq!(
            jobs[1].try_get::<i64>("", "telegraph_rewrite_retry_count")?,
            0
        );
        assert_eq!(
            jobs[1].try_get::<Option<String>>("", "telegraph_rewrite_error")?,
            None
        );
        assert_eq!(
            jobs[1].try_get::<Option<String>>("", "telegraph_rewritten_at")?,
            None
        );

        // Delivery rows are still normalized to `waiting` with cleared fields.
        let deliveries = db
            .query_all(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT id, status, zip_path, telegraph_url, file_size, gp_cost, telegraph_rewrite_data, telegraph_rewrite_status FROM eh_download_queue WHERE id IN (1, 2, 3) ORDER BY id".to_owned(),
            ))
            .await?;
        assert_eq!(deliveries.len(), 3);
        for delivery in &deliveries {
            assert_eq!(delivery.try_get::<String>("", "status")?, "waiting");
            assert_eq!(delivery.try_get::<Option<String>>("", "zip_path")?, None);
            assert_eq!(
                delivery.try_get::<Option<String>>("", "telegraph_url")?,
                None
            );
            assert_eq!(delivery.try_get::<i64>("", "file_size")?, 0);
            assert_eq!(delivery.try_get::<i64>("", "gp_cost")?, 0);
            assert_eq!(
                delivery.try_get::<Option<String>>("", "telegraph_rewrite_data")?,
                None
            );
            assert_eq!(
                delivery.try_get::<Option<String>>("", "telegraph_rewrite_status")?,
                None
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn migration_backfills_append_only_download_completions_before_clearing_compatibility(
    ) -> Result<()> {
        let db = new_db().await?;
        create_legacy_shared_jobs_tables(&db).await?;
        db.execute_unprepared(
            "INSERT INTO eh_download_queue (id, chat_id, gid, token, title, source, status, file_size, completed_at) VALUES \
                (1, 10, 201, 'active-201', 'Active 201', 'subscription', 'downloaded', 111, '2026-08-10 01:00:00'), \
                (2, 20, 202, 'active-202', 'Active 202', 'direct', 'pending', 222, '2026-08-10 02:00:00'), \
                (3, 30, 203, 'terminal-203', 'Terminal 203', 'subscription', 'done', 333, '2026-08-10 03:00:00'), \
                (4, 40, 204, 'terminal-204', 'Terminal 204', 'direct', 'canceled', 444, '2026-08-10 04:00:00'), \
                (5, 50, 205, 'zero-size', 'Zero size', 'subscription', 'done', 0, '2026-08-10 05:00:00'), \
                (6, 60, 206, 'no-completion', 'No completion', 'direct', 'failed', 555, NULL)",
        )
        .await?;

        migrate_shared_jobs_up(&db).await?;

        let completions = db
            .query_all(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT job_id, gid, file_size, created_at FROM eh_download_completions ORDER BY gid"
                    .to_owned(),
            ))
            .await?;
        assert_eq!(completions.len(), 4);
        assert_eq!(completions[0].try_get::<i64>("", "file_size")?, 111);
        assert_eq!(completions[1].try_get::<i64>("", "file_size")?, 222);
        assert_eq!(completions[2].try_get::<i64>("", "file_size")?, 333);
        assert_eq!(completions[3].try_get::<i64>("", "file_size")?, 444);
        assert_eq!(
            completions[0].try_get::<String>("", "created_at")?,
            "2026-08-10 01:00:00"
        );
        assert_eq!(
            completions[1].try_get::<String>("", "created_at")?,
            "2026-08-10 02:00:00"
        );
        assert!(completions[0]
            .try_get::<Option<i64>>("", "job_id")?
            .is_some());
        assert!(completions[1]
            .try_get::<Option<i64>>("", "job_id")?
            .is_some());
        assert_eq!(completions[2].try_get::<Option<i64>>("", "job_id")?, None);
        assert_eq!(completions[3].try_get::<Option<i64>>("", "job_id")?, None);

        let active_deliveries = db
            .query_all(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT id, job_id, file_size, completed_at FROM eh_download_queue WHERE id IN (1, 2) ORDER BY id"
                    .to_owned(),
            ))
            .await?;
        assert_eq!(
            completions[0].try_get::<Option<i64>>("", "job_id")?,
            active_deliveries[0].try_get::<Option<i64>>("", "job_id")?
        );
        assert_eq!(
            completions[1].try_get::<Option<i64>>("", "job_id")?,
            active_deliveries[1].try_get::<Option<i64>>("", "job_id")?
        );
        for delivery in active_deliveries {
            assert_eq!(delivery.try_get::<i64>("", "file_size")?, 0);
            assert_eq!(
                delivery.try_get::<Option<String>>("", "completed_at")?,
                None
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn migration_rolls_back_ddl_when_legacy_backfill_fails() -> Result<()> {
        let db = new_db().await?;
        db.execute_unprepared(
            "CREATE TABLE eh_download_queue (\
                id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, \
                gid INTEGER NOT NULL, \
                token TEXT NOT NULL, \
                title TEXT NOT NULL, \
                telegraph BOOLEAN NOT NULL DEFAULT 0, \
                telegraph_sent_at TIMESTAMP, \
                status TEXT NOT NULL DEFAULT 'pending', \
                created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP\
            )",
        )
        .await?;
        db.execute_unprepared(
            "CREATE TABLE eh_gp_spend_attempts (id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL)",
        )
        .await?;

        let error = migrate_shared_jobs_up(&db)
            .await
            .expect_err("missing source must make the legacy grouping backfill fail after DDL");
        assert!(error.to_string().contains("source"));
        assert!(
            !sqlite_master_entry_exists(&db, "table", "eh_gallery_jobs").await?,
            "failed SQLite migration must roll back the job table"
        );
        assert!(
            !sqlite_master_entry_exists(&db, "table", "eh_download_completions").await?,
            "failed SQLite migration must roll back the completion table"
        );
        assert!(
            !sqlite_master_entry_exists(&db, "index", "idx_eh_download_queue_job_id").await?,
            "failed SQLite migration must roll back dependent indexes"
        );
        assert!(
            !table_has_column(&db, "eh_download_queue", "job_id").await?,
            "failed SQLite migration must roll back the queue job_id column"
        );
        assert!(
            !table_has_column(&db, "eh_gp_spend_attempts", "job_id").await?,
            "failed SQLite migration must roll back the ledger job_id column"
        );
        Ok(())
    }

    #[tokio::test]
    async fn migration_defaults_created_at_when_omitted() -> Result<()> {
        let db = new_db().await?;
        create_legacy_queue_table(&db).await?;
        migrate_up(&db).await?;

        db.execute_unprepared("INSERT INTO eh_gp_spend_attempts (gid, gp_cost) VALUES (101, 7)")
            .await?;

        let row = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                format!("SELECT created_at FROM {TABLE} WHERE gid = 101"),
            ))
            .await?
            .expect("inserted ledger row must exist");
        let created_at: String = row.try_get("", "created_at")?;
        assert!(!created_at.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn migration_backfills_only_completed_positive_gp_attempts() -> Result<()> {
        let db = new_db().await?;
        create_legacy_queue_table(&db).await?;
        db.execute_unprepared(
            "INSERT INTO eh_download_queue (id, gid, gp_cost, completed_at) VALUES \
                (11, 101, 7, '2026-07-01 01:02:03'), \
                (12, 102, 0, '2026-07-02 01:02:03'), \
                (13, 103, 9, NULL)",
        )
        .await?;

        migrate_up(&db).await?;

        let rows = db
            .query_all(Statement::from_string(
                DbBackend::Sqlite,
                format!("SELECT queue_id, gid, gp_cost, created_at FROM {TABLE}"),
            ))
            .await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].try_get::<Option<i64>>("", "queue_id")?, Some(11));
        assert_eq!(rows[0].try_get::<i64>("", "gid")?, 101);
        assert_eq!(rows[0].try_get::<i64>("", "gp_cost")?, 7);
        assert_eq!(
            rows[0].try_get::<String>("", "created_at")?,
            "2026-07-01 01:02:03"
        );
        Ok(())
    }

    #[tokio::test]
    async fn migration_rejects_non_positive_gp_cost() -> Result<()> {
        let db = new_db().await?;
        create_legacy_queue_table(&db).await?;
        migrate_up(&db).await?;

        for gp_cost in [0, -1] {
            let result = db
                .execute_unprepared(&format!(
                    "INSERT INTO {TABLE} (gid, gp_cost, created_at) VALUES (1, {gp_cost}, '2026-07-01 01:02:03')"
                ))
                .await;
            assert!(result.is_err(), "gp_cost {gp_cost} must violate CHECK");
        }
        Ok(())
    }

    #[tokio::test]
    async fn migration_sets_queue_id_to_null_when_queue_is_deleted() -> Result<()> {
        let db = new_db().await?;
        create_legacy_queue_table(&db).await?;
        db.execute_unprepared(
            "INSERT INTO eh_download_queue (id, gid, gp_cost, completed_at) \
             VALUES (11, 101, 7, '2026-07-01 01:02:03')",
        )
        .await?;
        migrate_up(&db).await?;

        db.execute_unprepared("DELETE FROM eh_download_queue WHERE id = 11")
            .await?;

        let row = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                format!("SELECT queue_id FROM {TABLE}"),
            ))
            .await?
            .expect("backfill created one ledger row");
        assert_eq!(row.try_get::<Option<i64>>("", "queue_id")?, None);
        Ok(())
    }

    #[tokio::test]
    async fn migration_down_drops_ledger_table() -> Result<()> {
        let db = new_db().await?;
        create_legacy_queue_table(&db).await?;
        let migration = target_migration()?;
        let manager = SchemaManager::new(&db);

        migration.up(&manager).await?;
        migration.down(&manager).await?;

        if migration_table_exists(&db).await? {
            bail!("ledger table still exists after migration down");
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_schema_matches_ledger_constraints() -> Result<()> {
        let repo = setup_test_db().await?;
        let db = repo.db();
        db.execute_unprepared("PRAGMA foreign_keys = ON").await?;
        db.execute_unprepared(
            "INSERT INTO eh_download_queue (id, chat_id, gid, token, title) \
             VALUES (11, 1, 101, 'token', 'title')",
        )
        .await?;
        db.execute_unprepared(
            "INSERT INTO eh_gp_spend_attempts (queue_id, gid, gp_cost, created_at) \
             VALUES (11, 101, 7, '2026-07-01 01:02:03')",
        )
        .await?;

        assert!(db
            .execute_unprepared(
                "INSERT INTO eh_gp_spend_attempts (gid, gp_cost, created_at) \
                 VALUES (102, 0, '2026-07-01 01:02:03')"
            )
            .await
            .is_err());
        db.execute_unprepared("DELETE FROM eh_download_queue WHERE id = 11")
            .await?;

        let row = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT queue_id FROM eh_gp_spend_attempts".to_owned(),
            ))
            .await?
            .expect("ledger row survives queue deletion");
        assert_eq!(row.try_get::<Option<i64>>("", "queue_id")?, None);

        let index = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'idx_eh_gp_spend_attempts_created_at') AS present".to_owned(),
            ))
            .await?
            .expect("SELECT EXISTS returns one row");
        assert!(index.try_get::<bool>("", "present")?);

        db.execute_unprepared("INSERT INTO eh_gp_spend_attempts (gid, gp_cost) VALUES (102, 8)")
            .await?;
        let defaulted_row = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT created_at FROM eh_gp_spend_attempts WHERE gid = 102".to_owned(),
            ))
            .await?
            .expect("row with DB-provided created_at must exist");
        let created_at: String = defaulted_row.try_get("", "created_at")?;
        assert!(!created_at.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn shared_job_test_schema_enforces_variant_and_foreign_keys() -> Result<()> {
        let repo = setup_test_db().await?;
        let db = repo.db();
        let job = eh_gallery_jobs::ActiveModel {
            gid: Set(301),
            token: Set("shared-token".to_owned()),
            download_mode: Set("archive".to_owned()),
            title: Set("Shared job".to_owned()),
            ..Default::default()
        }
        .insert(db)
        .await?;
        assert_eq!(job.resolution, "");
        assert_eq!(job.status, "pending");
        assert_eq!(job.telegraph_status, "not_required");
        assert!(!job.telegraph_required);
        assert_eq!(job.file_size, 0);
        assert_eq!(job.gp_cost, 0);
        assert_eq!(job.retry_count, 0);
        assert_eq!(job.cleanup_status, "none");
        assert_eq!(job.background_download_attempt_count, 0);
        assert_eq!(job.telegraph_rewrite_retry_count, 0);

        eh_gallery_jobs::ActiveModel {
            gid: Set(301),
            token: Set("shared-token".to_owned()),
            download_mode: Set("archive".to_owned()),
            resolution: Set("large".to_owned()),
            source_fingerprint: Set(Some("fingerprint-a".to_owned())),
            title: Set("Large variant".to_owned()),
            ..Default::default()
        }
        .insert(db)
        .await?;
        assert!(
            eh_gallery_jobs::ActiveModel {
                gid: Set(301),
                token: Set("shared-token".to_owned()),
                download_mode: Set("archive".to_owned()),
                title: Set("Duplicate variant".to_owned()),
                ..Default::default()
            }
            .insert(db)
            .await
            .is_err(),
            "identical unknown-fingerprint gallery job variants must be unique"
        );

        eh_gallery_jobs::ActiveModel {
            gid: Set(301),
            token: Set("shared-token".to_owned()),
            download_mode: Set("archive".to_owned()),
            resolution: Set("large".to_owned()),
            source_fingerprint: Set(Some("fingerprint-b".to_owned())),
            title: Set("Different known generation".to_owned()),
            ..Default::default()
        }
        .insert(db)
        .await?;
        assert!(
            eh_gallery_jobs::ActiveModel {
                gid: Set(301),
                token: Set("shared-token".to_owned()),
                download_mode: Set("archive".to_owned()),
                resolution: Set("large".to_owned()),
                source_fingerprint: Set(Some("fingerprint-a".to_owned())),
                title: Set("Duplicate known generation".to_owned()),
                ..Default::default()
            }
            .insert(db)
            .await
            .is_err(),
            "identical known-fingerprint gallery job generations must be unique"
        );

        let queue = eh_download_queue::ActiveModel {
            job_id: Set(Some(job.id)),
            chat_id: Set(1),
            gid: Set(301),
            token: Set("shared-token".to_owned()),
            title: Set("Delivery".to_owned()),
            telegraph: Set(false),
            source: Set("direct".to_owned()),
            status: Set("waiting".to_owned()),
            ..Default::default()
        }
        .insert(db)
        .await?;
        let gp_attempt = eh_gp_spend_attempts::ActiveModel {
            job_id: Set(Some(job.id)),
            queue_id: Set(Some(queue.id)),
            gid: Set(301),
            gp_cost: Set(7),
            ..Default::default()
        }
        .insert(db)
        .await?;
        let completion = eh_download_completions::ActiveModel {
            job_id: Set(Some(job.id)),
            gid: Set(301),
            file_size: Set(4096),
            ..Default::default()
        }
        .insert(db)
        .await?;

        db.execute_unprepared(&format!(
            "DELETE FROM eh_gallery_jobs WHERE id = {}",
            job.id
        ))
        .await?;

        assert_eq!(
            eh_download_queue::Entity::find_by_id(queue.id)
                .one(db)
                .await?
                .expect("queue delivery survives job deletion")
                .job_id,
            None
        );
        assert_eq!(
            eh_gp_spend_attempts::Entity::find_by_id(gp_attempt.id)
                .one(db)
                .await?
                .expect("GP ledger entry survives job deletion")
                .job_id,
            None
        );
        let completion_after_delete = eh_download_completions::Entity::find_by_id(completion.id)
            .one(db)
            .await?
            .expect("completion ledger entry survives job deletion");
        assert_eq!(completion_after_delete.job_id, None);
        assert_eq!(completion_after_delete.file_size, 4096);
        Ok(())
    }

    #[tokio::test]
    async fn append_eh_gp_spend_attempt_inserts_positive_attempt() -> Result<()> {
        let repo = setup_test_db().await?;
        let queue = repo
            .enqueue_eh_download(
                1,
                101,
                "token",
                "title",
                false,
                "direct",
                &crate::db::repo::eh_gallery_jobs::EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await?
            .expect("delivery should be enqueued");

        let attempt = repo
            .append_eh_gp_spend_attempt(queue.id, queue.gid, 218)
            .await?;

        assert_eq!(attempt.queue_id, Some(queue.id));
        assert_eq!(attempt.gid, queue.gid);
        assert_eq!(attempt.gp_cost, 218);
        assert!(attempt.created_at <= Local::now().naive_local());

        let rows = eh_gp_spend_attempts::Entity::find().all(repo.db()).await?;
        assert_eq!(rows, vec![attempt]);
        Ok(())
    }

    #[tokio::test]
    async fn append_eh_gp_spend_attempt_keeps_each_attempt_for_a_queue() -> Result<()> {
        let repo = setup_test_db().await?;
        let queue = repo
            .enqueue_eh_download(
                1,
                102,
                "token",
                "title",
                false,
                "direct",
                &crate::db::repo::eh_gallery_jobs::EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await?
            .expect("delivery should be enqueued");

        let first = repo
            .append_eh_gp_spend_attempt(queue.id, queue.gid, 218)
            .await?;
        let second = repo
            .append_eh_gp_spend_attempt(queue.id, queue.gid, 218)
            .await?;

        assert_ne!(first.id, second.id);
        let rows = eh_gp_spend_attempts::Entity::find().all(repo.db()).await?;
        assert_eq!(rows.len(), 2);
        assert_eq!(repo.get_eh_gp_cost_in_window(24).await?, 436);
        Ok(())
    }

    #[tokio::test]
    async fn get_eh_gp_cost_in_window_excludes_old_attempts() -> Result<()> {
        let repo = setup_test_db().await?;
        eh_gp_spend_attempts::ActiveModel {
            queue_id: Set(None),
            gid: Set(103),
            gp_cost: Set(218),
            created_at: Set(Local::now().naive_local() - Duration::hours(25)),
            ..Default::default()
        }
        .insert(repo.db())
        .await?;
        eh_gp_spend_attempts::ActiveModel {
            queue_id: Set(None),
            gid: Set(104),
            gp_cost: Set(7),
            created_at: Set(Local::now().naive_local()),
            ..Default::default()
        }
        .insert(repo.db())
        .await?;

        assert_eq!(repo.get_eh_gp_cost_in_window(24).await?, 7);
        Ok(())
    }

    #[tokio::test]
    async fn get_eh_gp_cost_in_window_rejects_extreme_windows_without_panicking() -> Result<()> {
        let repo = setup_test_db().await?;

        for window_hours in [3_000_000_000, i64::MAX as u64, u64::MAX] {
            let result = repo.get_eh_gp_cost_in_window(window_hours).await;
            assert!(
                result.is_err(),
                "window_hours={window_hours} must return an error rather than panic"
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn append_eh_gp_spend_attempt_rejects_non_positive_costs() -> Result<()> {
        let repo = setup_test_db().await?;

        for gp_cost in [0, -1] {
            let error = repo
                .append_eh_gp_spend_attempt(1, 105, gp_cost)
                .await
                .expect_err("non-positive GP cost must be rejected before insertion");
            assert!(
                error.to_string().contains("positive"),
                "unexpected error for {gp_cost}: {error:#}"
            );
        }

        let rows = eh_gp_spend_attempts::Entity::find().all(repo.db()).await?;
        assert!(rows.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn get_eh_gp_cost_in_window_reads_only_the_ledger() -> Result<()> {
        let repo = setup_test_db().await?;
        let queue = repo
            .enqueue_eh_download(
                1,
                106,
                "token",
                "title",
                false,
                "direct",
                &crate::db::repo::eh_gallery_jobs::EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await?
            .expect("delivery should be enqueued");
        let queue_id = queue.id;
        let mut queue: eh_download_queue::ActiveModel = queue.into();
        queue.gp_cost = Set(218);
        queue.completed_at = Set(Some(Local::now().naive_local()));
        queue.update(repo.db()).await?;

        assert_eq!(repo.get_eh_gp_cost_in_window(24).await?, 0);

        repo.append_eh_gp_spend_attempt(queue_id, 106, 7).await?;

        assert_eq!(repo.get_eh_gp_cost_in_window(24).await?, 7);
        Ok(())
    }
}
