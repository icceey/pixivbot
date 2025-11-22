pub mod entities;
pub mod repo;

pub use entities::*;
pub use repo::*;

use sea_orm::{Database, DatabaseConnection};
use crate::config::DatabaseConfig;

pub async fn establish_connection(config: &DatabaseConfig) -> Result<DatabaseConnection, crate::error::AppError> {
    let db = Database::connect(&config.url).await?;
    Ok(db)
}