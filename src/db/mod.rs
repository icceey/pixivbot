//! Database module
pub mod entities;
pub mod repo;

use sea_orm::{Database, DatabaseConnection, ConnectOptions};
use crate::error::AppResult;
use tracing::info;
use std::time::Duration;

pub async fn establish_connection(database_url: &str) -> AppResult<DatabaseConnection> {
    let mut opt = ConnectOptions::new(database_url);
    opt.max_connections(100)
        .min_connections(5)
        .connect_timeout(Duration::from_secs(8))
        .acquire_timeout(Duration::from_secs(8))
        .idle_timeout(Duration::from_secs(8))
        .max_lifetime(Duration::from_secs(8));

    let connection = Database::connect(opt).await?;
    info!("Connected to database: {}", database_url);
    
    Ok(connection)
}
