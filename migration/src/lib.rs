pub use sea_orm_migration::prelude::*;

mod m20251125_000001_create_tables;
mod m20251127_000001_add_subscription_latest_data;
mod m20251127_000002_remove_task_latest_data;
mod m20251128_000001_remove_task_updated_by;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20251125_000001_create_tables::Migration),
            Box::new(m20251127_000001_add_subscription_latest_data::Migration),
            Box::new(m20251127_000002_remove_task_latest_data::Migration),
            Box::new(m20251128_000001_remove_task_updated_by::Migration),
        ]
    }
}
