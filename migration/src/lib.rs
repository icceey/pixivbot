pub use sea_orm_migration::prelude::*;

mod m20251128_000001_init;
mod m20251202_000001_add_chat_sensitive_tags;
mod m20251205_000001_convert_null_tags_to_default;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20251128_000001_init::Migration),
            Box::new(m20251202_000001_add_chat_sensitive_tags::Migration),
            Box::new(m20251205_000001_convert_null_tags_to_default::Migration),
        ]
    }
}
