use sea_orm::{ConnectionTrait, TransactionTrait};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let txn = manager.get_connection().begin().await?;
        // Completed snapshots may be empty; older history falls back to its
        // latest acknowledgement, or migration time when no timestamp survives.
        txn.execute_unprepared(
            "ALTER TABLE pixiv_push_history ADD COLUMN published_at INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE pixiv_push_history ADD COLUMN author_id INTEGER;
             ALTER TABLE pixiv_tag_deliveries ADD COLUMN published_at INTEGER NOT NULL DEFAULT 0;
             UPDATE pixiv_tag_deliveries AS d SET published_at=COALESCE(
                 unixepoch(json_extract(d.illust, '$.create_date')),
                 (SELECT MAX(created_at) FROM pixiv_tag_candidates WHERE illust_id=d.illust_id),
                 (SELECT MAX(unixepoch(m.created_at, 'utc')) FROM messages m
                  JOIN subscriptions s ON s.chat_id=m.chat_id
                  WHERE s.id=d.subscription_id AND m.illust_id=d.illust_id), unixepoch());
             UPDATE pixiv_push_history AS h SET published_at=COALESCE(
                 (SELECT MAX(created_at) FROM pixiv_tag_candidates WHERE illust_id=h.illust_id),
                 (SELECT MAX(unixepoch(json_extract(illust, '$.create_date')))
                  FROM pixiv_tag_deliveries WHERE illust_id=h.illust_id),
                 (SELECT MAX(unixepoch(created_at, 'utc')) FROM messages
                  WHERE chat_id=h.chat_id AND illust_id=h.illust_id), unixepoch());
             UPDATE pixiv_push_history AS h SET author_id=COALESCE(
                 (SELECT json_extract(illust, '$.user.id') FROM pixiv_tag_candidates
                  WHERE illust_id=h.illust_id LIMIT 1),
                 (SELECT MAX(json_extract(illust, '$.user.id')) FROM pixiv_tag_deliveries
                  WHERE illust_id=h.illust_id),
                 (SELECT CAST(t.value AS INTEGER) FROM messages m
                  JOIN subscriptions s ON s.id=m.subscription_id
                  JOIN tasks t ON t.id=s.task_id
                  WHERE m.illust_id=h.illust_id AND t.type='author' LIMIT 1));
             CREATE INDEX idx_pixiv_history_expiry ON pixiv_push_history(published_at);
             CREATE INDEX idx_pixiv_tag_delivery_expiry ON pixiv_tag_deliveries(published_at) WHERE done=1;",
        )
        .await?;
        txn.commit().await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let txn = manager.get_connection().begin().await?;
        txn.execute_unprepared(
            "DROP INDEX idx_pixiv_history_expiry;
             DROP INDEX idx_pixiv_tag_delivery_expiry;
             ALTER TABLE pixiv_push_history DROP COLUMN published_at;
             ALTER TABLE pixiv_push_history DROP COLUMN author_id;
             ALTER TABLE pixiv_tag_deliveries DROP COLUMN published_at;",
        )
        .await?;
        txn.commit().await
    }
}
