use super::Repo;
use crate::db::entities::tasks;
use crate::db::types::TaskType;
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use sea_orm::{
    sea_query::OnConflict, ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel,
    QueryFilter, QueryOrder, QuerySelect, Set,
};

impl Repo {
    pub async fn get_task_by_type_value(
        &self,
        task_type: TaskType,
        value: &str,
    ) -> Result<Option<tasks::Model>> {
        tasks::Entity::find()
            .filter(tasks::Column::Type.eq(task_type))
            .filter(tasks::Column::Value.eq(value))
            .one(&self.db)
            .await
            .context("Failed to find task by type and value")
    }

    pub async fn get_or_create_task(
        &self,
        task_type: TaskType,
        value: String,
        author_name: Option<String>,
    ) -> Result<tasks::Model> {
        let next_poll = Local::now() + chrono::Duration::seconds(60);

        let new_task = tasks::ActiveModel {
            r#type: Set(task_type),
            value: Set(value.clone()),
            next_poll_at: Set(next_poll.naive_local()),
            last_polled_at: Set(None),
            author_name: Set(author_name.clone()),
            ..Default::default()
        };

        // On conflict (same type+value), do NOT overwrite author_name.
        // The first subscriber's display_name should be preserved;
        // otherwise later subscribers could overwrite it for all chats.
        let conflict_handler = OnConflict::columns([tasks::Column::Type, tasks::Column::Value])
            .update_column(tasks::Column::Value)
            .to_owned();

        tasks::Entity::insert(new_task)
            .on_conflict(conflict_handler)
            .exec_without_returning(&self.db)
            .await
            .context("Failed to upsert task")?;

        self.get_task_by_type_value(task_type, &value)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Task with value {} not found after upsert", value))
    }

    pub async fn get_pending_tasks_by_type(
        &self,
        task_type: TaskType,
        limit: u64,
    ) -> Result<Vec<tasks::Model>> {
        let now = Local::now().naive_local();

        tasks::Entity::find()
            .filter(tasks::Column::NextPollAt.lte(now))
            .filter(tasks::Column::Type.eq(task_type))
            .order_by_asc(tasks::Column::NextPollAt)
            .limit(limit)
            .all(&self.db)
            .await
            .context("Failed to get pending tasks by type")
    }

    pub async fn get_all_tasks_by_type(&self, task_type: TaskType) -> Result<Vec<tasks::Model>> {
        tasks::Entity::find()
            .filter(tasks::Column::Type.eq(task_type))
            .order_by_asc(tasks::Column::Id)
            .all(&self.db)
            .await
            .context("Failed to get all tasks by type")
    }

    pub async fn update_task_after_poll(
        &self,
        task_id: i32,
        next_poll_at: DateTime<Local>,
    ) -> Result<tasks::Model> {
        let task = tasks::Entity::find_by_id(task_id)
            .one(&self.db)
            .await
            .context("Failed to query task")?
            .ok_or_else(|| anyhow::anyhow!("Task {} not found", task_id))?;

        let now = Local::now().naive_local();
        let mut active: tasks::ActiveModel = task.into_active_model();
        active.next_poll_at = Set(next_poll_at.naive_local());
        active.last_polled_at = Set(Some(now));

        active
            .update(&self.db)
            .await
            .context("Failed to update task after poll")
    }

    pub async fn update_task_author_name(
        &self,
        task_id: i32,
        author_name: Option<String>,
    ) -> Result<tasks::Model> {
        let task = tasks::Entity::find_by_id(task_id)
            .one(&self.db)
            .await
            .context("Failed to query task")?
            .ok_or_else(|| anyhow::anyhow!("Task {} not found", task_id))?;

        let mut active: tasks::ActiveModel = task.into_active_model();
        active.author_name = Set(author_name);

        active
            .update(&self.db)
            .await
            .context("Failed to update task author_name")
    }

    pub async fn delete_task(&self, task_id: i32) -> Result<()> {
        tasks::Entity::delete_by_id(task_id)
            .exec(&self.db)
            .await
            .context("Failed to delete task")?;
        Ok(())
    }
}
