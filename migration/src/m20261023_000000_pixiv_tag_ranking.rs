use sea_orm::{ConnectionTrait, TransactionTrait};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let txn = manager.get_connection().begin().await?;
        txn.execute_unprepared(
            "CREATE TABLE pixiv_tag_settings (
                subscription_id INTEGER PRIMARY KEY REFERENCES subscriptions(id) ON DELETE CASCADE,
                min_bookmarks INTEGER NOT NULL CHECK(min_bookmarks > 0)
            );
            CREATE TABLE pixiv_tag_scans (
                task_id INTEGER PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
                cursor_date TEXT NOT NULL,
                cursor_offset INTEGER NOT NULL DEFAULT 0,
                window_days INTEGER NOT NULL,
                pages_scanned INTEGER NOT NULL DEFAULT 0,
                total_pages INTEGER NOT NULL DEFAULT 0,
                complete BOOLEAN NOT NULL DEFAULT 0
            );
            CREATE TABLE pixiv_tag_candidates (
                task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
                illust_id INTEGER NOT NULL,
                illust TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY(task_id, illust_id)
            );
            CREATE TABLE pixiv_tag_deliveries (
                subscription_id INTEGER NOT NULL REFERENCES subscriptions(id) ON DELETE CASCADE,
                illust_id INTEGER NOT NULL,
                illust TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                next_attempt_at INTEGER NOT NULL,
                done BOOLEAN NOT NULL DEFAULT 0,
                PRIMARY KEY(subscription_id, illust_id)
            );
            CREATE INDEX idx_pixiv_tag_delivery_due
                ON pixiv_tag_deliveries(subscription_id, done, next_attempt_at);
            CREATE TABLE pixiv_push_history (
                chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
                illust_id INTEGER NOT NULL,
                sent_pages TEXT NOT NULL DEFAULT '[]',
                done BOOLEAN NOT NULL DEFAULT 0,
                PRIMARY KEY(chat_id, illust_id)
            );

            -- Import existing author pushes when enabling shared chat history.
            -- Ranking and other services' message IDs are deliberately excluded.
            INSERT INTO pixiv_push_history(chat_id, illust_id, done)
            SELECT DISTINCT m.chat_id, m.illust_id, 1
            FROM messages m JOIN subscriptions s ON s.id=m.subscription_id
            JOIN tasks t ON t.id=s.task_id JOIN chats c ON c.id=m.chat_id
            WHERE t.type='author' AND m.illust_id IS NOT NULL
              AND COALESCE(json_extract(s.latest_data, '$.state.pending_illust.illust_id'), -1)<>m.illust_id;

            -- A message for a pending work only acknowledges its saved pages.
            INSERT INTO pixiv_push_history(chat_id, illust_id, sent_pages)
            SELECT s.chat_id, json_extract(s.latest_data, '$.state.pending_illust.illust_id'),
                   json_group_array(DISTINCT j.value)
            FROM subscriptions s JOIN tasks t ON t.id=s.task_id,
                 json_each(json_extract(s.latest_data, '$.state.pending_illust.sent_pages')) j
            WHERE t.type='author' AND json_extract(s.latest_data, '$.type')='Author'
              AND json_extract(s.latest_data, '$.state.pending_illust.illust_id') IS NOT NULL
            GROUP BY s.chat_id, json_extract(s.latest_data, '$.state.pending_illust.illust_id')
            ON CONFLICT(chat_id, illust_id) DO NOTHING;",
        )
        .await?;
        txn.commit().await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let txn = manager.get_connection().begin().await?;
        txn.execute_unprepared(
            "DROP TABLE pixiv_tag_deliveries;
             DROP TABLE pixiv_tag_candidates;
             DROP TABLE pixiv_tag_scans;
             DROP TABLE pixiv_tag_settings;
             DROP TABLE pixiv_push_history;",
        )
        .await?;
        txn.commit().await
    }
}
