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
    use anyhow::{bail, Result};
    use chrono::{Duration, Local};
    use migration::{MigrationTrait, Migrator, MigratorTrait, SchemaManager};
    use sea_orm::{
        ActiveModelTrait, ConnectionTrait, Database, DatabaseConnection, DbBackend, EntityTrait,
        Set, Statement,
    };

    const TABLE: &str = "eh_gp_spend_attempts";
    const CREATED_AT_INDEX: &str = "idx_eh_gp_spend_attempts_created_at";
    const MIGRATION_NAME: &str = "m20260719_000000_eh_gp_spend_attempts";
    const SHARED_JOBS_MIGRATION_NAME: &str = "m20260824_000000_eh_shared_gallery_jobs";
    const REUSE_LEDGER_MIGRATION_NAME: &str = "m20260826_000000_eh_result_reuse_and_push_ledger";

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
            "identical gallery job variants must be unique"
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
