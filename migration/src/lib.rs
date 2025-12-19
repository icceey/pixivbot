pub use sea_orm_migration::prelude::*;

mod m20251206_000001_init_database;
mod m20251219_000001_add_messages_table;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20251206_000001_init_database::Migration),
            Box::new(m20251219_000001_add_messages_table::Migration),
        ]
    }
}
