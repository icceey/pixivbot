use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Convert NULL filter_tags to default empty TagFilter JSON
        // TagFilter default: {"include":[],"exclude":[]}
        db.execute_unprepared(
            r#"UPDATE subscriptions SET filter_tags = '{"include":[],"exclude":[]}' WHERE filter_tags IS NULL"#,
        )
        .await?;

        // Convert NULL excluded_tags to default empty Tags JSON
        // Tags default: []
        db.execute_unprepared(
            r#"UPDATE chats SET excluded_tags = '[]' WHERE excluded_tags IS NULL"#,
        )
        .await?;

        // Convert NULL sensitive_tags to default empty Tags JSON
        // Tags default: []
        db.execute_unprepared(
            r#"UPDATE chats SET sensitive_tags = '[]' WHERE sensitive_tags IS NULL"#,
        )
        .await?;

        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // This migration only converts NULL to default values.
        // Rolling back would require knowing which rows were originally NULL,
        // which we don't track. The down migration is a no-op.
        // The default values are functionally equivalent to NULL for filtering purposes.
        Ok(())
    }
}
