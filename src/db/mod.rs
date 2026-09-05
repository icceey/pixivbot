//! Database module
pub mod entities;
pub mod repo;
pub mod types;

use anyhow::Result;
use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use std::time::Duration;
use tracing::info;

/// WAL lets readers proceed while a writer commits, and NORMAL fsync cadence
/// keeps write locks short enough for the concurrent scheduler engines sharing
/// this SQLite file. The busy timeout absorbs remaining writer contention
/// instead of surfacing `(code: 5) database is locked` to the EH engines.
const SQLITE_BUSY_TIMEOUT_MS: u64 = 30_000;

pub async fn establish_connection(database_url: &str) -> Result<DatabaseConnection> {
    let mut opt = ConnectOptions::new(database_url);
    opt.max_connections(100)
        .min_connections(5)
        .connect_timeout(Duration::from_secs(8))
        .acquire_timeout(Duration::from_secs(8))
        .idle_timeout(Duration::from_secs(8))
        .max_lifetime(Duration::from_secs(8));
    if opt.get_url().starts_with("sqlite:") {
        opt.map_sqlx_sqlite_opts(|sqlx_opt| {
            sqlx_opt
                .journal_mode(sea_orm::sqlx::sqlite::SqliteJournalMode::Wal)
                .synchronous(sea_orm::sqlx::sqlite::SqliteSynchronous::Normal)
                .busy_timeout(Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS))
        });
    }

    let connection = Database::connect(opt).await?;
    info!("Connected to database: {}", database_url);

    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sqlite_pool_spinup_keeps_wal_across_concurrent_connections() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite:{}?mode=rwc", dir.path().join("spinup.db").display());
        // Repeated pool spin-ups race the first WAL switch: every connection
        // issues `PRAGMA journal_mode = WAL` while others may hold the file.
        for _ in 0..8 {
            establish_connection(&url).await.unwrap();
        }
        let db = establish_connection(&url).await.unwrap();
        let mut conn = db.get_sqlite_connection_pool().acquire().await.unwrap();
        let journal_mode: String = sea_orm::sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
    }

    #[tokio::test]
    async fn sqlite_existing_delete_mode_database_upgrades_to_wal_without_data_loss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        let url = format!("sqlite:{}?mode=rwc", path.display());
        {
            let db = Database::connect(&url).await.unwrap();
            let mut conn = db.get_sqlite_connection_pool().acquire().await.unwrap();
            sea_orm::sqlx::query("CREATE TABLE legacy(x INTEGER PRIMARY KEY)")
                .execute(&mut *conn)
                .await
                .unwrap();
            sea_orm::sqlx::query("INSERT INTO legacy(x) VALUES (41)")
                .execute(&mut *conn)
                .await
                .unwrap();
            let mode: String = sea_orm::sqlx::query_scalar("PRAGMA journal_mode")
                .fetch_one(&mut *conn)
                .await
                .unwrap();
            assert_eq!(mode.to_ascii_lowercase(), "delete");
        }
        // Production upgrade path: the pool must flip the existing file to WAL.
        let db = establish_connection(&url).await.unwrap();
        let mut conn = db.get_sqlite_connection_pool().acquire().await.unwrap();
        let mode: String = sea_orm::sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        let value: i64 = sea_orm::sqlx::query_scalar("SELECT x FROM legacy")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(value, 41);
    }
}
