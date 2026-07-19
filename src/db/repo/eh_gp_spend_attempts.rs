use super::Repo;
use crate::db::entities::eh_gp_spend_attempts;
use anyhow::{Context, Result};
use chrono::{Duration, Local};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

impl Repo {
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
    use crate::db::entities::{eh_download_queue, eh_gp_spend_attempts};
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
    async fn append_eh_gp_spend_attempt_inserts_positive_attempt() -> Result<()> {
        let repo = setup_test_db().await?;
        let queue = repo
            .enqueue_eh_download(1, 101, "token", "title", false, "direct")
            .await?;

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
            .enqueue_eh_download(1, 102, "token", "title", false, "direct")
            .await?;

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
            .enqueue_eh_download(1, 106, "token", "title", false, "direct")
            .await?;
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
