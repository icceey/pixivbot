pub use sea_orm_migration::prelude::*;

mod m20250123_000001_create_tables;
mod m20251124_000001_add_author_name;
mod m20251124_000002_add_chat_content_settings;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20250123_000001_create_tables::Migration),
            Box::new(m20251124_000001_add_author_name::Migration),
            Box::new(m20251124_000002_add_chat_content_settings::Migration),
        ]
    }
}
