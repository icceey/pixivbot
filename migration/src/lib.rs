pub use sea_orm_migration::prelude::*;

mod m20251206_000001_init_database;
mod m20251219_000001_add_messages_table;
mod m20260116_000000_add_allow_without_mention;
mod m20260410_000000_add_booru_filter;
mod m20260421_000000_refactor_booru_task_value;
mod m20260626_000000_add_ehentai;
mod m20260628_000000_eh_pipeline_decouple;
mod m20260628_000100_eh_unique_constraint;
mod m20260629_000000_eh_review_fixes;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20251206_000001_init_database::Migration),
            Box::new(m20251219_000001_add_messages_table::Migration),
            Box::new(m20260116_000000_add_allow_without_mention::Migration),
            Box::new(m20260410_000000_add_booru_filter::Migration),
            Box::new(m20260421_000000_refactor_booru_task_value::Migration),
            Box::new(m20260626_000000_add_ehentai::Migration),
            Box::new(m20260628_000000_eh_pipeline_decouple::Migration),
            Box::new(m20260628_000100_eh_unique_constraint::Migration),
            Box::new(m20260629_000000_eh_review_fixes::Migration),
        ]
    }
}
