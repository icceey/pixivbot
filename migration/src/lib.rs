pub use sea_orm_migration::prelude::*;

mod m20251206_000001_init_database;
mod m20251219_000001_add_messages_table;
mod m20260116_000000_add_allow_without_mention;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20251206_000001_init_database::Migration),
            Box::new(m20251219_000001_add_messages_table::Migration),
            Box::new(m20260116_000000_add_allow_without_mention::Migration),
        ]
    }
}
