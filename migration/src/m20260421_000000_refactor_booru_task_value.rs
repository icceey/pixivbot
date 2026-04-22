use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(Debug, FromQueryResult)]
struct BooruSubRow {
    sub_id: i32,
    task_id: i32,
    task_value: String,
    booru_filter: Option<String>,
}

#[derive(Debug, FromQueryResult)]
struct IdRow {
    id: i32,
}

#[derive(Debug, FromQueryResult)]
struct AuthorRow {
    author_name: Option<String>,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let backend = db.get_database_backend();

        let rows = BooruSubRow::find_by_statement(Statement::from_string(
            backend,
            r#"
            SELECT s.id as sub_id,
                   s.task_id as task_id,
                   t.value as task_value,
                   CAST(s.booru_filter AS TEXT) as booru_filter
            FROM subscriptions s
            JOIN tasks t ON s.task_id = t.id
            WHERE t.type = 'booru_tag'
            "#
            .to_string(),
        ))
        .all(db)
        .await?;

        let mut touched_old_task_ids: Vec<i32> = Vec::new();

        for row in rows {
            let sig = compute_signature(row.booru_filter.as_deref());
            let new_value = if sig.is_empty() {
                row.task_value.clone()
            } else {
                format!("{}|f={}", row.task_value, sig)
            };

            if new_value == row.task_value {
                continue;
            }

            let author = AuthorRow::find_by_statement(Statement::from_sql_and_values(
                backend,
                "SELECT author_name FROM tasks WHERE id = ?",
                [row.task_id.into()],
            ))
            .one(db)
            .await?
            .and_then(|r| r.author_name);

            let target_id =
                find_or_create_booru_tag_task(db, backend, &new_value, author.as_deref()).await?;

            db.execute(Statement::from_sql_and_values(
                backend,
                "UPDATE subscriptions SET task_id = ? WHERE id = ?",
                [target_id.into(), row.sub_id.into()],
            ))
            .await?;

            if !touched_old_task_ids.contains(&row.task_id) {
                touched_old_task_ids.push(row.task_id);
            }
        }

        db.execute(Statement::from_string(
            backend,
            r#"
            DELETE FROM tasks
            WHERE type = 'booru_tag'
              AND id NOT IN (SELECT DISTINCT task_id FROM subscriptions)
            "#
            .to_string(),
        ))
        .await?;

        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

fn compute_signature(json: Option<&str>) -> String {
    let Some(s) = json else {
        return String::new();
    };
    let v: serde_json::Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let mut sig = String::new();
    if v.get("score_min").map(|x| !x.is_null()).unwrap_or(false) {
        sig.push('s');
    }
    if v.get("fav_count_min")
        .map(|x| !x.is_null())
        .unwrap_or(false)
    {
        sig.push('f');
    }
    if v.get("allowed_ratings")
        .and_then(|x| x.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false)
    {
        sig.push('r');
    }
    sig
}

async fn find_or_create_booru_tag_task(
    db: &SchemaManagerConnection<'_>,
    backend: sea_orm::DatabaseBackend,
    value: &str,
    author_name: Option<&str>,
) -> Result<i32, DbErr> {
    if let Some(row) = IdRow::find_by_statement(Statement::from_sql_and_values(
        backend,
        "SELECT id FROM tasks WHERE type = 'booru_tag' AND value = ?",
        [value.into()],
    ))
    .one(db)
    .await?
    {
        return Ok(row.id);
    }

    let author_val: sea_orm::Value = match author_name {
        Some(s) => s.into(),
        None => sea_orm::Value::String(None),
    };
    db.execute(Statement::from_sql_and_values(
        backend,
        "INSERT INTO tasks (type, value, author_name, next_poll_at) VALUES ('booru_tag', ?, ?, CURRENT_TIMESTAMP)",
        [value.into(), author_val],
    ))
    .await?;

    let row = IdRow::find_by_statement(Statement::from_sql_and_values(
        backend,
        "SELECT id FROM tasks WHERE type = 'booru_tag' AND value = ?",
        [value.into()],
    ))
    .one(db)
    .await?
    .ok_or_else(|| DbErr::Custom("Inserted booru_tag task not found".into()))?;

    Ok(row.id)
}
