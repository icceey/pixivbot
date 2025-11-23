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

    // Initialise database schema
    // This example uses a simplified approach. In production, use migrations!
    create_table_if_not_exists(&connection).await?;
    
    Ok(connection)
}

async fn create_table_if_not_exists(db: &DatabaseConnection) -> AppResult<()> {
    use sea_orm::{Schema, ConnectionTrait};
    
    let builder = db.get_database_backend();
    let schema = Schema::new(builder);

    let stmt = schema.create_table_from_entity(entities::users::Entity).if_not_exists().to_owned();
    let _ = db.execute(builder.build(&stmt)).await;

    let stmt = schema.create_table_from_entity(entities::chats::Entity).if_not_exists().to_owned();
    let _ = db.execute(builder.build(&stmt)).await;

    let stmt = schema.create_table_from_entity(entities::tasks::Entity).if_not_exists().to_owned();
    let _ = db.execute(builder.build(&stmt)).await;
    
    let stmt = schema.create_table_from_entity(entities::subscriptions::Entity).if_not_exists().to_owned();
    let _ = db.execute(builder.build(&stmt)).await;

    info!("Database tables initialized.");
    Ok(())
}
