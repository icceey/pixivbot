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
