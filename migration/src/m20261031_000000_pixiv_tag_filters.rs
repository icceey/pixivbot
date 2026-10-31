use sea_orm::{ConnectionTrait, TransactionTrait};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let txn = manager.get_connection().begin().await?;
        txn.execute_unprepared(
            "CREATE TABLE pixiv_tag_settings_new (
                subscription_id INTEGER PRIMARY KEY REFERENCES subscriptions(id) ON DELETE CASCADE,
                min_bookmarks INTEGER CHECK(min_bookmarks > 0),
                min_views INTEGER CHECK(min_views > 0),
                min_bookmark_rate REAL CHECK(min_bookmark_rate BETWEEN 0 AND 1),
                max_pages INTEGER CHECK(max_pages > 0),
                CHECK(min_views IS NOT NULL OR min_bookmarks IS NOT NULL)
            );
            INSERT INTO pixiv_tag_settings_new(subscription_id, min_bookmarks, max_pages)
                SELECT subscription_id, min_bookmarks, max_pages FROM pixiv_tag_settings;
            DROP TABLE pixiv_tag_settings;
            ALTER TABLE pixiv_tag_settings_new RENAME TO pixiv_tag_settings;",
        )
        .await?;
        txn.commit().await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "Pixiv tag filter migration is irreversible".to_string(),
        ))
    }
}
